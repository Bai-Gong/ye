//! 容器注册表 [`ContainerRegistry`]：显式构造、显式传递（无全局单例）。

use std::io::SeekFrom;

use crate::{
    ContainerError, Demuxer, DemuxerFactory, MediaHeader, Muxer, MuxerFactory, ProbeScore,
    ReadSeek, WriteSeek,
};

/// 容器注册表：demuxer/muxer 工厂的显式集合与 auto-probe 入口。
///
/// ## 语义
///
/// - **无全局单例**：调用方显式 `new`、显式传递（ye-cli 想要全局时自己包一层）；
/// - **auto-probe**（[`ContainerRegistry::open_demuxer`]）：读
///   [`ContainerRegistry::PROBE_WINDOW`] 字节窗口（输入不足窗口时读到 EOF
///   为止）→ seek 回 0 → 逐工厂打分 → 取最高（同分取先注册者）；
///   最高分 < [`ProbeScore::MINIMUM`] → `Err(UnknownFormat)`；
/// - **点名**（[`ContainerRegistry::open_demuxer_with`] /
///   [`ContainerRegistry::create_muxer`]）：按工厂名直取，未注册 →
///   `Err(UnknownFormat)`；
/// - **同名注册替换**：`register_*` 遇同名工厂原地替换并返回旧工厂，
///   `demuxer_names()`/`muxer_names()` 因此不含重复名；
/// - [`ContainerRegistry::probe`]：一步式「打开 + 读头 + 丢弃」，
///   jian 列素材信息直接用它。
///
/// ## derive 清单
///
/// 仅 `Debug`（手工实现、略过 `Box<dyn …Factory>` 字段——trait 对象无
/// `Debug` 上界）；`Clone`/`PartialEq` 不作要求（derive 兜底政策 (a)）；
/// `Default` 与 `new()` 等价。
pub struct ContainerRegistry {
    demuxers: Vec<Box<dyn DemuxerFactory>>,
    muxers: Vec<Box<dyn MuxerFactory>>,
}

impl core::fmt::Debug for ContainerRegistry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ContainerRegistry")
            .field("demuxers", &self.demuxer_names())
            .field("muxers", &self.muxer_names())
            .finish()
    }
}

impl Default for ContainerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ContainerRegistry {
    /// auto-probe 窗口字节数（对齐 FFmpeg `PROBE_BUF_MIN` 量级；
    /// probe 只看开头这么多个字节）。
    pub const PROBE_WINDOW: usize = 8192;

    /// 空注册表。
    pub fn new() -> Self {
        Self {
            demuxers: Vec::new(),
            muxers: Vec::new(),
        }
    }

    /// 注册 demuxer 工厂；同名（`name()` 相等）替换并返回旧工厂，
    /// 首次注册返回 `None`。
    pub fn register_demuxer(
        &mut self,
        factory: Box<dyn DemuxerFactory>,
    ) -> Option<Box<dyn DemuxerFactory>> {
        replace_by_name(&mut self.demuxers, factory, |f| f.name())
    }

    /// 注册 muxer 工厂；同名替换并返回旧工厂，首次注册返回 `None`。
    pub fn register_muxer(
        &mut self,
        factory: Box<dyn MuxerFactory>,
    ) -> Option<Box<dyn MuxerFactory>> {
        replace_by_name(&mut self.muxers, factory, |f| f.name())
    }

    /// auto-probe 打开：读窗口 → seek 回 0 → 逐工厂打分取最高 → 打开。
    /// 最高分 < [`ProbeScore::MINIMUM`] → `Err(UnknownFormat)`；
    /// 一个工厂都没注册同样 `Err(UnknownFormat)`。
    /// 返回（胜出工厂名, demuxer）。
    pub fn open_demuxer(
        &self,
        mut input: Box<dyn ReadSeek>,
    ) -> Result<(String, Box<dyn Demuxer>), ContainerError> {
        let head = self.probe_window(&mut *input)?;
        let (index, score) = self.best_demuxer(&head)?;
        if score < ProbeScore::MINIMUM {
            return Err(ContainerError::UnknownFormat);
        }
        let factory = &self.demuxers[index];
        let demuxer = factory.open(input)?;
        Ok((factory.name().to_string(), demuxer))
    }

    /// 只打分、不打开（2026-10-24 评审补）：读窗口 → 回卷 → 逐工厂打分
    /// 取最高，返回（胜出工厂名, 得分）。与 [`ContainerRegistry::open_demuxer`]
    /// 同一套打分，但**不消费输入**（读后即回卷，也不构造 demuxer）——
    /// 消费方的路由决策应复用本查询，而不是各自手抄一份魔数规则
    /// （两处规则只能人工保持同步）。最高分 <
    /// [`ProbeScore::MINIMUM`] → `Err(UnknownFormat)`。
    pub fn detect(&self, input: &mut dyn ReadSeek) -> Result<(String, ProbeScore), ContainerError> {
        let head = self.probe_window(input)?;
        let (index, score) = self.best_demuxer(&head)?;
        if score < ProbeScore::MINIMUM {
            return Err(ContainerError::UnknownFormat);
        }
        Ok((self.demuxers[index].name().to_string(), score))
    }

    /// 逐工厂打分取最高（同分取先注册者）；无注册工厂 → UnknownFormat。
    fn best_demuxer(&self, head: &[u8]) -> Result<(usize, ProbeScore), ContainerError> {
        let mut best: Option<(usize, ProbeScore)> = None;
        for (i, factory) in self.demuxers.iter().enumerate() {
            let score = factory.probe(head);
            // 严格大于才替换：同分取先注册者（FFmpeg 先到先得语义）
            if best.is_none_or(|(_, s)| score > s) {
                best = Some((i, score));
            }
        }
        best.ok_or(ContainerError::UnknownFormat)
    }

    /// 点名打开：`name` 未注册 → `Err(UnknownFormat)`。
    pub fn open_demuxer_with(
        &self,
        name: &str,
        input: Box<dyn ReadSeek>,
    ) -> Result<Box<dyn Demuxer>, ContainerError> {
        let factory = self
            .demuxers
            .iter()
            .find(|f| f.name() == name)
            .ok_or(ContainerError::UnknownFormat)?;
        factory.open(input)
    }

    /// 点名构造 muxer：`name` 未注册 → `Err(UnknownFormat)`。
    pub fn create_muxer(
        &self,
        name: &str,
        output: Box<dyn WriteSeek>,
    ) -> Result<Box<dyn Muxer>, ContainerError> {
        let factory = self
            .muxers
            .iter()
            .find(|f| f.name() == name)
            .ok_or(ContainerError::UnknownFormat)?;
        factory.open(output)
    }

    /// 一步式探查：auto-probe 打开 + 读头 + 丢弃实例。
    /// 返回（格式名, 头信息）。
    pub fn probe(&self, input: Box<dyn ReadSeek>) -> Result<(String, MediaHeader), ContainerError> {
        let (name, mut demuxer) = self.open_demuxer(input)?;
        let header = demuxer.read_header()?;
        Ok((name, header))
    }

    /// 已注册 demuxer 工厂名（注册顺序，无重复）。
    pub fn demuxer_names(&self) -> Vec<&'static str> {
        self.demuxers.iter().map(|f| f.name()).collect()
    }

    /// 已注册 muxer 工厂名（注册顺序，无重复）。
    pub fn muxer_names(&self) -> Vec<&'static str> {
        self.muxers.iter().map(|f| f.name()).collect()
    }

    /// 读探查窗口（≤ [`ContainerRegistry::PROBE_WINDOW`] 字节，读到 EOF
    /// 为止），随后把输入 seek 回 0（工厂 `open` 从头解析）。
    /// IO 失败 → `Err(ContainerError::Io(..))`。
    fn probe_window(&self, input: &mut dyn ReadSeek) -> Result<Vec<u8>, ContainerError> {
        let mut head = vec![0u8; Self::PROBE_WINDOW];
        let mut filled = 0usize;
        while filled < head.len() {
            let n = input.read(&mut head[filled..])?;
            if n == 0 {
                break; // EOF：短输入按实际长度打分
            }
            filled += n;
        }
        head.truncate(filled);
        input.seek(SeekFrom::Start(0))?;
        Ok(head)
    }
}

/// 同名替换：`name_of` 相等的位置原位换入新工厂并返回旧工厂；
/// 无同名时**压入**新工厂并返回 `None`。
fn replace_by_name<T: ?Sized, F: Fn(&T) -> &'static str>(
    slots: &mut Vec<Box<T>>,
    factory: Box<T>,
    name_of: F,
) -> Option<Box<T>> {
    let name = name_of(&factory);
    match slots.iter().position(|f| name_of(f) == name) {
        Some(pos) => Some(std::mem::replace(&mut slots[pos], factory)),
        None => {
            slots.push(factory);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::{IoEvent, MockDemuxerFactory, registry_with_mocks};
    use crate::{Metadata, ProbeScore, ReadSeek};
    use std::io::Cursor;
    use ye_time::{CANONICAL_TIME_BASE, Rational, Timestamp};

    // ContainerError 无 PartialEq：全部 Err 断言用 matches!（政策 (d) 例外）。

    fn tracked(
        data: Vec<u8>,
    ) -> (
        Box<dyn ReadSeek>,
        std::sync::Arc<std::sync::Mutex<Vec<IoEvent>>>,
    ) {
        let (input, events) = crate::mock::TrackedInput::new(data);
        (Box::new(input), events)
    }

    /// 取 Err（`Box<dyn …>` 无 Debug，unwrap_err 不可用；Err 侧断言用 matches!）。
    fn err_of<T>(r: Result<T, ContainerError>) -> ContainerError {
        match r {
            Ok(_) => panic!("expected Err, got Ok"),
            Err(e) => e,
        }
    }

    // ---- testPlan：注册表 auto-probe ----

    /// 两工厂分别 EXTENSION 与 MAXIMUM → 选中高分者；
    /// 输入事件顺序为「read*（窗口）→ seek(0)」；胜出工厂 open 时位置为 0。
    #[test]
    fn open_demuxer_picks_highest_score_and_rewinds_input() {
        let mut reg = ContainerRegistry::new();
        reg.register_demuxer(Box::new(MockDemuxerFactory::new_named(
            ProbeScore::EXTENSION,
            "ext-heuristic",
        )));
        let hi = MockDemuxerFactory::new_named(ProbeScore::MAXIMUM, "magic-bytes");
        let positions = hi.open_positions_handle();
        reg.register_demuxer(Box::new(hi));

        let (input, events) = tracked(vec![0xAB; 32]);
        let (name, demuxer) = reg.open_demuxer(input).expect("open");
        assert_eq!(name, "magic-bytes");
        assert_eq!(demuxer.format_name(), "mock");

        let events = events.lock().unwrap().clone();
        // 顺序：若干 read（探查窗口）→ seek 回 0；不允许 seek 在 read 前。
        let first_seek = events.iter().position(|e| matches!(e, IoEvent::Seek(_)));
        let (fs, reads_before_seek) = match first_seek {
            Some(i) => (i, i),
            None => (events.len(), events.len()),
        };
        assert!(
            reads_before_seek > 0,
            "window must be read before seek: {events:?}"
        );
        assert_eq!(
            events[fs],
            IoEvent::Seek(0),
            "must seek back to 0: {events:?}"
        );
        // 工厂 open 时输入位置必须是 0（探查窗口读后被注册表回卷）
        assert_eq!(*positions.lock().unwrap(), vec![0u64]);
    }

    /// 全部低分（< 25）→ Err(UnknownFormat)。
    #[test]
    fn open_demuxer_all_below_minimum_is_unknown_format() {
        let mut reg = ContainerRegistry::new();
        reg.register_demuxer(Box::new(MockDemuxerFactory::new_named(
            ProbeScore::new(10).unwrap(),
            "weak",
        )));
        let (input, _) = tracked(vec![0x01; 64]);
        let err = err_of(reg.open_demuxer(input));
        assert!(matches!(err, ContainerError::UnknownFormat));
    }

    /// 点名未注册 → Err(UnknownFormat)。
    #[test]
    fn open_demuxer_with_unknown_name() {
        let reg = registry_with_mocks();
        let (input, _) = tracked(vec![0x01; 16]);
        let err = err_of(reg.open_demuxer_with("no-such-format", input));
        assert!(matches!(err, ContainerError::UnknownFormat));
        // muxer 点名同语义
        let err = err_of(reg.create_muxer("no-such-format", Box::new(Cursor::new(Vec::new()))));
        assert!(matches!(err, ContainerError::UnknownFormat));
    }

    /// 短输入（不足窗口）按实际长度打分：仍可选中。
    #[test]
    fn open_demuxer_short_input() {
        let reg = registry_with_mocks();
        let (name, _) = reg
            .open_demuxer(Box::new(Cursor::new(vec![0x42])))
            .expect("short input still probes");
        assert_eq!(name, "mock");
    }

    // ---- testPlan：detect（只打分、不打开）----

    /// detect：返回胜出工厂名与得分；不构造 demuxer；输入读后回卷
    /// （事件末尾 seek(0)），同一输入随后仍可 open_demuxer——消费方
    /// 路由复用注册表打分而不必各抄一份魔数规则（2026-10-24 评审补）。
    #[test]
    fn detect_scores_rewinds_and_does_not_open() {
        let mut reg = ContainerRegistry::new();
        reg.register_demuxer(Box::new(MockDemuxerFactory::new_named(
            ProbeScore::EXTENSION,
            "ext-heuristic",
        )));
        let hi = MockDemuxerFactory::new_named(ProbeScore::MAXIMUM, "magic-bytes");
        let open_positions = hi.open_positions_handle();
        reg.register_demuxer(Box::new(hi));

        let (mut input, events) = tracked(vec![0xAB; 32]);
        let (name, score) = reg.detect(&mut *input).expect("detect");
        assert_eq!(name, "magic-bytes");
        assert_eq!(score, ProbeScore::MAXIMUM);
        // 只打分、不打开：工厂的 open 从未被调用。
        assert!(
            open_positions.lock().unwrap().is_empty(),
            "detect 不得 open"
        );
        // 输入已回卷（事件末尾 seek(0)）。
        let events = events.lock().unwrap().clone();
        assert_eq!(*events.last().unwrap(), IoEvent::Seek(0));
        // 回卷后同一输入可直接打开（detect → open 的路由用法）。
        let (name2, _) = reg.open_demuxer(input).expect("open after detect");
        assert_eq!(name2, "magic-bytes");

        // 全低分（< 25）→ Err(UnknownFormat)，与 open_demuxer 同阈值。
        let mut reg2 = ContainerRegistry::new();
        reg2.register_demuxer(Box::new(MockDemuxerFactory::new_named(
            ProbeScore::new(10).unwrap(),
            "weak",
        )));
        let (mut input2, _) = tracked(vec![0x01; 64]);
        let err = err_of(reg2.detect(&mut *input2));
        assert!(matches!(err, ContainerError::UnknownFormat));
    }

    // ---- testPlan：同名注册替换 ----

    #[test]
    fn register_same_name_replaces_and_returns_old() {
        let mut reg = ContainerRegistry::new();
        assert!(
            reg.register_demuxer(Box::new(MockDemuxerFactory::new_named(
                ProbeScore::MAXIMUM,
                "mock",
            )))
            .is_none()
        );
        let old = reg
            .register_demuxer(Box::new(MockDemuxerFactory::new_named(
                ProbeScore::EXTENSION,
                "mock",
            )))
            .expect("same-name register returns old factory");
        assert_eq!(old.name(), "mock");
        let names = reg.demuxer_names();
        assert_eq!(names, vec!["mock"]);
        // muxer 侧同语义
        assert!(
            reg.register_muxer(Box::new(crate::mock::MockMuxerFactory::new()))
                .is_none()
        );
        assert!(
            reg.register_muxer(Box::new(crate::mock::MockMuxerFactory::new()))
                .is_some()
        );
        assert_eq!(reg.muxer_names(), vec!["mock"]);
    }

    // ---- testPlan：probe 一步式 ----

    /// probe：一次调用返回 format_name 与完整 MediaHeader
    /// （一条流、duration 在 canonical 时基）；输入被完全消费后 Drop 不 panic。
    #[test]
    fn probe_returns_name_and_header() {
        let reg = registry_with_mocks();
        let (name, header) = reg
            .probe(Box::new(Cursor::new(vec![0x7F; 300])))
            .expect("probe ok");
        assert_eq!(name, "mock");
        assert_eq!(header.format_name, "mock");
        assert_eq!(header.streams.len(), 1);
        let stream = &header.streams[0];
        assert_eq!(stream.index, 0);
        assert_eq!(stream.time_base, Rational::new(1, 25));
        // duration 在 canonical 时基：125 ticks @1/25 = 5 s → 5·7_056_000
        let duration = header.duration.expect("duration present");
        assert_eq!(duration.time_base(), CANONICAL_TIME_BASE);
        assert_eq!(duration.ticks(), 5 * 7_056_000);
        // demuxer 在此被 Drop（输入已被 read_header 完全消费）——不 panic 即通过。
    }

    // ---- testPlan：mock demuxer 契约（包打戳 / EOF 幂等 / seek 量化）----

    #[test]
    fn mock_demuxer_packets_carry_stream_time_base() {
        let reg = registry_with_mocks();
        let (_, mut demuxer) = reg
            .open_demuxer(Box::new(Cursor::new(vec![0x00; 8])))
            .expect("open");
        let header = demuxer.read_header().expect("header");
        let stream = &header.streams[0];

        let p0 = demuxer.read_packet().expect("p0").expect("some");
        assert_eq!(p0.stream_index as usize, 0);
        assert_eq!(stream.index, p0.stream_index);
        let pts = p0.pts.expect("pts");
        assert_eq!(pts.time_base(), stream.time_base, "pts tb == stream tb");
        assert_eq!(pts.ticks(), 0);

        let p1 = demuxer.read_packet().expect("p1").expect("some");
        let pts1 = p1.pts.expect("pts");
        assert_eq!(pts1.time_base(), stream.time_base);
        assert_eq!(pts1.ticks(), 5);

        // EOF 后重复 read_packet 均 Ok(None)
        assert!(demuxer.read_packet().expect("eof 1").is_none());
        assert!(demuxer.read_packet().expect("eof 2").is_none());
        assert!(demuxer.read_packet().expect("eof 3").is_none());
    }

    /// seek：target=5s 量化到关键帧 4.96s（124 ticks @1/25），
    /// 返回值时基 == 流时基。
    #[test]
    fn mock_demuxer_seek_quantizes_to_previous_keyframe() {
        let reg = registry_with_mocks();
        let (_, mut demuxer) = reg
            .open_demuxer(Box::new(Cursor::new(vec![0x00; 8])))
            .expect("open");
        let header = demuxer.read_header().expect("header");
        let stream_tb = header.streams[0].time_base;

        // target 用毫秒时基（1000）表达 5 s，跨时基进入 mock
        let target = Timestamp::new(5_000, Rational::new(1, 1000));
        let landed = demuxer.seek(0, target).expect("seek");
        assert_eq!(landed.time_base(), stream_tb, "return tb == stream tb");
        assert_eq!(landed.ticks(), 124); // 4.96 s @1/25
        assert_eq!(landed.time_base(), Rational::new(1, 25));

        // 流不存在 → StreamNotFound
        let err = err_of(demuxer.seek(7, target));
        assert!(matches!(err, ContainerError::StreamNotFound(7)));
    }

    // ---- testPlan：mock muxer 状态机与重标度 ----

    fn video_template(tb: Rational) -> crate::StreamTemplate {
        use ye_codec::{CodecId, CodecParameters, MediaType};
        crate::StreamTemplate {
            media_type: MediaType::Video,
            codec: CodecParameters::new(CodecId::H264, MediaType::Video),
            time_base: tb,
            frame_rate: Some(Rational::RATE_25),
            metadata: Metadata::new(),
        }
    }

    /// 未 write_header 直接 write_packet → Err(InvalidState(_))。
    #[test]
    fn muxer_rejects_packet_before_header() {
        let reg = registry_with_mocks();
        let mut muxer = reg
            .create_muxer("mock", Box::new(Cursor::new(Vec::new())))
            .expect("create");
        let err = err_of(muxer.write_packet(&ye_codec::Packet::empty(0)));
        assert!(matches!(err, ContainerError::InvalidState(_)));
    }

    /// write_trailer 后再 write_packet → Err(InvalidState(_))；
    /// 正常 header → packet×2 → trailer 全序通过；
    /// 时基重标度：pts=3600@1/90000、模板 1/1000 → 40 ticks@1/1000。
    #[test]
    fn muxer_state_machine_and_rescale() {
        let factory = crate::mock::MockMuxerFactory::new();
        let records = factory.written_handle();
        let mut reg = ContainerRegistry::new();
        reg.register_muxer(Box::new(factory));
        let mut muxer = reg
            .create_muxer("mock", Box::new(Cursor::new(Vec::new())))
            .expect("create");

        let tb_1000 = Rational::new(1, 1000);
        muxer
            .write_header(&[video_template(tb_1000)])
            .expect("header");

        let mut p0 = ye_codec::Packet::empty(0);
        p0.pts = Some(Timestamp::new(3600, Rational::new(1, 90_000)));
        muxer.write_packet(&p0).expect("packet 0");

        let mut p1 = ye_codec::Packet::empty(0);
        p1.pts = Some(Timestamp::new(7200, Rational::new(1, 90_000)));
        muxer.write_packet(&p1).expect("packet 1");

        muxer.write_trailer().expect("trailer");
        let err = err_of(muxer.write_packet(&p0));
        assert!(matches!(err, ContainerError::InvalidState(_)));

        // 重标度断言（工厂与 muxer 共享写入记录）
        let written = records.lock().unwrap().clone();
        assert_eq!(written.len(), 2);
        assert_eq!(written[0].ticks, 40); // 3600/90000 s = 0.04 s → 40 @1/1000
        assert_eq!(written[0].time_base, tb_1000);
        assert_eq!(written[1].ticks, 80); // 0.08 s → 80
        assert_eq!(written[1].time_base, tb_1000);
    }

    /// Muxer 契约固化（2026-10-24 评审）：**重标度失败必须以
    /// `Err(ContainerError::Time(..))` 透出**（trait 文档原文），不得
    /// 归入 InvalidState/InvalidData——mock 与 mp4 两个实现此前分叉
    /// （mock→Time、mp4→InvalidState），本断言锁住契约语义；真实实现
    /// 侧的对应断言见 ye-container-mp4 adapter 测试
    /// `muxer_rescale_overflow_surfaces_as_time_variant`。
    #[test]
    fn muxer_rescale_failure_must_surface_as_time_variant() {
        let factory = crate::mock::MockMuxerFactory::new();
        let mut reg = ContainerRegistry::new();
        reg.register_muxer(Box::new(factory));
        let mut muxer = reg
            .create_muxer("mock", Box::new(Cursor::new(Vec::new())))
            .expect("create");

        let tb_1000 = Rational::new(1, 1000);
        muxer
            .write_header(&[video_template(tb_1000)])
            .expect("header");
        // pts=i64::MAX @1/1 → 重标度到 1/1000 溢出 ⇒ Time。
        let mut p = ye_codec::Packet::empty(0);
        p.pts = Some(Timestamp::new(i64::MAX, Rational::ONE));
        let err = err_of(muxer.write_packet(&p));
        assert!(
            matches!(err, ContainerError::Time(_)),
            "重标度失败必须以 Time 透出，实为 {err:?}"
        );
    }

    // ---- 线程安全（全库约定）----

    #[test]
    fn registry_and_traits_are_send_sync() {
        fn assert_send_sync<T: Send + Sync + ?Sized>() {}
        assert_send_sync::<ContainerRegistry>();
        assert_send_sync::<dyn Demuxer>();
        assert_send_sync::<dyn Muxer>();
        assert_send_sync::<dyn DemuxerFactory>();
        assert_send_sync::<dyn MuxerFactory>();
        assert_send_sync::<Box<dyn Demuxer>>();
        assert_send_sync::<Box<dyn Muxer>>();
    }

    #[test]
    fn registry_debug_and_default() {
        let reg = registry_with_mocks();
        let dbg = format!("{reg:?}");
        assert!(dbg.contains("ContainerRegistry"), "{dbg}");
        assert_eq!(
            ContainerRegistry::default().demuxer_names(),
            Vec::<&str>::new()
        );
        assert_eq!(ContainerRegistry::PROBE_WINDOW, 8192);
    }
}
