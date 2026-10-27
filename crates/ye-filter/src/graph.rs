//! 处理图：滤镜节点与垫到垫连线的组装，及拓扑排序接口。

use std::collections::VecDeque;

use crate::descriptor::FilterDescriptor;
use crate::error::FilterError;

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
/// ## 能力边界（M3 处理图最小集）
///
/// - **可构造**：[`add_filter`](Self::add_filter) 追加节点并返回句柄，
///   [`connect`](Self::connect) 真实记边（做节点/垫索引与**两侧垫媒体
///   类型一致性**校验；不做去重——平行边合法，拓扑排序按边计数正确
///   处理）；
/// - **可查询**：`node_count`/`edge_count`/`filter`/`edges` 可完整读回
///   图结构；
/// - **可排序**：[`topological_order`](Self::topological_order) 返回依赖
///   序（Kahn 算法，成环报错）——通用调度与帧处理（按序执行滤镜）归
///   后续里程碑，本轮只交付排序。
///
/// derive 清单：`Debug, Clone, PartialEq`（节点描述符因
/// [`FilterParams`](crate::FilterParams) 的 `Volume` 载荷为 `f32` 而不可
/// `Eq`，图随之为；`Default` 见手工实现，等价 [`FilterGraph::new`]）。
#[derive(Debug, Clone, PartialEq)]
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

    /// 拓扑排序（Kahn 算法）：返回依依赖序可执行的节点句柄序列——
    /// 每个节点排在其全部上游（有边指向它的节点）之后。
    ///
    /// - **确定性**：入度为 0 的节点按句柄升序入队，节点就绪（全部上游
    ///   已出队）即入队（FIFO）——同一图构造序下结果唯一；
    /// - **孤立节点**（无边相连）按句柄序包含在结果中；空图返回空序；
    /// - 平行边（同对节点重复 `connect`）按边计数，不影响正确性。
    ///
    /// # Errors
    /// 图中存在环（含自环）→ [`FilterError::Cycle`]：队列排空时仍有
    /// 节点未出队，即存在循环依赖。
    pub fn topological_order(&self) -> Result<Vec<NodeId>, FilterError> {
        let n = self.nodes.len();
        // 邻接表与入度（平行边逐条计数，保证一致）
        let mut adjacency: Vec<Vec<NodeId>> = vec![Vec::new(); n];
        let mut indegree = vec![0usize; n];
        for &(from, to) in &self.edges {
            adjacency[from.node].push(to.node);
            indegree[to.node] += 1;
        }
        // 初始就绪集：入度 0 的节点按句柄升序
        let mut queue: VecDeque<NodeId> = (0..n).filter(|&i| indegree[i] == 0).collect();
        let mut order = Vec::with_capacity(n);
        while let Some(node) = queue.pop_front() {
            order.push(node);
            for &down in &adjacency[node] {
                indegree[down] -= 1;
                if indegree[down] == 0 {
                    queue.push_back(down);
                }
            }
        }
        if order.len() != n {
            return Err(FilterError::Cycle("filter graph contains a cycle"));
        }
        Ok(order)
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
    use crate::descriptor::{FilterParams, PadDescriptor};
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
            params: FilterParams::None,
        }
    }

    /// 源滤镜（无输入垫）。
    fn source_filter(name: &str) -> FilterDescriptor {
        FilterDescriptor {
            name: name.to_string(),
            inputs: Vec::new(),
            outputs: vec![pad("default", MediaType::Video)],
            params: FilterParams::None,
        }
    }

    /// 连线便捷封装：`a` 输出垫 0 → `b` 输入垫 0。
    fn link(g: &mut FilterGraph, a: NodeId, b: NodeId) {
        g.connect(PadRef { node: a, pad: 0 }, PadRef { node: b, pad: 0 })
            .unwrap();
    }

    /// 依赖序性质校验：结果为全体节点的排列，且每条边 from → to
    /// 满足 pos(from) < pos(to)。
    fn assert_valid_topo_order(g: &FilterGraph, order: &[NodeId]) {
        assert_eq!(order.len(), g.node_count(), "order covers all nodes");
        let mut seen = vec![false; g.node_count()];
        for &n in order {
            assert!(!seen[n], "node {n} appears twice");
            seen[n] = true;
        }
        let pos = |node: NodeId| order.iter().position(|&x| x == node).unwrap();
        for &(from, to) in g.edges() {
            assert!(
                pos(from.node) < pos(to.node),
                "edge {from:?} -> {to:?} violates dependency order {order:?}"
            );
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

    // ---- 拓扑排序（M3 处理图最小集：Kahn 真实现）----

    #[test]
    fn topological_order_empty_graph_is_empty() {
        let order = FilterGraph::new().topological_order().unwrap();
        assert!(order.is_empty());
    }

    #[test]
    fn topological_order_chain_respects_dependency() {
        // scale(0) → format(1) → crop(2)：唯一合法序 [0, 1, 2]
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        let c = g.add_filter(video_filter("crop"));
        link(&mut g, a, b);
        link(&mut g, b, c);
        assert_eq!(g.topological_order().unwrap(), vec![a, b, c]);
    }

    #[test]
    fn topological_order_reverse_insertion_chain() {
        // 节点按 0/1 添加，但依赖 1 → 0：0 必须排在 1 之后
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        link(&mut g, b, a);
        assert_eq!(g.topological_order().unwrap(), vec![b, a]);
    }

    #[test]
    fn topological_order_diamond() {
        // 菱形：src(0) → {b(1), c(2)} → sink(3)；FIFO 确定性给 [0,1,2,3]
        let mut g = FilterGraph::new();
        let src = g.add_filter(source_filter("src"));
        let b = g.add_filter(video_filter("split-b"));
        let c = g.add_filter(video_filter("split-c"));
        let sink = g.add_filter(video_filter("sink"));
        link(&mut g, src, b);
        link(&mut g, src, c);
        link(&mut g, b, sink);
        link(&mut g, c, sink);
        let order = g.topological_order().unwrap();
        assert_eq!(order, vec![src, b, c, sink]);
        assert_valid_topo_order(&g, &order);
    }

    #[test]
    fn topological_order_isolated_nodes_included() {
        // 纯孤立节点：按句柄序全包含
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        let c = g.add_filter(video_filter("crop"));
        assert_eq!(g.topological_order().unwrap(), vec![a, b, c]);

        // 混合：a(0) → c(2)，b(1) 孤立——初始就绪 {0, 1}（句柄升序），
        // FIFO 出队 0 → 2 就绪入队，再 1、2
        let mut g2 = FilterGraph::new();
        let x = g2.add_filter(video_filter("scale"));
        let lone = g2.add_filter(video_filter("format"));
        let z = g2.add_filter(video_filter("crop"));
        link(&mut g2, x, z);
        let order = g2.topological_order().unwrap();
        assert_valid_topo_order(&g2, &order);
        assert_eq!(order.len(), 3);
        assert!(order.contains(&lone), "isolated node must be included");
    }

    #[test]
    fn topological_order_two_node_cycle_is_error() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        link(&mut g, a, b);
        link(&mut g, b, a);
        match g.topological_order() {
            Err(FilterError::Cycle(msg)) => {
                assert_eq!(msg, "filter graph contains a cycle");
                assert!(
                    g.topological_order()
                        .unwrap_err()
                        .to_string()
                        .contains("cycle"),
                    "display mentions cycle"
                );
            }
            other => panic!("expected Cycle, got {other:?}"),
        }
    }

    #[test]
    fn topological_order_self_loop_is_error() {
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        // 自环：输出垫 0 → 自己的输入垫 0（connect 允许记录）
        link(&mut g, a, a);
        assert!(matches!(
            g.topological_order(),
            Err(FilterError::Cycle("filter graph contains a cycle"))
        ));
    }

    #[test]
    fn topological_order_larger_cycle_with_tail() {
        // 0 → 1 → 2 → 1（环在 1/2），0 在环外：仍须报错
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("a"));
        let b = g.add_filter(video_filter("b"));
        let c = g.add_filter(video_filter("c"));
        link(&mut g, a, b);
        link(&mut g, b, c);
        link(&mut g, c, b);
        assert!(matches!(g.topological_order(), Err(FilterError::Cycle(_))));
    }

    #[test]
    fn topological_order_parallel_edges_are_legal() {
        // 平行边（不去了重）：a → b 连两次，仍为合法 DAG
        let mut g = FilterGraph::new();
        let a = g.add_filter(video_filter("scale"));
        let b = g.add_filter(video_filter("format"));
        link(&mut g, a, b);
        link(&mut g, a, b);
        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.topological_order().unwrap(), vec![a, b]);
    }

    #[test]
    fn topological_order_arbitrary_dag_property() {
        // 较大的任意 DAG：只验依赖序性质（每条边 from 先于 to）
        let mut g = FilterGraph::new();
        let ids: Vec<NodeId> = (0..7)
            .map(|i| g.add_filter(video_filter(&format!("f{i}"))))
            .collect();
        let chain = [
            (0, 1),
            (0, 2),
            (1, 3),
            (2, 3),
            (3, 4),
            (0, 5),
            (5, 6),
            (3, 6),
        ];
        for &(from, to) in &chain {
            link(&mut g, ids[from], ids[to]);
        }
        let order = g.topological_order().unwrap();
        assert_valid_topo_order(&g, &order);
        // 上游最深的节点 6 排最后
        assert_eq!(*order.last().unwrap(), ids[6]);
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
            params: FilterParams::None,
        });
        // 同类相连合法。
        g.connect(
            PadRef {
                node: video,
                pad: 0,
            },
            PadRef {
                node: video,
                pad: 0,
            },
        )
        .unwrap();
        // video 输出 → audio 输入：媒体类型失配 ⇒ InvalidArguments，不记边。
        let err = g.connect(
            PadRef {
                node: video,
                pad: 0,
            },
            PadRef {
                node: audio,
                pad: 0,
            },
        );
        assert_eq!(
            err,
            Err(FilterError::InvalidArguments(
                "pad media type mismatch (output pad vs input pad)"
            ))
        );
        // audio 输出 → video 输入同拒。
        let err = g.connect(
            PadRef {
                node: audio,
                pad: 0,
            },
            PadRef {
                node: video,
                pad: 0,
            },
        );
        assert!(matches!(err, Err(FilterError::InvalidArguments(_))));
        // audio→audio 合法（此时共 2 条边）。
        g.connect(
            PadRef {
                node: audio,
                pad: 0,
            },
            PadRef {
                node: audio,
                pad: 0,
            },
        )
        .unwrap();
        assert_eq!(g.edge_count(), 2);
    }
}
