//! remux 助手：跨容器重封装直通（对标 `ffmpeg -c copy`，不转码）。
//!
//! 自 m1 的 ye-cli `remux` 上提到门面（约 80 行归并循环照搬，时戳域从
//! m1 的全局 tick 换成流内自描述 [`Timestamp`]）。归并本身已内化到
//! [`Demuxer::read_packet`]（demuxer 侧按 dts 交错产出），本层是
//! 「auto-probe 打开 → 模板建 muxer → 逐包平移直通」的泵。

use ye_container::{ContainerError, ContainerRegistry, ReadSeek, StreamTemplate, WriteSeek};
use ye_time::Timestamp;

/// remux 结果统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemuxStats {
    /// 直通的流数。
    pub streams: usize,
    /// 直通的包数。
    pub packets: u64,
}

/// 重封装直通：`input` auto-probe 打开 → 按 `output_format` 点名 muxer
/// → 逐包透传（包数据、关键帧、时戳原样，无转码）。
///
/// 行为契约：
///
/// - **流映射**：按 demuxer 流序一一对应（`StreamTemplate` 取自各流，
///   时基沿用源流时基——同容器直通时 muxer 重标度恒等）；
/// - **时戳平移**：每轨时间线整体平移到首包 dts = 0（平移量 = 该轨
///   首包 dts，常见文件为 0 即 no-op）。progressive MP4 的 stts 从 0
///   起算是动因；轨内相对时序与时长不变，跨轨起始差不保真（无法表达，
///   与 m1 行为一致）；
/// - **交错序**：由 demuxer 的 `read_packet` 产出序决定（跨轨按 dts
///   归并），本层不重排；
/// - 包缺 `dts`/`duration`、时基换算失败等由 muxer 的写入契约拒绝
///   （`ContainerError::InvalidState` / `Time`）。
pub fn remux(
    registry: &ContainerRegistry,
    input: Box<dyn ReadSeek>,
    output_format: &str,
    output: Box<dyn WriteSeek>,
) -> Result<RemuxStats, ContainerError> {
    let (_in_format, mut demuxer) = registry.open_demuxer(input)?;
    let header = demuxer.read_header()?;
    // 流表 → 封装模板（时基/编解码参数/元数据原样；帧率取视频参数里的
    // 标称值，容器未携带即 None）。
    let templates: Vec<StreamTemplate> = header
        .streams
        .iter()
        .map(|s| StreamTemplate {
            media_type: s.media_type,
            codec: s.codec.clone(),
            time_base: s.time_base,
            frame_rate: s.codec.video.as_ref().and_then(|v| v.frame_rate),
            metadata: s.metadata.clone(),
        })
        .collect();
    let stream_count = templates.len();
    let mut muxer = registry.create_muxer(output_format, output)?;
    muxer.write_header(&templates)?;

    // 每轨首包 dts（流时基）：该轨的整体平移量；None = 未遇首包。
    let mut shifts: Vec<Option<Timestamp>> = vec![None; stream_count];
    let mut packets = 0u64;
    while let Some(mut pkt) = demuxer.read_packet()? {
        packets += 1;
        let si = usize::try_from(pkt.stream_index).unwrap_or(usize::MAX);
        if let (Some(dts), Some(shift)) = (pkt.dts, shifts.get_mut(si)) {
            // 首包定平移量（同流同轨同源 demuxer，时基一致，减法精确）；
            // 后续包按同值平移（轨内相对时序不变）。
            let shift = *shift.get_or_insert(dts);
            pkt.dts = Some(dts.checked_sub(shift).map_err(ContainerError::Time)?);
            if let Some(pts) = pkt.pts {
                pkt.pts = Some(pts.checked_sub(shift).map_err(ContainerError::Time)?);
            }
        }
        muxer.write_packet(&pkt)?;
    }
    muxer.write_trailer()?;
    Ok(RemuxStats {
        streams: stream_count,
        packets,
    })
}
