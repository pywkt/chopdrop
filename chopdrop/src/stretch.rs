//! Offline time-stretch / pitch-shift of a chop.
//!
//! A thin, pure wrapper over the `signalsmith-stretch` crate: one call takes a
//! whole chop and renders it to an exact frame count with an independent
//! pitch shift. The render worker (`render.rs`) calls it off every real-time
//! thread; nothing here touches nice-plug.
//!
//! Onset alignment is pinned by the click-train tests below: the crate's
//! `exact` already starts the output on the input's first frame (it seeks
//! `input_latency()` frames in and shifts the output back by
//! `output_latency()`), so no extra compensation is applied here.

use basedrop::Shared;
use signalsmith_stretch::Stretch;

use crate::sample::{SampleData, MAX_DECODED_FRAMES};

/// A render waiting for the worker: lane `lane`'s chop at `generation`,
/// rendered to `target_frames` frames shifted by `pitch_st` semitones.
pub struct RenderRequest {
    pub lane: u32,
    pub generation: u64,
    pub chop: Shared<SampleData>,
    pub target_frames: usize,
    pub pitch_st: f32,
}

/// The longest render one chop may ask for: 16 times the chop, and never past
/// [`MAX_DECODED_FRAMES`].
pub fn target_frames_max(chop_frames: usize) -> usize {
    chop_frames.saturating_mul(16).min(MAX_DECODED_FRAMES)
}

/// The widest pitch shift a render accepts, in semitones either way.
pub const PITCH_LIMIT_ST: f32 = 48.0;

/// Why a render failed. The lane keeps its previous render (or the raw chop)
/// and shows [`RenderError::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderError {
    /// Asked for zero frames, or the chop itself is empty.
    Empty,
    /// The stretcher refused the job (`exact` returned `false` — the target
    /// is shorter than the stretcher's own latency).
    Stretcher,
}

impl RenderError {
    /// The short sentence the lane shows.
    pub fn message(&self) -> &'static str {
        match self {
            RenderError::Empty => "Nothing to render.",
            RenderError::Stretcher => "Render failed: the target is too short to stretch.",
        }
    }
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for RenderError {}

/// Renders `chop` to exactly `target_frames` frames (clamped to
/// [`target_frames_max`]) shifted by `pitch_st` semitones. The result keeps
/// the chop's rate and channel count.
pub fn render(
    chop: &SampleData,
    target_frames: usize,
    pitch_st: f32,
) -> Result<SampleData, RenderError> {
    let channels = chop.channels.max(1) as usize;
    let chop_frames = chop.frames.len() / channels;
    let target_frames = target_frames.min(target_frames_max(chop_frames));
    if target_frames == 0 || chop_frames == 0 {
        return Err(RenderError::Empty);
    }
    let pitch_st = if pitch_st.is_nan() {
        0.0
    } else {
        pitch_st.clamp(-PITCH_LIMIT_ST, PITCH_LIMIT_ST)
    };

    let mut stretch = Stretch::preset_default(channels as u32, chop.rate.max(1));
    stretch.set_transpose_factor_semitones(pitch_st, None);

    let input = &chop.frames[..chop_frames * channels];
    // `exact` seeks over the first `input_latency()` input frames without a
    // bounds check, so a shorter chop is zero-padded up to that length and the
    // output grown in the same proportion (then trimmed), keeping the ratio.
    let latency = stretch.input_latency();
    let (padded, out_frames) = if chop_frames < latency {
        let mut padded = input.to_vec();
        padded.resize(latency * channels, 0.0);
        // target ≤ 16 × chop, so this stays ≤ 16 × latency: small.
        (Some(padded), (target_frames * latency).div_ceil(chop_frames))
    } else {
        (None, target_frames)
    };
    let input = padded.as_deref().unwrap_or(input);

    let mut output = vec![0.0f32; out_frames * channels];
    if !stretch.exact(input, &mut output) {
        return Err(RenderError::Stretcher);
    }
    output.truncate(target_frames * channels);
    // A misbehaving stretch must never reach the voices as NaN or infinity.
    for s in &mut output {
        if !s.is_finite() {
            *s = 0.0;
        }
    }
    Ok(SampleData { rate: chop.rate, channels: channels as u8, frames: output.into_boxed_slice() })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn mono(frames: Vec<f32>) -> SampleData {
        SampleData { rate: RATE, channels: 1, frames: frames.into_boxed_slice() }
    }

    fn sine(freq: f32, frames: usize) -> Vec<f32> {
        (0..frames)
            .map(|i| (std::f32::consts::TAU * freq * i as f32 / RATE as f32).sin() * 0.5)
            .collect()
    }

    /// 4 s of one-sample clicks every 0.5 s (8 clicks, the first at 0).
    fn click_train() -> SampleData {
        let mut frames = vec![0.0f32; RATE as usize * 4];
        for k in 0..8 {
            frames[k * RATE as usize / 2] = 1.0;
        }
        mono(frames)
    }

    /// Renders the click train at `factor` × its length and `pitch_st`, and
    /// asserts every click's peak lands within 2 ms of `k × 0.5 s × factor`.
    fn assert_clicks_aligned(factor: f64, pitch_st: f32) {
        let chop = click_train();
        let target = (chop.frame_count() as f64 * factor).round() as usize;
        let out = render(&chop, target, pitch_st).expect("render");
        assert_eq!(out.frame_count(), target);
        let tolerance = (RATE as f64 * 0.002) as usize;
        let window = (RATE as f64 * 0.1) as usize;
        for k in 0..8usize {
            let expected = (k as f64 * 0.5 * factor * RATE as f64).round() as usize;
            let lo = expected.saturating_sub(window);
            let hi = (expected + window).min(target);
            let peak = (lo..hi)
                .max_by(|&a, &b| out.frames[a].abs().total_cmp(&out.frames[b].abs()))
                .unwrap();
            let off = peak.abs_diff(expected);
            assert!(
                off <= tolerance,
                "factor {factor} pitch {pitch_st}: click {k} at {peak}, expected {expected} \
                 ({:.2} ms off)",
                off as f64 * 1000.0 / RATE as f64
            );
        }
    }

    #[test]
    fn clicks_stay_aligned_stretched_to_one_and_a_half() {
        assert_clicks_aligned(1.5, 0.0);
    }

    #[test]
    fn clicks_stay_aligned_squeezed_to_three_quarters() {
        assert_clicks_aligned(0.75, 0.0);
    }

    #[test]
    fn clicks_stay_aligned_shifted_up_five_semitones() {
        assert_clicks_aligned(1.0, 5.0);
    }

    #[test]
    fn clicks_stay_aligned_shifted_down_five_semitones() {
        assert_clicks_aligned(1.0, -5.0);
    }

    #[test]
    fn output_length_is_exact_for_mono_stereo_and_odd_lengths() {
        for &(channels, frames, target) in &[
            (1u8, 48_000usize, 72_001usize),
            (2, 48_000, 36_000),
            (1, 12_345, 9_877),
            (2, 33_333, 50_001),
        ] {
            let data: Vec<f32> = (0..frames * channels as usize)
                .map(|i| ((i * 7919) % 200) as f32 / 200.0 - 0.5)
                .collect();
            let chop = SampleData { rate: RATE, channels, frames: data.into_boxed_slice() };
            let out = render(&chop, target, 3.0).expect("render");
            assert_eq!(out.rate, RATE);
            assert_eq!(out.channels, channels);
            assert_eq!(out.frame_count(), target, "channels {channels} frames {frames}");
            assert_eq!(out.frames.len(), target * channels as usize);
            assert!(out.frames.iter().all(|s| s.is_finite()));
        }
    }

    #[test]
    fn a_chop_shorter_than_the_latency_still_renders_exactly() {
        let chop = mono(sine(440.0, 300));
        let out = render(&chop, 3_000, 0.0).expect("render");
        assert_eq!(out.frame_count(), 3_000);
    }

    /// Positive-going zero crossings in the middle half of `frames`.
    fn crossings(frames: &[f32]) -> usize {
        let mid = &frames[frames.len() / 4..frames.len() * 3 / 4];
        mid.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    #[test]
    fn twelve_semitones_halves_or_doubles_a_sines_period() {
        let chop = mono(sine(440.0, RATE as usize));
        let base = crossings(&render(&chop, RATE as usize, 0.0).unwrap().frames) as f64;
        let up = crossings(&render(&chop, RATE as usize, 12.0).unwrap().frames) as f64;
        let down = crossings(&render(&chop, RATE as usize, -12.0).unwrap().frames) as f64;
        assert!((base / 220.0 - 1.0).abs() < 0.03, "base {base}");
        assert!((up / base - 2.0).abs() < 0.06, "up {up} vs {base}");
        assert!((down / base - 0.5).abs() < 0.03, "down {down} vs {base}");
    }

    #[test]
    fn stretcher_refusal_is_a_render_error() {
        // `exact` returns false when the output is shorter than twice the
        // stretcher's output latency; a 10-frame target is far below it.
        let chop = mono(sine(440.0, RATE as usize));
        assert_eq!(render(&chop, 10, 0.0).unwrap_err(), RenderError::Stretcher);
    }

    #[test]
    fn zero_target_or_empty_chop_is_a_render_error() {
        let chop = mono(sine(440.0, 1_000));
        assert_eq!(render(&chop, 0, 0.0).unwrap_err(), RenderError::Empty);
        let empty = mono(Vec::new());
        assert_eq!(render(&empty, 1_000, 0.0).unwrap_err(), RenderError::Empty);
    }

    #[test]
    fn target_is_clamped_to_sixteen_times_the_chop() {
        let chop = mono(sine(440.0, 4_800));
        let out = render(&chop, usize::MAX, f32::NAN).expect("render");
        assert_eq!(out.frame_count(), 4_800 * 16);
    }
}
