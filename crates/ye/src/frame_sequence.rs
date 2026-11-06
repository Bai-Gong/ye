//! Ordered filmstrip extraction with one demuxer and decoder per request.
//! Frames are delivered immediately; the library does not retain the filmstrip.

use std::sync::atomic::{AtomicBool, Ordering};

use ye_codec::{CodecRegistry, Frame, MediaType};
use ye_container::{ContainerRegistry, ReadSeek};
use ye_pixfmt::PixelFormat;
use ye_time::{Rational, Timestamp};

use crate::{
    convert::{ConvertError, filter_convert},
    frame_grab::GrabbedFrame,
};

/// Decode each requested time using the same first-PTS-at-or-after rule as
/// [`crate::frame_grab::decode_frame_rgba`]. Negative times clamp to zero.
/// Times must be nondecreasing after clamping; duplicates are delivered separately.
/// `stream_index` is an actual container stream index; `None` requires one video.
///
/// A single keyframe seek precedes sequential decoding. This is intended for
/// nearby filmstrip cells: widely separated requests may be faster as independent
/// grabs. Conversion is performed only for selected frames and only once when
/// several requests select the same frame. The callback receives the request
/// index, and may resize/encode or consume the borrowed image immediately.
///
/// Cancellation and callback errors stop further delivery. EOF before any target
/// is an error, even if earlier targets were delivered; callers must account for
/// these partial results. Empty requests do not open the input.
pub fn decode_frames_rgba(
    input: Box<dyn ReadSeek>,
    times_us: &[i64],
    stream_index: Option<u32>,
    cancel: &AtomicBool,
    mut sink: impl FnMut(usize, &GrabbedFrame) -> Result<(), ConvertError>,
) -> Result<(), ConvertError> {
    let check_cancel = || {
        if cancel.load(Ordering::Relaxed) {
            Err(ConvertError::Cancelled)
        } else {
            Ok(())
        }
    };
    check_cancel()?;
    if times_us
        .windows(2)
        .any(|pair| pair[0].max(0) > pair[1].max(0))
    {
        return Err(ConvertError::InvalidArguments(
            "frame times must be nondecreasing".into(),
        ));
    }
    let Some(&first) = times_us.first() else {
        return Ok(());
    };
    let mut containers = ContainerRegistry::new();
    crate::container::mp4::register_mp4(&mut containers);
    let mut codecs = CodecRegistry::new();
    crate::codec_h264::register_h264(&mut codecs);
    let mut source = crate::timeline::Source::open_selected(
        &containers,
        &codecs,
        input,
        MediaType::Video,
        first.max(0),
        None,
        stream_index,
    )?;
    let micro = Rational::new(1, 1_000_000);
    let mut index = 0;
    while let Some(frame) = source.next(cancel)? {
        let Frame::Video(video) = frame else {
            return Err(ConvertError::InvalidData(
                "video decoder returned audio".into(),
            ));
        };
        let Some(pts) = video.pts() else {
            continue;
        };
        if Timestamp::compare(pts, Timestamp::new(times_us[index].max(0), micro)).is_lt() {
            continue;
        }
        let rgba = filter_convert(&video, PixelFormat::Rgba8)?;
        let stride = rgba.layout().plane(0).stride;
        let row = rgba.width() as usize * 4;
        let mut bytes = Vec::with_capacity(row * rgba.height() as usize);
        for y in 0..rgba.height() as usize {
            bytes.extend_from_slice(&rgba.plane(0)[y * stride..y * stride + row]);
        }
        let image = GrabbedFrame {
            width: rgba.width(),
            height: rgba.height(),
            rgba: bytes,
        };
        while index < times_us.len()
            && Timestamp::compare(pts, Timestamp::new(times_us[index].max(0), micro)).is_ge()
        {
            check_cancel()?;
            sink(index, &image)?;
            check_cancel()?;
            index += 1;
        }
        if index == times_us.len() {
            return Ok(());
        }
    }
    Err(ConvertError::InvalidData(format!(
        "frame request {index} at {} µs has no frame at or after its target",
        times_us[index].max(0),
    )))
}
