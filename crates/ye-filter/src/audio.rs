//! Bounded streaming PCM resampling. Output count is ceil(input frames * out / in).
//! A 32-tap windowed sinc supplies anti-alias filtering; phase uses integer clocks.
use crate::FilterError;
use std::collections::VecDeque;

pub struct Resampler {
    input_rate: u32,
    output_rate: u32,
    input_channels: usize,
    output_channels: usize,
    samples: VecDeque<[f32; 2]>,
    base: u64,
    received: u64,
    emitted: u64,
    finished: bool,
}

impl Resampler {
    pub fn new(
        input_rate: u32,
        output_rate: u32,
        input_channels: usize,
        output_channels: usize,
    ) -> Result<Self, FilterError> {
        if ![44100, 48000].contains(&input_rate)
            || ![44100, 48000].contains(&output_rate)
            || !(1..=2).contains(&input_channels)
            || !(1..=2).contains(&output_channels)
        {
            return Err(FilterError::InvalidArguments(
                "resampler supports 44.1/48kHz mono/stereo",
            ));
        }
        Ok(Self {
            input_rate,
            output_rate,
            input_channels,
            output_channels,
            samples: VecDeque::new(),
            base: 0,
            received: 0,
            emitted: 0,
            finished: false,
        })
    }

    /// Each call returns only newly available packed PCM. Push bounded chunks.
    pub fn push(&mut self, input: &[f32]) -> Result<Vec<f32>, FilterError> {
        if self.finished
            || !input.len().is_multiple_of(self.input_channels)
            || input.iter().any(|s| !s.is_finite())
        {
            return Err(FilterError::InvalidArguments(
                "invalid PCM or resampler already finished",
            ));
        }
        for frame in input.chunks_exact(self.input_channels) {
            let stereo = [frame[0], frame[self.input_channels - 1]];
            self.samples.push_back(stereo);
            self.received += 1;
        }
        Ok(self.drain(false))
    }

    pub fn finish(&mut self) -> Vec<f32> {
        self.finished = true;
        self.drain(true)
    }

    fn drain(&mut self, eof: bool) -> Vec<f32> {
        let mut out = Vec::new();
        let count = (u128::from(self.received) * u128::from(self.output_rate))
            .div_ceil(u128::from(self.input_rate));
        let cutoff = (f64::from(self.output_rate) / f64::from(self.input_rate)).min(1.0);
        while u128::from(self.emitted) < count {
            let phase = u128::from(self.emitted) * u128::from(self.input_rate);
            let center = (phase / u128::from(self.output_rate)) as i64;
            if !eof && center + 16 >= self.received as i64 {
                break;
            }
            let frac = (phase % u128::from(self.output_rate)) as f64 / f64::from(self.output_rate);
            let mut value = [0.0f64; 2];
            let mut norm = 0.0;
            for offset in -15..=16 {
                let distance = f64::from(offset) - frac;
                let x = distance * cutoff;
                let sinc = if x.abs() < 1e-12 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                let window = 0.5 + 0.5 * (std::f64::consts::PI * distance / 16.0).cos();
                let weight = sinc * window * cutoff;
                norm += weight;
                // Zero extension, never hold the final sample into a following clip.
                let at = center + i64::from(offset);
                if at >= self.base as i64
                    && at < self.received as i64
                    && let Some(sample) = self.samples.get((at as u64 - self.base) as usize)
                {
                    for c in 0..2 {
                        value[c] += f64::from(sample[c]) * weight;
                    }
                }
            }
            if self.output_channels == 1 {
                out.push(((value[0] + value[1]) / (2.0 * norm)) as f32);
            } else {
                out.extend(value.map(|v| (v / norm) as f32));
            }
            self.emitted += 1;
            let next = (u128::from(self.emitted) * u128::from(self.input_rate)
                / u128::from(self.output_rate)) as u64;
            while self.base < next.saturating_sub(16) && !self.samples.is_empty() {
                self.samples.pop_front();
                self.base += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn run(chunk: usize, rate: u32, out: u32) -> Vec<f32> {
        let input: Vec<_> = (0..4417).map(|n| (n as f32 * 0.01).sin()).collect();
        let mut resampler = Resampler::new(rate, out, 1, 2).unwrap();
        let mut got = Vec::new();
        for block in input.chunks(chunk) {
            got.extend(resampler.push(block).unwrap());
            assert!(resampler.samples.len() <= 33);
        }
        got.extend(resampler.finish());
        got
    }
    #[test]
    fn chunk_boundaries_do_not_change_pcm_or_count() {
        for (a, b) in [(44100, 48000), (48000, 44100), (48000, 48000)] {
            let got = run(1024, a, b);
            assert_eq!(got, run(7, a, b));
            assert_eq!(got.len(), (4417usize * b as usize).div_ceil(a as usize) * 2);
            assert!(got.chunks_exact(2).all(|s| s[0] == s[1]));
        }
    }
    #[test]
    fn rejects_invalid_pcm_and_configuration() {
        assert!(Resampler::new(96000, 48000, 2, 2).is_err());
        let mut r = Resampler::new(48000, 48000, 2, 2).unwrap();
        assert!(r.push(&[0.0]).is_err());
        assert!(r.push(&[f32::NAN, 0.0]).is_err());
        r.finish();
        assert!(r.push(&[0.0, 0.0]).is_err());
    }
}
