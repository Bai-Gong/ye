//! 向量清单（`manifest.toml`）的解析：行式 TOML 子集。
//!
//! ## 为什么手写子集解析
//!
//! 全库零第三方依赖，清单格式由本框架自定义——固定字段、固定层级，
//! 不支持的语法一律报错（解析器宁严勿宽：清单是入库资产，手滑改坏
//! 要在解析期炸出来，不能静默漏跑向量）。
//!
//! ## 格式（v1）
//!
//! 就位集（参与对拍与摘要行 ran/skipped/failed/sha_ok 计数）——
//! 来源二选一：`source`（下载）或 `generate`（本机 ffmpeg 配方，
//! 2026-10-24 施工图①裁决：外网被墙环境以确定性 lavfi 源自造，
//! `ffmpeg_version` 必填以钉死生成环境）：
//!
//! ```toml
//! [[vector]]
//! name = "h264-testsrc2-176x144"        # 缓存文件名主干（缓存文件 = name.container）
//! codec = "h264"
//! container = "mp4"
//! set = "intra"                         # 可选：集标签（施工图②子阶段选通，见 runner --set）
//! generate = "ffmpeg -y … {OUT}"         # {OUT} 由获取脚本替换为缓存路径
//! ffmpeg_version = "ffmpeg version N-…（ffmpeg -version 首行）"
//! sha256 = "64 位小写十六进制（只准实测）"
//! ```
//!
//! 集标签（2026-10-24 施工图②增补）：`intra` = ②a 纯 I 集（`-g 1`，
//! 即被本机 ffmpeg 移除的 `-intra` 的等价形式）；`ip` = ②b I/P 混合集；
//! **不带 `set` 的既有 v1 条目 = 全集**（②c 跑全集 = 不加 `--set`）。
//!
//! 待网络清册（**不参与** ran/skipped 计数与终验断言，摘要行另落
//! `pending=K`；网络恢复后实测 sha256 并升入 [[vector]]）：
//!
//! ```toml
//! [[pending_vector]]
//! name = "fate-h264-ba1-ft-c"
//! source = "https://samples.ffmpeg.org/…"
//! note = "待网络登记理由"
//! ```

/// 就位集向量条目。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorEntry {
    /// 向量名（缓存文件名主干）。
    pub name: String,
    /// 编解码器（对拍口径分派键，如 "h264"）。
    pub codec: String,
    /// 容器（缓存文件扩展名，如 "mp4"）。
    pub container: String,
    /// 集标签（施工图②子阶段选通：intra/ip；None = v1 全集）。
    pub set: Option<String>,
    /// 下载地址（与 `generate` 二选一）。
    pub source: Option<String>,
    /// 生成配方（完整 ffmpeg 命令行，`{OUT}` 为缓存路径占位）。
    pub generate: Option<String>,
    /// 生成环境的 ffmpeg 版本（`ffmpeg -version` 首行；配方向量必填）。
    pub ffmpeg_version: Option<String>,
    /// 期望 SHA-256（64 位小写十六进制；**只准实测**，禁止推测值）。
    pub sha256: String,
}

/// 待网络清册条目（登记项，不参与对拍与断言）。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingVector {
    /// 向量名。
    pub name: String,
    /// 下载地址。
    pub source: String,
    /// 登记理由/备注。
    pub note: Option<String>,
}

/// 清单。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// 清单格式版本（当前 1）。
    pub version: u64,
    /// 就位集（按文件顺序）。
    pub vectors: Vec<VectorEntry>,
    /// 待网络清册（按文件顺序）。
    pub pending: Vec<PendingVector>,
}

/// 就位集必填键。
const REQUIRED: [&str; 4] = ["name", "codec", "container", "sha256"];
/// 就位集可选键。
const OPTIONAL: [&str; 4] = ["set", "source", "generate", "ffmpeg_version"];
/// 清册必填键。
const PENDING_REQUIRED: [&str; 2] = ["name", "source"];
/// 清册可选键。
const PENDING_OPTIONAL: [&str; 1] = ["note"];

/// 块类型（解析状态）。
#[derive(PartialEq)]
enum Block {
    Top,
    Vector,
    Pending,
}

/// 从已见键值取条目字段。
struct Fields<'a> {
    pairs: &'a [(String, String)],
}

impl Fields<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// 校验键集：必填齐全、无未知键；返回字段视图。
fn validate_fields(
    fields: &[(String, String)],
    required: &[&str],
    optional: &[&str],
    block: &str,
) -> Result<(), String> {
    for key in required {
        if !fields.iter().any(|(k, _)| k == key) {
            return Err(format!("{block} 缺字段 {key}"));
        }
    }
    for (k, _) in fields {
        if !required.contains(&k.as_str()) && !optional.contains(&k.as_str()) {
            return Err(format!("{block} 不认识键 `{k}`"));
        }
    }
    Ok(())
}

/// 把已见键值组装成就位集条目。
fn take_vector(fields: &[(String, String)]) -> Result<VectorEntry, String> {
    validate_fields(fields, &REQUIRED, &OPTIONAL, "vector")?;
    let f = Fields { pairs: fields };
    let source = f.get("source").map(str::to_string);
    let generate = f.get("generate").map(str::to_string);
    let ffmpeg_version = f.get("ffmpeg_version").map(str::to_string);
    // 来源二选一
    match (&source, &generate) {
        (None, None) => {
            return Err(format!(
                "vector {} 缺来源（source/generate 二选一）",
                f.get("name").unwrap_or("?")
            ));
        }
        (Some(_), Some(_)) => {
            return Err(format!(
                "vector {} source 与 generate 同时给出（二选一）",
                f.get("name").unwrap_or("?")
            ));
        }
        _ => {}
    }
    // ffmpeg_version 随 generate
    if generate.is_some() && ffmpeg_version.is_none() {
        return Err(format!(
            "vector {} 用 generate 须记 ffmpeg_version（ffmpeg -version 首行）",
            f.get("name").unwrap_or("?")
        ));
    }
    if source.is_some() && ffmpeg_version.is_some() {
        return Err(format!(
            "vector {} 用 source 不应记 ffmpeg_version",
            f.get("name").unwrap_or("?")
        ));
    }
    let entry = VectorEntry {
        name: f.get("name").unwrap_or("").to_string(),
        codec: f.get("codec").unwrap_or("").to_string(),
        container: f.get("container").unwrap_or("").to_string(),
        set: f.get("set").map(str::to_string),
        source,
        generate,
        ffmpeg_version,
        sha256: f.get("sha256").unwrap_or("").to_string(),
    };
    if !is_hex64(&entry.sha256) {
        return Err(format!(
            "vector {}: sha256 非法（需 64 位小写十六进制；只准实测）",
            entry.name
        ));
    }
    Ok(entry)
}

/// 把已见键值组装成清册条目。
fn take_pending(fields: &[(String, String)]) -> Result<PendingVector, String> {
    validate_fields(
        fields,
        &PENDING_REQUIRED,
        &PENDING_OPTIONAL,
        "pending_vector",
    )?;
    let f = Fields { pairs: fields };
    Ok(PendingVector {
        name: f.get("name").unwrap_or("").to_string(),
        source: f.get("source").unwrap_or("").to_string(),
        note: f.get("note").map(str::to_string),
    })
}

impl Manifest {
    /// 解析清单文本。语法/缺字段/来源冲突/sha256 非法 → `Err`（带行号）。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut version: Option<u64> = None;
        let mut vectors: Vec<VectorEntry> = Vec::new();
        let mut pending: Vec<PendingVector> = Vec::new();
        let mut fields: Vec<(String, String)> = Vec::new();
        let mut block = Block::Top;

        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lineno = idx + 1;
            let new_block = match line {
                "[[vector]]" => Some(Block::Vector),
                "[[pending_vector]]" => Some(Block::Pending),
                _ => None,
            };
            if let Some(nb) = new_block {
                match block {
                    Block::Vector => vectors.push(take_vector(&fields)?),
                    Block::Pending => pending.push(take_pending(&fields)?),
                    Block::Top => {}
                }
                fields.clear();
                block = nb;
                continue;
            }
            let Some((key, value)) = split_kv(line) else {
                return Err(format!("第 {lineno} 行：无法解析 `{line}`"));
            };
            match block {
                Block::Vector | Block::Pending => {
                    if fields.iter().any(|(k, _)| *k == key) {
                        return Err(format!("第 {lineno} 行：键 `{key}` 重复"));
                    }
                    fields.push((key, value));
                }
                Block::Top => {
                    if key != "manifest_version" {
                        return Err(format!(
                            "第 {lineno} 行：顶层不认识键 `{key}`（字段须写在 [[vector]]/[[pending_vector]] 之后）"
                        ));
                    }
                    version = Some(
                        value
                            .parse()
                            .map_err(|_| format!("第 {lineno} 行：manifest_version 非整数"))?,
                    );
                }
            }
        }
        match block {
            Block::Vector => vectors.push(take_vector(&fields)?),
            Block::Pending => pending.push(take_pending(&fields)?),
            Block::Top => {}
        }
        let version = version.ok_or("缺 manifest_version")?;
        if version != 1 {
            return Err(format!("manifest_version = {version} 不受支持（当前仅 1）"));
        }
        Ok(Manifest {
            version,
            vectors,
            pending,
        })
    }
}

/// 拆 `key = "value"` / `key = 123`；返回 (key, value 去引号)。
fn split_kv(line: &str) -> Option<(String, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim().to_string();
    let mut value = line[eq + 1..].trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value = &value[1..value.len() - 1];
    }
    if key.is_empty() || value.is_empty() {
        return None;
    }
    Some((key, value.to_string()))
}

/// 64 位小写十六进制校验。
fn is_hex64(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD_SHA: &str = "a3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn generated_vector(name: &str) -> String {
        format!(
            "[[vector]]\nname = \"{name}\"\ncodec = \"h264\"\ncontainer = \"mp4\"\n\
             generate = \"ffmpeg -y -v error -f lavfi -i testsrc2=size=176x144:rate=15 -t 3 {{OUT}}\"\n\
             ffmpeg_version = \"ffmpeg version N-123627 (测试)\"\nsha256 = \"{GOOD_SHA}\"\n"
        )
    }

    fn sample() -> String {
        format!(
            "# 注释\nmanifest_version = 1\n\n{}[[vector]]\nname = \"dl\"\ncodec = \"h264\"\ncontainer = \"mp4\"\n\
             source = \"https://example.com/dl.mp4\"\nsha256 = \"{GOOD_SHA}\"\n\n\
             [[pending_vector]]\nname = \"fate-ba1\"\nsource = \"https://samples.ffmpeg.org/...\"\nnote = \"待网络\"\n",
            generated_vector("gen-1")
        )
    }

    #[test]
    fn parse_mixed_manifest() {
        let m = Manifest::parse(&sample()).unwrap();
        assert_eq!(m.version, 1);
        assert_eq!(m.vectors.len(), 2);
        assert_eq!(m.vectors[0].name, "gen-1");
        assert_eq!(
            m.vectors[0]
                .generate
                .as_deref()
                .map(|s| s.contains("{OUT}")),
            Some(true)
        );
        assert_eq!(m.vectors[0].source, None);
        assert!(m.vectors[0].ffmpeg_version.is_some());
        assert_eq!(m.vectors[0].set, None);
        assert_eq!(m.vectors[1].name, "dl");
        assert_eq!(
            m.vectors[1].source.as_deref(),
            Some("https://example.com/dl.mp4")
        );
        assert_eq!(m.vectors[1].generate, None);
        assert_eq!(m.pending.len(), 1);
        assert_eq!(m.pending[0].name, "fate-ba1");
        assert_eq!(m.pending[0].note.as_deref(), Some("待网络"));
    }

    #[test]
    fn rejects_source_generate_rules() {
        // 两者都缺
        let bad = sample().replacen("source = \"https://example.com/dl.mp4\"\n", "", 1);
        match Manifest::parse(&bad) {
            Err(msg) => assert!(msg.contains("二选一"), "{msg}"),
            Ok(_) => panic!("缺来源应报错"),
        }
        // 两者同给
        let both = sample().replacen(
            "source = \"https://example.com/dl.mp4\"",
            "source = \"https://example.com/dl.mp4\"\ngenerate = \"ffmpeg {OUT}\"",
            1,
        );
        match Manifest::parse(&both) {
            Err(msg) => assert!(msg.contains("同时给出"), "{msg}"),
            Ok(_) => panic!("双来源应报错"),
        }
        // generate 缺 ffmpeg_version
        let nover = sample().replacen(
            "ffmpeg_version = \"ffmpeg version N-123627 (测试)\"\n",
            "",
            1,
        );
        match Manifest::parse(&nover) {
            Err(msg) => assert!(msg.contains("ffmpeg_version"), "{msg}"),
            Ok(_) => panic!("generate 缺 ffmpeg_version 应报错"),
        }
    }

    #[test]
    fn rejects_missing_field_and_bad_sha() {
        let bad = sample().replacen("container = \"mp4\"\n", "", 1);
        match Manifest::parse(&bad) {
            Err(msg) => assert!(msg.contains("container"), "{msg}"),
            Ok(_) => panic!("缺 container 应报错"),
        }
        let sha = sample().replacen(GOOD_SHA, "XYZ", 1);
        assert!(Manifest::parse(&sha).is_err());
    }

    #[test]
    fn rejects_unknown_and_duplicate_keys() {
        let bad = sample().replacen("codec = \"h264\"", "bitrate = \"1M\"\ncodec = \"h264\"", 1);
        match Manifest::parse(&bad) {
            Err(msg) => assert!(msg.contains("bitrate"), "{msg}"),
            Ok(_) => panic!("未知键应报错"),
        }
        let dup = sample().replacen("codec = \"h264\"", "codec = \"h264\"\ncodec = \"h264\"", 1);
        assert!(Manifest::parse(&dup).is_err());
    }

    #[test]
    fn empty_and_pending_only_manifests() {
        let m = Manifest::parse("manifest_version = 1\n").unwrap();
        assert!(m.vectors.is_empty() && m.pending.is_empty());
        let p = Manifest::parse(
            "manifest_version = 1\n\n[[pending_vector]]\nname = \"x\"\nsource = \"https://x\"\n",
        )
        .unwrap();
        assert_eq!(p.pending.len(), 1);
        assert!(p.vectors.is_empty());
    }

    /// 施工图②集标签：可选键 `set`（intra/ip）；不带 = 全集（None）。
    #[test]
    fn set_tag_is_optional_and_parsed() {
        let with = format!(
            "manifest_version = 1\n\n{}",
            generated_vector("intra-1").replacen(
                "container = \"mp4\"\n",
                "container = \"mp4\"\nset = \"intra\"\n",
                1,
            )
        );
        let m = Manifest::parse(&with).unwrap();
        assert_eq!(m.vectors[0].set.as_deref(), Some("intra"));
        // 不带 set → None（v1 全集语义）
        let plain = format!("manifest_version = 1\n\n{}", generated_vector("plain"));
        let m = Manifest::parse(&plain).unwrap();
        assert_eq!(m.vectors[0].set, None);
    }
}
