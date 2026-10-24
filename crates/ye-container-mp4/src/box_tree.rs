//! box 树：探查/调试用，解析的副产品。

use crate::types::FourCc;

/// 一个 box 的位置与大小。`offset` 为 box 起始绝对位置，
/// `size` 为含头的完整大小。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxInfo {
    pub kind: FourCc,
    pub offset: u64,
    pub size: u64,
}

/// box 树节点。容器 box（moov/trak/mdia/minf/stbl 与 moof/traf/mvex）
/// 有子节点。
#[derive(Clone, Debug)]
pub struct BoxNode {
    pub info: BoxInfo,
    pub children: Vec<BoxNode>,
}
