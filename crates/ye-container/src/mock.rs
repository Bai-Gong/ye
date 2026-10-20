//! 测试专用 mock：demuxer / muxer / 带调用记录的输入流。
//!
//! 仅 `#[cfg(test)]` 编译，不进入公开 API。

use std::io::{Cursor, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};

use ye_codec::{CodecId, CodecParameters, MediaType, Packet, PacketFlags};
use ye_time::{CANONICAL_TIME_BASE, Rational, Timestamp};

use crate::{
    ContainerError, ContainerRegistry, Demuxer, DemuxerFactory, MediaHeader, Muxer, MuxerFactory,
    ProbeScore, ReadSeek, Stream, StreamTemplate, WriteSeek,
};

// ---- mock 输入：记录 read/seek 事件顺序 ----

/// 记录的事件（顺序保真，供断言 auto-probe 的「读窗口 → seek 回 0」顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IoEvent {
    Read,
    Seek(i64),
}

/// 包装 [`Cursor`] 并记录全部 read/seek 事件的测试输入。
pub(crate) struct TrackedInput {
    inner: Cursor<Vec<u8>>,
    events: Arc<Mutex<Vec<IoEvent>>>,
}

impl TrackedInput {
    /// 以给定字节构造，并返回事件日志的共享句柄。
    pub(crate) fn new(data: Vec<u8>) -> (Self, Arc<Mutex<Vec<IoEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                inner: Cursor::new(data),
                events: Arc::clone(&events),
            },
            events,
        )
    }
}

impl Read for TrackedInput {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.events.lock().expect("events lock").push(IoEvent::Read);
        self.inner.read(buf)
    }
}

impl Seek for TrackedInput {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let offset = match pos {
            SeekFrom::Start(o) => o as i64,
            SeekFrom::End(o) => o,
            SeekFrom::Current(o) => o,
        };
        self.events
            .lock()
            .expect("events lock")
            .push(IoEvent::Seek(offset));
        self.inner.seek(pos)
    }
}

// TrackedInput 满足 Read + Seek + Send + Sync（Arc<Mutex<_>> 为 Send + Sync），
// 经 io 模块的空白实现自动获得 ReadSeek，无须（也不得）手写 impl。

// ---- mock demuxer ----

/// mock 流时基：1/25（每帧 40 ms）。
pub(crate) fn mock_time_base() -> Rational {
    Rational::new(1, 25)
}

/// mock 关键帧间隔：124 ticks @1/25（4.96 s）——seek 量化目标。
pub(crate) const KEYFRAME_INTERVAL: i64 = 124;

/// mock 流表：单条视频流（H.264、1/25 时基）。
pub(crate) fn mock_streams() -> Vec<Stream> {
    vec![Stream {
        index: 0,
        media_type: MediaType::Video,
        codec: CodecParameters::new(CodecId::H264, MediaType::Video),
        time_base: mock_time_base(),
        start_time: None,
        duration: Some(Timestamp::new(125, mock_time_base())),
        nb_frames: Some(125),
        metadata: crate::Metadata::new(),
    }]
}

/// 带「工厂 open 时输入位置」记录的 mock 解封装器。
pub(crate) struct MockDemuxer {
    input: Option<Box<dyn ReadSeek>>,
    packets: Vec<Packet>,
    read_pos: usize,
    eof: bool,
    /// 工厂 open 时刻的输入位置（字节）；auto-probe 契约要求为 0。
    pub(crate) open_position: Option<u64>,
}

impl MockDemuxer {
    fn new() -> Self {
        // 两帧：pts 0 与 5 ticks（均 @流时基 1/25），第一帧关键帧。
        let mut p0 = Packet::empty(0);
        p0.pts = Some(Timestamp::new(0, mock_time_base()));
        p0.flags = PacketFlags::KEYFRAME;
        p0.data = Arc::from(vec![0u8, 1, 2]);
        let mut p1 = Packet::empty(0);
        p1.pts = Some(Timestamp::new(5, mock_time_base()));
        p1.flags = PacketFlags::KEYFRAME;
        p1.data = Arc::from(vec![3u8, 4, 5]);
        Self {
            input: None,
            packets: vec![p0, p1],
            read_pos: 0,
            eof: false,
            open_position: None,
        }
    }
}

impl Demuxer for MockDemuxer {
    fn format_name(&self) -> &str {
        "mock"
    }

    fn read_header(&mut self) -> Result<MediaHeader, ContainerError> {
        // 头解析会把输入整个消费掉（模拟「输入被完全消费后 Drop 不 panic」）
        if let Some(input) = self.input.as_mut() {
            let mut sink = Vec::new();
            let mut window = [0u8; 256];
            loop {
                let n = input.read(&mut window).map_err(ContainerError::Io)?;
                if n == 0 {
                    break;
                }
                sink.extend_from_slice(&window[..n]);
            }
        }
        let duration = Timestamp::new(125, mock_time_base())
            .rescale(CANONICAL_TIME_BASE)
            .map_err(ContainerError::Time)?;
        Ok(MediaHeader {
            format_name: "mock".to_string(),
            streams: mock_streams(),
            duration: Some(duration),
            start_time: None,
            bit_rate: None,
            metadata: crate::Metadata::new(),
        })
    }

    fn read_packet(&mut self) -> Result<Option<Packet>, ContainerError> {
        if self.eof {
            return Ok(None); // EOF 后幂等
        }
        match self.read_pos < self.packets.len() {
            true => {
                let p = self.packets[self.read_pos].clone();
                self.read_pos += 1;
                Ok(Some(p))
            }
            false => {
                self.eof = true;
                Ok(None)
            }
        }
    }

    fn seek(&mut self, stream_index: u32, target: Timestamp) -> Result<Timestamp, ContainerError> {
        if stream_index as usize >= mock_streams().len() {
            return Err(ContainerError::StreamNotFound(stream_index));
        }
        // 量化到前一个关键帧：floor(ticks / 124) * 124，目标时基为流时基。
        let stream_tb = mock_time_base();
        let ticks = target.rescale(stream_tb).map_err(ContainerError::Time)?;
        let landed = ticks.ticks() / KEYFRAME_INTERVAL * KEYFRAME_INTERVAL;
        Ok(Timestamp::new(landed, stream_tb))
    }
}

/// mock 解封装工厂：探查分数可配置，open 记录输入当时的位置
/// （经 [`MockDemuxerFactory::open_positions`] 暴露给测试断言 auto-probe 的
/// seek 回 0 契约）。
pub(crate) struct MockDemuxerFactory {
    score: ProbeScore,
    name: &'static str,
    positions: Arc<Mutex<Vec<u64>>>,
}

impl MockDemuxerFactory {
    /// 指定探查分数（名字固定 "mock"）。
    pub(crate) fn new(score: ProbeScore) -> Self {
        Self::new_named(score, "mock")
    }

    /// 同时指定分数与名字（同名替换 / 多工厂打分测试用）。
    pub(crate) fn new_named(score: ProbeScore, name: &'static str) -> Self {
        Self {
            score,
            name,
            positions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// open 位置记录的共享句柄（工厂装箱注册后，测试仍可经它断言
    /// auto-probe 的 seek 回 0 契约）。
    pub(crate) fn open_positions_handle(&self) -> Arc<Mutex<Vec<u64>>> {
        Arc::clone(&self.positions)
    }
}

impl DemuxerFactory for MockDemuxerFactory {
    fn name(&self) -> &'static str {
        self.name
    }

    fn probe(&self, head: &[u8]) -> ProbeScore {
        // 只看开头字节：空输入不认
        if head.is_empty() {
            ProbeScore::NONE
        } else {
            self.score
        }
    }

    fn open(&self, mut input: Box<dyn ReadSeek>) -> Result<Box<dyn Demuxer>, ContainerError> {
        let position = input.stream_position().map_err(ContainerError::Io)?;
        self.positions
            .lock()
            .expect("positions lock")
            .push(position);
        let mut demuxer = MockDemuxer::new();
        demuxer.open_position = Some(position);
        demuxer.input = Some(input);
        Ok(Box::new(demuxer))
    }
}

// ---- mock muxer：状态机 + 重标度记录 ----

/// mock 封装器内部状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MuxState {
    /// 未写头。
    Idle,
    /// 头已写、可写包。
    Open,
    /// 尾已写、终态。
    Finalized,
}

/// 写入记录：重标度到模板时基后的（stream_index, ticks, time_base）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WrittenPacket {
    pub(crate) stream_index: u32,
    pub(crate) ticks: i64,
    pub(crate) time_base: Rational,
}

/// mock 封装器：状态机违约报 [`ContainerError::InvalidState`]，
/// 写包时把 pts 重标度到模板时基并记录（记录经 [`MockMuxerFactory`]
/// 的共享句柄暴露给测试断言）。
pub(crate) struct MockMuxer {
    state: MuxState,
    templates: Vec<StreamTemplate>,
    written: Arc<Mutex<Vec<WrittenPacket>>>,
}

impl MockMuxer {
    fn new(written: Arc<Mutex<Vec<WrittenPacket>>>) -> Self {
        Self {
            state: MuxState::Idle,
            templates: Vec::new(),
            written,
        }
    }

    fn template_tb(&self, stream_index: u32) -> Result<Rational, ContainerError> {
        self.templates
            .get(stream_index as usize)
            .map(|t| t.time_base)
            .ok_or(ContainerError::StreamNotFound(stream_index))
    }
}

impl Muxer for MockMuxer {
    fn write_header(&mut self, streams: &[StreamTemplate]) -> Result<(), ContainerError> {
        if self.state != MuxState::Idle {
            return Err(ContainerError::InvalidState("write_header after use"));
        }
        self.templates = streams.to_vec();
        self.state = MuxState::Open;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<(), ContainerError> {
        if self.state == MuxState::Idle {
            return Err(ContainerError::InvalidState("write_header first"));
        }
        if self.state == MuxState::Finalized {
            return Err(ContainerError::InvalidState("already finalized"));
        }
        // 重标度契约：包时基与模板不符时按模板时基重标度（NearInf）。
        let tb = self.template_tb(packet.stream_index)?;
        let pts = packet
            .pts
            .ok_or_else(|| ContainerError::InvalidData("packet missing pts".to_string()))?;
        let scaled = pts.rescale(tb).map_err(ContainerError::Time)?;
        self.written
            .lock()
            .expect("written lock")
            .push(WrittenPacket {
                stream_index: packet.stream_index,
                ticks: scaled.ticks(),
                time_base: tb,
            });
        Ok(())
    }

    /// 收尾：状态推进到 Finalized；此后任何 write_packet 均为状态违约。
    fn write_trailer(&mut self) -> Result<(), ContainerError> {
        if self.state != MuxState::Open {
            return Err(ContainerError::InvalidState("write_header first"));
        }
        self.state = MuxState::Finalized;
        Ok(())
    }
}

/// mock 封装工厂：与产出的 [`MockMuxer`] 共享写入记录句柄，
/// 测试经 [`MockMuxerFactory::written_handle`] 断言重标度结果。
pub(crate) struct MockMuxerFactory {
    written: Arc<Mutex<Vec<WrittenPacket>>>,
}

impl MockMuxerFactory {
    pub(crate) fn new() -> Self {
        Self {
            written: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// 写入记录的共享句柄（工厂装箱注册后，测试仍可经它断言）。
    pub(crate) fn written_handle(&self) -> Arc<Mutex<Vec<WrittenPacket>>> {
        Arc::clone(&self.written)
    }
}

impl Default for MockMuxerFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl MuxerFactory for MockMuxerFactory {
    fn name(&self) -> &'static str {
        "mock"
    }

    fn open(&self, _output: Box<dyn WriteSeek>) -> Result<Box<dyn Muxer>, ContainerError> {
        Ok(Box::new(MockMuxer::new(Arc::clone(&self.written))))
    }
}

/// 便捷：装配含一个 mock demuxer + 一个 mock muxer 的注册表。
pub(crate) fn registry_with_mocks() -> ContainerRegistry {
    let mut reg = ContainerRegistry::new();
    reg.register_demuxer(Box::new(MockDemuxerFactory::new(ProbeScore::MAXIMUM)));
    reg.register_muxer(Box::new(MockMuxerFactory::new()));
    reg
}
