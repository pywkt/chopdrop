//! Time and pitch types (v2): how a lane relates its sample's length to the
//! host tempo.
//!
//! Pure: no nice-plug types. The types live here and are re-exported from
//! `model.rs`, as [`LaneMode`] is from `voices.rs`, so the writer API and the
//! voices agree by construction.
//!
//! Strings are the project-file spelling (kept as strings, not serde enums,
//! so an unknown value from a newer or hostile file falls back instead of
//! failing the whole load): [`as_str`](NoteLength::as_str) writes them,
//! `from_str` reads them and answers `None` for anything it does not know.
//!
//! [`LaneMode`]: crate::voices::LaneMode

use std::sync::atomic::{AtomicU64, Ordering};

/// How a lane's playback length follows the tempo.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum TimeMode {
    /// As cut: the pitch knob repitches tape-style, as v1. The default.
    #[default]
    Off,
    /// Synced to a [`Length`] by changing playback speed (live).
    Repitch,
    /// Synced to a [`Length`] by time-stretching (rendered); pitch is
    /// independent.
    Stretch,
}

impl TimeMode {
    /// The project-file spelling: `"off"`, `"repitch"` or `"stretch"`.
    pub fn as_str(self) -> &'static str {
        match self {
            TimeMode::Off => "off",
            TimeMode::Repitch => "repitch",
            TimeMode::Stretch => "stretch",
        }
    }

    /// Parses [`as_str`](Self::as_str)'s spelling; anything else is `None`.
    #[allow(clippy::should_implement_trait)] // deliberate name; Option, not FromStr's Result
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "off" => Some(TimeMode::Off),
            "repitch" => Some(TimeMode::Repitch),
            "stretch" => Some(TimeMode::Stretch),
            _ => None,
        }
    }
}

/// The length a synced lane plays for: the chop as it is, or a note length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Length {
    /// The chop's own length. The default.
    #[default]
    AsCut,
    /// A musical length at the current tempo.
    Beats(NoteLength),
}

/// The project-file spelling of [`Length::AsCut`].
const AS_CUT: &str = "as_cut";

impl Length {
    /// The project-file spelling: `"as_cut"` or the [`NoteLength`] string.
    pub fn as_str(self) -> &'static str {
        match self {
            Length::AsCut => AS_CUT,
            Length::Beats(n) => n.as_str(),
        }
    }

    /// Parses [`as_str`](Self::as_str)'s spelling; anything else is `None`.
    #[allow(clippy::should_implement_trait)] // deliberate name; Option, not FromStr's Result
    pub fn from_str(s: &str) -> Option<Self> {
        if s == AS_CUT {
            Some(Length::AsCut)
        } else {
            NoteLength::from_str(s).map(Length::Beats)
        }
    }
}

/// A musical length. 1 bar = 4 beats (time signature is not read in v2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoteLength {
    Sixteenth,
    Eighth,
    Quarter,
    Half,
    Bar,
    TwoBars,
    FourBars,
    EightBars,
    SixteenthDotted,
    EighthDotted,
    QuarterDotted,
    HalfDotted,
    SixteenthTriplet,
    EighthTriplet,
    QuarterTriplet,
    HalfTriplet,
}

/// Every [`NoteLength`] in menu order: straight 1/16 … 8 bars, then dotted,
/// then triplet.
pub const NOTE_LENGTHS: [NoteLength; 16] = [
    NoteLength::Sixteenth,
    NoteLength::Eighth,
    NoteLength::Quarter,
    NoteLength::Half,
    NoteLength::Bar,
    NoteLength::TwoBars,
    NoteLength::FourBars,
    NoteLength::EightBars,
    NoteLength::SixteenthDotted,
    NoteLength::EighthDotted,
    NoteLength::QuarterDotted,
    NoteLength::HalfDotted,
    NoteLength::SixteenthTriplet,
    NoteLength::EighthTriplet,
    NoteLength::QuarterTriplet,
    NoteLength::HalfTriplet,
];

impl NoteLength {
    /// The length in beats (quarter notes); dotted ×1.5, triplet ×2/3.
    pub fn beats(self) -> f64 {
        match self {
            NoteLength::Sixteenth => 0.25,
            NoteLength::Eighth => 0.5,
            NoteLength::Quarter => 1.0,
            NoteLength::Half => 2.0,
            NoteLength::Bar => 4.0,
            NoteLength::TwoBars => 8.0,
            NoteLength::FourBars => 16.0,
            NoteLength::EightBars => 32.0,
            NoteLength::SixteenthDotted => 0.25 * 1.5,
            NoteLength::EighthDotted => 0.5 * 1.5,
            NoteLength::QuarterDotted => 1.5,
            NoteLength::HalfDotted => 3.0,
            NoteLength::SixteenthTriplet => 0.25 * 2.0 / 3.0,
            NoteLength::EighthTriplet => 0.5 * 2.0 / 3.0,
            NoteLength::QuarterTriplet => 2.0 / 3.0,
            NoteLength::HalfTriplet => 2.0 * 2.0 / 3.0,
        }
    }

    /// The short label the GUI shows ("1/16", "1 bar", "2 bars", "1/8d",
    /// "1/4t").
    pub fn label(self) -> &'static str {
        match self {
            NoteLength::Bar => "1 bar",
            NoteLength::TwoBars => "2 bars",
            NoteLength::FourBars => "4 bars",
            NoteLength::EightBars => "8 bars",
            // The fractions read the same in the file and on screen.
            other => other.as_str(),
        }
    }

    /// The project-file spelling ("1/16", "1bar", "2bars", "1/8d", "1/4t").
    pub fn as_str(self) -> &'static str {
        match self {
            NoteLength::Sixteenth => "1/16",
            NoteLength::Eighth => "1/8",
            NoteLength::Quarter => "1/4",
            NoteLength::Half => "1/2",
            NoteLength::Bar => "1bar",
            NoteLength::TwoBars => "2bars",
            NoteLength::FourBars => "4bars",
            NoteLength::EightBars => "8bars",
            NoteLength::SixteenthDotted => "1/16d",
            NoteLength::EighthDotted => "1/8d",
            NoteLength::QuarterDotted => "1/4d",
            NoteLength::HalfDotted => "1/2d",
            NoteLength::SixteenthTriplet => "1/16t",
            NoteLength::EighthTriplet => "1/8t",
            NoteLength::QuarterTriplet => "1/4t",
            NoteLength::HalfTriplet => "1/2t",
        }
    }

    /// Parses [`as_str`](Self::as_str)'s spelling; anything else is `None`.
    #[allow(clippy::should_implement_trait)] // deliberate name; Option, not FromStr's Result
    pub fn from_str(s: &str) -> Option<Self> {
        NOTE_LENGTHS.into_iter().find(|n| n.as_str() == s)
    }
}

/// The slowest tempo the maths accepts, BPM.
pub const TEMPO_MIN: f64 = 20.0;
/// The fastest tempo the maths accepts, BPM.
pub const TEMPO_MAX: f64 = 999.0;
/// The tempo before the host has sent one, and the stand-in for a NaN.
pub const TEMPO_DEFAULT: f64 = 120.0;

/// The slowest repitch playback speed (4 octaves down).
pub const SPEED_MIN: f64 = 1.0 / 16.0;
/// The fastest repitch playback speed (4 octaves up).
pub const SPEED_MAX: f64 = 16.0;

/// A host tempo made safe: NaN → [`TEMPO_DEFAULT`], everything else (0,
/// negative, infinite, huge) clamped into [`TEMPO_MIN`] ..= [`TEMPO_MAX`].
pub fn clamp_tempo(bpm: f64) -> f64 {
    if bpm.is_nan() {
        return TEMPO_DEFAULT;
    }
    bpm.clamp(TEMPO_MIN, TEMPO_MAX)
}

/// The host tempo as everyone reads it: one per instance, owned by the model
/// and handed (as an `Arc`) to the engine, the editor and the render worker —
/// never a second cell.
///
/// An `AtomicU64` holding the `f64`'s bits, relaxed both ways: `process()`
/// stores the host's value once a block and every reader takes whatever was
/// stored last, so no ordering against anything else is needed. Always holds
/// a clamped tempo — [`TEMPO_DEFAULT`] until the host sends one.
#[derive(Debug)]
pub struct TempoCell(AtomicU64);

impl TempoCell {
    /// A cell at [`TEMPO_DEFAULT`].
    pub fn new() -> Self {
        TempoCell(AtomicU64::new(TEMPO_DEFAULT.to_bits()))
    }

    /// The last tempo stored, BPM (clamped).
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }

    /// Stores `bpm` through [`clamp_tempo`]. One relaxed store: safe on the
    /// audio thread.
    pub fn set(&self, bpm: f64) {
        self.0.store(clamp_tempo(bpm).to_bits(), Ordering::Relaxed);
    }

    /// The host's tempo for a block: `Some` is stored (clamped), `None`
    /// leaves the last value in place.
    pub fn update(&self, host: Option<f64>) {
        if let Some(bpm) = host {
            self.set(bpm);
        }
    }
}

impl Default for TempoCell {
    fn default() -> Self {
        Self::new()
    }
}

/// Seconds a [`NoteLength`] lasts at `tempo` (clamped), stretched by
/// `fine_pct` percent: `beats × 60 / tempo × (1 + fine / 100)`. A non-finite
/// fine counts as 0 (the lane clamps it to ±10 % before it gets here).
pub fn target_secs(length: NoteLength, fine_pct: f32, tempo: f64) -> f64 {
    beats_secs(length.beats(), fine_pct, tempo)
}

/// [`target_secs`] for a length already in beats — what the published lane
/// carries (`PublishedLane::length_beats`), so the audio thread need not map
/// it back to a [`NoteLength`]. Same formula, same guards.
pub fn beats_secs(beats: f64, fine_pct: f32, tempo: f64) -> f64 {
    let fine = if fine_pct.is_finite() { f64::from(fine_pct) } else { 0.0 };
    beats * 60.0 / clamp_tempo(tempo) * (1.0 + fine / 100.0)
}

/// A sample's own length in seconds; a rate of 0 answers 0, never a division
/// by zero.
pub fn natural_secs(frames: usize, rate: u32) -> f64 {
    if rate == 0 {
        return 0.0;
    }
    frames as f64 / f64::from(rate)
}

/// The playback speed that makes `natural` seconds last `target` seconds,
/// clamped to [`SPEED_MIN`] ..= [`SPEED_MAX`]; an undefined ratio (0/0, NaN)
/// plays at 1.
pub fn repitch_speed(natural: f64, target: f64) -> f64 {
    let speed = natural / target;
    if speed.is_nan() {
        return 1.0;
    }
    speed.clamp(SPEED_MIN, SPEED_MAX)
}

/// The pitch change a playback speed makes, in semitones (`12·log2(speed)`):
/// what the disabled pitch field shows in Repitch. The speed is clamped as in
/// [`repitch_speed`]; NaN → 0.
pub fn speed_to_semis(speed: f64) -> f32 {
    if speed.is_nan() {
        return 0.0;
    }
    (12.0 * speed.clamp(SPEED_MIN, SPEED_MAX).log2()) as f32
}

/// How much going from `natural` to `target` seconds changes the length, in
/// percent: `(target / natural − 1) × 100`. Undefined (natural 0, NaN) → 0.
pub fn change_pct(natural: f64, target: f64) -> f64 {
    let pct = (target / natural - 1.0) * 100.0;
    if pct.is_finite() { pct } else { 0.0 }
}

/// The [`NoteLength`] (fine 0) whose length at `tempo` is closest to
/// `natural` seconds, by smallest `|change_pct|`; ties go to the earlier
/// entry in [`NOTE_LENGTHS`].
pub fn nearest(natural: f64, tempo: f64) -> NoteLength {
    let mut best = NOTE_LENGTHS[0];
    let mut best_off = f64::INFINITY;
    for n in NOTE_LENGTHS {
        let off = change_pct(natural, target_secs(n, 0.0, tempo)).abs();
        if off < best_off {
            best = n;
            best_off = off;
        }
    }
    best
}

/// A [`change_pct`] as the menu shows it: "−1.4 %", "+97 %", "0.0 %". One
/// decimal while the rounded magnitude is below 10, none from there; a real
/// minus sign (U+2212); no sign on zero. Non-finite → "—".
pub fn format_change(pct: f64) -> String {
    if !pct.is_finite() {
        return "\u{2014}".to_string();
    }
    let tenths = (pct * 10.0).round() / 10.0;
    let (magnitude, decimals) =
        if tenths.abs() < 10.0 { (tenths.abs(), 1) } else { (pct.abs().round(), 0) };
    let sign = if magnitude == 0.0 {
        ""
    } else if pct < 0.0 {
        "\u{2212}"
    } else {
        "+"
    };
    format!("{sign}{magnitude:.decimals$} %")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn tempo_cell_defaults_clamps_and_keeps_the_last_value() {
        let cell = TempoCell::default();
        assert_eq!(cell.get(), TEMPO_DEFAULT);
        cell.update(Some(90.0));
        assert_eq!(cell.get(), 90.0);
        cell.update(None);
        assert_eq!(cell.get(), 90.0);
        cell.update(Some(f64::NAN));
        assert_eq!(cell.get(), TEMPO_DEFAULT);
        cell.set(0.0);
        assert_eq!(cell.get(), TEMPO_MIN);
        cell.set(1e9);
        assert_eq!(cell.get(), TEMPO_MAX);
    }

    #[test]
    fn target_secs_quarter_at_120_is_half_a_second() {
        assert_eq!(target_secs(NoteLength::Quarter, 0.0, 120.0), 0.5);
        assert!(close(target_secs(NoteLength::Bar, 0.0, 120.0), 2.0));
        assert!(close(target_secs(NoteLength::EighthTriplet, 0.0, 60.0), 1.0 / 3.0));
    }

    #[test]
    fn target_secs_fine_plus_minus_ten_percent() {
        assert!(close(target_secs(NoteLength::Quarter, 10.0, 120.0), 0.55));
        assert!(close(target_secs(NoteLength::Quarter, -10.0, 120.0), 0.45));
        assert!(close(target_secs(NoteLength::Bar, 10.0, 120.0), 2.2));
        // A non-finite fine counts as 0.
        assert_eq!(target_secs(NoteLength::Quarter, f32::NAN, 120.0), 0.5);
        assert_eq!(target_secs(NoteLength::Quarter, f32::INFINITY, 120.0), 0.5);
    }

    #[test]
    fn target_secs_clamps_its_tempo() {
        assert!(close(target_secs(NoteLength::Quarter, 0.0, 0.0), 3.0)); // 20 BPM
        assert_eq!(target_secs(NoteLength::Quarter, 0.0, f64::NAN), 0.5); // 120 BPM
        assert!(close(target_secs(NoteLength::Quarter, 0.0, 1e9), 60.0 / 999.0));
    }

    #[test]
    fn tempo_clamps() {
        assert_eq!(clamp_tempo(0.0), TEMPO_MIN);
        assert_eq!(clamp_tempo(-120.0), TEMPO_MIN);
        assert_eq!(clamp_tempo(f64::NAN), TEMPO_DEFAULT);
        assert_eq!(clamp_tempo(1e9), TEMPO_MAX);
        assert_eq!(clamp_tempo(f64::INFINITY), TEMPO_MAX);
        assert_eq!(clamp_tempo(f64::NEG_INFINITY), TEMPO_MIN);
        assert_eq!(clamp_tempo(122.5), 122.5);
        assert_eq!(clamp_tempo(TEMPO_MIN), TEMPO_MIN);
        assert_eq!(clamp_tempo(TEMPO_MAX), TEMPO_MAX);
    }

    #[test]
    fn natural_secs_guards_rate_zero() {
        assert_eq!(natural_secs(48_000, 48_000), 1.0);
        assert_eq!(natural_secs(22_050, 44_100), 0.5);
        assert_eq!(natural_secs(0, 48_000), 0.0);
        assert_eq!(natural_secs(48_000, 0), 0.0);
        assert_eq!(natural_secs(0, 0), 0.0);
    }

    #[test]
    fn repitch_speed_ratio_and_clamps() {
        assert_eq!(repitch_speed(2.0, 1.0), 2.0);
        assert_eq!(repitch_speed(1.0, 2.0), 0.5);
        assert_eq!(repitch_speed(1.0, 1.0), 1.0);
        assert_eq!(repitch_speed(100.0, 1.0), SPEED_MAX);
        assert_eq!(repitch_speed(1.0, 100.0), SPEED_MIN);
        // Degenerate inputs never escape the range or go NaN.
        assert_eq!(repitch_speed(1.0, 0.0), SPEED_MAX);
        assert_eq!(repitch_speed(0.0, 1.0), SPEED_MIN);
        assert_eq!(repitch_speed(0.0, 0.0), 1.0);
        assert_eq!(repitch_speed(f64::NAN, 1.0), 1.0);
        assert_eq!(repitch_speed(f64::INFINITY, 1.0), SPEED_MAX);
    }

    #[test]
    fn speed_to_semis_is_twelve_per_octave() {
        assert_eq!(speed_to_semis(2.0), 12.0);
        assert_eq!(speed_to_semis(0.5), -12.0);
        assert_eq!(speed_to_semis(1.0), 0.0);
        assert!((speed_to_semis(1.5) - 7.0195).abs() < 1e-3);
        // Clamped to the speed range: ±48 st.
        assert_eq!(speed_to_semis(1000.0), 48.0);
        assert_eq!(speed_to_semis(0.0), -48.0);
        assert_eq!(speed_to_semis(-1.0), -48.0);
        assert_eq!(speed_to_semis(f64::NAN), 0.0);
    }

    #[test]
    fn change_pct_formula_and_guards() {
        assert!(close(change_pct(1.0, 2.0), 100.0));
        assert!(close(change_pct(2.0, 1.0), -50.0));
        assert_eq!(change_pct(1.0, 1.0), 0.0);
        assert_eq!(change_pct(0.0, 1.0), 0.0);
        assert_eq!(change_pct(0.0, 0.0), 0.0);
        assert_eq!(change_pct(f64::NAN, 1.0), 0.0);
    }

    /// The worked example chop: 1.973 s at 122 BPM. Its nearest length is
    /// 1 bar. The example's "−1.4 %" / "+97 %" figures do not follow from
    /// 1.973 s at 122 BPM (a bar there is 1.967 s, −0.3 %); they are the
    /// pair a 1.995 s chop gives at 122 BPM, pinned below with the formula.
    #[test]
    fn brief_example_nearest_bar_and_change() {
        let natural = 1.973;
        assert_eq!(nearest(natural, 122.0), NoteLength::Bar);
        let bar = target_secs(NoteLength::Bar, 0.0, 122.0);
        assert_eq!(format_change(change_pct(natural, bar)), "\u{2212}0.3 %");
        // At 120 BPM the same chop is 1.4 % short of the 2 s bar.
        assert_eq!(nearest(natural, 120.0), NoteLength::Bar);
        let bar_120 = target_secs(NoteLength::Bar, 0.0, 120.0);
        assert_eq!(format_change(change_pct(natural, bar_120)), "+1.4 %");

        let natural = 1.995;
        assert_eq!(nearest(natural, 122.0), NoteLength::Bar);
        assert_eq!(format_change(change_pct(natural, bar)), "\u{2212}1.4 %");
        let two_bars = target_secs(NoteLength::TwoBars, 0.0, 122.0);
        assert_eq!(format_change(change_pct(natural, two_bars)), "+97 %");
    }

    #[test]
    fn nearest_picks_exact_matches_and_ties_go_first() {
        for n in NOTE_LENGTHS {
            assert_eq!(nearest(target_secs(n, 0.0, 97.0), 97.0), n, "{n:?}");
        }
        // Undefined chops (0 s, NaN) all score 0 → the first entry.
        assert_eq!(nearest(0.0, 120.0), NOTE_LENGTHS[0]);
        assert_eq!(nearest(f64::NAN, 120.0), NOTE_LENGTHS[0]);
        // Far beyond 8 bars → 8 bars; far below 1/16t → 1/16t.
        assert_eq!(nearest(1000.0, 120.0), NoteLength::EightBars);
        assert_eq!(nearest(0.001, 120.0), NoteLength::SixteenthTriplet);
    }

    #[test]
    fn format_change_decimals_and_signs() {
        assert_eq!(format_change(-1.35), "\u{2212}1.4 %");
        assert_eq!(format_change(97.2), "+97 %");
        assert_eq!(format_change(-50.0), "\u{2212}50 %");
        assert_eq!(format_change(9.94), "+9.9 %");
        assert_eq!(format_change(9.96), "+10 %"); // rounds into the no-decimal band
        assert_eq!(format_change(10.0), "+10 %");
        assert_eq!(format_change(0.0), "0.0 %");
        assert_eq!(format_change(-0.04), "0.0 %");
        assert_eq!(format_change(0.05), "+0.1 %");
        assert_eq!(format_change(1500.0), "+1500 %");
        assert_eq!(format_change(f64::NAN), "\u{2014}");
        assert_eq!(format_change(f64::INFINITY), "\u{2014}");
    }

    /// Every variant's (beats, label, file string), in menu order.
    const TABLE: [(NoteLength, f64, &str, &str); 16] = [
        (NoteLength::Sixteenth, 0.25, "1/16", "1/16"),
        (NoteLength::Eighth, 0.5, "1/8", "1/8"),
        (NoteLength::Quarter, 1.0, "1/4", "1/4"),
        (NoteLength::Half, 2.0, "1/2", "1/2"),
        (NoteLength::Bar, 4.0, "1 bar", "1bar"),
        (NoteLength::TwoBars, 8.0, "2 bars", "2bars"),
        (NoteLength::FourBars, 16.0, "4 bars", "4bars"),
        (NoteLength::EightBars, 32.0, "8 bars", "8bars"),
        (NoteLength::SixteenthDotted, 0.375, "1/16d", "1/16d"),
        (NoteLength::EighthDotted, 0.75, "1/8d", "1/8d"),
        (NoteLength::QuarterDotted, 1.5, "1/4d", "1/4d"),
        (NoteLength::HalfDotted, 3.0, "1/2d", "1/2d"),
        (NoteLength::SixteenthTriplet, 1.0 / 6.0, "1/16t", "1/16t"),
        (NoteLength::EighthTriplet, 1.0 / 3.0, "1/8t", "1/8t"),
        (NoteLength::QuarterTriplet, 2.0 / 3.0, "1/4t", "1/4t"),
        (NoteLength::HalfTriplet, 4.0 / 3.0, "1/2t", "1/2t"),
    ];

    #[test]
    fn every_note_length_beats_and_label() {
        for (n, beats, label, s) in TABLE {
            assert!((n.beats() - beats).abs() < 1e-12, "{n:?} beats {}", n.beats());
            assert_eq!(n.label(), label, "{n:?} label");
            assert_eq!(n.as_str(), s, "{n:?} string");
        }
    }

    #[test]
    fn menu_order_is_straight_then_dotted_then_triplet() {
        let expected: Vec<NoteLength> = TABLE.iter().map(|t| t.0).collect();
        assert_eq!(NOTE_LENGTHS.to_vec(), expected);
        // And every variant appears exactly once.
        let mut seen = std::collections::HashSet::new();
        assert!(NOTE_LENGTHS.iter().all(|n| seen.insert(*n)));
    }

    #[test]
    fn note_length_string_round_trip() {
        for n in NOTE_LENGTHS {
            assert_eq!(NoteLength::from_str(n.as_str()), Some(n));
        }
    }

    #[test]
    fn unknown_strings_are_none() {
        for s in ["", "banana", "1 bar", "1/3", "1/16 ", "1BAR", "as_cut"] {
            assert_eq!(NoteLength::from_str(s), None, "{s:?}");
        }
        for s in ["", "banana", "Off", "OFF", "repitch "] {
            assert_eq!(TimeMode::from_str(s), None, "{s:?}");
        }
        for s in ["", "banana", "ascut", "1 bar"] {
            assert_eq!(Length::from_str(s), None, "{s:?}");
        }
    }

    #[test]
    fn time_mode_strings_round_trip() {
        let modes = [
            (TimeMode::Off, "off"),
            (TimeMode::Repitch, "repitch"),
            (TimeMode::Stretch, "stretch"),
        ];
        for (m, s) in modes {
            assert_eq!(m.as_str(), s);
            assert_eq!(TimeMode::from_str(s), Some(m));
        }
        assert_eq!(TimeMode::default(), TimeMode::Off);
    }

    #[test]
    fn length_strings_round_trip() {
        assert_eq!(Length::AsCut.as_str(), "as_cut");
        assert_eq!(Length::from_str("as_cut"), Some(Length::AsCut));
        for n in NOTE_LENGTHS {
            let l = Length::Beats(n);
            assert_eq!(l.as_str(), n.as_str());
            assert_eq!(Length::from_str(l.as_str()), Some(l));
        }
        assert_eq!(Length::default(), Length::AsCut);
    }
}
