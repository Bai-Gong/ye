//! 滤镜描述符：滤镜名与输入/输出垫的静态描述。

use ye_codec::MediaType;

/// 单个垫（pad）的静态描述。
///
/// 公有字段、无不变式。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PadDescriptor {
    /// 垫名（同一滤镜内唯一，如 `"default"`）。
    pub name: String,
    /// 垫承载的媒体类型。
    pub media: MediaType,
}

/// 滤镜描述符：一个滤镜节点的静态契约（名 + 输入/输出垫表）。
///
/// 公有字段、无不变式。垫表可以为空：源滤镜（读文件/发生器）无输入垫，
/// 汇滤镜（写文件/显示）无输出垫。滤镜的运行参数（如 scale 的目标
/// 尺寸）与垫的格式协商（像素/采样格式）推迟到实现轮次再定契约。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterDescriptor {
    /// 滤镜名（未来注册表的键，如 `"scale"`）。
    pub name: String,
    /// 输入垫表（索引即 [`PadRef`](super::PadRef) 的 `pad` 字段所用的垫号）。
    pub inputs: Vec<PadDescriptor>,
    /// 输出垫表（索引语义同 `inputs`）。
    pub outputs: Vec<PadDescriptor>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pad(name: &str, media: MediaType) -> PadDescriptor {
        PadDescriptor {
            name: name.to_string(),
            media,
        }
    }

    #[test]
    fn descriptors_are_value_comparable() {
        let a = FilterDescriptor {
            name: "scale".to_string(),
            inputs: vec![pad("default", MediaType::Video)],
            outputs: vec![pad("default", MediaType::Video)],
        };
        let b = a.clone();
        assert_eq!(a, b);
        let mut c = b.clone();
        c.inputs.clear();
        assert_ne!(a, c);
        let mut d = b.clone();
        d.outputs[0].media = MediaType::Audio;
        assert_ne!(a, d);
    }

    #[test]
    fn empty_pad_tables_are_allowed() {
        // 源滤镜无输入、汇滤镜无输出：静态契约允许空垫表
        let source = FilterDescriptor {
            name: "source".to_string(),
            inputs: Vec::new(),
            outputs: vec![pad("default", MediaType::Video)],
        };
        let sink = FilterDescriptor {
            name: "sink".to_string(),
            inputs: vec![pad("default", MediaType::Video)],
            outputs: Vec::new(),
        };
        assert!(source.inputs.is_empty());
        assert!(sink.outputs.is_empty());
        assert_ne!(source, sink);
    }
}
