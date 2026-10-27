//! Baseline I/P 宏块层语法写骨架——`crate::macroblock` 读侧
//! （`decode_macroblock` / `decode_inter_mb`）的写序镜像。
//!
//! 本模块只定形**语法映射与写序**三件事：
//! - mb_type 映射：I 片 ue(v) 0..=25 / P 片 0..=30 的值计算
//!   （I_NxN、Intra16x16 的 (pred_mode, cbp) 24 组合、P 帧间分区、
//!   I_PCM），与读侧 `I16_PRED`/`I16_CBP` 的 k 拆解互逆；
//! - coded_block_pattern 的 me(v) 写：6 位语义 cbp → golomb 码
//!   （读侧 `GOLOMB_TO_INTRA4X4_CBP`/`GOLOMB_TO_INTER_CBP` 的逆，
//!   直接在真表上线性反查，无副本表）；
//! - mb_qp_delta 写序：Intra16x16 恒写（spec：cbp>0 || Intra16x16），
//!   I_NxN / P 帧间仅 cbp > 0 写；QP 跨宏块传递的 ±52 恰一次回绕
//!   与读侧 `apply_qp_delta` 同式。
//!
//! 归后续编码路径（M3 施工图 T3/T4）的语法段——luma 预测模式
//! （I_NxN 的 prev/rem、Intra16x16 模式含在 mb_type）、P 片
//! ref_idx/mvd、残差数据本体、P_Skip 的 slice 级 mb_skip_run——
//! 由它们在 [`write_mb_header`] 的 mb_type 之后插入。
// MbTypeSyntax/MbHeader/write_mb_header 组合面是 mb_type 语法的文档化
// 镜像（测试覆盖全映射与写序），T3/T4 实现走了内联写（frame.rs/pframe.rs
// 直接算 ue 值）未消费此层；保留供分块 inter 模式（16x8/8x16/8x8，
// m3-plan §7.2 后续批次）接入时复用——dead_code 豁免即此理由。
#![allow(dead_code)]

use crate::encoder::bitwriter::BitWriter;
use crate::error::H264Error;
use crate::macroblock::{GOLOMB_TO_INTER_CBP, GOLOMB_TO_INTRA4X4_CBP, I16_CBP};

/// 片型（Baseline 只有 I/P 两种 slice_type）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯标签枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SliceKind {
    /// I 片（只含 intra 宏块；mb_type 0..=25 原语义）。
    I,
    /// P 片（mb_type 0..=4 帧间，5..=30 为 intra+5 / I_PCM=30）。
    P,
}

/// P 片帧间分区（mb_type 0..4）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯标签枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PPartition {
    /// P_L0_16x16（mb_type 0）。
    L0_16x16,
    /// P_L0_L0_16x8（mb_type 1）。
    L0_16x8,
    /// P_L0_8x16（mb_type 2）。
    L0_8x16,
    /// P_8x8（mb_type 3；4 个 sub_mb_type 随后）。
    L0_8x8,
    /// P_8x8ref0（mb_type 4；ref 恒 0 不读）。
    L0_8x8Ref0,
}

impl PPartition {
    /// ue(v) 值（0..=4）。
    pub(crate) fn mb_type_value(&self) -> u64 {
        match self {
            PPartition::L0_16x16 => 0,
            PPartition::L0_16x8 => 1,
            PPartition::L0_8x16 => 2,
            PPartition::L0_8x8 => 3,
            PPartition::L0_8x8Ref0 => 4,
        }
    }
}

/// 宏块类型（Baseline I/P 的 mb_type 写侧描述）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（值语义小枚举）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MbTypeSyntax {
    /// I_NxN（Intra4x4）：I 片 ue=0、P 片 ue=5。luma 预测模式语法
    /// （16 个 prev/rem）由 T3 在 mb_type 之后插入。
    Intra4x4,
    /// Intra16x16：`pred_mode` 0..=3（0=V,1=H,2=DC,3=Plane）、
    /// `cbp_variant` 0..=5（读侧 `I16_CBP` 序：0,0x10,0x20,0x0F,
    /// 0x1F,0x2F）。mb_type = 1 + 4·variant + pred（I 片 1..=24、
    /// P 片 +5 → 6..=29；读侧 k = mb_type−1 拆 `I16_CBP[k>>2]` /
    /// `I16_PRED[k&3]` 与本式互逆）。
    Intra16x16 {
        /// Intra16x16 预测模式（spec 编号）。
        pred_mode: u8,
        /// cbp 变体序号（`I16_CBP` 下标）。
        cbp_variant: u8,
    },
    /// P 帧间（仅 P 片）：mb_type 0..=4。ref_idx/mvd 由 T4 插入。
    PInter {
        /// 帧间分区。
        partition: PPartition,
    },
    /// I_PCM：I 片 ue=25、P 片 ue=30。样值直写（字节对齐后 384 字节）
    /// 归 T3，见 [`write_pcm_mb`]。
    Pcm,
}

impl MbTypeSyntax {
    /// 是否 intra（决定 intra_chroma_pred_mode 等语法的有无）。
    pub(crate) fn is_intra(&self) -> bool {
        matches!(
            self,
            MbTypeSyntax::Intra4x4 | MbTypeSyntax::Intra16x16 { .. }
        )
    }

    /// 本片型下的 ue(v) 值。组合超界（pred_mode > 3、cbp_variant > 5）
    /// → `InvalidData`；P 帧间出现在 I 片 → `InvalidArguments`
    /// （读侧对应 `i-slice/p-slice mb_type out of range` 的写侧先验）。
    pub(crate) fn mb_type_value(&self, slice: SliceKind) -> Result<u64, H264Error> {
        match self {
            MbTypeSyntax::Intra4x4 => Ok(match slice {
                SliceKind::I => 0,
                SliceKind::P => 5,
            }),
            MbTypeSyntax::Intra16x16 {
                pred_mode,
                cbp_variant,
            } => {
                if *pred_mode > 3 || *cbp_variant > 5 {
                    return Err(H264Error::InvalidData(format!(
                        "i16 pred_mode {pred_mode} / cbp_variant {cbp_variant} out of range"
                    )));
                }
                let v = 1 + u64::from(*cbp_variant) * 4 + u64::from(*pred_mode);
                Ok(match slice {
                    SliceKind::I => v,
                    SliceKind::P => v + 5,
                })
            }
            MbTypeSyntax::PInter { partition } => {
                if slice == SliceKind::I {
                    return Err(H264Error::InvalidArguments(
                        "p inter mb_type is invalid in an i-slice",
                    ));
                }
                Ok(partition.mb_type_value())
            }
            MbTypeSyntax::Pcm => Ok(match slice {
                SliceKind::I => 25,
                SliceKind::P => 30,
            }),
        }
    }

    /// 写 mb_type（macroblock_layer 的第一语法元素）。
    pub(crate) fn write_ue(&self, w: &mut BitWriter, slice: SliceKind) -> Result<(), H264Error> {
        w.ue(self.mb_type_value(slice)?)
    }
}

/// 6 位语义 cbp → me(v) 的 golomb 码（读侧两张 GOLOMB_TO_* 表的逆；
/// 在真表上线性反查——48 项不复制，一致性由结构保证）。
/// cbp 超出 48 个合法组合（表内无此值）→ `InvalidData`。
fn cbp_golomb_code(cbp: u8, intra: bool) -> Result<u64, H264Error> {
    let table: &[u8; 48] = if intra {
        &GOLOMB_TO_INTRA4X4_CBP
    } else {
        &GOLOMB_TO_INTER_CBP
    };
    let Some(code) = table.iter().position(|&c| c == cbp) else {
        return Err(H264Error::InvalidData(format!(
            "coded_block_pattern {cbp:#04x} not representable (48 valid combinations)"
        )));
    };
    Ok(code as u64)
}

/// coded_block_pattern 写（me(v)：ue 编码的 golomb 码再经读侧表映射）。
/// `intra` = true 走 I_NxN 表（I16x16 的 cbp 含在 mb_type，不写）、
/// false 走 P 帧间表。
pub(crate) fn write_coded_block_pattern(
    w: &mut BitWriter,
    cbp: u8,
    intra: bool,
) -> Result<(), H264Error> {
    w.ue(cbp_golomb_code(cbp, intra)?)
}

/// 解析 mb_qp_delta 并求新 QP（读侧 `apply_qp_delta` 的同式镜像：
/// 出界 ±52 恰回绕一次，仍出界 → `InvalidData`）。纯计算不写位，
/// 供 [`write_mb_header`] 先验后写（失败时写入器不留半截语法）。
fn resolve_qp_delta(delta: i64, qp: u8) -> Result<u8, H264Error> {
    let Ok(d) = i8::try_from(delta) else {
        return Err(H264Error::InvalidData(format!(
            "mb_qp_delta {delta} out of range"
        )));
    };
    let mut q = i32::from(qp) + i32::from(d);
    if !(0..=51).contains(&q) {
        q = if q < 0 { q + 52 } else { q - 52 };
        if !(0..=51).contains(&q) {
            return Err(H264Error::InvalidData(format!(
                "mb_qp_delta {delta} leaves qp out of range"
            )));
        }
    }
    Ok(q as u8)
}

/// mb_qp_delta 写（se(v)）。`qp` 为跨宏块传递的当前 QP，成功后同步
/// 更新——编码器与其本地解码环的 QP 状态必须逐宏块一致（量化与
/// 残差可解性都依赖它）。
pub(crate) fn write_mb_qp_delta(
    w: &mut BitWriter,
    delta: i64,
    qp: &mut u8,
) -> Result<(), H264Error> {
    let q = resolve_qp_delta(delta, *qp)?;
    w.se(delta)?;
    *qp = q;
    Ok(())
}

/// 宏块头写侧输入（非 PCM）。写序 = 读侧 `decode_macroblock` 的镜像：
/// mb_type ue →（intra）intra_chroma_pred_mode ue →（I_NxN / P 帧间）
/// coded_block_pattern me → mb_qp_delta se（Intra16x16 恒写，其余
/// 仅 cbp > 0 写）。T3/T4 在 mb_type 之后插入 luma 预测模式 / ref·mvd。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（值语义参数包）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MbHeader {
    /// 片型。
    pub(crate) slice: SliceKind,
    /// 宏块类型。
    pub(crate) mb_type: MbTypeSyntax,
    /// intra_chroma_pred_mode（0..=3：DC/H/V/Plane），仅 intra 宏块写。
    pub(crate) chroma_pred_mode: u8,
    /// 6 位语义 cbp：I_NxN / P 帧间经 me(v) 写出；Intra16x16 不写
    /// （含在 mb_type），须与 `cbp_variant` 一致（不一致 → `InvalidData`）。
    pub(crate) cbp: u8,
    /// mb_qp_delta（写序规则见类型 doc）。
    pub(crate) qp_delta: i64,
}

/// 写宏块头（见 [`MbHeader`] 写序）。全量先验后写：任一校验失败时
/// 写入器不落任何位。
///
/// # Errors
/// - `InvalidArguments`：PCM 走 [`write_pcm_mb`]、P 帧间在 I 片；
/// - `InvalidData`：pred/cbp/chroma/qp_delta 越界、I16 的 cbp 与
///   variant 矛盾、cbp 无 golomb 码。
pub(crate) fn write_mb_header(
    w: &mut BitWriter,
    mb: &MbHeader,
    qp: &mut u8,
) -> Result<(), H264Error> {
    // ---- 先验（不落位）----
    if matches!(mb.mb_type, MbTypeSyntax::Pcm) {
        return Err(H264Error::InvalidArguments(
            "pcm macroblock uses write_pcm_mb (no header fields)",
        ));
    }
    mb.mb_type.mb_type_value(mb.slice)?; // 组合合法性
    if mb.mb_type.is_intra() && mb.chroma_pred_mode > 3 {
        return Err(H264Error::InvalidData(format!(
            "intra_chroma_pred_mode {} out of range",
            mb.chroma_pred_mode
        )));
    }
    match mb.mb_type {
        MbTypeSyntax::Intra16x16 { cbp_variant, .. } => {
            if mb.cbp != I16_CBP[usize::from(cbp_variant)] {
                return Err(H264Error::InvalidData(format!(
                    "cbp {:#04x} inconsistent with i16 cbp_variant {cbp_variant}",
                    mb.cbp
                )));
            }
        }
        MbTypeSyntax::Intra4x4 | MbTypeSyntax::PInter { .. } => {
            cbp_golomb_code(mb.cbp, mb.mb_type.is_intra())?;
        }
        MbTypeSyntax::Pcm => {}
    }
    let qp_writes = match mb.mb_type {
        MbTypeSyntax::Intra16x16 { .. } => true,
        _ => mb.cbp > 0,
    };
    if qp_writes {
        resolve_qp_delta(mb.qp_delta, *qp)?;
    }

    // ---- 落位（写序）----
    mb.mb_type.write_ue(w, mb.slice)?;
    if mb.mb_type.is_intra() {
        w.ue(u64::from(mb.chroma_pred_mode))?;
    }
    match mb.mb_type {
        MbTypeSyntax::Intra4x4 => {
            write_coded_block_pattern(w, mb.cbp, true)?;
            if mb.cbp > 0 {
                write_mb_qp_delta(w, mb.qp_delta, qp)?;
            }
        }
        MbTypeSyntax::Intra16x16 { .. } => {
            write_mb_qp_delta(w, mb.qp_delta, qp)?; // 恒写（读侧同款）
        }
        MbTypeSyntax::PInter { .. } => {
            write_coded_block_pattern(w, mb.cbp, false)?;
            if mb.cbp > 0 {
                write_mb_qp_delta(w, mb.qp_delta, qp)?;
            }
        }
        MbTypeSyntax::Pcm => {} // 先验已拦
    }
    Ok(())
}

/// I_PCM 宏块写骨架：mb_type ue + pcm_alignment_zero_bit（读侧
/// 字节对齐零读的镜像）。384 字节样值直写归 T3（届时在此对齐后
/// 逐字节 `u(8)`）。
pub(crate) fn write_pcm_mb(w: &mut BitWriter, slice: SliceKind) -> Result<(), H264Error> {
    MbTypeSyntax::Pcm.write_ue(w, slice)?;
    w.align_zeros();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitreader::BitReader;
    use crate::macroblock::{I16_CBP, I16_PRED};

    /// mb_type 全映射：I16 全部 24 组合 × I/P 片 + I_NxN / P 分区 /
    /// PCM，ue 写出读回等值，且值与读侧 k 拆解（I16_PRED/I16_CBP）
    /// 互逆。
    #[test]
    fn mb_type_mapping_roundtrips_all_combinations() {
        for slice in [SliceKind::I, SliceKind::P] {
            // I_NxN
            let expect = if slice == SliceKind::I { 0u64 } else { 5 };
            check_mb_type(slice, &MbTypeSyntax::Intra4x4, expect);
            // I16 全 24 组合
            for variant in 0..=5u8 {
                for pred in 0..=3u8 {
                    let v = 1 + u64::from(variant) * 4 + u64::from(pred);
                    let expect = if slice == SliceKind::I { v } else { v + 5 };
                    check_mb_type(
                        slice,
                        &MbTypeSyntax::Intra16x16 {
                            pred_mode: pred,
                            cbp_variant: variant,
                        },
                        expect,
                    );
                    // 读侧拆解互逆：k = mb_type−1（P 片先 −5）
                    let k = (expect - u64::from(slice == SliceKind::P) * 5 - 1) as usize;
                    assert_eq!(I16_PRED[k & 3], pred, "pred (variant {variant})");
                    assert_eq!(I16_CBP[k >> 2], I16_CBP[usize::from(variant)]);
                }
            }
            // PCM
            check_mb_type(
                slice,
                &MbTypeSyntax::Pcm,
                if slice == SliceKind::I { 25 } else { 30 },
            );
        }
        // P 帧间四分区（仅 P 片）
        let parts = [
            (PPartition::L0_16x16, 0u64),
            (PPartition::L0_16x8, 1),
            (PPartition::L0_8x16, 2),
            (PPartition::L0_8x8, 3),
            (PPartition::L0_8x8Ref0, 4),
        ];
        for (p, v) in parts {
            check_mb_type(SliceKind::P, &MbTypeSyntax::PInter { partition: p }, v);
            // I 片拒绝
            let mut w = BitWriter::new();
            assert!(matches!(
                MbTypeSyntax::PInter { partition: p }.write_ue(&mut w, SliceKind::I),
                Err(H264Error::InvalidArguments(_))
            ));
            assert_eq!(w.bit_position(), 0, "失败不得落位");
        }
        // 组合越界
        for bad in [
            MbTypeSyntax::Intra16x16 {
                pred_mode: 4,
                cbp_variant: 0,
            },
            MbTypeSyntax::Intra16x16 {
                pred_mode: 0,
                cbp_variant: 6,
            },
        ] {
            assert!(matches!(
                bad.mb_type_value(SliceKind::I),
                Err(H264Error::InvalidData(_))
            ));
        }
    }

    fn check_mb_type(slice: SliceKind, syntax: &MbTypeSyntax, expect: u64) {
        let mut w = BitWriter::new();
        syntax.write_ue(&mut w, slice).unwrap();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), expect, "{syntax:?} in {slice:?}");
    }

    /// CBP 全 48 码 × 两张表：写 → ue 读回 → 读侧真表映射复原 cbp；
    /// 非法组合（48..63 / 语义未用值）拒收。
    #[test]
    fn coded_block_pattern_roundtrips_all_codes() {
        for intra in [true, false] {
            let table: &[u8; 48] = if intra {
                &GOLOMB_TO_INTRA4X4_CBP
            } else {
                &GOLOMB_TO_INTER_CBP
            };
            for &cbp in table.iter() {
                let mut w = BitWriter::new();
                write_coded_block_pattern(&mut w, cbp, intra).unwrap();
                let written = w.bit_position();
                let data = w.take_rbsp();
                let mut r = BitReader::new(&data);
                let code = r.ue().unwrap();
                assert_eq!(table[usize::try_from(code).unwrap()], cbp);
                assert_eq!(r.bit_position(), written);
            }
        }
        // 两表各含 0..=47 全集；48..=63 无码
        for cbp in 48..=63u8 {
            for intra in [true, false] {
                assert!(matches!(
                    cbp_golomb_code(cbp, intra),
                    Err(H264Error::InvalidData(_))
                ));
            }
        }
    }

    /// mb_qp_delta：QP 更新镜像读侧 ±52 一次回绕；手算黄金值；
    /// 越界拒收且不落位。
    #[test]
    fn mb_qp_delta_wrap_semantics() {
        let cases: &[(u8, i64, Option<u8>)] = &[
            (10, 0, Some(10)), // 恒等
            (10, 1, Some(11)),
            (10, -1, Some(9)),
            (0, -1, Some(51)), // 回绕：−1 → 51
            (0, -2, Some(50)),
            (51, 1, Some(0)), // 回绕：52 → 0
            (51, 2, Some(1)),
            (25, 30, Some(3)),   // 55 → −52 → 3（一次回绕入界）
            (25, 60, Some(33)),  // 85 → 85−52=33
            (25, -30, Some(47)), // −5 → +52 → 47
            (0, -52, Some(0)),   // −52 → +52 → 0
            (51, 52, Some(51)),  // 103 → −52 → 51
            (25, 80, None),      // 105 → 53 仍出界 → Err
            (25, -80, None),     // −55 → −3 仍出界 → Err
            (25, 200, None),     // 超 i8 → Err
            (25, -200, None),
            (25, i64::MIN, None), // 极值不 panic
            (25, i64::MAX, None),
        ];
        for &(qp, delta, expect) in cases {
            let mut w = BitWriter::new();
            let mut q = qp;
            let res = write_mb_qp_delta(&mut w, delta, &mut q);
            match expect {
                Some(eq) => {
                    res.unwrap();
                    assert_eq!(q, eq, "qp {qp} delta {delta}");
                    // se 值本身也要能读回
                    let data = w.take_rbsp();
                    let mut r = BitReader::new(&data);
                    assert_eq!(r.se().unwrap(), delta);
                }
                None => {
                    assert!(
                        matches!(res, Err(H264Error::InvalidData(_))),
                        "qp {qp} delta {delta} 应拒收"
                    );
                    assert_eq!(w.bit_position(), 0, "拒收不得落位");
                    assert_eq!(q, qp, "拒收不得改 QP");
                }
            }
        }
    }

    /// 宏块头写序：I16（mb_type → chroma → qp 恒写）、I_NxN cbp>0
    /// （mb_type → chroma → cbp → qp）、cbp==0（无 qp）、P 帧间
    /// （mb_type → cbp → qp）。按读侧解析序逐元素读回。
    #[test]
    fn mb_header_write_order_roundtrip() {
        // Intra16x16：mb_type → chroma ue → mb_qp_delta se
        let mb = MbHeader {
            slice: SliceKind::I,
            mb_type: MbTypeSyntax::Intra16x16 {
                pred_mode: 2,
                cbp_variant: 3, // cbp 0x0F
            },
            chroma_pred_mode: 1,
            cbp: 0x0F,
            qp_delta: -3,
        };
        let mut w = BitWriter::new();
        let mut qp = 20u8;
        write_mb_header(&mut w, &mb, &mut qp).unwrap();
        assert_eq!(qp, 17);
        let written = w.bit_position();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 1 + 3 * 4 + 2); // mb_type 15
        assert_eq!(r.ue().unwrap(), 1); // chroma
        assert_eq!(r.se().unwrap(), -3); // qp_delta（恒写）
        assert_eq!(r.bit_position(), written);

        // I_NxN cbp>0：mb_type → chroma → cbp → qp
        let mb = MbHeader {
            slice: SliceKind::I,
            mb_type: MbTypeSyntax::Intra4x4,
            chroma_pred_mode: 0,
            cbp: 0x2F,
            qp_delta: 2,
        };
        let mut w = BitWriter::new();
        let mut qp = 10u8;
        write_mb_header(&mut w, &mb, &mut qp).unwrap();
        assert_eq!(qp, 12);
        let written = w.bit_position();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0); // mb_type
        assert_eq!(r.ue().unwrap(), 0); // chroma
        let code = r.ue().unwrap();
        assert_eq!(GOLOMB_TO_INTRA4X4_CBP[usize::try_from(code).unwrap()], 0x2F);
        assert_eq!(r.se().unwrap(), 2);
        assert_eq!(r.bit_position(), written);

        // I_NxN cbp==0：无 qp_delta（cbp 码 0 → 表值 0）
        let mb = MbHeader {
            slice: SliceKind::I,
            mb_type: MbTypeSyntax::Intra4x4,
            chroma_pred_mode: 3,
            cbp: 0,
            qp_delta: 5,
        };
        let mut w = BitWriter::new();
        let mut qp = 10u8;
        write_mb_header(&mut w, &mb, &mut qp).unwrap();
        assert_eq!(qp, 10, "cbp==0 不写 qp_delta，QP 不变");
        let written = w.bit_position();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0);
        assert_eq!(r.ue().unwrap(), 3);
        let code = r.ue().unwrap(); // cbp 0 的 golomb 码是表位 3（非 0）
        assert_eq!(GOLOMB_TO_INTRA4X4_CBP[usize::try_from(code).unwrap()], 0);
        assert_eq!(r.bit_position(), written);

        // P 帧间：mb_type → cbp → qp（chroma 不写）
        let mb = MbHeader {
            slice: SliceKind::P,
            mb_type: MbTypeSyntax::PInter {
                partition: PPartition::L0_16x16,
            },
            chroma_pred_mode: 0,
            cbp: 16,
            qp_delta: -1,
        };
        let mut w = BitWriter::new();
        let mut qp = 0u8;
        write_mb_header(&mut w, &mb, &mut qp).unwrap();
        assert_eq!(qp, 51); // −1 回绕
        let written = w.bit_position();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 0); // P_L0_16x16
        let code = r.ue().unwrap();
        assert_eq!(GOLOMB_TO_INTER_CBP[usize::try_from(code).unwrap()], 16);
        assert_eq!(r.se().unwrap(), -1);
        assert_eq!(r.bit_position(), written);
    }

    /// 宏块头先验：PCM 拒收、I16 cbp 矛盾拒收、chroma 越界拒收、
    /// 非法 cbp 拒收——均不落位。
    #[test]
    fn mb_header_validates_before_writing() {
        let bad_cases: Vec<MbHeader> = vec![
            // PCM 走专用入口
            MbHeader {
                slice: SliceKind::I,
                mb_type: MbTypeSyntax::Pcm,
                chroma_pred_mode: 0,
                cbp: 0x2F,
                qp_delta: 0,
            },
            // I16 cbp 与 variant 矛盾
            MbHeader {
                slice: SliceKind::I,
                mb_type: MbTypeSyntax::Intra16x16 {
                    pred_mode: 0,
                    cbp_variant: 0,
                },
                chroma_pred_mode: 0,
                cbp: 0x10,
                qp_delta: 0,
            },
            // chroma 越界
            MbHeader {
                slice: SliceKind::I,
                mb_type: MbTypeSyntax::Intra4x4,
                chroma_pred_mode: 4,
                cbp: 0,
                qp_delta: 0,
            },
            // I_NxN cbp 无码（48..63）
            MbHeader {
                slice: SliceKind::I,
                mb_type: MbTypeSyntax::Intra4x4,
                chroma_pred_mode: 0,
                cbp: 60,
                qp_delta: 0,
            },
        ];
        for mb in bad_cases {
            let mut w = BitWriter::new();
            let mut qp = 25u8;
            assert!(
                write_mb_header(&mut w, &mb, &mut qp).is_err(),
                "{mb:?} 应拒收"
            );
            assert_eq!(w.bit_position(), 0, "先验失败不得落位");
            assert_eq!(qp, 25);
        }
        // I16 + qp_delta 越界（回绕后仍出界）也必须先验拦下
        let mb = MbHeader {
            slice: SliceKind::I,
            mb_type: MbTypeSyntax::Intra16x16 {
                pred_mode: 0,
                cbp_variant: 0,
            },
            chroma_pred_mode: 0,
            cbp: 0,
            qp_delta: 80,
        };
        let mut w = BitWriter::new();
        let mut qp = 25u8;
        assert!(write_mb_header(&mut w, &mb, &mut qp).is_err());
        assert_eq!(w.bit_position(), 0);
    }

    /// I_PCM 写骨架：mb_type ue(25) + 零对齐（黄金字节）。
    #[test]
    fn pcm_mb_writes_type_and_alignment() {
        let mut w = BitWriter::new();
        write_pcm_mb(&mut w, SliceKind::I).unwrap();
        // ue(25) = "00001"+"1010"（4 前导零 + 1 + 4 位尾数 1010）
        // → 9 位 "000011010"，对齐补 7 零 → [0x0D, 0x00]
        assert_eq!(w.take_rbsp(), [0x0D, 0x00]);

        let mut w = BitWriter::new();
        write_pcm_mb(&mut w, SliceKind::P).unwrap();
        let data = w.take_rbsp();
        let mut r = BitReader::new(&data);
        assert_eq!(r.ue().unwrap(), 30);
        assert_eq!(r.remaining_bits(), 7);
        assert_eq!(r.u(7).unwrap(), 0);
    }
}
