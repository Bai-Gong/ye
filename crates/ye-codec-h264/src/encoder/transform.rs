//! 前向变换与量化（M3 施工图 T3）——解码侧反变换/反量化算术的精确逆设计。
//!
//! 解码侧的算术是冻结面（本 crate 逐字对齐 FFmpeg），编码侧不允许另立
//! 山头：本模块的每个公式都从解码侧对应算术**反推**得出，本地解码环
//! （`frame.rs`）再把量化后的 level 用解码侧同一公式重建，回环位级一致
//! 因此由构造保证（黑盒回环测试兜底）。
//!
//! ## 前向 4x4 整数 DCT（[`fwd_dct4x4`]）
//!
//! 解码侧反变换（`macroblock.rs` `idct4x4_add`）两遍蝶形等价于
//! `M·X·M^T >> 6`，其中
//! `M = [[1,1,1,½],[1,½,−1,−1],[1,−½,−1,1],[1,−1,1,−½]]`（列主
//! 展开即蝶形：z0=b0+b8、z1=b0−b8、z2=b4/2−b12、z3=b4+b12/2）。
//! 无量化时重建 `x̂ = M·X′·M^T/64` 要等于残差 r，须
//! `X′ = 64·M⁻¹·r·M⁻ᵀ`；对 `M⁻¹` 精确求解得 `M⁻¹ = N/20`，N 为整数阵
//! （见 [`N_ROWS`]），故理想系数量 `X′ = (4/25)·N·r·Nᵀ` 全程整数可算。
//! [`fwd_dct4x4`] 即产出 `X″ = N·r·Nᵀ`（×25/4 的理想值），量化分母
//! 相应带上 25/4（见 [`quant_ac`]）。N = diag(5,4,5,4)·Cf（Cf 为
//! spec 8.5.2 前向核心矩阵），偶频率行/列缩放 5、奇频率缩放 4。
//!
//! ## 量化（逐槽位解码反量化的逆）
//!
//! 解码侧 AC 反量化（`cavlc.rs` `read_residual` 的 store）：系数 =
//! `(level·qmul[slot] + 32) >> 6`，`qmul = V[class]·64·2^(qp/6)`。逆：
//! [`quant_ac`] 取 `level = round(X″·256 / (25·qmul))`——V 表在分子
//! 分母间完全消去（`level·V·2^s = (4/25)·X″` 与 V 无关），类间差异
//! 只体现为舍入粒度（质量），不引入系统性偏差。
//!
//! ## DC Hadamard 路径
//!
//! - luma DC（Intra16x16）：解码侧 [`crate::macroblock::luma_dc_dequant`]
//!   为 `(H·L·H·qmul + 128) >> 8`（H 为 4x4 Hadamard）。块 b 的目标
//!   DC 系数 = `(4/25)·X″[0][0] = 4·Σr_b`（DC 项 N 缩放 5×5=25），故
//!   [`fwd_hadamard4x4`] 输入 `4·Σr_b`、输出 `T = H·D·H`，
//!   [`quant_luma_dc`] 取 `level = round(16·T/qmul)`。
//! - chroma DC：解码侧 [`crate::macroblock::chroma_dc_dequant`] 为
//!   `(H₂·L·H₂·qmul) >> 7`（H₂ 为 2x2 Hadamard），同理
//!   [`quant_chroma_dc`] 取 `level = round(32·T/qmul)`，T 由
//!   [`fwd_hadamard2x2`] 产出。
//!
//! 模式决策代价：[`satd_4x4`]（Cf 域 l1 范数）与 [`sad`]，口径只影响
//! 压缩质量不影响合法性，位级一致由上述量化/重建构造保证。

use crate::error::H264Error;

/// 前向核心矩阵 N = diag(5,4,5,4)·Cf（行主序；推导见模块 doc）。
// 前向/反变换恒等性的纯整数矩阵证明（fwd_dct_is_exact_inverse_of_decoder_idct
// 测试）专用表——产品路径经 fwd_dct4x4 的展开常量消费等价信息。
#[allow(dead_code)]
const N_ROWS: [[i32; 4]; 4] = [[5, 5, 5, 5], [8, 4, -4, -8], [5, -5, -5, 5], [4, -8, 8, -4]];

/// N·v（单向量前向核心；加减实现，无乘法）。
///
/// 行向量 = [`N_ROWS`] 各行：行 0 `[5,5,5,5]`、行 1 `[8,4,−4,−8]`、
/// 行 2 `[5,−5,−5,5]`、行 3 `[4,−8,8,−4]`。行 2 是 `5·(v0−v1−v2+v3)`
/// （Cf 行 `[1,−1,−1,1]`）——**不是** `5·(t01−t23)`：2026-10-26 T5 编码
/// 收口对拍发现此处蝶形写错（`[1,1,−1,−1]` ≠ spec 矩阵），任何含
/// 偶频率-2 能量的块（阶跃边沿）系数量错致重建大幅偏离；位级回环
/// 因两侧同错而不可见，由 PSNR 对拍暴露。修复后以
/// `fwd_dct4x4_matches_n_rows_matrix` 逐元素锁死。
fn n_vec(v: &[i32; 4]) -> [i32; 4] {
    let t01 = v[0] + v[1];
    let t23 = v[2] + v[3];
    // 行 0（缩放 5）：5·(t01+t23)；行 2（缩放 5）：5·((v0+v3)−(v1+v2))
    // （Cf 偶频率行 [1,1,1,1]/[1,−1,−1,1]）；行 1/3（缩放 4）：4·(2u±w)
    // 其中 u = v0−v3、w = v1−v2（Cf 奇频率行 [2,1,−1,−2]/[1,−2,2,−1]）。
    let u = v[0] - v[3];
    let w = v[1] - v[2];
    [
        5 * (t01 + t23),
        4 * (2 * u + w),
        5 * ((v[0] + v[3]) - (v[1] + v[2])),
        4 * (u - 2 * w),
    ]
}

/// 前向 4x4 整数 DCT：`X″ = N·r·Nᵀ`（r 为 4x4 光栅残差）。
///
/// 输出即「×25/4 的理想反变换系数量」（模块 doc）；量化走 [`quant_ac`]。
/// 残差 ∈ [−255,255]，X″ 有界（|X″| < 2³¹，i32 安全；中间量 i64）。
pub(crate) fn fwd_dct4x4(r: &[i16; 16]) -> [i32; 16] {
    // 先列后行：A = N·r（逐列 N·v），X″ = A·Nᵀ（逐行 N·v，N 对称性不依赖）
    let mut a = [0i32; 16];
    for c in 0..4 {
        let col = [
            i32::from(r[c]),
            i32::from(r[4 + c]),
            i32::from(r[8 + c]),
            i32::from(r[12 + c]),
        ];
        let out = n_vec(&col);
        for (rr, &o) in out.iter().enumerate() {
            a[rr * 4 + c] = o;
        }
    }
    let mut x = [0i32; 16];
    for r_idx in 0..4 {
        let row = [
            a[r_idx * 4],
            a[r_idx * 4 + 1],
            a[r_idx * 4 + 2],
            a[r_idx * 4 + 3],
        ];
        let out = n_vec(&row);
        x[r_idx * 4..r_idx * 4 + 4].copy_from_slice(&out);
    }
    x
}

/// 量化一个 AC 系数：`level = round(X″·256 / (25·qmul))`（对称舍入，
/// half-away-from-zero）。结果 i32（|X″| < 2.3e5、qp≥0 时 |level| 有界）。
pub(crate) fn quant_ac(x: i32, qmul: u32) -> i32 {
    let num = i64::from(x) * 256;
    let den = 25 * i64::from(qmul);
    if num >= 0 {
        ((2 * num + den) / (2 * den)) as i32
    } else {
        -((-2 * num + den) / (2 * den)) as i32
    }
}

/// AC 反量化（解码侧 `cavlc.rs` store 公式的同式镜像，本地解码环用）：
/// `(level·qmul + 32) >> 6`。
pub(crate) fn dequant_ac(level: i32, qmul: u32) -> i32 {
    ((i64::from(level) * i64::from(qmul) + 32) >> 6) as i32
}

/// 整数量化/反量化闭环检查（编码器侧先验，防呆不防错）：反量化结果
/// 与理想 `(4/25)·X″` 的偏差必落在半个反量化步长内。
#[cfg(test)]
pub(crate) fn ac_roundtrip_bound(x: i32, qmul: u32) -> bool {
    let level = quant_ac(x, qmul);
    let back = dequant_ac(level, qmul);
    let ideal = (i64::from(x) * 4) / 25;
    (i64::from(back) - ideal).abs() <= i64::from(qmul / 64) + 1
}

/// 4x4 前向 Hadamard：`T = H·D·H`（D 为 16 项光栅）。蝶形与解码侧
/// [`crate::macroblock::luma_dc_dequant`] 的两遍结构逐字相同（无缩放）。
pub(crate) fn fwd_hadamard4x4(d: &[i64; 16]) -> [i64; 16] {
    let mut temp = [0i64; 16];
    for i in 0..4 {
        let (a, b, c, e) = (d[4 * i], d[4 * i + 1], d[4 * i + 2], d[4 * i + 3]);
        let z0 = a + b;
        let z1 = a - b;
        let z2 = c - e;
        let z3 = c + e;
        temp[4 * i] = z0 + z3;
        temp[4 * i + 1] = z0 - z3;
        temp[4 * i + 2] = z1 - z2;
        temp[4 * i + 3] = z1 + z2;
    }
    let mut out = [0i64; 16];
    for i in 0..4 {
        let (t0, t4, t8, t12) = (temp[i], temp[4 + i], temp[8 + i], temp[12 + i]);
        let z0 = t0 + t8;
        let z1 = t0 - t8;
        let z2 = t4 - t12;
        let z3 = t4 + t12;
        out[i] = z0 + z3;
        out[4 + i] = z0 - z3;
        out[8 + i] = z1 - z2;
        out[12 + i] = z1 + z2;
    }
    out
}

/// 2x2 前向 Hadamard（chroma DC）：`T = H₂·D·H₂`（光栅 [UL,UR,LL,LR]）。
/// 与解码侧 [`crate::macroblock::chroma_dc_dequant`] 的蝶形互逆。
pub(crate) fn fwd_hadamard2x2(d: &[i64; 4]) -> [i64; 4] {
    let a = d[0] + d[1];
    let b = d[0] - d[1];
    let c = d[2] + d[3];
    let e = d[2] - d[3];
    [a + c, b + e, a - c, b - e]
}

/// luma DC 量化：`level = round(16·T / qmul)`（qmul = `dc_qmul(qp)`）。
pub(crate) fn quant_luma_dc(t: i64, qmul: u32) -> i32 {
    round_div(t * 16, i64::from(qmul))
}

/// chroma DC 量化：`level = round(32·T / qmul)`（qmul = `dc_qmul(qpc)`）。
pub(crate) fn quant_chroma_dc(t: i64, qmul: u32) -> i32 {
    round_div(t * 32, i64::from(qmul))
}

/// 对称舍入除法（half-away-from-zero；分子可负）。
fn round_div(num: i64, den: i64) -> i32 {
    debug_assert!(den > 0);
    let r = if num >= 0 {
        (2 * num + den) / (2 * den)
    } else {
        -((-2 * num + den) / (2 * den))
    };
    i32::try_from(r).unwrap_or(i32::MAX)
}

/// 4x4 SATD（模式决策代价）：Cf 域系数 l1 范数的 1/4（近似像素域误差
/// 能量；口径只影响模式选择质量）。输入为残差（光栅）。
pub(crate) fn satd_4x4(d: &[i16; 16]) -> i32 {
    // Cf·d·Cfᵀ：两次无缩放 Cf 蝶形（等价 N 蝶形去掉 5/4 缩放）
    // 行 2 = (v0+v3)−(v1+v2)（Cf [1,−1,−1,1]，与 n_vec 修正同步——
    // 2026-10-26 对拍发现的同源笔误）
    let cf_vec = |v: &[i32; 4]| -> [i32; 4] {
        [
            v[0] + v[1] + v[2] + v[3],
            2 * (v[0] - v[3]) + (v[1] - v[2]),
            (v[0] + v[3]) - (v[1] + v[2]),
            (v[0] - v[3]) - 2 * (v[1] - v[2]),
        ]
    };
    let mut a = [0i32; 16];
    for c in 0..4 {
        let col = [
            i32::from(d[c]),
            i32::from(d[4 + c]),
            i32::from(d[8 + c]),
            i32::from(d[12 + c]),
        ];
        let out = cf_vec(&col);
        for (rr, &o) in out.iter().enumerate() {
            a[rr * 4 + c] = o;
        }
    }
    let mut sum = 0i32;
    for r_idx in 0..4 {
        let row = [
            a[r_idx * 4],
            a[r_idx * 4 + 1],
            a[r_idx * 4 + 2],
            a[r_idx * 4 + 3],
        ];
        let out = cf_vec(&row);
        sum += out.iter().map(|&v| v.abs()).sum::<i32>();
    }
    sum >> 2
}

/// 16x16 SAD（I16 模式决策）：`src` 与 `pred` 均为 16x16（pred 光栅紧凑、
/// src 按其 stride 取）。
pub(crate) fn sad_16x16(src: &[u8], src_stride: usize, pred: &[u8; 256]) -> i32 {
    let mut s = 0i32;
    for y in 0..16 {
        for x in 0..16 {
            s += i32::from(src[y * src_stride + x]).abs_diff(i32::from(pred[y * 16 + x])) as i32;
        }
    }
    s
}

/// 8x8 SAD（chroma 模式决策）。
pub(crate) fn sad_8x8(src: &[u8], src_stride: usize, pred: &[u8; 64]) -> i32 {
    let mut s = 0i32;
    for y in 0..8 {
        for x in 0..8 {
            s += i32::from(src[y * src_stride + x]).abs_diff(i32::from(pred[y * 8 + x])) as i32;
        }
    }
    s
}

/// QP 合法性（slice qp 与 PPS pic_init_qp_minus26 的联合值域）：
/// 0..=51 之外 → `InvalidArguments`。
pub(crate) fn check_qp(qp: u8) -> Result<(), H264Error> {
    if qp > 51 {
        return Err(H264Error::InvalidArguments("qp must be 0..=51"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macroblock::{chroma_dc_dequant, dc_qmul, dequant4_coeff, luma_dc_dequant};

    /// `fwd_dct4x4` 蝶形 == `N_ROWS` 矩阵积，逐元素锁死（含阶跃边沿
    /// 块）：2026-10-26 T5 对拍发现蝶形行 2 笔误（`[1,1,−1,−1]` ≠ spec
    /// `[1,−1,−1,1]`）——矩阵恒等测试只证 [`N_ROWS`] 正确、未覆盖蝶形
    /// 展开，位级回环又因两侧同错不可见；本测试堵住该缝隙。
    #[test]
    fn fwd_dct4x4_matches_n_rows_matrix() {
        // 确定性 LCG 造块（含纯阶跃/棋盘/随机三形态）
        struct Lcg(u32);
        impl Lcg {
            fn next_i16(&mut self, span: i16) -> i16 {
                self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                ((self.0 >> 16) as i16) % span
            }
        }
        let mut cases: Vec<[i16; 16]> = Vec::new();
        // 垂直阶跃（上两行 3、下两行 171——对拍翻案块）
        let mut step = [0i16; 16];
        for (i, v) in step.iter_mut().enumerate() {
            *v = if i < 8 { 3 } else { 171 };
        }
        cases.push(step);
        // 棋盘
        let mut cb = [0i16; 16];
        for (i, v) in cb.iter_mut().enumerate() {
            *v = if (i + i / 4) % 2 == 0 { 96 } else { 160 };
        }
        cases.push(cb);
        // LCG 随机 ×3
        let mut rng = Lcg(0x5EED_C0DE);
        for _ in 0..3 {
            let mut r = [0i16; 16];
            for v in r.iter_mut() {
                *v = rng.next_i16(255);
            }
            cases.push(r);
        }
        for (ci, r) in cases.iter().enumerate() {
            // 期望值 X = N·r·Nᵀ：X[i][j] = Σ_k Σ_l N[i][k]·r[k][l]·N[j][l]
            // （i64 精确；N 非对称，注意下标方向）
            let mut want = [0i64; 16];
            for i in 0..4usize {
                for j in 0..4usize {
                    let mut acc = 0i64;
                    for k in 0..4usize {
                        for l in 0..4usize {
                            acc += i64::from(N_ROWS[i][k])
                                * i64::from(r[k * 4 + l])
                                * i64::from(N_ROWS[j][l]);
                        }
                    }
                    want[i * 4 + j] = acc;
                }
            }
            let got = fwd_dct4x4(r);
            for s in 0..16usize {
                assert_eq!(
                    i64::from(got[s]),
                    want[s],
                    "case {ci} slot {s}: fwd butterfly must equal N·r·Nᵀ"
                );
            }
        }
    }

    /// 前向/反变换恒等性（纯整数矩阵证明）：解码侧 idct 两遍蝶形 =
    /// `M·X·Mᵀ`（M 从 `idct4x4_add` 逐行展开），本模块前向 = `N·r·Nᵀ`。
    /// 恒等条件 `M·N = 20·I`（⟹ M·(4/25·NrNᵀ)·Mᵀ = 64r，即 >>6 后回 r）。
    #[test]
    fn fwd_dct_is_exact_inverse_of_decoder_idct() {
        // M：idct4x4_add 单遍蝶形的矩阵展开（z0=b0+b8、z1=b0−b8、
        // z2=b4/2−b12、z3=b4+b12/2；输出 [z0+z3, z1+z2, z1−z2, z0−z3]），
        // z2/z3 的 ½ 系数以 ×2 整数域表示（M₂ = 2M）：
        let m2: [[i64; 4]; 4] = [[2, 2, 2, 1], [2, 1, -2, -2], [2, -1, -2, 2], [2, -2, 2, -1]];
        let n: [[i64; 4]; 4] =
            std::array::from_fn(|i| std::array::from_fn(|j| i64::from(N_ROWS[i][j])));
        for (i, m_row) in m2.iter().enumerate() {
            // 按列取 n[k][j]（矩阵乘的列向量），行迭代反而误导
            #[allow(clippy::needless_range_loop)]
            for j in 0..4 {
                let dot: i64 = (0..4).map(|k| m_row[k] * n[k][j]).sum();
                assert_eq!(dot, 40 * i64::from(i == j), "M·N[{i}][{j}] = {dot}");
            }
        }
        // M₂·N = 40·I ⟹ M·N = 20·I（M = M₂/2），恒等得证。
    }

    /// 纯 DC 残差黄金值：r 全 δ ⟹ X″[0][0] = 400·δ（N·ones = (20,0,0,0)），
    /// 其余系数为 0；qp0（qmul 类0=640，反量化步长 640/64=10）
    /// level = round(6.4δ)，反量化回到 64δ ± 半步长（δ 的 1/10 内）。
    #[test]
    fn fwd_dct_dc_golden() {
        for delta in [1i16, 5, 17, 100] {
            let r = [delta; 16];
            let x = fwd_dct4x4(&r);
            assert_eq!(x[0], 400 * i32::from(delta));
            assert!(x[1..].iter().all(|&v| v == 0));
            let level = quant_ac(x[0], dequant4_coeff(0)[0]);
            let expect = ((i64::from(delta) * 64 + 5) / 10) as i32; // round(6.4δ)
            assert_eq!(level, expect, "delta {delta}");
            // 反量化 → 解码侧系数 ≈ 64δ（idct 后每像素 δ ± 0.625/16…）
            let back = dequant_ac(level, dequant4_coeff(0)[0]);
            assert!(
                (back - 64 * i32::from(delta)).abs() <= 5,
                "delta {delta}: back {back}"
            );
        }
    }

    /// 量化/反量化全 QP 域闭环：偏差不超一个反量化步长（对称舍入性质；
    /// 覆盖 4x4 块的 4 个 class 槽位）。
    #[test]
    fn quant_dequant_roundtrip_all_qps() {
        let mut rng = 0x2545F491u32;
        let mut next = move || {
            rng = rng.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (rng >> 16) as i32
        };
        for qp in 0u8..=51 {
            let qmul = dequant4_coeff(qp);
            for &slot in &[0usize, 1, 5, 15] {
                for _ in 0..100 {
                    let x = (next() % 230_000) - 115_000; // |x| < X″ 界
                    assert!(ac_roundtrip_bound(x, qmul[slot]), "qp {qp} x {x}");
                }
            }
        }
    }

    /// luma DC 路径黄金值：D = 4·Σr 全 δ ⟹ T[0][0] = 4·T 其余 0，
    /// level00 = round(16·256δ/640·…) 与解码侧 luma_dc_dequant 回环
    /// 重建 DC = 64δ。
    #[test]
    fn luma_dc_path_golden_roundtrip() {
        for delta in [1i64, 3, 7, 31] {
            let d = [4 * 16 * delta; 16]; // D[b] = 4·Σr_b = 64δ
            let t = fwd_hadamard4x4(&d);
            assert_eq!(t[0], 1024 * delta); // H·D·H：16·64δ
            assert!(t[1..].iter().all(|&v| v == 0));
            let qmul = dc_qmul(0); // 640
            let mut levels = [0i16; 16];
            for (o, &tv) in levels.iter_mut().zip(t.iter()) {
                *o = i16::try_from(quant_luma_dc(tv, qmul)).unwrap();
            }
            assert_eq!(
                i64::from(levels[0]),
                (1024 * delta * 16 * 2 + 640) / (2 * 640)
            );
            let dcs = luma_dc_dequant(&levels, qmul).unwrap();
            for (k, dc) in dcs.iter().enumerate() {
                assert!(
                    (i64::from(*dc) - 64 * delta).abs() <= 2,
                    "dc[{k}] = {dc} expect ~{}",
                    64 * delta
                );
            }
        }
    }

    /// chroma DC 路径：D = 64δ ⟹ T = 128δ·I₂，level ≈ 6.4δ，
    /// chroma_dc_dequant 回环 ≈ 64δ。
    #[test]
    fn chroma_dc_path_golden_roundtrip() {
        for delta in [1i64, 5, 21] {
            let d = [64 * delta; 4];
            let t = fwd_hadamard2x2(&d);
            assert_eq!(t, [256 * delta, 0, 0, 0]);
            let qmul = dc_qmul(0);
            let levels = [
                quant_chroma_dc(t[0], qmul) as i16,
                quant_chroma_dc(t[1], qmul) as i16,
                quant_chroma_dc(t[2], qmul) as i16,
                quant_chroma_dc(t[3], qmul) as i16,
            ];
            let out = chroma_dc_dequant(&levels, qmul).unwrap();
            for (k, o) in out.iter().enumerate() {
                assert!((i64::from(*o) - 64 * delta).abs() <= 2, "out[{k}] = {o}");
            }
        }
    }

    /// satd/sad 基本性质：零残差零代价；常数残差 satd = 16·|δ|（DC 域
    /// 能量）/4·… 由实现定义，断言非负与单调。
    #[test]
    fn cost_primitives_sanity() {
        let zero = [0i16; 16];
        assert_eq!(satd_4x4(&zero), 0);
        let src = [128u8; 256];
        let pred = [128u8; 256];
        assert_eq!(sad_16x16(&src, 16, &pred), 0);
        let pred2 = [130u8; 256];
        assert_eq!(sad_16x16(&src, 16, &pred2), 2 * 256);
        let mut d = [0i16; 16];
        d[0] = 10;
        let s1 = satd_4x4(&d);
        d[0] = 20;
        assert!(satd_4x4(&d) > s1);
        assert!(satd_4x4(&d) >= 0);
    }

    #[test]
    fn qp_validation() {
        assert!(check_qp(0).is_ok());
        assert!(check_qp(51).is_ok());
        assert!(matches!(check_qp(52), Err(H264Error::InvalidArguments(_))));
    }
}
