//! Compare independent grabs with one streaming cursor, comparing image fingerprints.
//! Usage: cargo run --release -p ye --all-features --example filmstrip_benchmark -- input.mp4 start_us step_us count
use std::{fs::File, io::BufReader, sync::atomic::AtomicBool, time::Instant};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 4 {
        return Err("expected input.mp4 start_us step_us count".into());
    }
    let start: i64 = args[1].parse()?;
    let step: i64 = args[2].parse()?;
    let count: usize = args[3].parse()?;
    if !(1..=256).contains(&count) || step < 0 {
        return Err("count must be 1..256 and step nonnegative".into());
    }
    let times: Vec<i64> = (0..count)
        .map(|i| {
            step.checked_mul(i as i64)
                .and_then(|delta| start.checked_add(delta))
                .ok_or("time overflow")
        })
        .collect::<Result<_, _>>()?;
    let open = || -> Result<Box<dyn ye::container::ReadSeek>, std::io::Error> {
        Ok(Box::new(BufReader::new(File::open(&args[0])?)))
    };
    // Hash each image to keep benchmark memory independent of the number of cells.
    let fingerprint = |frame: &ye::frame_grab::GrabbedFrame| {
        (
            frame.width,
            frame.height,
            frame.rgba.iter().fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
            }),
        )
    };
    let began = Instant::now();
    let expected: Vec<_> = times
        .iter()
        .map(|&time| {
            Ok(fingerprint(&ye::frame_grab::decode_frame_rgba(
                open()?,
                time,
            )?))
        })
        .collect::<Result<_, Box<dyn std::error::Error>>>()?;
    let independent = began.elapsed();
    let began = Instant::now();
    let mut delivered = 0;
    ye::frame_sequence::decode_frames_rgba(
        open()?,
        &times,
        None,
        &AtomicBool::new(false),
        |index, frame| {
            assert_eq!(fingerprint(frame), expected[index], "frame {index}");
            delivered += 1;
            Ok(())
        },
    )?;
    assert_eq!(delivered, count);
    let sequence = began.elapsed();
    println!(
        "frames={count} independent={independent:?} sequence={sequence:?} speedup={:.2}x fingerprints_equal=true",
        independent.as_secs_f64() / sequence.as_secs_f64()
    );
    Ok(())
}
