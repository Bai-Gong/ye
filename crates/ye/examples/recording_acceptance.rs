//! Opt-in real-material inspection; input and generated pixels stay outside source control.
use std::{
    fs::File,
    io::{BufReader, Write},
    path::PathBuf,
    time::Instant,
};
use ye::{
    codec::CodecRegistry,
    convert::{AudioEncodeSettings, ConvertOptions, VideoEncodeSettings},
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = PathBuf::from(args.next().ok_or("input path required")?);
    let output = PathBuf::from(args.next().ok_or("output directory required")?);
    std::fs::create_dir_all(&output)?;
    let header = ye::media::inspect(Box::new(BufReader::new(File::open(&input)?)))?;
    for stream in &header.streams {
        println!(
            "stream {} {:?} {:?} {:?} {:?}",
            stream.index,
            stream.media_type,
            stream.codec.codec,
            stream.codec.video,
            stream.codec.audio
        );
    }
    let mut codecs = CodecRegistry::new();
    ye::codec_h264::register_h264(&mut codecs);
    ye::codec_h264::register_h264_encoder(&mut codecs);
    ye::codec_aac::register_aac(&mut codecs);
    ye::codec_aac::register_aac_encoder(&mut codecs);
    for issue in ye::convert::check_convert(
        &header,
        &codecs,
        Some(&VideoEncodeSettings::default()),
        &AudioEncodeSettings::Aac {
            bitrate_bps: 128000,
        },
        &ConvertOptions::default(),
    ) {
        println!("conversion blocked: {issue}");
    }
    for time in args {
        let time_us: i64 = time.to_str().ok_or("invalid time")?.parse()?;
        let started = Instant::now();
        match ye::frame_grab::decode_frame_rgba(
            Box::new(BufReader::new(File::open(&input)?)),
            time_us,
        ) {
            Ok(frame) => {
                let png = ye::png::encode_rgba(frame.width, frame.height, &frame.rgba)?;
                for (extension, bytes) in [("rgba", frame.rgba), ("png", png)] {
                    let transaction = ye::output::OutputTransaction::new(
                        &output.join(format!("{time_us}.{extension}")),
                    )?;
                    transaction.writer()?.write_all(&bytes)?;
                    transaction.commit()?;
                }
                println!(
                    "frame {time_us}: {}x{} in {:?}",
                    frame.width,
                    frame.height,
                    started.elapsed()
                );
            }
            Err(error) => {
                return Err(
                    format!("frame {time_us} after {:?}: {error}", started.elapsed()).into(),
                );
            }
        }
    }
    Ok(())
}
