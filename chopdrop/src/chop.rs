//! The chop editor's pure model (v2).
//!
//! Pure: no nice-plug or egui types, so the maths is unit-tested without the
//! framework. The egui strip that draws it is `editor/chop_strip.rs`.
//!
//! So far: [`Peaks`], the waveform overview the strip draws from (built once
//! per source on the loader thread and only read afterwards); [`View`], the
//! main view's scroll and zoom; [`Selection`], the region being chopped;
//! [`snap_zero_crossing`], [`format_time`], [`format_origin`] (with its
//! [`ChopOrigin`]), [`chop_name`] and [`readout`]; [`cut`] and its edge fade
//! ([`edge_gain`], shared with the engine's preview); [`ChopEditor`], the
//! per-instance editor state the [`crate::model::Model`] owns (its writer
//! API is there, its loader job is [`crate::loader::Source::ChopSource`]).

use std::path::Path;
use std::sync::Arc;

use basedrop::Shared;

use crate::sample::SampleData;
use crate::time;

/// The longest source the editor opens: 15 minutes, checked once the decoded
/// rate is known.
pub const MAX_SOURCE_SECONDS: f64 = 15.0 * 60.0;

/// The strip's default height in logical pixels, header included: 400, the
/// height the user had dragged it to in their own window (2026-11-16; it
/// was 180, which showed the waveform too small to chop by eye).
pub const STRIP_HEIGHT: f32 = 400.0;

/// The shortest the strip may be dragged (expanded).
pub const STRIP_MIN_HEIGHT: f32 = 96.0;

/// The tallest the strip may be saved or set: a bound for hostile input,
/// well past any real screen's use for it.
pub const STRIP_MAX_HEIGHT: f32 = 2000.0;

/// A strip height clamped to [`STRIP_MIN_HEIGHT`] … [`STRIP_MAX_HEIGHT`];
/// NaN reads as [`STRIP_HEIGHT`].
pub fn clamp_strip_height(height_px: f32) -> f32 {
    if height_px.is_nan() {
        return STRIP_HEIGHT;
    }
    height_px.clamp(STRIP_MIN_HEIGHT, STRIP_MAX_HEIGHT)
}

/// The main view's width assumed until the strip reports its own (the
/// window's default width): what [`View::fit`] fits a fresh source to.
pub const DEFAULT_VIEW_WIDTH_PX: f32 = 560.0;

/// What a source whose file is gone (or unreadable) says, before its name.
pub const SOURCE_MISSING_PREFIX: &str = "source not found: ";

/// Frames per bucket of each peak level, finest first, of the mono mix.
/// Each level is a whole multiple of the one before, so a coarser level is
/// built exactly from the finer one.
pub const PEAK_LEVELS: [usize; 3] = [64, 512, 4096];

/// A selection is always at least this many frames long.
pub const MIN_SELECTION_FRAMES: u64 = 2;

/// The linear fade at both ends of a chop ([`cut`]) and of the preview of
/// one (the engine's region voice), so the preview is what the lane plays.
pub const EDGE_FADE_MS: f32 = 1.0;

/// The zero-crossing snap searches this far either side of the frame.
pub const SNAP_WINDOW_MS: f32 = 2.0;

/// ←/→ in the strip nudge the active edge this far; with Shift, the fine
/// step.
pub const NUDGE_COARSE_MS: f32 = 10.0;
pub const NUDGE_FINE_MS: f32 = 1.0;

/// A press in the main view this close to a selection edge (in pixels)
/// grabs that edge.
pub const EDGE_GRAB_PX: f32 = 6.0;

/// One resolution of a [`Peaks`]: the minimum and maximum of the mono mix
/// over each run of `frames_per_bucket` frames (the last bucket may be
/// shorter).
#[derive(Clone, Debug, PartialEq)]
pub struct Level {
    frames_per_bucket: usize,
    min: Box<[f32]>,
    max: Box<[f32]>,
}

impl Level {
    /// How many source frames each bucket covers.
    pub fn frames_per_bucket(&self) -> usize {
        self.frames_per_bucket
    }

    /// How many buckets the level has: the frame count divided by
    /// [`frames_per_bucket`](Self::frames_per_bucket), rounded up.
    pub fn len(&self) -> usize {
        self.min.len()
    }

    /// Whether the level has no buckets (never, for a decoded sample).
    pub fn is_empty(&self) -> bool {
        self.min.is_empty()
    }

    /// Bucket `i`'s `(min, max)`, or `None` past the end.
    pub fn bucket(&self, i: usize) -> Option<(f32, f32)> {
        Some((*self.min.get(i)?, *self.max.get(i)?))
    }
}

/// The source's waveform at [`PEAK_LEVELS`]' three resolutions.
#[derive(Clone, Debug, PartialEq)]
pub struct Peaks {
    levels: Vec<Level>,
}

impl Peaks {
    /// Builds every level from `data`'s mono mix (the mean of its channels).
    /// Runs on the loader thread: it allocates, and a 15 min source takes a
    /// moment.
    pub fn build(data: &SampleData) -> Peaks {
        let channels = (data.channels as usize).max(1);
        let finest = PEAK_LEVELS[0];
        let mut min = Vec::with_capacity(data.frame_count().div_ceil(finest));
        let mut max = Vec::with_capacity(min.capacity());
        for chunk in data.frames.chunks(finest * channels) {
            let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
            for frame in chunk.chunks_exact(channels) {
                let mono = frame.iter().sum::<f32>() / channels as f32;
                lo = lo.min(mono);
                hi = hi.max(mono);
            }
            if lo > hi {
                // A trailing partial frame only (never for a decoded sample).
                break;
            }
            min.push(lo);
            max.push(hi);
        }
        let mut levels = vec![Level {
            frames_per_bucket: finest,
            min: min.into_boxed_slice(),
            max: max.into_boxed_slice(),
        }];
        for &frames_per_bucket in &PEAK_LEVELS[1..] {
            let finer = levels.last().expect("the finest level is pushed first");
            let ratio = frames_per_bucket / finer.frames_per_bucket;
            let min = finer.min.chunks(ratio)
                .map(|c| c.iter().copied().fold(f32::INFINITY, f32::min));
            let max = finer.max.chunks(ratio)
                .map(|c| c.iter().copied().fold(f32::NEG_INFINITY, f32::max));
            levels.push(Level { frames_per_bucket, min: min.collect(), max: max.collect() });
        }
        Peaks { levels }
    }

    /// The levels, finest first.
    pub fn levels(&self) -> &[Level] {
        &self.levels
    }

    /// The level to draw at `frames_per_pixel`: the coarsest whose bucket is
    /// no wider than a pixel, else (zoomed in past the finest) the finest.
    /// NaN picks the finest.
    pub fn level_for(&self, frames_per_pixel: f64) -> &Level {
        self.levels
            .iter()
            .rev()
            .find(|l| l.frames_per_bucket as f64 <= frames_per_pixel)
            .unwrap_or(&self.levels[0])
    }

    /// The `(min, max)` of the mono mix over frames `[from_frame, to_frame)`
    /// as `level` sees it: every bucket the span touches counts, so the
    /// result may reach slightly past the span. At least one bucket is read
    /// when the span is non-empty and starts inside the source; a span that
    /// is empty or wholly past the end answers `(0.0, 0.0)` (silence).
    pub fn min_max(&self, level: &Level, from_frame: u64, to_frame: u64) -> (f32, f32) {
        let fpb = level.frames_per_bucket.max(1) as u64;
        let buckets = level.len() as u64;
        let first = from_frame / fpb;
        let last = to_frame.div_ceil(fpb).min(buckets);
        if to_frame <= from_frame || first >= last {
            return (0.0, 0.0);
        }
        let (first, last) = (first as usize, last as usize);
        let lo = level.min[first..last].iter().copied().fold(f32::INFINITY, f32::min);
        let hi = level.max[first..last].iter().copied().fold(f32::NEG_INFINITY, f32::max);
        (lo, hi)
    }
}

/// The main view's scroll and zoom: the leftmost frame drawn and how many
/// source frames one pixel covers. Zoom runs from 1 frame/px (all the way
/// in) to "fit" (the whole source across the width).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub first: u64,
    pub frames_per_pixel: f64,
}

/// A usable width: NaN, infinite or under one pixel counts as one pixel.
fn sane_width(width_px: f32) -> f64 {
    let w = width_px as f64;
    if w.is_finite() { w.max(1.0) } else { 1.0 }
}

impl View {
    /// The zoom that shows all `total` frames across `width_px`, never
    /// closer than 1 frame/px.
    fn fit_fpp(total: u64, width_px: f32) -> f64 {
        (total as f64 / sane_width(width_px)).max(1.0)
    }

    /// The whole source across the width.
    pub fn fit(total: u64, width_px: f32) -> View {
        View { first: 0, frames_per_pixel: Self::fit_fpp(total, width_px) }
    }

    /// The largest `first` that still fills the width (0 when everything
    /// fits).
    fn max_first(&self, total: u64, width_px: f32) -> u64 {
        let visible = (sane_width(width_px) * self.frames_per_pixel).ceil();
        total.saturating_sub(visible as u64)
    }

    /// Clamps the zoom to 1 frame/px … fit (NaN → fit) and `first` so the
    /// view stays inside the source.
    fn clamp(&mut self, total: u64, width_px: f32) {
        let fit = Self::fit_fpp(total, width_px);
        // NaN first: `max` would quietly turn it into 1 frame/px.
        if self.frames_per_pixel.is_nan() {
            self.frames_per_pixel = fit;
        }
        self.frames_per_pixel = self.frames_per_pixel.max(1.0).min(fit);
        self.first = self.first.min(self.max_first(total, width_px));
    }

    /// Ctrl+scroll: multiplies `frames_per_pixel` by `factor` (below 1 zooms
    /// in) keeping the frame under `pointer_px` where it is, then clamps the
    /// zoom to 1 frame/px … fit and the scroll to the source. A factor that
    /// is not a positive finite number changes nothing.
    pub fn zoom_at(&mut self, pointer_px: f32, factor: f64, total: u64, width_px: f32) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        let px = (pointer_px as f64).max(0.0).min(sane_width(width_px));
        let px = if px.is_nan() { 0.0 } else { px };
        let anchor = self.first as f64 + px * self.frames_per_pixel;
        self.frames_per_pixel *= factor;
        self.clamp(total, width_px);
        let first = (anchor - px * self.frames_per_pixel).round().max(0.0);
        self.first = first as u64;
        self.clamp(total, width_px);
    }

    /// Plain scroll: moves the view `delta_px` pixels' worth of frames
    /// (positive = later in the source), stopping at either end.
    pub fn pan(&mut self, delta_px: f32, total: u64, width_px: f32) {
        let delta = delta_px as f64 * self.frames_per_pixel;
        if !delta.is_finite() {
            return;
        }
        let first = (self.first as f64 + delta).round().max(0.0);
        self.first = first as u64;
        self.clamp(total, width_px);
    }

    /// The frame drawn at `px` from the left edge (negative pixels → the
    /// first frame). Not clamped to the source.
    pub fn frame_at(&self, px: f32) -> u64 {
        let offset = (px as f64 * self.frames_per_pixel).max(0.0);
        self.first.saturating_add(offset as u64)
    }

    /// Where `frame` is drawn, in pixels from the left edge (negative when
    /// it lies left of the view).
    pub fn px_of(&self, frame: u64) -> f32 {
        let offset = frame as f64 - self.first as f64;
        (offset / self.frames_per_pixel.max(f64::MIN_POSITIVE)) as f32
    }
}

/// Which edge of a [`Selection`] the arrow keys move.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Edge {
    #[default]
    Start,
    End,
}

/// The region being chopped: frames `[start, end)`, at least
/// [`MIN_SELECTION_FRAMES`] long once clamped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
    pub active: Edge,
}

impl Selection {
    /// Puts the selection inside `[0, total]` in order and at least
    /// [`MIN_SELECTION_FRAMES`] long, growing the end (or, at the source's
    /// end, the start). A source shorter than that selects all of it.
    pub fn clamp(&mut self, total: u64) {
        if self.start > self.end {
            std::mem::swap(&mut self.start, &mut self.end);
        }
        if total < MIN_SELECTION_FRAMES {
            self.start = 0;
            self.end = total;
            return;
        }
        self.end = self.end.min(total);
        self.start = self.start.min(self.end);
        if self.end - self.start < MIN_SELECTION_FRAMES {
            self.end = self.start + MIN_SELECTION_FRAMES;
            if self.end > total {
                self.end = total;
                self.start = total - MIN_SELECTION_FRAMES;
            }
        }
    }

    /// Moves `edge` to `frame` and makes it the active edge. The edges never
    /// cross: each stops [`MIN_SELECTION_FRAMES`] short of the other.
    pub fn set_edge(&mut self, edge: Edge, frame: u64, total: u64) {
        self.clamp(total);
        let frame = frame.min(total);
        match edge {
            Edge::Start => {
                self.start = frame.min(self.end.saturating_sub(MIN_SELECTION_FRAMES));
            }
            Edge::End => {
                self.end = frame.max(self.start + MIN_SELECTION_FRAMES).min(total);
            }
        }
        self.active = edge;
        self.clamp(total);
    }

    /// The edge nearer `frame`: the side of the selection a press inside it
    /// lands on, which that press moves. The start on a tie, at the exact
    /// middle.
    pub fn nearer_edge(&self, frame: u64) -> Edge {
        if frame.abs_diff(self.start) <= frame.abs_diff(self.end) {
            Edge::Start
        } else {
            Edge::End
        }
    }

    /// Moves the whole selection by `delta_frames`, keeping its length; it
    /// stops at either end of the source.
    pub fn shift(&mut self, delta_frames: i64, total: u64) {
        self.clamp(total);
        let len = self.len();
        let magnitude = delta_frames.unsigned_abs();
        self.start = if delta_frames < 0 {
            self.start.saturating_sub(magnitude)
        } else {
            self.start.saturating_add(magnitude).min(total - len)
        };
        self.end = self.start + len;
    }

    /// Next: the selection moves to start where it ended, same length.
    /// Returns false (and changes nothing) if it would run past the end.
    pub fn advance(&mut self, total: u64) -> bool {
        let len = self.len();
        match self.end.checked_add(len) {
            Some(end) if len > 0 && end <= total => {
                self.start = self.end;
                self.end = end;
                true
            }
            _ => false,
        }
    }

    /// Length in frames.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether it covers no frames (never, once clamped to a source).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a press in the main view starts, by where it lands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressZone {
    /// Within [`EDGE_GRAB_PX`] of an edge: drag that edge.
    Edge(Edge),
    /// Inside the selection, away from its edges: moves the nearer edge to
    /// the press and drags it ([`Selection::nearer_edge`]), or, with Ctrl
    /// held, carries the selection to a lane.
    Inside,
    /// Anywhere else: a new selection (or, without moving, a click).
    Outside,
}

/// The zone of a press `px` pixels from the main view's left edge. Both
/// edges in reach (a selection only a few pixels wide) → the nearer one,
/// the end on a tie, so a sliver can still be grown to the right.
pub fn press_zone(selection: Option<&Selection>, view: &View, px: f32) -> PressZone {
    let Some(selection) = selection else { return PressZone::Outside };
    if px.is_nan() {
        return PressZone::Outside;
    }
    let to_start = (px - view.px_of(selection.start)).abs();
    let to_end = (px - view.px_of(selection.end)).abs();
    if to_start <= EDGE_GRAB_PX || to_end <= EDGE_GRAB_PX {
        return PressZone::Edge(if to_end <= to_start { Edge::End } else { Edge::Start });
    }
    let frame = view.frame_at(px);
    if px >= 0.0 && frame >= selection.start && frame < selection.end {
        PressZone::Inside
    } else {
        PressZone::Outside
    }
}

/// A press on the overview row this close to the selection's band (in
/// pixels, either side) grabs the band, so a sliver can still be moved.
pub const OVERVIEW_GRAB_PX: f32 = 2.0;

/// The overview's scale: frames per pixel across `width_px` showing all
/// `total` frames (NaN, infinite or under a pixel counts as one pixel).
fn overview_fpp(width_px: f32, total: u64) -> f64 {
    total as f64 / sane_width(width_px)
}

/// The frame under `px` along an overview `width_px` wide showing all
/// `total` frames, inside the source (NaN or left of it → 0).
pub fn overview_frame(px: f32, width_px: f32, total: u64) -> u64 {
    let frame = (f64::from(px) * overview_fpp(width_px, total)).max(0.0);
    if frame.is_nan() { 0 } else { (frame as u64).min(total) }
}

/// A drag of `delta_px` along the overview, in frames (negative = earlier).
/// Not a finite number of pixels → 0.
pub fn overview_frames(delta_px: f32, width_px: f32, total: u64) -> i64 {
    let frames = (f64::from(delta_px) * overview_fpp(width_px, total)).round();
    if frames.is_finite() { frames as i64 } else { 0 }
}

/// Whether a press `px` from the overview's left edge lands on the
/// selection's band: where it is drawn (at least a pixel wide), widened by
/// [`OVERVIEW_GRAB_PX`] either side.
pub fn on_overview_band(selection: &Selection, px: f32, width_px: f32, total: u64) -> bool {
    if total == 0 || px.is_nan() {
        return false;
    }
    let fpp = overview_fpp(width_px, total);
    let x0 = (selection.start as f64 / fpp) as f32;
    let x1 = ((selection.end as f64 / fpp) as f32).max(x0 + 1.0);
    px >= x0 - OVERVIEW_GRAB_PX && px <= x1 + OVERVIEW_GRAB_PX
}

/// [`SNAP_WINDOW_MS`] in frames at `rate`.
pub fn snap_window_frames(rate: u32) -> u64 {
    (f64::from(SNAP_WINDOW_MS) * f64::from(rate) / 1000.0).round() as u64
}

/// `ms` milliseconds (a nudge) in frames at `rate`, never less than one.
pub fn nudge_frames(ms: f32, rate: u32) -> i64 {
    ((f64::from(ms) * f64::from(rate) / 1000.0).round() as i64).max(1)
}

/// A new selection dragged from `anchor` to `frame`, in either direction,
/// clamped to `total`; the edge under the pointer is the active one.
pub fn drag_selection(anchor: u64, frame: u64, total: u64) -> Selection {
    let mut selection = if frame >= anchor {
        Selection { start: anchor, end: frame, active: Edge::End }
    } else {
        Selection { start: frame, end: anchor, active: Edge::Start }
    };
    selection.clamp(total);
    selection
}

/// How many frames the [`EDGE_FADE_MS`] fades of a `len`-frame chop at
/// `rate` take: the fade rounded to frames, capped at half the length so
/// the fade-in and fade-out never overlap.
pub fn edge_fade_frames(rate: u32, len: u64) -> u64 {
    let fade = (f64::from(EDGE_FADE_MS) * f64::from(rate) / 1000.0).round() as u64;
    fade.min(len / 2)
}

/// The edge-fade gain `offset` frames into a `len`-frame chop whose fades
/// are `fade` frames ([`edge_fade_frames`]): linear from 0 at the first
/// frame to 1 at frame `fade`, and back to 0 at the last frame; 1 in
/// between. `offset` may be fractional (the preview voice reads between
/// frames); anything outside the chop is 0. The one shape [`cut`] and the
/// engine's region preview both use.
pub fn edge_gain(offset: f64, len: u64, fade: u64) -> f32 {
    if offset.is_nan() || offset < 0.0 || offset > len.saturating_sub(1) as f64 {
        return 0.0;
    }
    if fade == 0 {
        return 1.0;
    }
    let fade = fade as f64;
    let fade_in = offset / fade;
    let fade_out = (len - 1) as f64 - offset;
    let fade_out = fade_out / fade;
    fade_in.min(fade_out).min(1.0) as f32
}

/// The frames `[start, end)` of `data` as a sample of their own (same rate
/// and channels), with [`EDGE_FADE_MS`] linear fades in and out
/// ([`edge_gain`]). The edges are clamped into the sample; an empty span
/// comes back as one silent frame, since a [`SampleData`] is never empty.
pub fn cut(data: &SampleData, start: u64, end: u64) -> SampleData {
    let channels = (data.channels as usize).max(1);
    let total = (data.frames.len() / channels) as u64;
    let end = end.min(total);
    let start = start.min(end);
    let len = end - start;
    if len == 0 {
        return SampleData {
            rate: data.rate,
            channels: channels as u8,
            frames: vec![0.0; channels].into_boxed_slice(),
        };
    }
    let fade = edge_fade_frames(data.rate, len);
    let from = start as usize * channels;
    let to = end as usize * channels;
    let frames = data.frames[from..to]
        .chunks_exact(channels)
        .enumerate()
        .flat_map(|(i, frame)| {
            let gain = edge_gain(i as f64, len, fade);
            frame.iter().map(move |x| x * gain)
        })
        .collect();
    SampleData { rate: data.rate, channels: channels as u8, frames }
}

/// The zero-crossing snap: the frame nearest `frame`, within
/// `window_frames` either side, where the mono mix crosses zero — a frame
/// that is exactly 0, or the quieter of two neighbours of opposite sign.
/// With none in the window (or `frame` outside the sample) it answers
/// `frame` unchanged. Ties go to the earlier frame.
pub fn snap_zero_crossing(data: &SampleData, frame: u64, window_frames: u64) -> u64 {
    let channels = (data.channels as usize).max(1);
    let count = (data.frames.len() / channels) as u64;
    if count == 0 || frame > count {
        return frame;
    }
    let mono = |n: u64| -> f32 {
        let i = n as usize * channels;
        data.frames[i..i + channels].iter().sum::<f32>() / channels as f32
    };
    let crosses = |n: u64| -> bool {
        let x = mono(n);
        if x == 0.0 {
            return true;
        }
        let opposite = |m: u64| {
            let y = mono(m);
            (x > 0.0) != (y > 0.0) && y != 0.0 && x.abs() <= y.abs()
        };
        (n > 0 && opposite(n - 1)) || (n + 1 < count && opposite(n + 1))
    };
    for d in 0..=window_frames {
        if let Some(before) = frame.checked_sub(d) {
            if before < count && crosses(before) {
                return before;
            }
        } else if frame.saturating_add(d) >= count {
            break;
        }
        let after = frame.saturating_add(d);
        if d > 0 && after < count && crosses(after) {
            return after;
        }
    }
    frame
}

/// Where a chop lane's sample was cut from: the source file's path and the
/// exact frames `[start_frame, end_frame)` taken from it. Carried by the
/// lane's [`SampleRef`](crate::model::SampleRef) and saved as
/// [`OriginSnapshot`](crate::state::OriginSnapshot); a sample loaded any
/// other way has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChopOrigin {
    /// The source file, as the chop editor opened it.
    pub source_path: String,
    /// The first frame of the cut, in the source's frames.
    pub start_frame: u64,
    /// One past the last frame of the cut (exclusive).
    pub end_frame: u64,
}

/// The hover text on a chop lane's sample name:
/// `"from Eye.wav, 0:30.120–0:32.090"` — the source's file name and the
/// cut's start and end in the source's time ([`format_time`]). A rate of 0
/// reads as zero seconds, as everywhere.
pub fn format_origin(origin: &ChopOrigin, rate: u32, file_name: &str) -> String {
    let secs = |frame: u64| if rate == 0 { 0.0 } else { frame as f64 / f64::from(rate) };
    format!(
        "from {file_name}, {}\u{2013}{}",
        format_time(secs(origin.start_frame)),
        format_time(secs(origin.end_frame)),
    )
}

/// A new chop lane's name: the source's file stem and one more than the
/// number of lanes already cut from it — `("/songs/Eye.wav", 0)` → `"Eye 1"`.
/// A path with no stem keeps the whole path, as [`source_missing_message`]
/// does; the lane trims the result to its name limit.
pub fn chop_name(source_path: &str, existing: usize) -> String {
    let stem = Path::new(source_path).file_stem().map_or_else(
        || source_path.to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    format!("{stem} {}", existing.saturating_add(1))
}

/// Seconds as `m:ss.mmm`, rounded to the millisecond: `1850.123` →
/// `"30:50.123"`. Negative or NaN reads as zero.
pub fn format_time(secs: f64) -> String {
    let ms = (secs * 1000.0).round().max(0.0);
    let ms = if ms.is_nan() { 0 } else { ms as u64 };
    format!("{}:{:02}.{:03}", ms / 60_000, ms / 1000 % 60, ms % 1000)
}

/// The readout line under the ruler:
/// `start – end · length s · ≈ nearest, off at tempo`, e.g.
/// `"0:30.120 – 0:32.093 · 1.973 s · ≈ 1 bar, 1.4 % short at 120 BPM"`.
/// The nearest [`NoteLength`](crate::time::NoteLength) and how far off it
/// is come from [`time::nearest`] / [`time::format_change`]: "short" when
/// the chop is shorter than that length (it would be stretched), "long"
/// when longer, "exact" when it rounds to 0.0 %. The tempo is clamped as
/// everywhere ([`time::clamp_tempo`]) and shown with one decimal only when
/// it has a fraction. A rate of 0 reads as zero seconds throughout.
pub fn readout(selection: &Selection, rate: u32, tempo: f64) -> String {
    let secs = |frames: u64| time::natural_secs(frames as usize, rate);
    let length = secs(selection.len());
    let tempo = time::clamp_tempo(tempo);
    let nearest = time::nearest(length, tempo);
    let pct = time::change_pct(length, time::target_secs(nearest, 0.0, tempo));
    let change = time::format_change(pct);
    let off = match change.strip_prefix('\u{2212}') {
        Some(magnitude) => format!("{magnitude} long"),
        None => match change.strip_prefix('+') {
            Some(magnitude) => format!("{magnitude} short"),
            None => "exact".to_owned(),
        },
    };
    let bpm = if tempo.fract().abs() < 0.05 || tempo.fract() > 0.95 {
        format!("{tempo:.0}")
    } else {
        format!("{tempo:.1}")
    };
    format!(
        "{} \u{2013} {} \u{b7} {length:.3} s \u{b7} \u{2248} {}, {off} at {bpm} BPM",
        format_time(secs(selection.start)),
        format_time(secs(selection.end)),
        nearest.label(),
    )
}

/// Whether `frames` at `rate` runs past [`MAX_SOURCE_SECONDS`].
pub fn source_too_long(frames: u64, rate: u32) -> bool {
    frames as f64 > MAX_SOURCE_SECONDS * rate as f64
}

/// "source not found: Eye.wav" — the strip's message for a source path
/// that cannot be read.
pub fn source_missing_message(path: &str) -> String {
    let name = Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    format!("{SOURCE_MISSING_PREFIX}{name}")
}

/// An open source: the decoded audio (allocated against the model's
/// collector, so the preview voice may hold it) and its peaks, built on the
/// loader thread. Held only while open and never saved as audio.
#[derive(Clone)]
pub struct Source {
    pub path: String,
    pub data: Shared<SampleData>,
    pub peaks: Arc<Peaks>,
}

impl Source {
    /// The source's length in frames.
    pub fn total(&self) -> u64 {
        self.data.frame_count() as u64
    }
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("path", &self.path)
            .field("rate", &self.data.rate)
            .field("channels", &self.data.channels)
            .field("frames", &self.data.frame_count())
            .finish()
    }
}

/// The chop editor's per-instance state, owned by the model so the loader can
/// write it and the snapshot can save it. Written only through the model's
/// writer API; never part of undo and never published to the audio thread.
#[derive(Clone, Debug)]
pub struct ChopEditor {
    /// The open source, once it has landed.
    pub source: Option<Source>,
    /// The path asked for, kept while loading and when the file is gone.
    pub source_path: Option<String>,
    /// A loader job for `source_path` is in flight.
    pub loading: bool,
    /// Why the source could not be opened, in the user's words.
    pub error: Option<String>,
    pub view: View,
    /// The width the view was last fitted or set at, so a fresh source fits
    /// the strip as it is drawn.
    pub view_width_px: f32,
    pub selection: Option<Selection>,
    /// A selection to apply when the source lands (the lane menu's "open
    /// source in editor", a restored project), frames `[start, end)`.
    pub pending_selection: Option<(u64, u64)>,
    /// A view to apply (clamped) when the source lands — a restored
    /// project's; it wins over framing the pending selection.
    pub pending_view: Option<View>,
    pub snap: bool,
    pub looping: bool,
    pub collapsed: bool,
    pub height_px: f32,
}

impl Default for ChopEditor {
    fn default() -> Self {
        Self {
            source: None,
            source_path: None,
            loading: false,
            error: None,
            view: View::fit(0, DEFAULT_VIEW_WIDTH_PX),
            view_width_px: DEFAULT_VIEW_WIDTH_PX,
            selection: None,
            pending_selection: None,
            pending_view: None,
            snap: true,
            looping: false,
            collapsed: false,
            height_px: STRIP_HEIGHT,
        }
    }
}

impl ChopEditor {
    /// The open source's length in frames (0 with none).
    pub fn total(&self) -> u64 {
        self.source.as_ref().map_or(0, Source::total)
    }
}

impl View {
    /// A view showing `[start, end)` with a margin of a quarter of its
    /// length either side, clamped to the source.
    pub fn around(start: u64, end: u64, total: u64, width_px: f32) -> View {
        let len = end.saturating_sub(start).max(1) as f64;
        let span = len * 1.5;
        let mut view = View { first: 0, frames_per_pixel: span / sane_width(width_px) };
        view.clamp(total, width_px);
        let first = start as f64 - len * 0.25;
        view.first = first.max(0.0) as u64;
        view.clamp(total, width_px);
        view
    }

    /// The same zoom with `frame` in the middle of the width, clamped to
    /// the source (an overview click).
    pub fn centred_on(self, frame: u64, total: u64, width_px: f32) -> View {
        let mut view = self;
        view.clamp(total, width_px);
        let half = sane_width(width_px) * view.frames_per_pixel / 2.0;
        view.first = (frame.min(total) as f64 - half).round().max(0.0) as u64;
        view.clamp(total, width_px);
        view
    }

    /// The same zoom scrolled `delta_frames` (negative = earlier), stopping
    /// at either end of the source (an overview drag).
    pub fn scrolled(self, delta_frames: i64, total: u64, width_px: f32) -> View {
        let mut view = self;
        let magnitude = delta_frames.unsigned_abs();
        view.first = if delta_frames < 0 {
            view.first.saturating_sub(magnitude)
        } else {
            view.first.saturating_add(magnitude)
        };
        view.clamp(total, width_px);
        view
    }

    /// The view clamped to a source of `total` frames drawn `width_px`
    /// wide: zoom 1 frame/px … fit, scroll inside the source.
    pub fn clamped(mut self, total: u64, width_px: f32) -> View {
        self.clamp(total, width_px);
        self
    }
}

/// The ruler's tick spacings, in seconds, finest first.
pub const RULER_STEPS: [f64; 18] = [
    0.001, 0.002, 0.005, 0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 120.0,
    300.0, 600.0,
];

/// The least room, in pixels, between two ruler ticks: a `m:ss.mmm` label
/// fits with a gap.
pub const RULER_MIN_TICK_PX: f64 = 64.0;

/// The ruler's tick spacing in seconds at `secs_per_px`: the finest of
/// [`RULER_STEPS`] at least [`RULER_MIN_TICK_PX`] apart, else the coarsest.
/// Zero, negative, NaN or infinite reads as the coarsest (fewest ticks).
pub fn ruler_step(secs_per_px: f64) -> f64 {
    let coarsest = RULER_STEPS[RULER_STEPS.len() - 1];
    if !(secs_per_px.is_finite() && secs_per_px > 0.0) {
        return coarsest;
    }
    RULER_STEPS
        .iter()
        .copied()
        .find(|step| step / secs_per_px >= RULER_MIN_TICK_PX)
        .unwrap_or(coarsest)
}

/// The `(min, max)` of `data`'s mono mix over frames `[from_frame,
/// to_frame)`, read sample by sample — the main view's waveform when zoomed
/// in past the finest peak level. Clipped to the source; an empty span
/// answers `(0.0, 0.0)`.
pub fn raw_min_max(data: &SampleData, from_frame: u64, to_frame: u64) -> (f32, f32) {
    let channels = (data.channels as usize).max(1);
    let count = data.frames.len() / channels;
    let from = from_frame.min(count as u64) as usize;
    let to = to_frame.min(count as u64) as usize;
    if to <= from {
        return (0.0, 0.0);
    }
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for frame in data.frames[from * channels..to * channels].chunks_exact(channels) {
        let mono = frame.iter().sum::<f32>() / channels as f32;
        lo = lo.min(mono);
        hi = hi.max(mono);
    }
    (lo, hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(frames: Vec<f32>) -> SampleData {
        SampleData { rate: 48_000, channels: 1, frames: frames.into_boxed_slice() }
    }

    /// A ramp where frame `n` is `n as f32`.
    fn ramp(len: usize) -> SampleData {
        mono((0..len).map(|n| n as f32).collect())
    }

    #[test]
    fn cut_keeps_exact_frames_with_linear_edge_fades() {
        let data = mono(vec![1.0; 48_000]);
        let chop = cut(&data, 1000, 5800);
        assert_eq!((chop.rate, chop.channels, chop.frame_count()), (48_000, 1, 4800));
        // 1 ms at 48 kHz is 48 frames: 0 at the first and last frame.
        assert_eq!(chop.frames[0], 0.0);
        assert_eq!(chop.frames[4799], 0.0);
        assert_eq!(chop.frames[24], 0.5);
        assert_eq!(chop.frames[48], 1.0);
        assert!(chop.frames[48..4752].iter().all(|&x| x == 1.0), "middle untouched");
        // Exact frames: a ramp cut keeps its values in the middle.
        let ramp_chop = cut(&ramp(10_000), 100, 300);
        assert_eq!(ramp_chop.frames[100], 200.0);
        // A short chop's fade is at most half of it.
        assert_eq!(edge_fade_frames(48_000, 10), 5);
        let short = cut(&data, 0, 10);
        assert_eq!(short.frames[0], 0.0);
        assert_eq!(short.frames[9], 0.0);
        assert!(short.frames.iter().all(|&x| x < 1.0 + f32::EPSILON));
    }

    #[test]
    fn cut_is_exactly_start_to_end_with_rate_and_channels_kept() {
        // Stereo at 44.1 kHz: left is the frame index, right its negation.
        let frames = (0..20_000).flat_map(|n| [n as f32, -(n as f32)]).collect::<Vec<_>>();
        let data = SampleData { rate: 44_100, channels: 2, frames: frames.into_boxed_slice() };
        let (start, end) = (5_000u64, 9_000u64);
        let chop = cut(&data, start, end);
        assert_eq!((chop.rate, chop.channels), (44_100, 2));
        assert_eq!(chop.frame_count() as u64, end - start, "[start, end), end excluded");
        // 1 ms at 44.1 kHz rounds to 44 frames.
        let fade = edge_fade_frames(44_100, end - start);
        assert_eq!(fade, 44);
        for i in 0..(end - start) as usize {
            let source = (start as usize + i) as f32;
            let gain = edge_gain(i as f64, end - start, fade);
            assert_eq!(chop.frames[2 * i], source * gain, "left frame {i}");
            assert_eq!(chop.frames[2 * i + 1], -source * gain, "right frame {i}");
        }
        // The fade is linear: gain i/fade on the way in, untouched after it.
        assert_eq!(chop.frames[0], 0.0);
        assert_eq!(chop.frames[2 * 22], 5_022.0 * 0.5);
        assert_eq!(chop.frames[2 * 44], 5_044.0);
        assert_eq!(chop.frames[2 * 2_000], 7_000.0);
        assert_eq!(chop.frames[2 * 3_999], 0.0);
        assert_eq!(chop.frames[2 * 3_999 + 1], 0.0);
    }

    #[test]
    fn cut_fade_is_capped_at_half_a_short_selection() {
        let data = mono(vec![1.0; 1_000]);
        // 8 frames: the 48-frame fade is capped at 4, a symmetric triangle.
        let short = cut(&data, 100, 108);
        assert_eq!(short.frame_count(), 8);
        assert_eq!(edge_fade_frames(48_000, 8), 4);
        let want = [0.0, 0.25, 0.5, 0.75, 0.75, 0.5, 0.25, 0.0];
        assert_eq!(&short.frames[..], &want[..]);
        // Edges past the end clamp into the sample; an empty span is one
        // silent frame (a SampleData is never empty).
        assert_eq!(cut(&data, 900, 5_000).frame_count(), 100);
        let empty = cut(&data, 500, 500);
        assert_eq!((empty.frame_count(), empty.frames[0]), (1, 0.0));
    }

    #[test]
    fn edge_gain_is_zero_outside_and_nan_safe() {
        assert_eq!(edge_gain(-1.0, 100, 10), 0.0);
        assert_eq!(edge_gain(100.0, 100, 10), 0.0);
        assert_eq!(edge_gain(f64::NAN, 100, 10), 0.0);
        assert_eq!(edge_gain(50.0, 100, 0), 1.0);
        assert_eq!(edge_gain(5.0, 100, 10), 0.5);
    }

    #[test]
    fn nudge_is_ten_or_one_milliseconds_at_the_rate() {
        assert_eq!(nudge_frames(NUDGE_COARSE_MS, 48_000), 480);
        assert_eq!(nudge_frames(NUDGE_FINE_MS, 48_000), 48);
        assert_eq!(nudge_frames(NUDGE_FINE_MS, 44_100), 44);
        assert_eq!(nudge_frames(NUDGE_FINE_MS, 1), 1, "never zero");
    }

    #[test]
    fn level_counts_for_a_known_length() {
        // 10_000 frames: ceil(10000/64) = 157, ceil(10000/512) = 20,
        // ceil(10000/4096) = 3.
        let peaks = Peaks::build(&ramp(10_000));
        let counts: Vec<_> = peaks.levels().iter().map(Level::len).collect();
        assert_eq!(counts, [157, 20, 3]);
        let sizes: Vec<_> = peaks.levels().iter().map(Level::frames_per_bucket).collect();
        assert_eq!(sizes, PEAK_LEVELS.to_vec());

        // An exact multiple has no partial bucket; one frame has one bucket.
        let exact = Peaks::build(&ramp(8192));
        let counts: Vec<_> = exact.levels().iter().map(Level::len).collect();
        assert_eq!(counts, [128, 16, 2]);
        let one = Peaks::build(&ramp(1));
        assert!(one.levels().iter().all(|l| l.len() == 1 && l.bucket(0) == Some((0.0, 0.0))));
    }

    #[test]
    fn ramp_min_max_per_bucket() {
        let len = 10_000;
        let peaks = Peaks::build(&ramp(len));
        for level in peaks.levels() {
            let fpb = level.frames_per_bucket();
            for i in 0..level.len() {
                let lo = (i * fpb) as f32;
                let hi = ((i + 1) * fpb).min(len) as f32 - 1.0;
                assert_eq!(level.bucket(i), Some((lo, hi)), "fpb {fpb} bucket {i}");
            }
            assert_eq!(level.bucket(level.len()), None);
        }
    }

    #[test]
    fn stereo_peaks_use_the_mono_mix() {
        // Left +1, right −1 cancels; left 1, right 0.5 averages to 0.75.
        let mut frames = [1.0, -1.0].repeat(64);
        frames.extend([1.0, 0.5].repeat(64));
        let data = SampleData { rate: 44_100, channels: 2, frames: frames.into_boxed_slice() };
        let peaks = Peaks::build(&data);
        assert_eq!(peaks.levels()[0].bucket(0), Some((0.0, 0.0)));
        assert_eq!(peaks.levels()[0].bucket(1), Some((0.75, 0.75)));
        assert_eq!(peaks.levels()[1].bucket(0), Some((0.0, 0.75)));
    }

    #[test]
    fn level_for_picks_the_coarsest_bucket_not_wider_than_a_pixel() {
        let peaks = Peaks::build(&ramp(100_000));
        let fpb = |fpp: f64| peaks.level_for(fpp).frames_per_bucket();
        assert_eq!(fpb(1.0), 64, "zoomed in past the finest → finest");
        assert_eq!(fpb(63.9), 64);
        assert_eq!(fpb(64.0), 64);
        assert_eq!(fpb(511.9), 64);
        assert_eq!(fpb(512.0), 512);
        assert_eq!(fpb(4095.0), 512);
        assert_eq!(fpb(4096.0), 4096);
        assert_eq!(fpb(1e9), 4096);
        assert_eq!(fpb(f64::NAN), 64);
        assert_eq!(fpb(-5.0), 64);
    }

    #[test]
    fn min_max_spans_touched_buckets() {
        let peaks = Peaks::build(&ramp(10_000));
        let fine = &peaks.levels()[0];
        // Inside one bucket: the whole bucket counts.
        assert_eq!(peaks.min_max(fine, 10, 20), (0.0, 63.0));
        // Exactly buckets 1..3.
        assert_eq!(peaks.min_max(fine, 64, 192), (64.0, 191.0));
        // Straddling a boundary touches both.
        assert_eq!(peaks.min_max(fine, 63, 65), (0.0, 127.0));
        // Running past the end clamps to the last bucket.
        assert_eq!(peaks.min_max(fine, 9_990, 50_000), (9_984.0, 9_999.0));
        // The coarsest level over the whole source.
        assert_eq!(peaks.min_max(&peaks.levels()[2], 0, 10_000), (0.0, 9_999.0));
        // Empty, reversed or wholly past the end → silence.
        assert_eq!(peaks.min_max(fine, 100, 100), (0.0, 0.0));
        assert_eq!(peaks.min_max(fine, 200, 100), (0.0, 0.0));
        assert_eq!(peaks.min_max(fine, 20_000, 30_000), (0.0, 0.0));
    }

    // --- View -------------------------------------------------------------

    #[test]
    fn fit_shows_everything_and_never_closer_than_a_frame_per_pixel() {
        assert_eq!(View::fit(48_000, 800.0), View { first: 0, frames_per_pixel: 60.0 });
        assert_eq!(View::fit(100, 800.0).frames_per_pixel, 1.0);
        assert_eq!(View::fit(1000, 0.0).frames_per_pixel, 1000.0);
        assert_eq!(View::fit(1000, f32::NAN).frames_per_pixel, 1000.0);
    }

    #[test]
    fn zoom_at_keeps_the_pointer_frame_fixed() {
        let (total, width) = (480_000, 800.0);
        let mut view = View::fit(total, width);
        let pointer = 300.0;
        let under = view.frame_at(pointer);
        for factor in [0.5, 0.5, 0.25, 0.8, 1.5] {
            view.zoom_at(pointer, factor, total, width);
            let now = view.frame_at(pointer);
            assert!(now.abs_diff(under) <= 1, "factor {factor}: {now} vs {under}");
        }
        assert!(view.frames_per_pixel < 600.0);
        assert!(view.first > 0);
    }

    #[test]
    fn zoom_clamps_to_one_frame_per_pixel_and_to_fit() {
        let (total, width) = (480_000, 800.0);
        let mut view = View::fit(total, width);
        view.zoom_at(400.0, 1e-9, total, width);
        assert_eq!(view.frames_per_pixel, 1.0);
        // Still centred near the frame that was under the pointer.
        assert!(view.frame_at(400.0).abs_diff(240_000) <= 1);
        view.zoom_at(400.0, 1e9, total, width);
        assert_eq!(view, View::fit(total, width));
        // Nonsense factors change nothing.
        let before = view;
        for factor in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            view.zoom_at(400.0, factor, total, width);
            assert_eq!(view, before);
        }
    }

    #[test]
    fn zoom_near_the_end_stays_inside_the_source() {
        let (total, width) = (480_000, 800.0);
        let mut view = View { first: 0, frames_per_pixel: 10.0 };
        view.pan(1e9, total, width);
        assert_eq!(view.first, total - 8000);
        view.zoom_at(800.0, 2.0, total, width);
        assert_eq!(view.frames_per_pixel, 20.0);
        assert_eq!(view.first, total - 16_000, "clamped: the view ends at the source's end");
    }

    #[test]
    fn pan_clamps_at_both_ends() {
        let (total, width) = (100_000, 1000.0);
        let mut view = View { first: 5000, frames_per_pixel: 10.0 };
        view.pan(100.0, total, width);
        assert_eq!(view.first, 6000);
        view.pan(-1e6, total, width);
        assert_eq!(view.first, 0);
        view.pan(1e6, total, width);
        assert_eq!(view.first, 90_000);
        view.pan(f32::NAN, total, width);
        assert_eq!(view.first, 90_000);
        // At fit there is nowhere to pan.
        let mut fit = View::fit(total, width);
        fit.pan(500.0, total, width);
        assert_eq!(fit.first, 0);
    }

    #[test]
    fn frame_at_and_px_of_invert() {
        let view = View { first: 1000, frames_per_pixel: 4.0 };
        assert_eq!(view.frame_at(0.0), 1000);
        assert_eq!(view.frame_at(10.0), 1040);
        assert_eq!(view.frame_at(-5.0), 1000);
        assert_eq!(view.px_of(1040), 10.0);
        assert_eq!(view.px_of(996), -1.0);
    }

    // --- Selection --------------------------------------------------------

    fn sel(start: u64, end: u64) -> Selection {
        Selection { start, end, active: Edge::Start }
    }

    #[test]
    fn clamp_orders_bounds_and_keeps_the_minimum_length() {
        let clamped = |mut s: Selection, total| { s.clamp(total); (s.start, s.end) };
        assert_eq!(clamped(sel(10, 20), 100), (10, 20));
        assert_eq!(clamped(sel(20, 10), 100), (10, 20), "reversed → ordered");
        assert_eq!(clamped(sel(50, 500), 100), (50, 100), "end past the source");
        assert_eq!(clamped(sel(300, 500), 100), (98, 100), "wholly past the end");
        assert_eq!(clamped(sel(10, 10), 100), (10, 12), "empty grows the end");
        assert_eq!(clamped(sel(99, 100), 100), (98, 100), "at the end grows the start");
        assert_eq!(clamped(sel(0, 5), 1), (0, 1), "a one-frame source selects it all");
        assert_eq!(clamped(sel(0, 0), 0), (0, 0));
    }

    #[test]
    fn set_edge_moves_one_edge_without_crossing() {
        let mut s = sel(100, 200);
        s.set_edge(Edge::End, 300, 1000);
        assert_eq!((s.start, s.end, s.active), (100, 300, Edge::End));
        s.set_edge(Edge::Start, 50, 1000);
        assert_eq!((s.start, s.end, s.active), (50, 300, Edge::Start));
        s.set_edge(Edge::Start, 900, 1000);
        assert_eq!((s.start, s.end), (298, 300), "start stops short of end");
        s.set_edge(Edge::End, 0, 1000);
        assert_eq!((s.start, s.end), (298, 300), "end stops short of start");
        s.set_edge(Edge::End, 5000, 1000);
        assert_eq!((s.start, s.end), (298, 1000), "end clamps to the source");
        let mut edge = sel(0, 2);
        edge.set_edge(Edge::Start, 1, 1000);
        assert_eq!((edge.start, edge.end), (0, 2));
    }

    #[test]
    fn shift_keeps_length_and_stops_at_the_ends() {
        let mut s = sel(100, 200);
        s.shift(50, 1000);
        assert_eq!((s.start, s.end), (150, 250));
        s.shift(-1000, 1000);
        assert_eq!((s.start, s.end), (0, 100));
        s.shift(i64::MAX, 1000);
        assert_eq!((s.start, s.end), (900, 1000));
        s.shift(i64::MIN, 1000);
        assert_eq!((s.start, s.end), (0, 100));
        assert_eq!(s.len(), 100);
    }

    #[test]
    fn advance_steps_by_its_own_length_until_the_end() {
        let mut s = sel(0, 300);
        assert!(s.advance(1000));
        assert_eq!((s.start, s.end), (300, 600));
        assert!(s.advance(900), "landing exactly on the end is fine");
        assert_eq!((s.start, s.end), (600, 900));
        assert!(!s.advance(1000), "would pass the end");
        assert_eq!((s.start, s.end), (600, 900), "unchanged when refused");
        let mut empty = sel(5, 5);
        assert!(!empty.advance(1000));
        assert!(empty.is_empty());
        let mut huge = sel(u64::MAX - 10, u64::MAX);
        assert!(!huge.advance(u64::MAX));
    }

    // --- snap -------------------------------------------------------------

    /// A sine of `period` frames starting at `phase` radians.
    fn sine(len: usize, period: f32, phase: f32) -> SampleData {
        let w = std::f32::consts::TAU / period;
        mono((0..len).map(|n| (w * n as f32 + phase).sin()).collect())
    }

    #[test]
    fn snap_lands_on_the_sine_crossing() {
        // Period 100 frames, phase shifted by 0.3 frame: crossings sit
        // between frames, near 49.7/50 and 99.7/100.
        let data = sine(1000, 100.0, 0.3 * std::f32::consts::TAU / 100.0);
        let snapped = snap_zero_crossing(&data, 45, 10);
        assert_eq!(snapped, 50, "the quieter side of the 49.7 crossing");
        assert_eq!(snap_zero_crossing(&data, 105, 10), 100);
        // Exact zero at frame 0 of an unshifted sine.
        let exact = sine(1000, 100.0, 0.0);
        assert_eq!(snap_zero_crossing(&exact, 3, 5), 0);
        // Sign really changes across the answer.
        let m = |n: usize| data.frames[n];
        assert!(m(49) > 0.0 && m(50) < 0.0 && m(50).abs() < m(49).abs());
    }

    #[test]
    fn snap_returns_the_input_without_a_crossing_in_the_window() {
        let data = sine(1000, 100.0, 0.3 * std::f32::consts::TAU / 100.0);
        assert_eq!(snap_zero_crossing(&data, 25, 10), 25);
        assert_eq!(snap_zero_crossing(&data, 25, 0), 25);
        let dc = mono(vec![0.5; 100]);
        assert_eq!(snap_zero_crossing(&dc, 40, 100), 40);
        // Past the end of the sample: unchanged.
        assert_eq!(snap_zero_crossing(&data, 5000, 10), 5000);
        // The exclusive end (frame == len) searches back inside: the last
        // crossing (≈ 949.7/950) is out of a 10-frame window, in a 60-frame one.
        assert_eq!(snap_zero_crossing(&data, 1000, 10), 1000);
        assert_eq!(snap_zero_crossing(&data, 1000, 60), 950);
    }

    #[test]
    fn snap_uses_the_stereo_mono_mix() {
        // Left alone would cross at 1/2; the mix (L+R)/2 is
        // [1, 1, 1, 0.5, −1, −1]: it crosses between 3 and 4, 3 the quieter.
        let l = [1.0, 1.0, -1.0, -1.0, -1.0, -1.0f32];
        let r = [1.0, 1.0, 3.0, 2.0, -1.0, -1.0f32];
        let frames: Vec<f32> = l.iter().zip(r).flat_map(|(&a, b)| [a, b]).collect();
        let data = SampleData { rate: 48_000, channels: 2, frames: frames.into_boxed_slice() };
        assert_eq!(snap_zero_crossing(&data, 1, 5), 3);
    }

    // --- chop_name --------------------------------------------------------

    #[test]
    fn chop_name_is_the_stem_and_a_count() {
        assert_eq!(chop_name("/songs/Eye.wav", 0), "Eye 1");
        assert_eq!(chop_name("/songs/Eye.wav", 1), "Eye 2");
        assert_eq!(chop_name("/songs/Eye of the.tiger.flac", 9), "Eye of the.tiger 10");
        assert_eq!(chop_name("noext", 0), "noext 1");
        assert_eq!(chop_name("/", 0), "/ 1", "no stem keeps the whole path");
    }

    // --- format_time ------------------------------------------------------

    #[test]
    fn format_time_is_minutes_seconds_millis() {
        assert_eq!(format_time(1850.123), "30:50.123");
        assert_eq!(format_time(0.0), "0:00.000");
        assert_eq!(format_time(30.12), "0:30.120");
        assert_eq!(format_time(59.9996), "1:00.000", "rounds up across a minute");
        assert_eq!(format_time(-3.0), "0:00.000");
        assert_eq!(format_time(f64::NAN), "0:00.000");
    }

    #[test]
    fn format_origin_names_the_file_and_the_cut() {
        let origin = ChopOrigin {
            source_path: String::from("/samples/Eye.wav"),
            start_frame: 1_445_760, // 30.120 s at 48 kHz
            end_frame: 1_540_320,   // 32.090 s
        };
        assert_eq!(
            format_origin(&origin, 48_000, "Eye.wav"),
            "from Eye.wav, 0:30.120\u{2013}0:32.090"
        );
        assert_eq!(format_origin(&origin, 0, "Eye.wav"), "from Eye.wav, 0:00.000\u{2013}0:00.000");
    }

    /// The worked readout example. Its "1.4 % short at 122 BPM" does not
    /// follow from a 1.973 s chop at 122 BPM (a bar there is 1.967 s, so the
    /// chop is 0.3 % long — see `time.rs`'s `brief_example_nearest_bar_and_
    /// change`); at 120 BPM the same chop is 1.4 % short of the 2 s bar,
    /// which is the example's string with that tempo.
    #[test]
    fn readout_pins_the_brief_example() {
        // 0:30.120 – 0:32.093 at 48 kHz: exactly 1.973 s.
        let selection = Selection { start: 1_445_760, end: 1_540_464, active: Edge::End };
        assert_eq!(
            readout(&selection, 48_000, 120.0),
            "0:30.120 \u{2013} 0:32.093 \u{b7} 1.973 s \u{b7} \u{2248} 1 bar, 1.4 % short \
             at 120 BPM"
        );
        assert_eq!(
            readout(&selection, 48_000, 122.0),
            "0:30.120 \u{2013} 0:32.093 \u{b7} 1.973 s \u{b7} \u{2248} 1 bar, 0.3 % long \
             at 122 BPM"
        );
        // A chop of exactly 1 bar at 120, a fractional tempo.
        let bar = Selection { start: 0, end: 96_000, active: Edge::Start };
        assert_eq!(
            readout(&bar, 48_000, 120.0),
            "0:00.000 \u{2013} 0:02.000 \u{b7} 2.000 s \u{b7} \u{2248} 1 bar, exact at 120 BPM"
        );
        assert!(readout(&bar, 48_000, 92.5).ends_with(" at 92.5 BPM"));
        // Hostile tempo and rate never panic and stay in range.
        assert!(readout(&bar, 48_000, f64::NAN).ends_with(" at 120 BPM"));
        assert!(readout(&bar, 48_000, 0.0).ends_with(" at 20 BPM"));
        assert!(readout(&bar, 48_000, 1e9).ends_with(" at 999 BPM"));
        assert!(readout(&bar, 0, 120.0).starts_with("0:00.000 \u{2013} 0:00.000"));
    }

    #[test]
    fn source_cap_is_fifteen_minutes() {
        assert_eq!(MAX_SOURCE_SECONDS, 900.0);
        assert!(!source_too_long(900 * 48_000, 48_000));
        assert!(source_too_long(900 * 48_000 + 1, 48_000));
    }

    #[test]
    fn missing_source_message_names_the_file() {
        assert_eq!(source_missing_message("/songs/Eye.wav"), "source not found: Eye.wav");
        assert_eq!(source_missing_message(""), "source not found: ");
    }

    #[test]
    fn strip_height_clamps() {
        assert_eq!(clamp_strip_height(180.0), 180.0);
        assert_eq!(clamp_strip_height(-5.0), STRIP_MIN_HEIGHT);
        assert_eq!(clamp_strip_height(1e9), STRIP_MAX_HEIGHT);
        assert_eq!(clamp_strip_height(f32::NAN), STRIP_HEIGHT);
    }

    /// `View::around` shows the region with a margin, inside the source.
    #[test]
    fn view_around_frames_the_region() {
        let view = View::around(4000, 6000, 100_000, 300.0);
        assert_eq!(view.frames_per_pixel, 10.0);
        assert_eq!(view.first, 3500);
        // Near the start the margin is cut off rather than going negative.
        assert_eq!(View::around(0, 100, 100_000, 300.0).first, 0);
        // A region longer than the source is the fit.
        assert_eq!(View::around(0, 1_000_000, 1000, 100.0), View::fit(1000, 100.0));
    }

    #[test]
    fn ruler_step_at_five_zoom_levels() {
        let rate = 48_000.0;
        // All the way in: 1 frame/px at 48 kHz, 64 px ≈ 1.3 ms → 2 ms.
        assert_eq!(ruler_step(1.0 / rate), 0.002);
        // 64 frames/px: 64 px ≈ 85 ms → 100 ms.
        assert_eq!(ruler_step(64.0 / rate), 0.1);
        // 1024 frames/px: 64 px ≈ 1.37 s → 2 s.
        assert_eq!(ruler_step(1024.0 / rate), 2.0);
        // A 3 min song across 560 px: 64 px ≈ 20.6 s → 30 s.
        assert_eq!(ruler_step(180.0 / 560.0), 30.0);
        // A 15 min source across 300 px: 64 px = 192 s → 300 s.
        assert_eq!(ruler_step(900.0 / 300.0), 300.0);
        // Exactly at the threshold the finer step is kept.
        assert_eq!(ruler_step(0.01 / RULER_MIN_TICK_PX), 0.01);
        // Further out than every step: the coarsest.
        assert_eq!(ruler_step(1000.0), 600.0);
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(ruler_step(bad), 600.0);
        }
    }

    #[test]
    fn raw_min_max_reads_the_mono_mix() {
        let data = SampleData {
            rate: 48_000,
            channels: 2,
            frames: vec![1.0, 0.0, -1.0, -1.0, 0.5, 0.5].into_boxed_slice(),
        };
        assert_eq!(raw_min_max(&data, 0, 3), (-1.0, 0.5));
        assert_eq!(raw_min_max(&data, 0, 1), (0.5, 0.5));
        assert_eq!(raw_min_max(&data, 2, 99), (0.5, 0.5));
        assert_eq!(raw_min_max(&data, 3, 9), (0.0, 0.0));
        assert_eq!(raw_min_max(&data, 2, 1), (0.0, 0.0));
    }

    #[test]
    fn nearer_edge_is_the_side_of_the_middle_the_frame_is_on() {
        let selection = Selection { start: 1000, end: 2000, active: Edge::End };
        assert_eq!(selection.nearer_edge(1001), Edge::Start);
        assert_eq!(selection.nearer_edge(1499), Edge::Start);
        assert_eq!(selection.nearer_edge(1500), Edge::Start, "the middle goes to the start");
        assert_eq!(selection.nearer_edge(1501), Edge::End);
        assert_eq!(selection.nearer_edge(1999), Edge::End);
        // Outside it still answers by distance.
        assert_eq!(selection.nearer_edge(0), Edge::Start);
        assert_eq!(selection.nearer_edge(u64::MAX), Edge::End);
    }

    #[test]
    fn press_zone_by_where_the_press_lands() {
        // 10 frames a pixel: the selection spans px 100…200.
        let view = View { first: 0, frames_per_pixel: 10.0 };
        let selection = Selection { start: 1000, end: 2000, active: Edge::Start };
        let zone = |px: f32| press_zone(Some(&selection), &view, px);
        assert_eq!(EDGE_GRAB_PX, 6.0);
        assert_eq!(zone(100.0), PressZone::Edge(Edge::Start));
        assert_eq!(zone(94.0), PressZone::Edge(Edge::Start));
        assert_eq!(zone(106.0), PressZone::Edge(Edge::Start));
        assert_eq!(zone(206.0), PressZone::Edge(Edge::End));
        assert_eq!(zone(194.0), PressZone::Edge(Edge::End));
        assert_eq!(zone(150.0), PressZone::Inside);
        assert_eq!(zone(107.0), PressZone::Inside);
        assert_eq!(zone(93.0), PressZone::Outside);
        assert_eq!(zone(207.0), PressZone::Outside);
        assert_eq!(zone(-50.0), PressZone::Outside);
        assert_eq!(zone(f32::NAN), PressZone::Outside);
        assert_eq!(press_zone(None, &view, 150.0), PressZone::Outside);

        // A sliver: both edges in reach, the nearer wins, the end on a tie.
        let sliver = Selection { start: 1000, end: 1040, active: Edge::Start };
        let zone = |px: f32| press_zone(Some(&sliver), &view, px);
        assert_eq!(zone(99.0), PressZone::Edge(Edge::Start));
        assert_eq!(zone(102.0), PressZone::Edge(Edge::End));
        assert_eq!(zone(105.0), PressZone::Edge(Edge::End));

        // Scrolled: the zones follow the view.
        let view = View { first: 500, frames_per_pixel: 10.0 };
        assert_eq!(press_zone(Some(&selection), &view, 50.0), PressZone::Edge(Edge::Start));
        assert_eq!(press_zone(Some(&selection), &view, 100.0), PressZone::Inside);
    }

    #[test]
    fn drag_selection_goes_either_way_and_clamps() {
        let s = drag_selection(100, 400, 1000);
        assert_eq!(s, Selection { start: 100, end: 400, active: Edge::End });
        let s = drag_selection(400, 100, 1000);
        assert_eq!(s, Selection { start: 100, end: 400, active: Edge::Start });
        let s = drag_selection(100, 100, 1000);
        assert_eq!((s.start, s.end), (100, 100 + MIN_SELECTION_FRAMES));
        let s = drag_selection(900, 5000, 1000);
        assert_eq!((s.start, s.end), (900, 1000));
        assert_eq!(snap_window_frames(48_000), 96);
        assert_eq!(snap_window_frames(44_100), 88);
    }

    #[test]
    fn overview_maps_pixels_to_frames() {
        // 1000 px showing 100_000 frames: 100 frames a pixel.
        assert_eq!(overview_frame(0.0, 1000.0, 100_000), 0);
        assert_eq!(overview_frame(250.5, 1000.0, 100_000), 25_050);
        assert_eq!(overview_frame(-30.0, 1000.0, 100_000), 0);
        assert_eq!(overview_frame(5000.0, 1000.0, 100_000), 100_000);
        assert_eq!(overview_frame(f32::NAN, 1000.0, 100_000), 0);
        assert_eq!(overview_frame(10.0, f32::NAN, 100_000), 100_000, "NaN width: 1 px");
        assert_eq!(overview_frame(10.0, 1000.0, 0), 0);
        assert_eq!(overview_frames(12.0, 1000.0, 100_000), 1200);
        assert_eq!(overview_frames(-12.0, 1000.0, 100_000), -1200);
        assert_eq!(overview_frames(f32::INFINITY, 1000.0, 100_000), 0);
        assert_eq!(overview_frames(f32::NAN, 1000.0, 100_000), 0);
    }

    #[test]
    fn overview_band_hit_includes_the_grab_margin() {
        // The band spans px 100…200.
        let selection = Selection { start: 10_000, end: 20_000, active: Edge::End };
        let hit = |px: f32| on_overview_band(&selection, px, 1000.0, 100_000);
        assert_eq!(OVERVIEW_GRAB_PX, 2.0);
        assert!(hit(150.0) && hit(100.0) && hit(200.0) && hit(98.0) && hit(202.0));
        assert!(!hit(97.0) && !hit(203.0) && !hit(f32::NAN));
        assert!(!on_overview_band(&selection, 150.0, 1000.0, 0));
        // A sliver is drawn a pixel wide and grabbed with the margin.
        let sliver = Selection { start: 10_000, end: 10_010, active: Edge::End };
        assert!(on_overview_band(&sliver, 102.5, 1000.0, 100_000));
        assert!(!on_overview_band(&sliver, 103.5, 1000.0, 100_000));
    }

    #[test]
    fn centred_on_and_scrolled_keep_the_zoom_and_clamp() {
        let view = View { first: 0, frames_per_pixel: 10.0 };
        // 100 px wide shows 1000 frames.
        let c = view.centred_on(50_000, 100_000, 100.0);
        assert_eq!(c, View { first: 49_500, frames_per_pixel: 10.0 });
        assert_eq!(view.centred_on(200, 100_000, 100.0).first, 0);
        assert_eq!(view.centred_on(99_900, 100_000, 100.0).first, 99_000);
        assert_eq!(view.centred_on(u64::MAX, 100_000, 100.0).first, 99_000);
        let fit = View::fit(100_000, 100.0);
        assert_eq!(fit.centred_on(50_000, 100_000, 100.0), fit, "a fitted view stays put");

        let s = c.scrolled(1234, 100_000, 100.0);
        assert_eq!(s, View { first: 50_734, frames_per_pixel: 10.0 });
        assert_eq!(c.scrolled(-60_000, 100_000, 100.0).first, 0);
        assert_eq!(c.scrolled(i64::MAX, 100_000, 100.0).first, 99_000);
        assert_eq!(c.scrolled(i64::MIN, 100_000, 100.0).first, 0);
    }
}
