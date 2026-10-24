//! 处理图：滤镜节点与垫到垫连线的组装，及拓扑排序接口（桩）。

use crate::descriptor::FilterDescriptor;
use crate::error::FilterError;

/// 拓扑排序桩的失败文案（M3：处理图最小集随编码/转码里程碑交付，
/// 见 roadmap.md「里程碑」现行排期）。
const TOPOSORT_STUB_MSG: &str = "filter graph topological sort not implemented (桩：M3 交付)";

/// 节点句柄（[`FilterGraph::add_filter`] 的返回值，即图内节点索引）。
pub type NodeId = usize;

/// 一个垫的定位：节点 + 该节点描述符内的垫索引。
///
/// 输入垫与输出垫的索引各自从 0 起（`connect` 的 `from` 查输出垫表、
/// `to` 查输入垫表）。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`（纯值类型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PadRef {
    /// 节点句柄（[`FilterGraph::add_filter`] 返回值）。
    pub node: NodeId,
    /// 垫索引（输入/输出垫表各自从 0 起）。
    pub pad: u32,
}

/// 处理图（私有字段 + 访问器）：滤镜节点集合 + 垫到垫连线集合。
///
/// ## 桩的语义边界（M3 交付前）
///
/// - **可构造**：[`add_filter`](Self::add_filter) 追加节点并返回句柄，
///   [`connect`](Self::connect) 真实记边（做节点/垫索引与**两侧垫媒体
///   类型一致性**校验——均为静态契约，2026-10-24 评审补媒体类型；
///   不做去重与成环检测——那需要拓扑排序，随其实现交付）；
/// - **可查询**：`node_count`/`edge_count`/`filter`/`edges` 可完整读回
///   图结构；
/// - **不可用**：[`topological_order`](Self::topological_order) 调用即
///   返回 [`FilterError::Unsupported`]（文案含里程碑标注）——调度与
///   帧处理能力随实现交付。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`（节点描述符与连线均为
/// 值类型；`Default` 见手工实现，等价 [`FilterGraph::new`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilterGraph {
    nodes: Vec<FilterDescriptor>,
    edges: Vec<(PadRef, PadRef)>,
}

impl FilterGraph {
    /// 空图。
    pub fn new() -> FilterGraph {
        FilterGraph {
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    /// 追加一个滤镜节点，返回其句柄（后续 [`connect`](Self::connect) 与
    /// [`filter`](Self::filter) 以此引用）。
    pub fn add_filter(&mut self, descriptor: FilterDescriptor) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(descriptor);
        id
    }

    /// 连一条垫到垫的边：`from` 为输出垫，`to` 为输入垫。
    ///
    /// # Errors
    /// 节点句柄或垫索引越界、或两侧垫的媒体类型不一致
    /// （静态契约，2026-10-24 评审补——此前 video→audio 失配静默记边，
    /// 错误推迟到拓扑排序实现才暴露）→ [`FilterError::InvalidArguments`]。
    pub fn connect(&mut self, from: PadRef, to: PadRef) -> Result<(), FilterError> {
        self.check_pad(from, PadSide::Output)?;
        self.check_pad(to, PadSide::Input)?;
        let from_media = self.nodes[from.node].outputs[from.pad as usize].media;
        let to_media = self.nodes[to.node].inputs[to.pad as usize].media;
        if from_media != to_media {
            return Err(FilterError::InvalidArguments(
                "pad media type mismatch (output pad vs input pad)",
            ));
        }
        self.edges.push((from, to));
        Ok(())
    }

    /// 节点数。
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// 边数。
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// 按句柄读回节点描述符（越界返回 `None`）。
    pub fn filter(&self, node: NodeId) -> Option<&FilterDescriptor> {
        self.nodes.get(node)
    }

    /// 全部连线（插入序；元素为 `(输出垫, 输入垫)`）。
    pub fn edges(&self) -> &[(PadRef, PadRef)] {
        &self.edges
    }

    /// 拓扑排序：返回依依赖序可执行的节点句柄序列。
    ///
    /// # Errors
    /// **桩**：调用即 [`FilterError::Unsupported`]，文案注明 M3 交付
    /// （成环报错等语义届时一并定义）。
    pub fn topological_order(&self) -> Result<Vec<NodeId>, FilterError> {
        Err(FilterError::Unsupported(TOPOSORT_STUB_MSG))
    }

    /// 校验 `pad` 落在 `side` 侧的节点与垫索引范围内。
    fn check_pad(&self, pad: PadRef, side: PadSide) -> Result<(), FilterError> {
        let Some(descriptor) = self.nodes.get(pad.node) else {
            return Err(FilterError::InvalidArguments("pad references unknown node"));
        };
        let count = match side {
            PadSide::Input => descriptor.inputs.len(),
            PadSide::Output => descriptor.outputs.len(),
        };
        if pad.pad as usize >= count {
            return Err(FilterError::InvalidArguments("pad index out of range"));
        }
        Ok(())
    }
}

impl Default for FilterGraph {
    /// 缺省为空图（等价 [`FilterGraph::new`]）。
    fn default() -> Self {
        FilterGraph::new()
    }
}

/// 连线校验用的垫侧别（`connect` 的 `from` 是输出垫、`to` 是输入垫）。
enum PadSide {
    Input,
    Output,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::PadDescriptor;
    use ye_codec::MediaType;

    fn pad(name: &str, media: MediaType) -> PadDescriptor {
        PadDescriptor {
            name: name.to_string(),
            media,
        }
    }

    /// 单输入单输出的视频滤镜（如 scale）。
    fn video_filter(name: &str) -> FilterDescriptor {
        FilterDescriptor {
            name: name.to_string(),
            inputs: vec![pad("default", MediaType::Video)],
            outputs: vec![pad("default", MediaType::Video)],
        }
    }

    #[test]
    fn new_is_empty_and_default_matches() {
        let g = FilterGraph::new();
        assert_eq!(g.node_count(), 0);
        assert_eq!(g.edge_count(), 0);
        assert_eq!(FilterGraph::default().node_count(), 0);
    }

    #[test]
    fn add_filter_returns_handles_and_is_queryable() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        assert_eq!((a, b), (0, 1));
        assert_eq!(g.node_count(), 2);
        assert_eq!(g.filter(a).unwrap().name, "scale");
        assert_eq!(g.filter(b).unwrap().name, "format");
        // 越界句柄返回 None（数据驱动，不 panic）
        assert!(g.filter(99).is_none());
    }

    #[test]
    fn connect_records_edges_and_validates_indices() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        let a_out = PadRef { node: a, pad: 0 };
        let b_in = PadRef { node: b, pad: 0 };

        // 合法连线成功并可读回
        g.connect(a_out, b_in).unwrap();
        assert_eq!(g.edge_count(), 1);
        assert_eq!(g.edges(), &[(a_out, b_in)]);

        // 节点越界 → InvalidArguments，且不记边
        let ghost = PadRef { node: 42, pad: 0 };
        assert_eq!(
            g.connect(ghost, b_in),
            Err(FilterError::InvalidArguments("pad references unknown node"))
        );
        assert_eq!(
            g.connect(a_out, ghost),
            Err(FilterError::InvalidArguments("pad references unknown node"))
        );
        // 垫索引越界（单输出滤镜的 1 号输出垫不存在）→ InvalidArguments
        let a_bad_out = PadRef { node: a, pad: 1 };
        let b_bad_in = PadRef { node: b, pad: 1 };
        assert_eq!(
            g.connect(a_bad_out, b_in),
            Err(FilterError::InvalidArguments("pad index out of range"))
        );
        assert_eq!(
            g.connect(a_out, b_bad_in),
            Err(FilterError::InvalidArguments("pad index out of range"))
        );
        // 失败的连线一律不记边
        assert_eq!(g.edge_count(), 1);
    }

    // ---- 桩：拓扑排序调用即 Unsupported，文案含里程碑标注 ----

    #[test]
    fn topological_order_is_stub_unsupported_with_m3_marker() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        g.connect(PadRef { node: a, pad: 0 }, PadRef { node: b, pad: 0 })
            .unwrap();

        match g.topological_order() {
            Err(FilterError::Unsupported(msg)) => {
                assert!(msg.contains("桩：M3 交付"), "msg missing milestone: {msg}");
                assert!(
                    msg.contains("topological sort"),
                    "msg missing capability name: {msg}"
                );
            }
            other => panic!("expected Unsupported, got {other:?}"),
        }
        // Display 文案同样含里程碑标注
        let display = g.topological_order().unwrap_err().to_string();
        assert!(
            display.contains("桩：M3 交付"),
            "display missing milestone: {display}"
        );

        // 空图同样调用即败（桩不区分图内容）
        assert!(matches!(
            FilterGraph::new().topological_order(),
            Err(FilterError::Unsupported(_))
        ));
    }

    // ---- connect 媒体类型一致性（2026-10-24 评审补）----

    #[test]
    fn connect_rejects_media_type_mismatch() {
        let mut g = FilterGraph::new();
        let video = g.add_filter(video_filter("scale"));
        // 音频滤镜（单输入单输出 audio 垫）。
        let audio = g.add_filter(FilterDescriptor {
            name: "volume".to_string(),
            inputs: vec![pad("default", MediaType::Audio)],
            outputs: vec![pad("default", MediaType::Audio)],
        });
        // 同类相连合法。
        g.connect(
            PadRef { node: video, pad: 0 },
            PadRef { node: video, pad: 0 },
        )
        .unwrap();
        // video 输出 → audio 输入：媒体类型失配 ⇒ InvalidArguments，不记边。
        let err = g.connect(
            PadRef { node: video, pad: 0 },
            PadRef { node: audio, pad: 0 },
        );
        assert_eq!(
            err,
            Err(FilterError::InvalidArguments(
                "pad media type mismatch (output pad vs input pad)"
            ))
        );
        // audio 输出 → video 输入同拒。
        let err = g.connect(
            PadRef { node: audio, pad: 0 },
            PadRef { node: video, pad: 0 },
        );
        assert!(matches!(err, Err(FilterError::InvalidArguments(_))));
        // audio→audio 合法（此时共 2 条边）。
        g.connect(
            PadRef { node: audio, pad: 0 },
            PadRef { node: audio, pad: 0 },
        )
        .unwrap();
        assert_eq!(g.edge_count(), 2);
    }
}
