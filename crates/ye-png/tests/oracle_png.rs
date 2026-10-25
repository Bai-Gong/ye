//! ffmpeg oracle roundtrip：**无损性零容差**（施工图④原文——
//! 「PNG 编码器单测=roundtrip 逐字节精确 0 容差：ffmpeg 解码我们的
//! PNG 的 rawvideo 输出==送入 RGBA 字节」，无损不给容差）。
//!
//! 流程：构造确定性图案（渐变 / 边缘棋盘 / LCG 随机）→
//! `encode_rgba_with` 出 PNG → 写入 `CARGO_TARGET_TMPDIR` →
//! `ffmpeg -i x.png -f rawvideo -pix_fmt rgba x.raw` →
//! 逐字节 `assert_eq!`（长度与内容都严格相等）。
//!
//! 矩阵：3 图案 × 2 压缩档（stored / fixed-Huffman）× 3 过滤策略
//! （None / Sub / Adaptive）+ 大图多 stored 块用例（257×131 过滤后
//! 134899 字节 > 65535，跨 3 块）+ 1×1 最小图。
//!
//! PATH 中找不到 ffmpeg 时打印原因并跳过（与 ye-container-mp4
//! oracle 组同一约定，非 CI 环境不 fail）。

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use ye_png::{DeflateProfile, FilterStrategy, encode_rgba_with};

// —— 工具查找（与 ye-container-mp4/tests/oracle_demux.rs 同型）—————

fn find_tool(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidates = if cfg!(windows) {
            [dir.join(format!("{name}.exe")), dir.join(name)]
        } else {
            [dir.join(name), dir.join(format!("{name}.exe"))]
        };
        for c in candidates {
            if c.is_file() {
                return Some(c);
            }
        }
    }
    None
}

// —— 确定性测试图案 ——————————————————————————————————————————

/// 渐变：R 随 x、G 随 y、B 随 x+y、A 全 255（尺寸 1 时退化为常数）。
fn pattern_gradient(w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            out[i] = if w > 1 {
                (x * 255 / (w - 1)) as u8
            } else {
                255
            };
            out[i + 1] = if h > 1 {
                (y * 255 / (h - 1)) as u8
            } else {
                255
            };
            out[i + 2] = ((x + y) * 255 / (w + h - 2).max(1)) as u8;
            out[i + 3] = 255;
        }
    }
    out
}

/// 边缘/棋盘：8×8 棋盘双色块 + 细十字线（高频边缘考验 Sub 过滤与
/// LZ77 匹配边界）。
fn pattern_edges(w: u32, h: u32) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let checker = ((x / 8) + (y / 8)) % 2 == 0;
            let cross = x == w / 2 || y == h / 2;
            let (r, g, b) = if cross {
                (255u8, 0, 0)
            } else if checker {
                (240, 240, 240)
            } else {
                (16, 16, 32)
            };
            out[i] = r;
            out[i + 1] = g;
            out[i + 2] = b;
            out[i + 3] = if x % 8 == 0 { 128 } else { 255 };
        }
    }
    out
}

/// LCG 确定性随机（含全值域字节 0 与 255），alpha 也随机——
/// 存储器逐字节全随机上界（压缩应近于不压缩，正确性不受影响）。
fn pattern_lcg(w: u32, h: u32) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as u8
    };
    (0..(w * h * 4)).map(|_| next()).collect()
}

// —— oracle 对拍 ——————————————————————————————————————————————

struct Fixture {
    ffmpeg: PathBuf,
    dir: PathBuf,
}

fn fixture() -> Option<&'static Fixture> {
    static FX: OnceLock<Option<Fixture>> = OnceLock::new();
    FX.get_or_init(|| {
        let Some(ffmpeg) = find_tool("ffmpeg") else {
            eprintln!("跳过 oracle_png：PATH 中未找到 ffmpeg");
            return None;
        };
        // CARGO_TARGET_TMPDIR 是编译期注入的环境（运行时进程看不到），用 env! 取。
        let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("ye_png_oracle");
        std::fs::create_dir_all(&dir).expect("建临时目录失败");
        Some(Fixture { ffmpeg, dir })
    })
    .as_ref()
}

/// 编码 → ffmpeg 解码到 rawvideo RGBA → 与原字节逐字节比对（0 容差）。
/// 返回 PNG 文件大小（供压缩率断言用）。
fn roundtrip(
    fx: &Fixture,
    name: &str,
    w: u32,
    h: u32,
    rgba: &[u8],
    profile: DeflateProfile,
    strategy: FilterStrategy,
) -> u64 {
    let png = encode_rgba_with(w, h, rgba, profile, strategy)
        .unwrap_or_else(|e| panic!("{name} 编码失败：{e}"));
    let png_path = fx.dir.join(format!("{name}.png"));
    let raw_path = fx.dir.join(format!("{name}.raw"));
    std::fs::write(&png_path, &png).expect("写 PNG 失败");
    let out = Command::new(&fx.ffmpeg)
        .args(["-y", "-v", "error"])
        .arg("-i")
        .arg(&png_path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg(&raw_path)
        .output()
        .unwrap_or_else(|e| panic!("{name} ffmpeg 启动失败：{e}"));
    assert!(
        out.status.success(),
        "{name} ffmpeg 解码失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let decoded = std::fs::read(&raw_path).expect("读 raw 失败");
    // 0 容差：长度与每个字节严格相等（无损不给容差——施工图原文）。
    assert_eq!(
        decoded.len(),
        rgba.len(),
        "{name}：解码长度不符（expected {} got {}）",
        rgba.len(),
        decoded.len()
    );
    let first_diff = decoded.iter().zip(rgba.iter()).position(|(a, b)| a != b);
    assert!(
        first_diff.is_none(),
        "{name}：字节不一致@{first_diff:?}（0 容差）"
    );
    std::fs::remove_file(&raw_path).ok();
    png.len() as u64
}

/// 全矩阵用例：(名, 宽, 高, 图案生成函数)。
type PatternCase = (&'static str, u32, u32, fn(u32, u32) -> Vec<u8>);

/// 全矩阵：3 图案 × 2 压缩档 × 3 过滤策略，逐字节 0 容差。
#[test]
fn all_patterns_all_profiles_exact_roundtrip() {
    let Some(fx) = fixture() else { return };
    let cases: &[PatternCase] = &[
        ("gradient_97x61", 97, 61, pattern_gradient),
        ("edges_64x64", 64, 64, pattern_edges),
        ("lcg_128x96", 128, 96, pattern_lcg),
    ];
    let profiles = [
        ("stored", DeflateProfile::Stored),
        ("fixed", DeflateProfile::FixedHuffman),
    ];
    let strategies = [
        ("none", FilterStrategy::None),
        ("sub", FilterStrategy::Sub),
        ("adaptive", FilterStrategy::Adaptive),
    ];
    for (pname, profile) in profiles {
        for (sname, strategy) in strategies {
            for (cname, w, h, make) in cases {
                let rgba = make(*w, *h);
                let name = format!("{cname}_{pname}_{sname}");
                roundtrip(fx, &name, *w, *h, &rgba, profile, strategy);
            }
        }
    }
}

/// 大图（257×131）：过滤后 257×131×4+131 = 134899 字节，stored 档
/// 跨 3 块；fixed 档同时核对压缩率（渐变应显著小于原始字节）。
#[test]
fn large_image_multiblock_and_ratio() {
    let Some(fx) = fixture() else { return };
    let (w, h) = (257u32, 131u32);
    let rgba = pattern_gradient(w, h);
    let raw_bytes = rgba.len() as u64;
    let stored = roundtrip(
        fx,
        "large_gradient_stored_adaptive",
        w,
        h,
        &rgba,
        DeflateProfile::Stored,
        FilterStrategy::Adaptive,
    );
    let fixed = roundtrip(
        fx,
        "large_gradient_fixed_adaptive",
        w,
        h,
        &rgba,
        DeflateProfile::FixedHuffman,
        FilterStrategy::Adaptive,
    );
    // stored 档 = 过滤流（raw + h 行 type 字节）+ 3×5 字节块头 +
    // zlib 6 字节 + PNG 框架 45 字节——上限按 raw + h + 100 收。
    let filtered = raw_bytes + h as u64;
    assert!(
        stored > filtered && stored < filtered + 100,
        "stored size {stored}"
    );
    // fixed 档对渐变图案至少 3× 压缩（Sub 过滤后大量零差值）。
    assert!(
        fixed * 3 < raw_bytes,
        "fixed 压缩率不足：png {fixed} vs raw {raw_bytes}"
    );
}

/// 最小图 1×1 与默认入口 [`ye_png::encode_rgba`] 路径。
#[test]
fn one_pixel_default_entry() {
    let Some(fx) = fixture() else { return };
    let px = [0xAB, 0xCD, 0xEF, 0x01];
    let png = ye_png::encode_rgba(1, 1, &px).expect("编码失败");
    let png_path = fx.dir.join("onepx_default.png");
    let raw_path = fx.dir.join("onepx_default.raw");
    std::fs::write(&png_path, &png).unwrap();
    let out = Command::new(&fx.ffmpeg)
        .args(["-y", "-v", "error"])
        .arg("-i")
        .arg(&png_path)
        .args(["-f", "rawvideo", "-pix_fmt", "rgba"])
        .arg(&raw_path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(&raw_path).unwrap(), px);
}
