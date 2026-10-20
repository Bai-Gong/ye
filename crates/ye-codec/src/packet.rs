//! 压缩域包 [`Packet`] 与位标志 [`PacketFlags`]。

use std::sync::Arc;

use ye_time::Timestamp;

/// 包标志位（手写位标志，全库禁 bitflags crate）。
///
/// 字段私有的元组结构体（内部 `u32` 位集），只能经常量与方法操作。
///
/// derive 清单：`Debug, Clone, Copy, PartialEq, Eq`；
/// `BitOr`/`BitOrAssign` 为手工实现（组合/累加标志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketFlags(u32);

impl PacketFlags {
    /// 关键帧（可独立解码的帧，解码起点）。
    pub const KEYFRAME: PacketFlags = PacketFlags(1 << 0);
    /// 数据损坏标记（解码器被告知该包不可信）。
    pub const CORRUPT: PacketFlags = PacketFlags(1 << 1);
    /// 丢弃标记（封装/解封装过程中应跳过该包）。
    pub const DISCARD: PacketFlags = PacketFlags(1 << 2);

    /// 空标志（无任何位）。
    pub const fn empty() -> PacketFlags {
        PacketFlags(0)
    }

    /// 是否包含 `other` 的全部位（`self & other == other`）。
    pub const fn contains(self, other: PacketFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// 并入 `other` 的全部位（原地）。
    pub fn insert(&mut self, other: PacketFlags) {
        self.0 |= other.0;
    }

    /// 是否没有任何位。
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for PacketFlags {
    type Output = PacketFlags;

    /// 位并（组合标志，如 `KEYFRAME | CORRUPT`）。
    fn bitor(self, rhs: PacketFlags) -> PacketFlags {
        PacketFlags(self.0 | rhs.0)
    }
}

impl core::ops::BitOrAssign for PacketFlags {
    /// 位并赋值（累加标志）。
    fn bitor_assign(&mut self, rhs: PacketFlags) {
        self.0 |= rhs.0;
    }
}

/// 压缩域包（纯数据载体，公有字段、无不变式——对标 FFmpeg `AVPacket`）。
///
/// `data` 用 [`Arc<[u8]>`]：解封装 → 解码、编码 → 封装之间零拷贝共享
/// （`Arc::ptr_eq` 可验共享，克隆不复制负载）。
///
/// ## 时戳契约
///
/// `pts`/`dts`/`duration` 各自带所属流的时基（[`Timestamp`] 内含
/// `time_base`，包自描述）；缺时戳用 `None`（无哨兵值）。打戳职责：
/// - 解封装器负责以流时基给包打戳；
/// - 编码器输出包以 `EncoderConfig.time_base` 打戳；
/// - 封装器写包时负责向目标流时基重标度。
///
/// derive 清单：`Debug, Clone, PartialEq, Eq`
/// （`Arc<[u8]>` 克隆共享、可判等——按值相等）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    /// 压缩码流字节（与 `stream_index` 指向流的编码格式一致）。
    pub data: Arc<[u8]>,
    /// 所属流索引（由解封装器/调用方编排）。
    pub stream_index: u32,
    /// 显示时戳（解封装器以流时基打戳；缺省 `None`）。
    pub pts: Option<Timestamp>,
    /// 解码时戳（B 流重排前的到达序；缺省 `None`）。
    pub dts: Option<Timestamp>,
    /// 包时长（同流时基；缺省 `None`）。
    pub duration: Option<Timestamp>,
    /// 标志位（关键帧/损坏/丢弃）。
    pub flags: PacketFlags,
}

impl Packet {
    /// 空包构造：`data` 为空切片，时戳全 `None`、标志全空。
    /// 供测试、编码器占位输出与封装器透传场景使用。
    pub fn empty(stream_index: u32) -> Packet {
        Packet {
            data: Arc::from(Vec::new()),
            stream_index,
            pts: None,
            dts: None,
            duration: None,
            flags: PacketFlags::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Decoded, Encoded};
    use ye_time::Rational;

    fn ts(ticks: i64) -> Timestamp {
        Timestamp::new(ticks, Rational::new(1, 25))
    }

    #[test]
    fn empty_packet_defaults() {
        let p = Packet::empty(3);
        assert_eq!(p.stream_index, 3);
        assert!(p.data.is_empty());
        assert_eq!(p.pts, None);
        assert_eq!(p.dts, None);
        assert_eq!(p.duration, None);
        assert!(p.flags.is_empty());
    }

    #[test]
    fn clone_shares_data_arc_and_is_independent() {
        let mut a = Packet::empty(0);
        a.pts = Some(ts(42));
        a.flags = PacketFlags::KEYFRAME;
        let mut b = a.clone();
        // 零拷贝共享：Arc 指向同一分配
        assert!(Arc::ptr_eq(&a.data, &b.data));
        // 任一侧改字段不影响对方
        b.flags.insert(PacketFlags::CORRUPT);
        assert_eq!(a.flags, PacketFlags::KEYFRAME);
        assert!(!a.flags.contains(PacketFlags::CORRUPT));
        assert_eq!(b.flags, PacketFlags::KEYFRAME | PacketFlags::CORRUPT);
        b.pts = Some(ts(7));
        assert_eq!(a.pts, Some(ts(42)));
        assert_eq!(b.pts, Some(ts(7)));
    }

    #[test]
    fn flags_bitor_contains_insert_is_empty() {
        let combo = PacketFlags::KEYFRAME | PacketFlags::CORRUPT;
        assert!(combo.contains(PacketFlags::KEYFRAME));
        assert!(combo.contains(PacketFlags::CORRUPT));
        assert!(!combo.contains(PacketFlags::DISCARD));
        assert!(!combo.is_empty());
        assert!(PacketFlags::empty().is_empty());

        let mut f = PacketFlags::empty();
        assert!(f.is_empty());
        f.insert(PacketFlags::KEYFRAME);
        assert!(f.contains(PacketFlags::KEYFRAME));
        f |= PacketFlags::DISCARD; // BitOrAssign
        assert!(f.contains(PacketFlags::DISCARD));
        assert!(!f.contains(PacketFlags::CORRUPT));

        // 位值对齐契约（1<<0、1<<1、1<<2）
        assert_eq!(
            PacketFlags::KEYFRAME,
            PacketFlags::KEYFRAME | PacketFlags::empty()
        );
    }

    #[test]
    fn result_enums_compare_by_value() {
        // pull == NeedInput 一类相等断言的前提：Decoded/Encoded 可判等
        assert_eq!(Decoded::NeedInput, Decoded::NeedInput);
        assert_eq!(Encoded::NeedInput, Encoded::NeedInput);
        assert_eq!(
            Encoded::Packet(Packet::empty(1)),
            Encoded::Packet(Packet::empty(1))
        );
        assert_ne!(
            Encoded::Packet(Packet::empty(1)),
            Encoded::Packet(Packet::empty(2))
        );
    }
}
