//! Clip-local animation curves and linear fade envelopes.
//! Times use timeline elapsed time, independent of the source trim offset.
use ye_time::Timestamp;

/// Interpolation from a keyframe to the following keyframe.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Interpolation {
    #[default]
    Linear,
    /// Keep this keyframe's value until the exact next keyframe time.
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Keyframe {
    pub time_us: i64,
    pub value: f64,
    pub interpolation: Interpolation,
}

/// A nonempty, strictly time-ordered track. Values outside its span clamp to
/// the nearest endpoint. Timeline preflight validates values and property ranges.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct KeyframeTrack {
    pub keyframes: Vec<Keyframe>,
}
impl KeyframeTrack {
    pub(crate) fn validate(&self, duration: i64, valid: impl Fn(f64) -> bool) -> bool {
        !self.keyframes.is_empty()
            && self.keyframes.iter().all(|key| {
                (0..=duration).contains(&key.time_us) && key.value.is_finite() && valid(key.value)
            })
            && self
                .keyframes
                .windows(2)
                .all(|pair| pair[0].time_us < pair[1].time_us)
    }
    // Integer cross products decide boundaries, including fractional frame and
    // audio sample times. Floating point is used only for value interpolation.
    fn sample(&self, time: LocalTime) -> f64 {
        let next = self
            .keyframes
            .partition_point(|key| i128::from(key.time_us) * time.den <= time.num);
        if next == 0 {
            return self.keyframes[0].value;
        }
        let left = self.keyframes[next - 1];
        if next == self.keyframes.len() || left.interpolation == Interpolation::Hold {
            return left.value;
        }
        let right = self.keyframes[next];
        let delta = time.num - i128::from(left.time_us) * time.den;
        let span = i128::from(right.time_us - left.time_us) * time.den;
        let alpha = delta as f64 / span as f64;
        // Keep a convex interpolation inside its validated endpoint range even
        // when subnormal terms underflow (e.g. the smallest positive scale).
        (left.value * (1.0 - alpha) + right.value * alpha)
            .clamp(left.value.min(right.value), left.value.max(right.value))
    }
}

/// An optional track overrides the static property of its corresponding layer.
/// Video uses x/y/scale/rotation/opacity; audio uses gain/pan. Wrong-kind tracks
/// are rejected rather than ignored. z order remains static.
///
/// Fades multiply opacity (video) or gain (audio). For a source-over dissolve,
/// keep the outgoing lower video opaque and fade in the overlapping upper video;
/// fading both video layers would intentionally reveal the black/background.
/// Audio crossfades can use complementary outgoing/incoming linear fades.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClipAnimation {
    pub x: Option<KeyframeTrack>,
    pub y: Option<KeyframeTrack>,
    pub scale: Option<KeyframeTrack>,
    pub rotation: Option<KeyframeTrack>,
    pub opacity: Option<KeyframeTrack>,
    pub gain: Option<KeyframeTrack>,
    pub pan: Option<KeyframeTrack>,
    pub fade_in_us: i64,
    pub fade_out_us: i64,
}

/// Empty `clips` preserves all static behavior. Otherwise entries correspond to
/// input clip indices (before z sorting), and the length must match exactly.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TimelineOptions {
    pub clips: Vec<ClipAnimation>,
}

#[derive(Clone, Copy)]
pub(crate) struct LocalTime {
    num: i128,
    den: i128,
}
impl LocalTime {
    pub(crate) fn from_timestamp(time: Timestamp, start_us: i64) -> Self {
        let base = time.time_base();
        let den = i128::from(base.den());
        Self {
            num: i128::from(time.ticks()) * i128::from(base.num()) * 1_000_000
                - i128::from(start_us) * den,
            den,
        }
    }
    pub(crate) fn from_sample(index: u64, rate: u32, start_us: i64) -> Self {
        let den = i128::from(rate);
        Self {
            num: i128::from(index) * 1_000_000 - i128::from(start_us) * den,
            den,
        }
    }
}
impl ClipAnimation {
    pub(crate) fn value(track: &Option<KeyframeTrack>, fallback: f64, time: LocalTime) -> f64 {
        track.as_ref().map_or(fallback, |track| track.sample(time))
    }
    pub(crate) fn envelope(&self, time: LocalTime, duration: i64) -> f64 {
        let mut value = 1.0;
        if self.fade_in_us > 0 {
            value *=
                (time.num as f64 / (i128::from(self.fade_in_us) * time.den) as f64).clamp(0.0, 1.0);
        }
        if self.fade_out_us > 0 {
            value *= ((i128::from(duration) * time.den - time.num) as f64
                / (i128::from(self.fade_out_us) * time.den) as f64)
                .clamp(0.0, 1.0);
        }
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ye_time::Rational;
    #[test]
    fn interpolation_preserves_finite_endpoint_bounds() {
        for (left, right) in [
            (f64::from_bits(1), f64::from_bits(1)),
            (f64::from_bits(1), f64::from_bits(2)),
            (f64::MAX, f64::MAX),
            (-f64::MAX, f64::MAX),
        ] {
            let track = KeyframeTrack {
                keyframes: vec![
                    Keyframe {
                        time_us: 0,
                        value: left,
                        interpolation: Interpolation::Linear,
                    },
                    Keyframe {
                        time_us: 1_000_000,
                        value: right,
                        interpolation: Interpolation::Linear,
                    },
                ],
            };
            for sample in 0..=10 {
                let value = track.sample(LocalTime::from_sample(sample, 10, 0));
                assert!(value.is_finite());
                assert!((left.min(right)..=left.max(right)).contains(&value));
            }
        }
    }
    #[test]
    fn fractional_hold_boundary_and_linear_values() {
        let mut track = KeyframeTrack {
            keyframes: vec![
                Keyframe {
                    time_us: 0,
                    value: 0.0,
                    interpolation: Interpolation::Hold,
                },
                Keyframe {
                    time_us: 33_367,
                    value: 1.0,
                    interpolation: Interpolation::Linear,
                },
                Keyframe {
                    time_us: 100_000,
                    value: 0.0,
                    interpolation: Interpolation::Linear,
                },
            ],
        };
        // Exact 30000/1001 frame time is 33366.666...; rounding it to µs would
        // activate the hold keyframe one fractional frame too early.
        assert_eq!(
            track.sample(LocalTime::from_timestamp(
                Timestamp::new(1, Rational::new(1001, 30000)),
                0
            )),
            0.0
        );
        assert_eq!(
            track.sample(LocalTime::from_timestamp(
                Timestamp::new(33_367, Rational::new(1, 1_000_000)),
                0
            )),
            1.0
        );
        track.keyframes[0].interpolation = Interpolation::Linear;
        let value = track.sample(LocalTime::from_sample(1, 48000, 0));
        assert!((value - (1_000_000.0 / 48000.0 / 33367.0)).abs() < 1e-12);
        assert_eq!(track.sample(LocalTime::from_sample(48000, 48000, 0)), 0.0);
    }
}
