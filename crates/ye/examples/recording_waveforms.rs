//! Explicit real-source full-track acceptance, including concurrent decode.
use std::{fs::File, io::BufReader, path::PathBuf, sync::atomic::AtomicBool};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let input = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("recording path required")?,
    );
    let header = ye::media::inspect(Box::new(BufReader::new(File::open(&input)?)))?;
    std::thread::scope(|scope| {
        let mut jobs = Vec::new();
        for stream in header
            .streams
            .iter()
            .filter(|s| s.media_type == ye::codec::MediaType::Audio)
        {
            let input = &input;
            jobs.push(scope.spawn(move || {
                let started = std::time::Instant::now();
                let peaks = ye::waveform::audio_peaks_aligned_stream_with_count(
                    Box::new(BufReader::new(
                        File::open(input).map_err(|e| e.to_string())?,
                    )),
                    Some(stream.index),
                    1024,
                    &AtomicBool::new(false),
                )
                .map_err(|e| e.to_string())?;
                println!(
                    "stream {}: {}Hz, {} channels, {} buckets, {:?}",
                    stream.index,
                    peaks.sample_rate,
                    peaks.channels,
                    peaks.buckets,
                    started.elapsed()
                );
                Ok::<_, String>(())
            }));
        }
        for job in jobs {
            job.join().expect("audio worker panicked")?;
        }
        Ok::<_, String>(())
    })?;
    Ok(())
}
