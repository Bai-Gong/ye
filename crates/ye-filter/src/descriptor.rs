//! 滤镜描述符：滤镜名、输入/输出垫与运行参数的静态描述。

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

/// 滤镜运行参数（[`FilterDescriptor::params`] 的载荷），M3 处理图最小集
/// 定形的三个滤镜参数形态：crop 的 `x/y/width/height`、fps 的
/// `num/den`、volume 的 `gain`。
///
/// ## 参数校验归属
///
/// 参数在本层是**纯数据、无不变式**（与描述符整体一致）；合法性
/// （裁剪窗越界、yuv420p 偶数对齐、帧率非零、增益有限）由执行核
/// （`video` 模块对应函数）在消费时校验并走 `Result`。
///
/// derive 清单：`Debug, Clone, PartialEq, Default`（`Default` =
/// [`FilterParams::None`]）。不 derive `Eq`：`Volume` 载荷为 `f32`
/// 线性增益，浮点无全序等价——增益是数学参数而非权威时戳/尺寸值，
/// 不属全库「f64 只用于显示」禁区的权威量。
#[derive(Debug, Clone, PartialEq, Default)]
pub enum FilterParams {
    /// 无参数（直通滤镜，或参数承载归后续轮次的滤镜——如 scale 的
    /// 目标尺寸仍见模块 doc「推迟到实现轮次」注记）。
    #[default]
    None,
    /// 矩形裁剪：裁剪窗左上角 `(x, y)`、尺寸 `width × height`（像素）。
    /// 执行核 [`crop_yuv420p`](crate::video::crop_yuv420p) 要求
    /// `x`/`y`/`width`/`height` 全偶（yuv420p 平面几何），违者 `Err`。
    Crop {
        /// 窗左上角横坐标。
        x: u32,
        /// 窗左上角纵坐标。
        y: u32,
        /// 窗宽。
        width: u32,
        /// 窗高。
        height: u32,
    },
    /// 帧率重采样：目标帧率 `num/den`（每秒帧数）。执行核
    /// [`fps_nearest_frame_indices`](crate::video::fps_nearest_frame_indices)
    /// 要求两者非零，违者 `Err`。
    Fps {
        /// 每秒帧数分子。
        num: u32,
        /// 每秒帧数分母。
        den: u32,
    },
    /// 音量：线性增益倍率（乘法 + [−1,1] 削波，见
    /// [`volume_f32`](crate::video::volume_f32)）。dB 换算口径：
    /// `gain = 10^(dB/20)`（0 dB = ×1、+20 dB = ×10、+6 dB ≈ ×1.9953），
    /// 便捷换算见 [`volume_gain_from_db`](crate::video::volume_gain_from_db)。
    Volume {
        /// 线性增益（1.0 = 恒等）。
        gain: f32,
    },
}

/// 滤镜描述符：一个滤镜节点的静态契约（名 + 输入/输出垫表 + 运行参数）。
///
/// 公有字段、无不变式。垫表可以为空：源滤镜（读文件/发生器）无输入垫，
/// 汇滤镜（写文件/显示）无输出垫。垫的格式协商（像素/采样格式）推迟到
/// 实现轮次再定契约；运行参数形态已定形三项（见 [`FilterParams`]），
/// 其余滤镜（如 scale 的目标尺寸）的参数承载归后续轮次。
///
/// derive 清单：`Debug, Clone, PartialEq`（`params` 含 `f32` 载荷，
/// 不可 `Eq`，理由见 [`FilterParams`]）。
#[derive(Debug, Clone, PartialEq)]
pub struct FilterDescriptor {
    /// 滤镜名（未来注册表的键，如 `"scale"`）。
    pub name: String,
    /// 输入垫表（索引即 [`PadRef`](super::PadRef) 的 `pad` 字段所用的垫号）。
    pub inputs: Vec<PadDescriptor>,
    /// 输出垫表（索引语义同 `inputs`）。
    pub outputs: Vec<PadDescriptor>,
    /// 运行参数（形态见 [`FilterParams`]；缺省无参数）。
    pub params: FilterParams,
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
            params: FilterParams::None,
        };
        let b = a.clone();
        assert_eq!(a, b);
        let mut c = b.clone();
        c.inputs.clear();
        assert_ne!(a, c);
        let mut d = b.clone();
        d.outputs[0].media = MediaType::Audio;
        assert_ne!(a, d);
        // 参数参与值比较
        let mut e = b.clone();
        e.params = FilterParams::Crop {
            x: 0,
            y: 0,
            width: 64,
            height: 48,
        };
        assert_ne!(a, e);
    }

    #[test]
    fn empty_pad_tables_are_allowed() {
        // 源滤镜无输入、汇滤镜无输出：静态契约允许空垫表
        let source = FilterDescriptor {
            name: "source".to_string(),
            inputs: Vec::new(),
            outputs: vec![pad("default", MediaType::Video)],
            params: FilterParams::None,
        };
        let sink = FilterDescriptor {
            name: "sink".to_string(),
            inputs: vec![pad("default", MediaType::Video)],
            outputs: Vec::new(),
            params: FilterParams::None,
        };
        assert!(source.inputs.is_empty());
        assert!(sink.outputs.is_empty());
        assert_ne!(source, sink);
    }

    // ---- 参数形态定形（M3：crop x/y/w/h、fps num/den、volume gain）----

    #[test]
    fn params_default_is_none() {
        assert_eq!(FilterParams::default(), FilterParams::None);
    }

    #[test]
    fn params_variants_construct_and_compare() {
        let crop = FilterParams::Crop {
            x: 8,
            y: 4,
            width: 64,
            height: 48,
        };
        assert_eq!(crop, crop.clone());
        assert_ne!(
            crop,
            FilterParams::Crop {
                x: 8,
                y: 4,
                width: 64,
                height: 36
            }
        );
        let fps = FilterParams::Fps { num: 30, den: 1 };
        assert_eq!(fps, FilterParams::Fps { num: 30, den: 1 });
        assert_ne!(fps, FilterParams::Fps { num: 25, den: 1 });
        let vol = FilterParams::Volume { gain: 0.5 };
        assert_eq!(vol, FilterParams::Volume { gain: 0.5 });
        assert_ne!(vol, FilterParams::Volume { gain: 1.0 });
        // 三个形态互不相等，且都与 None 不等
        assert_ne!(crop, fps);
        assert_ne!(fps, vol);
        assert_ne!(crop, FilterParams::None);
    }
}
