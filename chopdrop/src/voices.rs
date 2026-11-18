//! The voice pool: what it takes to turn a decoded sample into moving audio.
//!
//! A [`Voice`] is one sounding note: a [`SampleData`] it holds open, a
//! fractional read [`position`](Voice::position) into it and the
//! [`step`](Voice::step) that position moves by per output frame.
//! [`VoicePool`] is the fixed array of them — [`MAX_VOICES`] slots, allocated
//! once with the plugin and never grown.
//!
//! This module is the bottom of the audio path and obeys the real-time rule
//! absolutely: starting, rendering and ending a voice touch nothing but the
//! pool's own array and the sample's read-only frames. No allocation, no lock,
//! no logging, no branch into the host. It also obeys the purity rule — no
//! nice-plug type appears here, so every number below is unit-tested without
//! the framework.
//!
//! # What is here
//!
//! The pool, the pitch math, the interpolation, the gain and pan every voice
//! is mixed at, how a voice stops — with its sample ([`LaneMode::OneShot`]),
//! on a note-off ([`LaneMode::Gate`]), because a newer voice took its group
//! ([`VoicePool::choke_group`]) or because a newer voice took its slot
//! (§"Stealing parks the voice it takes" below) — the one ramp
//! ([`FadeState`]) all four of those endings share, the record every one of
//! them leaves for the host ([`TerminatedVoice`]), and which sounding voices
//! a per-note expression event is about ([`VoicePool::poly_tuning`],
//! [`VoicePool::poly_pan`], [`VoicePool::poly_volume`]). v2 adds the lane's
//! time settings to the step: a speed factor and the lane's pitch, which
//! adds to the tuning ([`Voice::set_time`], [`time_step_for`]).
//!
//! The "bitstep Sample pad" switch is deliberately not here. It needs no
//! field and no method of its own — it is an offset its caller folds into
//! the tuning it hands a note-on ([`VoiceStart::semis`]) and
//! [`VoicePool::poly_tuning`] ([`crate::engine::effective_semis`]).
//!
//! # Stealing parks the voice it takes
//!
//! A steal could take one of two shapes, and the choice is written down
//! here. It is the first of them: [`STEAL_TAILS`] reserve slots that hold
//! nothing but stolen voices finishing their ramp.
//!
//! The 65th simultaneous note needs a slot *now*, and the voice it takes
//! that slot from needs [`FADE_MS`] more of rendering before it can stop
//! without a click — the two cannot share one slot, so the stolen voice
//! moves out of the pool's addressable region into a tail slot and fades
//! there, in the same mix, while the new voice plays from the slot it
//! vacated. A tail is audio and nothing else: note-offs, chokes and
//! expression events never reach it (they walk [`VoicePool::voices_mut`],
//! which is the live region alone), it cannot itself be stolen, and the host
//! has already been told it is gone — a steal records its
//! [`TerminatedVoice`] at once, because the slot it was using is in use
//! again at once.
//!
//! The other shape — hard-switching after rendering the fade
//! tail into the current block — was not taken: a steal that lands in the
//! last frames of a block has no room left to render a tail into, and a fade
//! that is only sometimes rendered is not a fade.
//!
//! # "Unity gain" is a pan position
//!
//! The pan law is constant **power**: centre is `cos(π/4)` ≈ 0.707 a side,
//! −3 dB, so that sliding a voice across the image never changes how loud
//! it is. A centred voice at velocity 1.0,
//! 0 dB, master 1.0 therefore does *not* multiply by one. Where the mix does
//! multiply by exactly one is a hard pan: [`pan_gains`] returns `(1.0, 0.0)`
//! at −1.0 bit for bit, which is where the exactness contract below is
//! tested from.
//!
//! # Exactness is a contract, not a coincidence
//!
//! An untuned voice playing a file at its own rate must reproduce that file
//! sample for sample — at unity gain, which §"Unity gain" above places at a
//! hard pan — and +12 semitones must read
//! exactly every second frame. Both fall out of two deliberate choices:
//! the step is `1.0` and `2.0` *exactly* (`2f64.powf(0.0)` and
//! `2f64.powf(1.0)` are exact, and adding 1.0 or 2.0 to an f64 under 2^53
//! is exact), and the interpolation returns the source frame untouched when
//! the position has no fractional part. Tests pin both, because a cheaper
//! interpolator or a position kept in f32 would quietly break them.

use basedrop::Shared;

use std::ops::Range;

use crate::chop;
use crate::engine::Outputs;
use crate::model::clamp_output;
use crate::sample::SampleData;
use crate::time::{SPEED_MAX, SPEED_MIN, natural_secs};

/// How many voices can sound at once: 64.
///
/// The pool is this big from the moment the plugin is instantiated and never
/// changes size — a fixed array is the only shape the real-time rule allows. 64
/// is far more than a drum kit needs; what matters is that the 65th note-on has
/// a defined answer — stealing the oldest voice ([`VoicePool::start`]) — rather
/// than an allocation.
///
/// It counts among the model's limits, because that is where the rest of
/// them live; it is defined here, where the pool it sizes is, and `model.rs`
/// re-exports this one rather than declaring a second.
pub const MAX_VOICES: usize = 64;

/// How many stolen voices can be finishing their fade at once: 16.
///
/// The reserve slots of the module docs' §"Stealing parks the voice it
/// takes" — a region past the pool's [`MAX_VOICES`] addressable slots
/// holding only voices on their way out, so that a steal can hand its slot
/// over immediately and still render the ramp every early stop requires.
///
/// A steal happens only with [`MAX_VOICES`] voices already sounding, and a
/// tail lives for [`FADE_MS`]; filling all 16 therefore takes 16 steals
/// inside 4 ms — four thousand note-ons a second, on top of a pool that is
/// already full. Past that the next steal takes the tail nearest silence
/// ([`FadeState::frames_left`]), which stops that one voice hard: the
/// quietest tail is the smallest step available, it has already been
/// reported terminated, and nothing a kit does gets near it.
pub const STEAL_TAILS: usize = 16;

// A steal with nowhere to park the voice it takes could not fade it, and
// "every early stop fades" is the one thing this module does not bend.
const _: () = assert!(STEAL_TAILS > 0);

/// How many ended voices the terminated outbox holds between drains: 128,
/// two for every slot the pool has.
///
/// A fixed-capacity outbox that `lib.rs` drains per block into
/// `NoteEvent::VoiceTerminated`. A single block can end every sounding voice
/// and then end their replacements too, if its events reuse the slots, so one
/// pool's worth of entries is not quite a bound and twice it is a generous one.
/// A full outbox drops what will not fit ([`VoicePool::terminated`]): the host
/// goes on believing one note is still sounding until it next hears about it,
/// which costs it a voice's worth of bookkeeping and nothing audible, where the
/// only other answer on the audio thread would be to allocate.
pub const TERMINATED_CAPACITY: usize = 2 * MAX_VOICES;

/// How far a voice may be tuned from the sample's own pitch, in semitones:
/// eight octaves either way.
///
/// Not a musical limit — it is the clamp that keeps the step finite and the
/// voice mortal. Tuning reaches a voice from per-note expression, which comes
/// from the host, so it is hostile input like any other: at −96 semitones a
/// one-second sample still ends after about four minutes, where an unclamped
/// −10000 would leave a voice sounding for longer than the session.
pub const SEMIS_MAX: f32 = 96.0;
/// The low end of the tuning clamp. See [`SEMIS_MAX`].
pub const SEMIS_MIN: f32 = -96.0;

/// The quietest a lane's volume goes, in dB: −60, where the model clamps
/// it.
///
/// Defined here, beside the gain chain that consumes it, and re-exported by
/// `model.rs` rather than declared twice — so the setter that
/// clamps a lane's volume and the mix that applies it agree by construction
/// instead of by review. (`lib.rs`'s `VOLUME_MIN_DB`/`VOLUME_MAX_DB` are the
/// *master* parameter's range, a different control that happens to span the
/// same decibels.)
pub const LANE_VOLUME_DB_MIN: f32 = -60.0;
/// The loudest a lane's volume goes, in dB: +12. See [`LANE_VOLUME_DB_MIN`].
pub const LANE_VOLUME_DB_MAX: f32 = 12.0;

/// How many choke groups there are: 16, numbered 1..=16, with 0 meaning the
/// lane is in no group at all.
///
/// A group is just a number voices compare for equality, so the count is a
/// clamp rather than a capacity — nothing here is sized by it. It exists so
/// that the model's `set_choke` and the voice that answers a
/// choke agree on the range by construction, the arrangement [`MAX_VOICES`]
/// and [`LANE_VOLUME_DB_MIN`] already have: `model.rs` re-exports this one.
///
/// 16 is a kit's worth of groups — a hat pair, a pair of toms sharing a
/// mute, the odd lane that cuts itself — and well past what a lane list of
/// eight needs.
pub const CHOKE_GROUPS: u8 = 16;

/// The key an event means "any key" by: 255.
///
/// A CLAP host may address a note event to every key at once (`key = -1`),
/// which nice-plug delivers as `Key::Wildcard` and `lib.rs` maps onto this
/// stand-in — [`crate::engine::NoteTarget`] carries a plain channel and key,
/// because that is what a voice carries, so "the host did not say" needs a
/// value of its own rather than a second field every event would have to
/// pass along.
///
/// 255 can be that value because it is outside MIDI's 0..=127: no voice's
/// key and no lane's note can equal it, so the only thing it can ever mean
/// is the wildcard. [`Voice::matches`] reads it as *match any* in an
/// **event**, which is what makes a host's "release everything" or "choke
/// everything" land when it names no voice id. A voice never carries it
/// itself — a note-on on key 255 answers no lane and starts nothing, so a
/// wildcard note-on is silent.
///
/// Defined here, beside the predicate that reads it, and used by `lib.rs`
/// where the host's wildcard is translated — the arrangement [`MAX_VOICES`]
/// and [`CHOKE_GROUPS`] already have.
pub const KEY_WILDCARD: u8 = 255;
/// The channel an event means "any channel" by: 255, outside MIDI's 0..=15.
/// See [`KEY_WILDCARD`], which it works exactly like.
pub const CHANNEL_WILDCARD: u8 = 255;

/// Both stand-ins have to be unreachable for real note data, or a wildcard
/// would collide with a key or a channel a host can actually send. Asserted
/// rather than merely documented, since the whole scheme rests on it.
const _: () = assert!(KEY_WILDCARD > 127, "a real key could equal KEY_WILDCARD");
const _: () = assert!(CHANNEL_WILDCARD > 15, "a real channel could equal CHANNEL_WILDCARD");

/// The most any single gain factor may contribute: 4.0, a shade over +12 dB.
///
/// The ceiling for the two factors that arrive as plain gains rather than as
/// decibels — the master volume and PolyVolume. Neither should ever reach it
/// (the master parameter stops at +12 dB, and nice-plug documents
/// `PolyVolume::gain` as a ratio about unity), but both come from the host, so
/// neither is trusted: a mix of 64 voices must not be one hostile infinity away
/// from deafening someone.
pub const GAIN_MAX: f32 = 4.0;

/// The highest sample rate a host is believed to run at, as the ceiling of
/// the [`step_for`] guard: 768 kHz, the top of what any driver offers.
///
/// It exists for the same reason the 1 Hz floor does — not because a host
/// will ever report it, but because an infinite or absurd rate must still
/// leave a step a voice can finish on. Clamping here rather than trusting
/// the number keeps every voice mortal without a special case for infinity.
const HOST_RATE_MAX: f64 = 768_000.0;

/// How long every early stop takes, in milliseconds: 4, picked out of a
/// 3–5 ms range.
///
/// It is short enough that a gate release, a choke or a steal still reads as
/// immediate — 4 ms is well under the ~10 ms a transient takes to register as
/// a separate event — and long enough that the step from full gain to silence
/// is spread over a couple of hundred frames instead of happening between two
/// of them. Cutting a voice mid-waveform is a step discontinuity, and a step
/// is a click; nothing here ever stops a voice without this ramp.
pub const FADE_MS: f32 = 4.0;

/// Whether a lane's voices end with their sample or with the note.
///
/// Defined here, where the behaviour it names lives, and re-exported by
/// `model.rs` rather than declared twice — the same arrangement
/// [`MAX_VOICES`] and [`LANE_VOLUME_DB_MIN`] have, and for the same reason:
/// the setter that stores a lane's mode and the voice that obeys it agree by
/// construction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum LaneMode {
    /// Play the whole sample and ignore note-off: a drum hit is as long as
    /// it is, however briefly the pad was held. The default, because that is
    /// what a sample player is mostly for.
    #[default]
    OneShot,
    /// End the note when the note ends: note-off starts the release fade.
    /// For held material — a pad, a loop, a sustained texture — where note
    /// length is the point.
    Gate,
}

/// Whether a voice is sounding normally or on its way out ("every early stop
/// fades").
///
/// [`FadingOut`](Self::FadingOut) is a linear ramp from full gain to silence
/// over the voice's [`fade_frames`](Voice::fade_frames) — entered by a gate
/// release and by choke and stealing.
///
/// It could have been `{ gain, step }`. A frame countdown is the same
/// ramp read the other way round, and it ends on **exactly** zero rather than
/// near it: subtracting an inexact step two hundred times leaves a residue of
/// unknown sign, so a `gain`-carrying fade finishes a frame early or a frame
/// late depending on rounding. Counting frames makes "the fade is over" an
/// integer fact, which is what lets the tests assert the fade's length as an
/// equality and lets a voice's slot free on a predictable frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FadeState {
    /// Sounding at the voice's own gain; no ramp applied.
    None,
    /// On the way out: `frames_left` output frames of `frames` remain, and
    /// the ramp's gain is their ratio. At `frames_left == 0` the voice is
    /// silent and done.
    FadingOut {
        /// Frames of the ramp still to render. Counts down to zero.
        frames_left: u32,
        /// The ramp's whole length in frames, the denominator of its gain.
        /// Never zero (see [`fade_frames_for`]).
        frames: u32,
    },
}

impl FadeState {
    /// The ramp's gain for the frame about to be rendered: 1.0 while not
    /// fading, `frames_left / frames` while fading.
    ///
    /// Exact at both ends — `frames / frames` is 1.0 and `0 / frames` is 0.0
    /// bit for bit — so entering a fade does not step the gain and finishing
    /// one lands on silence rather than near it.
    pub fn gain(self) -> f32 {
        match self {
            FadeState::None => 1.0,
            FadeState::FadingOut { frames_left, frames } => frames_left as f32 / frames as f32,
        }
    }

    /// Whether the ramp has reached silence and the voice has nothing left to
    /// render. Always false for a voice that is not fading.
    pub fn is_finished(self) -> bool {
        matches!(self, FadeState::FadingOut { frames_left: 0, .. })
    }

    /// How many frames of ramp are left before silence — the measure of
    /// "nearest to silence" that evicting a tail needs ([`STEAL_TAILS`]).
    ///
    /// [`u32::MAX`] for a voice that is not fading: it is not on its way to
    /// silence at all, so by this measure it is as far from it as a number
    /// goes and can never be the nearest.
    pub fn frames_left(self) -> u32 {
        match self {
            FadeState::None => u32::MAX,
            FadeState::FadingOut { frames_left, .. } => frames_left,
        }
    }

    /// Move the ramp on by one rendered frame. A no-op when not fading, and
    /// saturating at zero so an over-long render cannot wrap it back to full
    /// gain.
    fn advance(&mut self) {
        if let FadeState::FadingOut { frames_left, .. } = self {
            *frames_left = frames_left.saturating_sub(1);
        }
    }
}

/// How many output frames [`FADE_MS`] is at `host_rate`: 192 frames at
/// 48 kHz.
///
/// Rounded **up**, and never zero: a fade of no frames is a cut, which is the
/// click the fade exists to avoid. `host_rate` is guarded exactly as
/// [`step_for`] guards it — it comes from the host, and a NaN or zero rate
/// must still leave a ramp that ends.
pub fn fade_frames_for(host_rate: f64) -> u32 {
    // `max`-then-`min` is the NaN guard, as in `step_for`.
    #[allow(clippy::manual_clamp)]
    let host = host_rate.max(1.0).min(HOST_RATE_MAX);
    ((FADE_MS as f64 / 1000.0) * host).ceil().max(1.0) as u32
}

/// How many output frames a gate release of `release_ms` is at `host_rate`:
/// 4800 at 100 ms and 48 kHz.
///
/// The time is clamped as a lane holds it ([`crate::model::clamp_release_ms`]
/// — NaN is the default) and the rate as [`fade_frames_for`] guards it.
/// Never shorter than [`fade_frames_for`]: 0 ms is the v1 [`FADE_MS`] fade,
/// and so is anything under it — a release that ended faster than the fade
/// every early stop takes would step the gain by more than that fade does,
/// and "no click ever" is the point of a release. Rounded up, like the fade.
pub fn fade_frames_for_ms(release_ms: f32, host_rate: f64) -> u32 {
    let ms = crate::model::clamp_release_ms(release_ms) as f64;
    // `max`-then-`min` is the NaN guard, as in `step_for`.
    #[allow(clippy::manual_clamp)]
    let host = host_rate.max(1.0).min(HOST_RATE_MAX);
    let frames = (ms * host / 1000.0).ceil() as u32;
    frames.max(fade_frames_for(host_rate))
}

/// A voice's attack: the linear fade-in from silence at the note-on.
///
/// It could have been `{ frames_left, gain, step }`; this counts frames
/// for the reason [`FadeState`] does — the gain is the exact ratio
/// `(frames - frames_left) / frames`, so the ramp starts on exactly 0.0 and
/// lands on exactly 1.0 on a known frame instead of on an accumulated sum's
/// rounding. Frame *k* of an *n*-frame attack is mixed at `k / n`, and frame
/// *n* — the first after the ramp — at 1.0: a 10 ms attack at 48 kHz reaches
/// full gain at frame 480.
///
/// A zero-length ramp ([`Ramp::default`], attack 0 ms) is done from the start
/// and its gain is 1.0 for every frame. Multiplying by 1.0 is exact, which is
/// what keeps attack 0 bit-identical to v1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Ramp {
    /// Frames of the ramp still to render. Counts down to zero, where the
    /// ramp is over and the gain is 1.0 for good.
    pub frames_left: u32,
    /// The ramp's whole length in frames, the denominator of its gain. Zero
    /// only for a ramp that was never there (`frames_left` is then zero too).
    pub frames: u32,
}

impl Ramp {
    /// A fade-in over `frames` output frames, starting at silence.
    pub fn attack(frames: u32) -> Ramp {
        Ramp { frames_left: frames, frames }
    }

    /// The ramp's gain for the frame about to be rendered: `0.0` on the
    /// first frame of a ramp, rising by `1 / frames` a frame, and exactly
    /// 1.0 once the ramp is over (or when there never was one).
    pub fn gain(self) -> f32 {
        if self.frames_left == 0 {
            1.0
        } else {
            (self.frames - self.frames_left) as f32 / self.frames as f32
        }
    }

    /// Whether the ramp is over: the voice is at its full gain.
    ///
    /// Test-only: the render loops read [`gain`](Self::gain), which is
    /// already 1.0 once the ramp is over, so nothing at runtime asks.
    #[cfg(test)]
    pub fn is_done(self) -> bool {
        self.frames_left == 0
    }

    /// Move the ramp on by one rendered frame, saturating at the end.
    fn advance(&mut self) {
        self.frames_left = self.frames_left.saturating_sub(1);
    }
}

/// How many output frames an attack of `attack_ms` is at `host_rate`: 480 at
/// 10 ms and 48 kHz, 0 at 0 ms (no ramp — v1's instant start).
///
/// The time is clamped as a lane holds it ([`crate::model::clamp_attack_ms`]
/// — a NaN is 0 ms) and the rate as [`step_for`] guards it, so a hostile
/// value still gives a finite ramp that ends. Rounded to the nearest frame;
/// `ms * rate / 1000` in that order keeps whole-frame times exact.
pub fn attack_frames_for(attack_ms: f32, host_rate: f64) -> u32 {
    let ms = crate::model::clamp_attack_ms(attack_ms) as f64;
    // `max`-then-`min` is the NaN guard, as in `step_for`.
    #[allow(clippy::manual_clamp)]
    let host = host_rate.max(1.0).min(HOST_RATE_MAX);
    (ms * host / 1000.0).round() as u32
}

/// The master gain over a span of frames: where it starts, and how far it
/// moves per frame.
///
/// The master gain reaches the audio thread once a block —
/// `ChopdropParams::next_master_gain` steps nice-plug's smoother by the whole
/// block and hands over the value it landed on. One number a block is fine
/// for a knob drag, whose own smoothing is longer than any usual buffer, but
/// a host can automate the parameter from one extreme to the other between
/// two blocks, and a gain that large a distance apart on two neighbouring
/// frames is a click. So the gain arrives here as a *line* rather than a
/// number: the engine remembers what the last block was heading for and
/// walks from there to this block's value one frame at a time.
///
/// A line and not a per-frame buffer of gains, because the real-time rule
/// leaves nowhere to put a buffer: two `f32`s copied into the render loop, and
/// the gain at frame *i* is
/// [`at(i)`](Self::at).
///
/// A bare `f32` converts into a flat ramp (`impl From<f32>`), which is what
/// every caller with no line to draw passes — the tests here and in
/// `engine.rs`, and the engine's own first block of a run.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MasterRamp {
    /// The gain at frame 0 of the span, already through [`clamp_gain`].
    start: f32,
    /// How far the gain moves per frame, which for a flat ramp is exactly
    /// `0.0` — and `start + 0.0 * i` is `start` for every frame, so a gain
    /// that is not moving costs the mix nothing and stays bit-identical to
    /// the single-scalar gain this type replaced (the module docs'
    /// §"Exactness is a contract" rests on that).
    step: f32,
}

impl MasterRamp {
    /// A gain that does not move: every frame of the span is `gain`.
    pub fn flat(gain: f32) -> Self {
        MasterRamp { start: clamp_gain(gain), step: 0.0 }
    }

    /// The line from `from` to `to` across `frames` frames: frame 0 is
    /// `from`, and `to` is reached at frame `frames` — which is the *next*
    /// block's frame 0, so two blocks in a row join up without repeating a
    /// value or skipping one.
    ///
    /// Both ends are clamped before the step is taken, so the line between
    /// them is finite whatever the host drove. A span of no frames has no
    /// line to draw, and two equal ends have none to take: both become
    /// [`flat`](Self::flat), which keeps the steady state — every block of
    /// a project whose volume nobody is automating — exactly as cheap and
    /// exactly as precise as a single scalar.
    pub fn across(from: f32, to: f32, frames: usize) -> Self {
        let from = clamp_gain(from);
        let to = clamp_gain(to);
        if frames == 0 || from == to {
            return MasterRamp::flat(to);
        }
        MasterRamp { start: from, step: (to - from) / frames as f32 }
    }

    /// The gain at frame `frame` of the span.
    ///
    /// `start + step * frame`, not a running sum: a sum carries its own
    /// rounding error forward frame by frame, and the closed form costs the
    /// same multiply-add. Clamped once more because the frame count is the
    /// caller's — a span rendered longer than the ramp was drawn for walks
    /// past `to`, and the clamp is what stops it walking out of the range
    /// the mix accepts.
    pub fn at(self, frame: usize) -> f32 {
        clamp_gain(self.start + self.step * frame as f32)
    }

    /// The same line seen from `frame` frames in.
    ///
    /// What a span starting part way through a block is rendered at:
    /// [`Engine::run_block`](crate::engine::Engine::run_block) splits the
    /// block at every event offset and each piece renders from its own frame
    /// 0, so each piece is handed the line moved up to where it begins.
    /// Without this a block with an event in it would restart the ramp at
    /// the event — the very step the ramp exists to remove.
    pub fn from_frame(self, frame: usize) -> Self {
        MasterRamp { start: self.at(frame), step: self.step }
    }
}

impl From<f32> for MasterRamp {
    fn from(gain: f32) -> Self {
        MasterRamp::flat(gain)
    }
}

/// Everything a note-on has to say to start a voice.
///
/// A plain struct rather than nine arguments to
/// [`VoicePool::start`]: the engine fills it from an event and the GUI's
/// audition ring fills the same shape from a lane, so the two paths cannot
/// drift.
///
/// The sample is moved in, not borrowed: a voice owns its own
/// `Shared<SampleData>` clone for as long as it sounds, which is what makes
/// replacing or clearing a lane's sample mid-note safe (the old audio
/// outlives the lane that pointed at it, and the collector frees it on the
/// writer side once the last voice is gone).
pub struct VoiceStart {
    /// Which lane the note belongs to, for the activity lights
    /// and for choke matching.
    pub lane_id: u32,
    /// The note that started it, kept for note-off and expression matching
    /// by `(channel, key)`.
    pub note_key: u8,
    /// The channel the note arrived on, same purpose as
    /// [`note_key`](Self::note_key).
    pub channel: u8,
    /// The host's voice id when it sent one — the preferred way to match
    /// later expression and note-off events.
    pub voice_id: Option<i32>,
    /// The sample to play, as published to the audio thread.
    pub sample: Shared<SampleData>,
    /// Note velocity, 0.0..=1.0: the first factor of the gain chain.
    pub velocity: f32,
    /// The lane's volume in dB at the moment the note started, −60..=+12.
    ///
    /// A dB value rather than a gain because that is what the published lane
    /// holds (an `AtomicU32` the GUI writes); the voice converts it once and
    /// caches the result.
    pub lane_volume_db: f32,
    /// The lane's pan at the moment the note started, −1.0..=1.0. PolyPan
    /// adds to it later ([`VoicePool::poly_pan`]); it does not replace it.
    pub lane_pan: f32,
    /// The lane's choke group at the moment the note started: 0 for none,
    /// else 1..=[`CHOKE_GROUPS`].
    ///
    /// Taken at the note-on and kept, for the same reason
    /// [`lane_mode`](Self::lane_mode) is: moving a lane into a group must
    /// not retroactively put the notes it has already played there.
    pub lane_choke: u8,
    /// Whether the lane ends its voices with the sample or with the note.
    ///
    /// Taken at the note-on and kept for the voice's life, not re-read per
    /// block like volume and pan: flipping a lane to Gate while a one-shot
    /// is ringing must not cut the note that was started as a one-shot.
    pub lane_mode: LaneMode,
    /// Semitones to shift the sample by: 0.0 plays it at its own pitch.
    /// Already includes anything the caller folds in (per-note tuning, and
    /// the "bitstep Sample pad" switch's key offset).
    pub semis: f32,
    /// The lane's time speed factor at the note-on: 1.0 in Off mode, the
    /// repitch ratio in Repitch, the speed correction on a Stretch render.
    /// Clamped to [`SPEED_MIN`]..=[`SPEED_MAX`]; the engine updates it per
    /// block through [`Voice::set_time`].
    pub speed: f64,
    /// The lane's own pitch in semitones at the note-on: the lane's
    /// `pitch_st` in Off mode, 0.0 otherwise. Adds to
    /// [`semis`](Self::semis) (poly tuning) before the step is taken.
    pub pitch_st: f32,
    /// The lane's attack in ms at the moment the note started: the voice fades
    /// in from silence over this many milliseconds of host frames. 0.0 is no
    /// ramp at all — v1's start, bit for bit. Taken at the note-on and kept:
    /// turning the attack knob does not reshape a note already sounding.
    pub attack_ms: f32,
    /// The lane's release in ms at the moment the note started: how long a
    /// [`LaneMode::Gate`] voice fades for after its note-off. 0.0 (and anything
    /// under [`FADE_MS`]) is the v1 fade. Ignored by one-shots, which ignore
    /// note-off; choke, steal and cut itself always fade over [`FADE_MS`].
    /// Taken at the note-on and kept, like [`attack_ms`](Self::attack_ms).
    pub release_ms: f32,
    /// The output the voice renders into, 0 (Out 1, the main output) ..= 15:
    /// the lane's at a note-on, 0 for an audition or a preview. Taken at the
    /// note-on and kept, like
    /// [`lane_choke`](Self::lane_choke): moving a lane to another output
    /// does not move the notes it is already sounding.
    pub output: u8,
    /// Whether [`sample`](Self::sample) is a Stretch lane's time-stretch render
    /// rather than its raw chop: a render is speed-corrected against the tempo,
    /// the raw chop plays at speed 1. Taken at the note-on and kept, like the
    /// sample itself.
    pub rendered: bool,
}

/// One sounding note.
///
/// Created by [`VoicePool::start`] and dropped the frame after it runs past
/// the end of its sample. Every field is public because the engine above
/// reaches into voices directly to retune, release and choke them — this is
/// an internal data structure shared by one caller, not an encapsulated
/// object, and the audio thread cannot afford accessors that do anything.
pub struct Voice {
    /// The lane this voice was started for. See [`VoiceStart::lane_id`].
    pub lane_id: u32,
    /// The note that started it. See [`VoiceStart::note_key`].
    pub note_key: u8,
    /// The channel it arrived on. See [`VoiceStart::channel`].
    pub channel: u8,
    /// The host's voice id, when it gave one. See [`VoiceStart::voice_id`].
    pub voice_id: Option<i32>,
    /// The audio being played, held open for the voice's whole life.
    pub sample: Shared<SampleData>,
    /// Where the voice is reading, in **source** frames, fractional: 0.0 is
    /// the first frame of the sample and `frame_count` is one past its last,
    /// at which point the voice is finished.
    pub position: f64,
    /// How far [`position`](Self::position) moves per output frame — the
    /// combined rate conversion and pitch shift, cached by
    /// [`set_semis`](Self::set_semis) rather than recomputed per frame.
    pub step: f64,
    /// Note velocity as it arrived, clamped to 0.0..=1.0: the first factor
    /// of the gain chain.
    pub velocity: f32,
    /// The lane's volume in dB as the voice last saw it, clamped to
    /// [`LANE_VOLUME_DB_MIN`]..=[`LANE_VOLUME_DB_MAX`]. Not a gain — the
    /// cache key for [`lane_volume`](Self::lane_volume).
    pub lane_volume_db: f32,
    /// The lane's volume as a gain: [`lane_volume_db`](Self::lane_volume_db)
    /// through [`db_to_gain`]. **Cached**, written only by
    /// [`set_lane_volume_db`](Self::set_lane_volume_db) — the conversion is
    /// cached rather than redone every block.
    pub lane_volume: f32,
    /// This note's PolyVolume gain, 1.0 until an expression event says
    /// otherwise ([`VoicePool::poly_volume`]). Clamped to 0.0..=[`GAIN_MAX`].
    pub poly_volume: f32,
    /// The lane's pan as the voice last saw it, clamped to −1.0..=1.0.
    pub lane_pan: f32,
    /// This note's PolyPan offset, 0.0 until an expression event says
    /// otherwise ([`VoicePool::poly_pan`]). Adds to
    /// [`lane_pan`](Self::lane_pan).
    pub poly_pan: f32,
    /// The left gain of the constant-power pan law for
    /// `lane_pan + poly_pan`. **Cached**, written only by
    /// [`set_lane_pan`](Self::set_lane_pan) and
    /// [`set_poly_pan`](Self::set_poly_pan).
    pub pan_l: f32,
    /// The right gain of the pan law. See [`pan_l`](Self::pan_l).
    pub pan_r: f32,
    /// The tuning the cached [`step`](Self::step) was computed from, in
    /// semitones, already clamped to [`SEMIS_MIN`]..=[`SEMIS_MAX`].
    pub semis: f32,
    /// The lane time speed the cached [`step`](Self::step) was computed
    /// from, clamped to [`SPEED_MIN`]..=[`SPEED_MAX`] (NaN → 1.0). See
    /// [`set_time`](Self::set_time).
    pub speed: f64,
    /// The lane pitch the cached [`step`](Self::step) was computed from,
    /// clamped like [`semis`](Self::semis). It adds to `semis`.
    pub pitch_st: f32,
    /// This voice's own sample's length in seconds (`frame_count / rate`, 0 for
    /// a rate of 0), computed once at the note-on: what a Repitch lane's speed
    /// is measured against, so a sample swapped on the lane later never retimes
    /// a voice still playing the old one.
    pub natural_secs: f64,
    /// Whether this voice plays a Stretch render ([`VoiceStart::rendered`]),
    /// so the engine's per-block speed correction measures
    /// [`natural_secs`](Self::natural_secs) — the render's own length, the
    /// target it was made for — against the target at the current tempo.
    pub rendered: bool,
    /// The choke group this voice belongs to, clamped to
    /// 0..=[`CHOKE_GROUPS`]: 0 is no group and chokes nothing. Copied from
    /// the lane at the note-on. See [`VoiceStart::lane_choke`].
    pub lane_choke: u8,
    /// Whether this voice ends with its sample or on its note-off. Copied
    /// from the lane at the note-on. See [`VoiceStart::lane_mode`].
    pub lane_mode: LaneMode,
    /// Whether the voice is on its way out, and how far along.
    /// [`FadeState::None`] for all but the last few milliseconds of a voice
    /// that is stopped early.
    pub fade: FadeState,
    /// The fade-in from the note-on ([`Ramp`]), over the lane's attack at
    /// the host rate the voice started on. Done from the start at attack 0.
    pub attack: Ramp,
    /// How long this voice's fade is, in output frames: [`FADE_MS`] at the
    /// host rate the voice started on ([`fade_frames_for`]).
    ///
    /// Resolved once at the note-on so that stopping a voice needs no
    /// arguments at all — [`VoicePool::choke_group`] and stealing
    /// ([`VoicePool::start`]) fade voices from inside the pool, where the
    /// host rate is not at hand.
    /// A host that changes its sample rate resets the plugin and the pool
    /// with it, so a voice never outlives the rate it was computed from.
    pub fade_frames: u32,
    /// How long a note-off fades this voice for, in output frames: the
    /// lane's release at the note-on's host rate ([`fade_frames_for_ms`]),
    /// never under [`fade_frames`](Self::fade_frames). Only a
    /// [`LaneMode::Gate`] voice's [`release`](Self::release) uses it.
    pub release_frames: u32,
    /// The output this voice renders into, clamped like a lane's (anything
    /// above 15 is the main output). See [`VoiceStart::output`].
    pub output: u8,
    /// Start order: the pool hands out a monotonically increasing number per
    /// voice, so "oldest sounding voice" is `min(age)` whatever order the
    /// slots happen to be in. This is what stealing picks by
    /// ([`VoicePool::start`]).
    pub age: u64,
}

impl Voice {
    /// A voice at the start of its sample, from a [`VoiceStart`] (clamped
    /// here, NaN-safe) at host rate `host_rate`, with start order `age`.
    ///
    /// The one constructor: [`VoicePool::start`] files the result in a slot,
    /// and the browser's preview ([`crate::engine`]) keeps one of its own
    /// outside the pool, so the two can never compute a voice differently.
    pub fn new(start: VoiceStart, host_rate: f64, age: u64) -> Voice {
        let semis = clamp_semis(start.semis);
        let speed = clamp_speed(start.speed);
        let pitch_st = clamp_semis(start.pitch_st);
        let step = time_step_for(start.sample.rate, host_rate, speed, pitch_st, semis);
        let lane_volume_db = clamp_volume_db(start.lane_volume_db);
        let lane_pan = clamp_pan(start.lane_pan);
        // PolyVolume and PolyPan start neutral: bitstep sends its expression
        // events immediately after the note-on at the same timing, and a host
        // that sends none leaves the voice at its lane's own volume and pan.
        let (pan_l, pan_r) = pan_gains(lane_pan);
        let natural_secs = natural_secs(start.sample.frame_count(), start.sample.rate);
        Voice {
            lane_id: start.lane_id,
            note_key: start.note_key,
            channel: start.channel,
            voice_id: start.voice_id,
            sample: start.sample,
            position: 0.0,
            step,
            velocity: clamp_velocity(start.velocity),
            lane_volume_db,
            lane_volume: db_to_gain(lane_volume_db),
            poly_volume: 1.0,
            lane_pan,
            poly_pan: 0.0,
            pan_l,
            pan_r,
            semis,
            speed,
            pitch_st,
            natural_secs,
            rendered: start.rendered,
            lane_choke: clamp_choke(start.lane_choke),
            lane_mode: start.lane_mode,
            fade: FadeState::None,
            attack: Ramp::attack(attack_frames_for(start.attack_ms, host_rate)),
            fade_frames: fade_frames_for(host_rate),
            release_frames: fade_frames_for_ms(start.release_ms, host_rate),
            output: clamp_output(start.output),
            age,
        }
    }

    /// Retune a sounding voice and recompute its cached step.
    ///
    /// The one way [`step`](Self::step) ever changes: per-note tuning
    /// arriving mid-note ([`VoicePool::poly_tuning`]) and the "bitstep
    /// Sample pad" switch ([`crate::engine::effective_semis`]) both land
    /// here, already added together. `semis` is clamped and NaN-guarded by
    /// [`step_for`], and the position is left alone — retuning changes how
    /// fast the rest of the sample plays, never which part of it is playing.
    pub fn set_semis(&mut self, semis: f32, host_rate: f64) {
        self.semis = clamp_semis(semis);
        self.cache_step(host_rate);
    }

    /// Point the voice at its lane's time settings and recompute its cached
    /// step when either changed.
    ///
    /// `speed` is the mode's speed factor (1.0 in Off mode) and `pitch_st`
    /// the lane's pitch (its `pitch_st` in Off mode, 0.0 in Repitch and
    /// Stretch). The poly tuning in [`semis`](Self::semis) still adds on
    /// top. Both are clamped and NaN-guarded; the position is left alone,
    /// as in [`set_semis`](Self::set_semis). The engine calls this once per
    /// block for every sounding voice, so the comparison keeps an unchanged
    /// block free of the `powf`.
    pub fn set_time(&mut self, speed: f64, pitch_st: f32, host_rate: f64) {
        let speed = clamp_speed(speed);
        let pitch_st = clamp_semis(pitch_st);
        if speed != self.speed || pitch_st != self.pitch_st {
            self.speed = speed;
            self.pitch_st = pitch_st;
            self.cache_step(host_rate);
        }
    }

    /// Recompute [`step`](Self::step) from the stored tuning and time.
    fn cache_step(&mut self, host_rate: f64) {
        self.step =
            time_step_for(self.sample.rate, host_rate, self.speed, self.pitch_st, self.semis);
    }

    /// Point the voice at its lane's volume, in dB, recomputing the gain it
    /// caches.
    ///
    /// The engine calls this per block for every sounding voice: a lane's
    /// volume lives as an atomic inside the published snapshot precisely so
    /// that moving the knob is heard on the next block without republishing the
    /// lane list. The dB value is kept beside the gain as its cache key, so the
    /// `powf` happens when the knob moves rather than 64 times a block ("cache
    /// the conversion").
    pub fn set_lane_volume_db(&mut self, volume_db: f32) {
        let volume_db = clamp_volume_db(volume_db);
        if volume_db != self.lane_volume_db {
            self.lane_volume_db = volume_db;
            self.lane_volume = db_to_gain(volume_db);
        }
    }

    /// Set this note's PolyVolume gain. A multiplier on top of the lane's
    /// volume, not a replacement for it.
    pub fn set_poly_volume(&mut self, gain: f32) {
        self.poly_volume = clamp_gain(gain);
    }

    /// Point the voice at its lane's pan, recomputing the pan law it caches.
    /// Read from the published atomic per block, like the lane's volume.
    pub fn set_lane_pan(&mut self, pan: f32) {
        let pan = clamp_pan(pan);
        if pan != self.lane_pan {
            self.lane_pan = pan;
            self.cache_pan_gains();
        }
    }

    /// Set this note's PolyPan offset. It adds to the lane's pan, and the sum
    /// is what the law is taken of.
    pub fn set_poly_pan(&mut self, pan: f32) {
        let pan = clamp_pan(pan);
        if pan != self.poly_pan {
            self.poly_pan = pan;
            self.cache_pan_gains();
        }
    }

    /// Recompute [`pan_l`](Self::pan_l)/[`pan_r`](Self::pan_r) from the two
    /// pans. Both are already clamped, so their sum is finite and
    /// [`pan_gains`] only has the ±1.0 ends left to enforce.
    fn cache_pan_gains(&mut self) {
        let (left, right) = pan_gains(self.lane_pan + self.poly_pan);
        self.pan_l = left;
        self.pan_r = right;
    }

    /// The voice's gain before panning: velocity × lane volume × PolyVolume ×
    /// master, in that order and with no other factor.
    ///
    /// The master arrives per block from a host parameter `lib.rs` has
    /// already folded into its own range; clamping it again here costs one
    /// `max`/`min` and means no caller can put an infinity into the mix.
    pub fn gain(&self, master_volume: f32) -> f32 {
        self.velocity * self.lane_volume * self.poly_volume * clamp_gain(master_volume)
    }

    /// The gain each output channel is mixed at: [`gain`](Self::gain)
    /// through the cached constant-power pan law.
    pub fn channel_gains(&self, master_volume: f32) -> (f32, f32) {
        let gain = self.gain(master_volume);
        (self.pan_l * gain, self.pan_r * gain)
    }

    /// Answer a note-off for this voice, and say whether this voice answers
    /// note-off at all.
    ///
    /// This is the whole of one-shot versus gate: a [`LaneMode::Gate`] voice
    /// starts its fade — over the lane's release
    /// ([`release_frames`](Self::release_frames)), not [`FADE_MS`] — and a
    /// [`LaneMode::OneShot`] voice **ignores the note-off entirely** and goes
    /// on to the end of its sample. Which is the point of a sample player: a
    /// kick triggered by a 1-tick step is a whole kick, not a 1-tick kick.
    ///
    /// Releasing a voice twice is as harmless as choking a fading one — see
    /// [`begin_fade`](Self::begin_fade).
    pub fn release(&mut self) -> bool {
        match self.lane_mode {
            LaneMode::OneShot => false,
            LaneMode::Gate => {
                if !self.is_fading() {
                    let frames = self.release_frames;
                    self.fade = FadeState::FadingOut { frames_left: frames, frames };
                }
                true
            }
        }
    }

    /// Stop the voice early, over [`FADE_MS`] rather than at once ("every
    /// early stop fades").
    ///
    /// The one way a voice ends other than running out of sample, and so the
    /// single place a gate release, a choke ([`VoicePool::choke_group`]) and
    /// a steal ([`VoicePool::start`]) all come through. The fade is left
    /// alone if one is already running:
    /// restarting the ramp would jump the gain back up to full, which is the
    /// very discontinuity the fade is for — a voice choked twice in two
    /// blocks must not click because of the second choke.
    ///
    /// The one exception is a fade *longer* than [`FADE_MS`] — a gate release:
    /// a choke or steal still means "gone in 4 ms", so the long ramp is swapped
    /// for a [`FADE_MS`] one that starts at, or a hair under, the gain the
    /// release had reached. `frames_left` is rounded down, so the gain never
    /// rises and drops by less than one step of the new ramp — no more than the
    /// fade's own steps, so no click.
    pub fn begin_fade(&mut self) {
        let frames = self.fade_frames;
        match self.fade {
            FadeState::None => {
                self.fade = FadeState::FadingOut { frames_left: frames, frames };
            }
            FadeState::FadingOut { frames_left, frames: long } if frames_left > frames => {
                // `frames_left / long` of the way up, on a ramp of `frames`.
                let left = u64::from(frames_left) * u64::from(frames) / u64::from(long);
                self.fade = FadeState::FadingOut { frames_left: left as u32, frames };
            }
            FadeState::FadingOut { .. } => {}
        }
    }

    /// Whether the voice is already on its way out. A fading voice is still
    /// sounding — it is not a free slot, a choke leaves its ramp alone
    /// ([`begin_fade`](Self::begin_fade)), and a steal that takes it carries
    /// the ramp it is on into its tail rather than starting a new one
    /// ([`VoicePool::start`]).
    pub fn is_fading(&self) -> bool {
        !matches!(self.fade, FadeState::None)
    }

    /// Who the host knows this voice as, as the outbox records it when the
    /// voice ends.
    ///
    /// Copied out of the voice rather than read from it later because the
    /// record outlives the voice by design: a slot is freed, and the voice
    /// in it dropped, in the same breath as the report.
    pub fn identity(&self) -> TerminatedVoice {
        TerminatedVoice {
            voice_id: self.voice_id,
            channel: self.channel,
            note_key: self.note_key,
        }
    }

    /// Whether a note event addressed to `(channel, key, voice_id)` is about
    /// this voice.
    ///
    /// By the host's voice id when the event carries one, else by channel and
    /// key. A host that tracks voices gives every note its own id, which is
    /// the only way to tell two voices on the same key apart; a host that
    /// does not leaves `None`, and then the note *is* its channel and key.
    ///
    /// "When the event carries one" is read per voice, not per event: a voice
    /// started without an id — an audition from the GUI is the one that
    /// exists — is still matched by channel and key even by an event
    /// that has an id, because an id it was never given cannot identify it,
    /// and the alternative is an auditioned note that no note-off can end.
    ///
    /// Note-off asks this question first ([`VoicePool::release`]), and
    /// per-note expression asks exactly the same one
    /// ([`VoicePool::poly_tuning`] and its siblings) — one predicate, so
    /// the two can never drift into disagreeing about which voice a host
    /// meant.
    ///
    /// # Wildcards
    ///
    /// [`KEY_WILDCARD`] or [`CHANNEL_WILDCARD`] in the **event** is *match
    /// any*: a host that asks for "every note off" or "choke everything"
    /// without naming a voice id addresses the event to any key, any
    /// channel, or both ([`crate::engine::NoteTarget`] carries what `lib.rs`
    /// mapped it to), and that has to reach every voice rather than none —
    /// a plugin that ignored it would hang a held kit on the host's panic
    /// button. The two are independent, so "all notes off on channel 1" is
    /// a wildcard key on a real channel and leaves channel 2 sounding.
    ///
    /// Only the event side is read that way. A voice never carries 255 (no
    /// lane answers note 255, so a wildcard note-on starts nothing), and an
    /// event naming a real key still has to find that key, so this stays
    /// the exact-match predicate it was for every event a note source
    /// actually sends.
    pub fn matches(&self, channel: u8, key: u8, voice_id: Option<i32>) -> bool {
        match (voice_id, self.voice_id) {
            (Some(event), Some(voice)) => event == voice,
            _ => {
                (channel == CHANNEL_WILDCARD || self.channel == channel)
                    && (key == KEY_WILDCARD || self.note_key == key)
            }
        }
    }

    /// Whether the voice has nothing left to render: it has run past the last
    /// frame of its sample, or its fade has reached silence.
    ///
    /// The first is the one-shot end condition — a voice at exactly
    /// `frame_count - 1` still has a frame to play; at `frame_count` it does
    /// not. The second is the end of every early stop.
    ///
    /// The named form of the condition, for the tests only.
    /// [`render_additive`](Self::render_additive) deliberately re-states it
    /// rather than calling this: it hoists the `playable_frames` lookup out
    /// of the per-frame loop, and asking here would put it back inside.
    #[cfg(test)]
    pub fn is_finished(&self) -> bool {
        self.position >= playable_frames(&self.sample) as f64 || self.fade.is_finished()
    }

    /// Mix this voice's remaining output into a stereo block, additively,
    /// and say whether it is still sounding afterwards.
    ///
    /// One frame of `out_l`/`out_r` per output frame, shortest-buffer wins. A
    /// mono sample feeds both channels **before** the pan law; a stereo one
    /// keeps its sides and the law becomes a simple balance across them. Both
    /// are then mixed at
    /// [`channel_gains`](Self::channel_gains), taken per frame at that
    /// frame's point on the [`MasterRamp`] — four multiplications and a
    /// clamp, because everything in the chain but the master is already
    /// cached (the pan law's cosines in
    /// [`pan_l`](Self::pan_l)/[`pan_r`](Self::pan_r), the lane's dB in
    /// [`lane_volume`](Self::lane_volume)) and "cache the conversion" is
    /// about those. A ramp that is flat — every block of a project nobody is
    /// automating — recomputes the same product every frame and mixes
    /// bit-identically to a single per-block gain.
    ///
    /// A voice on its way out is mixed at its ramp's gain on top of all of
    /// that, one step quieter per frame, and ends the moment the ramp reaches
    /// silence ([`FadeState`]). The ramp multiplies in rather than touching
    /// the cached gains, so a fade costs a voice that is not fading exactly
    /// one multiplication by 1.0 — which is exact, and leaves the module's
    /// reproduce-the-file contract alone.
    ///
    /// Returning early when the sample runs out is what makes block
    /// stitching exact — a voice that ends 30 frames into a 512-frame block
    /// writes those 30 frames and leaves the other 482 untouched, which is
    /// bit-identically what two 256-frame halves would have written. A fade
    /// crossing a block boundary stitches the same way: the ramp is counted
    /// in frames rendered, not in blocks.
    pub fn render_additive(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        master: MasterRamp,
    ) -> bool {
        let sample = &*self.sample;
        let channels = sample.channels as usize;
        let frames = playable_frames(sample);
        if frames == 0 {
            return false;
        }
        let end = frames as f64;

        for (frame, (left, right)) in out_l.iter_mut().zip(out_r.iter_mut()).enumerate() {
            if self.position >= end || self.fade.is_finished() {
                return false;
            }
            // A silent voice is still rendered rather than skipped: its
            // position has to keep moving, or a note held at zero gain would
            // never end.
            let (gain_l, gain_r) = self.channel_gains(master.at(frame));
            // 1.0 for a voice that is not fading, and `x * 1.0 == x`.
            let fade = self.fade.gain();
            // Likewise 1.0 once the attack is over, or with no attack at all:
            // the factor is exact, so attack 0 mixes bit-identically to v1.
            let attack = self.attack.gain();
            let l = interpolate(&sample.frames, channels, frames, 0, self.position);
            // Mono feeds both sides from the one channel it has.
            let r = if channels > 1 {
                interpolate(&sample.frames, channels, frames, 1, self.position)
            } else {
                l
            };
            *left += l * fade * attack * gain_l;
            *right += r * fade * attack * gain_r;
            self.fade.advance();
            self.attack.advance();
            self.position += self.step;
        }

        self.position < end && !self.fade.is_finished()
    }

    /// Mix this voice into a stereo block like
    /// [`render_additive`](Self::render_additive), but only while it reads
    /// inside `region` (source frames `[start, end)`, already clamped into
    /// the sample), with the chop's edge fades over it
    /// ([`chop::edge_gain`] with `edge_fade` frames, so a region previews
    /// exactly as [`chop::cut`] would play it). Returns how many output
    /// frames it wrote: fewer than the block means the voice reached the
    /// region's end (its seam) or finished its own fade, which the caller
    /// tells apart with the fade's [`FadeState::is_finished`] and
    /// [`position`](Self::position).
    ///
    /// The chop editor's preview voice, kept outside the pool by
    /// [`crate::engine`]. Allocation-free.
    pub fn render_region_additive(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        master: MasterRamp,
        region: Range<u64>,
        edge_fade: u64,
    ) -> usize {
        let sample = &*self.sample;
        let channels = sample.channels as usize;
        let frames = playable_frames(sample);
        let end = region.end.min(frames as u64) as f64;
        let start = region.start as f64;
        let len = region.end.saturating_sub(region.start);
        let mut written = 0;
        for (frame, (left, right)) in out_l.iter_mut().zip(out_r.iter_mut()).enumerate() {
            if self.position >= end || self.fade.is_finished() {
                break;
            }
            let (gain_l, gain_r) = self.channel_gains(master.at(frame));
            let edge = chop::edge_gain(self.position - start, len, edge_fade);
            let gain = self.fade.gain() * self.attack.gain() * edge;
            let l = interpolate(&sample.frames, channels, frames, 0, self.position);
            let r = if channels > 1 {
                interpolate(&sample.frames, channels, frames, 1, self.position)
            } else {
                l
            };
            *left += l * gain * gain_l;
            *right += r * gain * gain_r;
            self.fade.advance();
            self.attack.advance();
            self.position += self.step;
            written += 1;
        }
        written
    }
}

/// A voice the host should be told has ended.
///
/// The pool records one of these for every voice that stops — whether it ran
/// out of sample, faded out on a note-off or a choke, or was stolen — and
/// `lib.rs` drains them per block into
/// `NoteEvent::VoiceTerminated { timing, voice_id, channel, key }`, whose three
/// identifying fields are exactly these three (verified against nice-plug
/// 0.4.2's CLAP wrapper, which turns the event into a `CLAP_EVENT_NOTE_END`). A
/// host that hands out voice ids needs that event to know an id is free again;
/// one that does not is told which channel and key stopped, and ignores it.
///
/// An identity rather than the voice itself, because the record outlives the
/// voice: see [`Voice::identity`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct TerminatedVoice {
    /// The host's voice id, if the note-on that started the voice carried
    /// one. `None` for a note from a host that does not track voices, and
    /// for an audition from the GUI.
    pub voice_id: Option<i32>,
    /// The channel the note arrived on.
    pub channel: u8,
    /// The key the note arrived on.
    pub note_key: u8,
}

/// The fixed pool of voices.
///
/// [`MAX_VOICES`] slots inline, each either empty or holding a [`Voice`].
/// Nothing here allocates: starting a voice writes into a slot, ending one
/// clears it (which drops a `Shared` — a refcount decrement and, at zero, a
/// push onto basedrop's lock-free queue, never a free on this thread).
///
/// The pool is the engine's, lives as long as the plugin instance, and is
/// only ever touched from the audio thread.
pub struct VoicePool {
    /// Voice slots, in two regions.
    ///
    /// The first [`MAX_VOICES`] are **live**: the voices a note, a choke or
    /// an expression event can address. Slot order means nothing there — a
    /// voice's place is wherever the first empty slot was, and
    /// [`Voice::age`] carries the ordering.
    ///
    /// The [`STEAL_TAILS`] after them are **tails**: stolen voices finishing
    /// the fade they were given on the way out (module docs, §"Stealing
    /// parks the voice it takes"). They are rendered with the rest and
    /// addressed by nothing.
    slots: [Option<Voice>; MAX_VOICES + STEAL_TAILS],
    /// The [`Voice::age`] the next started voice gets. `u64`, so at 64
    /// voices a second it wraps in rather more than a human lifetime —
    /// there is no wrap case to handle.
    next_age: u64,
    /// The terminated outbox: voices that have ended since the last drain,
    /// oldest first, the first
    /// [`terminated_len`](Self::terminated_len) entries of it live.
    terminated: [TerminatedVoice; TERMINATED_CAPACITY],
    /// How many entries of [`terminated`](Self::terminated) are filled.
    terminated_len: usize,
}

impl VoicePool {
    /// An empty pool.
    pub const fn new() -> Self {
        // A whole array of these is 1.5 kB of nothing, written once when the
        // plugin is instantiated — the price of an outbox that never grows.
        const UNUSED: TerminatedVoice = TerminatedVoice { voice_id: None, channel: 0, note_key: 0 };
        VoicePool {
            slots: [const { None }; MAX_VOICES + STEAL_TAILS],
            next_age: 0,
            terminated: [UNUSED; TERMINATED_CAPACITY],
            terminated_len: 0,
        }
    }

    /// Start a voice, returning the slot it went into.
    ///
    /// `host_rate` is the host's sample rate; the step is computed once here
    /// from it and the sample's own rate.
    ///
    /// A full pool **steals**: the oldest live voice is reported terminated,
    /// started on its fade and parked in a tail slot to finish it, and the slot
    /// it leaves is this note's. So a note-on always sounds, and the only
    /// `None` left is a sample with nothing to play (no channels, or no
    /// frames).
    /// [`crate::sample::decode`] cannot produce such a `SampleData`, but a
    /// voice is the last place that could notice if something ever did, and
    /// the interpolator divides by the channel count.
    ///
    /// A voice that does sound applies its [`VoiceStart::lane_choke`] group on
    /// the way in ([`choke_group`](Self::choke_group)) — the whole of the
    /// choke-group rule, and the reason choke needs no separate call from the
    /// engine. A note that does not sound leaves the group untouched, which is
    /// why the group is applied here, after the slot is in hand, rather than
    /// before looking for one. The new voice cannot choke itself, because its
    /// slot is still empty when the group is swept; nor can the sweep reach the
    /// voice this note just stole, which is a tail by then and no longer in any
    /// group.
    pub fn start(&mut self, start: VoiceStart, host_rate: f64) -> Option<usize> {
        if playable_frames(&start.sample) == 0 {
            return None;
        }
        let slot = match self.slots[..MAX_VOICES].iter().position(Option::is_none) {
            Some(free) => free,
            None => self.steal_oldest()?,
        };

        self.choke_group(clamp_choke(start.lane_choke));

        self.slots[slot] = Some(Voice::new(start, host_rate, self.next_age));
        self.next_age += 1;
        Some(slot)
    }

    /// Take the oldest live voice's slot for a new note, and return that
    /// slot.
    ///
    /// "Oldest" is the smallest [`Voice::age`] — not the lowest slot, which
    /// is only the order voices happened to be filed in, and not the
    /// quietest, which would take a listener's ear to compute. A pool that
    /// has been holding 64 notes gives up the one that has been sounding
    /// longest, the one most likely to be a tail of something already past.
    ///
    /// A voice already fading is not spared: it is not a free slot, it has a
    /// ramp to finish wherever it sits, and sparing it would mean taking a
    /// younger voice still at full gain instead.
    ///
    /// The voice does not stop here. It is reported terminated at once —
    /// its slot is in use again at once — then faded and parked in a tail
    /// ([`park_tail`](Self::park_tail)), which is where its last 4 ms are
    /// rendered from. `None` only if the live region is empty, which is not
    /// a state a steal can be reached from.
    fn steal_oldest(&mut self) -> Option<usize> {
        let slot = self.slots[..MAX_VOICES]
            .iter()
            .enumerate()
            .filter_map(|(slot, voice)| voice.as_ref().map(|voice| (slot, voice.age)))
            .min_by_key(|&(_, age)| age)
            .map(|(slot, _)| slot)?;
        let mut stolen = self.slots[slot].take()?;
        self.report_terminated(stolen.identity());
        stolen.begin_fade();
        self.park_tail(stolen);
        Some(slot)
    }

    /// Park a stolen voice in a tail slot so it can finish its fade in the
    /// mix (module docs, §"Stealing parks the voice it takes").
    ///
    /// A free tail if there is one, else the tail nearest silence, whose
    /// voice stops hard — [`STEAL_TAILS`] has what it takes to get there and
    /// why the nearest to silence is the one to drop. Nothing is reported
    /// here either way: a voice is reported once, at the steal that took it.
    fn park_tail(&mut self, voice: Voice) {
        let tails = &self.slots[MAX_VOICES..];
        let tail = match tails.iter().position(Option::is_none) {
            Some(free) => free,
            // Every tail is a fading voice, so `frames_left` is the ramp's
            // own countdown and the smallest of them is the quietest.
            None => tails
                .iter()
                .enumerate()
                .min_by_key(|(_, tail)| tail.as_ref().map_or(0, |voice| voice.fade.frames_left()))
                .map_or(0, |(tail, _)| tail),
        };
        self.slots[MAX_VOICES + tail] = Some(voice);
    }

    /// Record a voice the host is to be told about, oldest first.
    ///
    /// Silently dropped when the outbox is full — [`TERMINATED_CAPACITY`]
    /// has why that is the right failure and what it costs.
    fn report_terminated(&mut self, ended: TerminatedVoice) {
        if self.terminated_len < TERMINATED_CAPACITY {
            self.terminated[self.terminated_len] = ended;
            self.terminated_len += 1;
        }
    }

    /// The voices that have ended since the last drain, oldest first — every
    /// voice that ran out of sample, faded out or was stolen.
    ///
    /// Read it, send it, then [`clear_terminated`](Self::clear_terminated):
    /// `lib.rs` walks this slice per block pushing a
    /// `NoteEvent::VoiceTerminated` for each entry. A slice and a separate
    /// clear rather than a draining iterator, because the audio thread should
    /// not have to hold a borrow of the pool while it talks to the host.
    pub fn terminated(&self) -> &[TerminatedVoice] {
        &self.terminated[..self.terminated_len]
    }

    /// Forget the terminated voices, once the host has them.
    pub fn clear_terminated(&mut self) {
        self.terminated_len = 0;
    }

    /// Answer a note-off: every matching gate voice starts fading, every
    /// matching one-shot carries on. Returns how many voices answered it — gate
    /// voices only, a voice already fading included, since the note-off did
    /// address it.
    ///
    /// Matching is [`Voice::matches`] — the host's voice id when the event
    /// has one, else channel and key. Several voices can match one note-off
    /// and all of them are released: lanes layer, so one key may have
    /// started a voice per lane ("start a voice for **every** lane whose
    /// note == key"), and a note-off that released only the first of
    /// them would leave the rest hanging.
    pub fn release(&mut self, channel: u8, key: u8, voice_id: Option<i32>) -> usize {
        let mut released = 0;
        for voice in self.voices_mut() {
            if voice.matches(channel, key, voice_id) && voice.release() {
                released += 1;
            }
        }
        released
    }

    /// Route a PolyTuning event to the voices it addresses and say how many
    /// took it.
    ///
    /// `semis` is the **effective** tuning, exactly as [`VoiceStart::semis`]
    /// is: the caller has already folded the "bitstep Sample pad" switch's
    /// key offset into it ([`crate::engine::effective_semis`]). Both ways a
    /// voice is ever tuned
    /// therefore arrive in the same units, and the one place tuning becomes
    /// a step ([`Voice::set_semis`]) cannot disagree with the one place a
    /// note-on computed it.
    ///
    /// Retuning a sounding voice changes how fast the rest of its sample
    /// plays and never where it is playing from — bitstep sends the tuning
    /// right after the note-on at the same timing, so the common
    /// case is a retune at position 0.0, and a slide arriving mid-note must
    /// bend the note rather than restart it.
    pub fn poly_tuning(
        &mut self,
        channel: u8,
        key: u8,
        voice_id: Option<i32>,
        semis: f32,
        host_rate: f64,
    ) -> usize {
        self.apply_to_matching(channel, key, voice_id, |voice| voice.set_semis(semis, host_rate))
    }

    /// Route a PolyPan event to the voices it addresses and say how many
    /// took it.
    ///
    /// An offset on the lane's own pan rather than a replacement for it
    /// ([`Voice::set_poly_pan`]): a lane panned left that is sent a PolyPan
    /// of +0.5 moves right of where it was, not to +0.5.
    pub fn poly_pan(&mut self, channel: u8, key: u8, voice_id: Option<i32>, pan: f32) -> usize {
        self.apply_to_matching(channel, key, voice_id, |voice| voice.set_poly_pan(pan))
    }

    /// Route a PolyVolume event to the voices it addresses and say how many
    /// took it.
    ///
    /// A factor in the gain chain beside velocity and the lane's volume
    /// ([`Voice::gain`]), not a replacement for either.
    pub fn poly_volume(&mut self, channel: u8, key: u8, voice_id: Option<i32>, gain: f32) -> usize {
        self.apply_to_matching(channel, key, voice_id, |voice| voice.set_poly_volume(gain))
    }

    /// Apply `apply` to every live voice a note event addressed to
    /// `(channel, key, voice_id)` is about, and return how many there were.
    ///
    /// The one place per-note expression decides what a host meant, so that
    /// tuning, pan and volume can never route differently. The rule is
    /// [`Voice::matches`] — the host's voice id when the event carries one,
    /// else channel and key — which is the same predicate note-off uses
    /// ([`release`](Self::release)).
    ///
    /// Three things follow from "every" and are the reason this returns a
    /// count rather than the first voice it finds:
    ///
    /// - Lanes layer (a note-on starts a voice for *every* lane on
    ///   that key), so one expression event routinely moves several voices,
    ///   and an engine that stopped at the first would pitch one lane of a
    ///   stacked kick and leave the other behind.
    /// - A fading voice still takes it. It is still being mixed, so a
    ///   release or a choke must not freeze its pitch and pan mid-fade; and
    ///   bitstep's own ordering (expression at the note-on's timing) means
    ///   an event and a fade can share a sample offset.
    /// - A stolen voice parked in a tail does **not**: expression walks
    ///   [`voices_mut`](Self::voices_mut), the live region alone (module
    ///   docs, §"Stealing parks the voice it takes"). The host has already
    ///   been told that voice is gone, and its id may well have been handed
    ///   to the note that took its slot.
    ///
    /// The closure is monomorphised in and the pool is a fixed array, so
    /// this is a bounded walk with no allocation and no indirection — what
    /// the real-time rule asks of everything on this path.
    fn apply_to_matching(
        &mut self,
        channel: u8,
        key: u8,
        voice_id: Option<i32>,
        mut apply: impl FnMut(&mut Voice),
    ) -> usize {
        let mut matched = 0;
        for voice in self.voices_mut() {
            if voice.matches(channel, key, voice_id) {
                apply(voice);
                matched += 1;
            }
        }
        matched
    }

    /// Fade out every sounding voice in choke group `group`, and say how many
    /// there were.
    ///
    /// Group 0 is "no group" and chokes nothing: it is the lane default, so
    /// the common case is a sweep that fades nobody, and a kit where every
    /// lane is ungrouped must not choke itself to silence.
    ///
    /// Matching is the group number alone — **not** the lane. A closed hat cuts
    /// the open hat it shares a group with, and a lane alone in a group cuts
    /// its own earlier hits, which is what makes a group of one a self-choking
    /// lane. Nothing is excluded: the only voice a sweep must spare is the one
    /// being started, and
    /// [`start`](Self::start) arranges that by sweeping before it fills the
    /// slot.
    ///
    /// Choked voices fade over [`FADE_MS`] like every other early stop, and
    /// a voice already fading keeps the ramp it is on
    /// ([`Voice::begin_fade`]) — a second choke in the same group must not
    /// jump its gain back to full. A fading voice is still counted: the
    /// choke did address it.
    ///
    /// `pub` because the host's own choke event and the engine both reach
    /// the pool from outside, and because the tests read it directly.
    pub fn choke_group(&mut self, group: u8) -> usize {
        if group == 0 {
            return 0;
        }
        let mut choked = 0;
        for voice in self.voices_mut() {
            if voice.lane_choke == group {
                voice.begin_fade();
                choked += 1;
            }
        }
        choked
    }

    /// Fade out every sounding voice of lane `lane_id` ("Cut itself"), and
    /// say how many there were.
    ///
    /// The engine calls it on a note-on or an audition for a lane whose
    /// published `cut_itself` is true, **before** [`start`](Self::start), so
    /// the voice about to start is not yet in the pool to be cut. Matching
    /// is the lane id alone, independent of the choke group: a lane in a
    /// group cuts itself here and its group in `start`, and a voice both
    /// sweeps reach just keeps the ramp it is on ([`Voice::begin_fade`]).
    ///
    /// An early stop like a choke, so it fades over [`FADE_MS`] whatever the
    /// lane's release, and it cuts a one-shot that a note-off would not.
    /// Live voices only: a stolen voice in a tail is already on its way out.
    pub fn cut_lane(&mut self, lane_id: u32) -> usize {
        let mut cut = 0;
        for voice in self.voices_mut() {
            if voice.lane_id == lane_id {
                voice.begin_fade();
                cut += 1;
            }
        }
        cut
    }

    /// How many live voices there are: never more than [`MAX_VOICES`].
    ///
    /// Stolen voices still fading in the tail region are **not** counted —
    /// they are audio on its way out, not notes the pool can address. See
    /// [`tail_count`](Self::tail_count).
    pub fn active_count(&self) -> usize {
        self.voices().count()
    }

    /// How many stolen voices are finishing their fade in the tail region
    /// (module docs, §"Stealing parks the voice it takes"): never more than
    /// [`STEAL_TAILS`].
    pub fn tail_count(&self) -> usize {
        self.slots[MAX_VOICES..].iter().flatten().count()
    }

    /// The live voices, in slot order.
    pub fn voices(&self) -> impl Iterator<Item = &Voice> {
        self.slots[..MAX_VOICES].iter().flatten()
    }

    /// The live voices, mutably, in slot order — how the engine retunes,
    /// releases and chokes them.
    ///
    /// The live region only, deliberately: a stolen voice parked in a tail
    /// is already gone as far as the host and the engine are concerned
    /// (reported terminated, fading out), so a note-off, a choke or an
    /// expression event addressed to the note it was playing must not find
    /// it and must not be able to restart its ramp.
    pub fn voices_mut(&mut self) -> impl Iterator<Item = &mut Voice> {
        self.slots[..MAX_VOICES].iter_mut().flatten()
    }

    /// Mix every sounding voice into its output's pair, frames `span` of
    /// each, additively, and free the slots of the voices that ended inside
    /// it.
    ///
    /// The buffers are **not** cleared first: the caller owns what is already
    /// in them (the engine renders between event offsets, so one block is
    /// several of these calls into spans of the same outputs).
    ///
    /// Each voice goes into [`Outputs::pair_for`] its own
    /// [`output`](Voice::output) — the main output when the host left that
    /// one out — voice by voice, as before routing existed: nothing here
    /// allocates. A span past the end of a pair is cut to the pair (shortest
    /// buffer wins); with no pair at all the voice renders nothing and keeps
    /// its place.
    ///
    /// `master` is the host's smoothed master gain across this span of frames,
    /// the last factor of every voice's gain chain — so it applies to every
    /// pair alike. A
    /// [`MasterRamp`], because a block's gain is a line and not a number; a
    /// bare `f32` converts into a flat one, which is what a caller with
    /// nothing to ramp between passes.
    ///
    /// Lanes layer by summing here — two lanes on one note and one output
    /// are two voices adding into the same buffer, each at its own gain and
    /// pan and with no extra scaling applied to either.
    ///
    /// Tails are rendered with the live voices and in the same mix: a stolen
    /// voice's last 4 ms sound exactly as they would have from the slot it
    /// gave up, on the output it was on. A live voice that ends here is
    /// recorded in the terminated outbox ([`terminated`](Self::terminated));
    /// a tail is not, having been recorded at the steal that made it.
    pub fn render_routed(
        &mut self,
        outputs: &mut Outputs<'_>,
        span: Range<usize>,
        master: impl Into<MasterRamp>,
    ) {
        let master = master.into();
        // By index, not by iterator: reporting the ended voice needs the
        // pool again, and the borrow of the slot has to be over by then.
        for slot in 0..self.slots.len() {
            let Some(voice) = self.slots[slot].as_mut() else { continue };
            let sounding = match outputs.pair_for(voice.output) {
                Some((left, right)) => {
                    let end = span.end.min(left.len()).min(right.len());
                    let start = span.start.min(end);
                    voice.render_additive(&mut left[start..end], &mut right[start..end], master)
                }
                None => voice.render_additive(&mut [], &mut [], master),
            };
            if sounding {
                continue;
            }
            let ended = voice.identity();
            // Dropping the voice releases its sample reference; the
            // collector, on the writer side, does the actual freeing.
            self.slots[slot] = None;
            if slot < MAX_VOICES {
                self.report_terminated(ended);
            }
        }
    }

    /// Mix every sounding voice into one stereo pair, whatever its output:
    /// [`render_routed`](Self::render_routed) over the main output alone,
    /// the shape the pool's own tests (which are about voices, not routing)
    /// are written against.
    #[cfg(test)]
    pub fn render_additive(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        master: impl Into<MasterRamp>,
    ) {
        let frames = out_l.len().min(out_r.len());
        self.render_routed(&mut Outputs::main(out_l, out_r), 0..frames, master);
    }

    /// Silence the pool at once, without fades: every voice and every tail
    /// is dropped, and the terminated outbox goes with them.
    ///
    /// For the host's reset/deactivate path, where there is no block to fade
    /// into and nothing is listening anyway. Never used for a musical stop —
    /// those all go through
    /// [`Voice::begin_fade`].
    ///
    /// The outbox is emptied rather than filled: a reset is the host
    /// throwing away its own voice bookkeeping too, and there is no block
    /// left to send the events in.
    pub fn clear(&mut self) {
        for slot in self.slots.iter_mut() {
            *slot = None;
        }
        self.clear_terminated();
    }
}

impl Default for VoicePool {
    fn default() -> Self {
        Self::new()
    }
}

/// How far a voice's position moves per output frame, for a sample at
/// `sample_rate` played on a host running at `host_rate`, shifted by
/// `semis` semitones.
///
/// `(sample_rate / host_rate) * 2^(semis / 12)`: the first factor is the
/// rate conversion that replaces resampling at load time, the second is the
/// pitch shift. At matching rates and no tuning the result is exactly
/// `1.0`, and at +12 semitones exactly `2.0` — the exactness the
/// reproduce-the-file and twice-as-fast contracts rest on.
///
/// Both inputs are guarded rather than trusted: `host_rate` arrives from the
/// host and `semis` from per-note expression. A zero, negative or NaN host
/// rate would otherwise produce an infinite or NaN step, and a NaN step is
/// the one value that makes a voice immortal — every comparison against the
/// sample's end would be false. `host_rate` is floored at 1 Hz (absurd, but
/// finite and positive), and NaN compares that way round on purpose:
/// `f64::max` returns the non-NaN operand.
pub fn step_for(sample_rate: u32, host_rate: f64, semis: f32) -> f64 {
    // `max`-then-`min`, not `clamp`: the order *is* the NaN guard, since
    // `f64::max` returns the non-NaN operand while `clamp` hands a NaN back.
    #[allow(clippy::manual_clamp)]
    let host = host_rate.max(1.0).min(HOST_RATE_MAX);
    let source = sample_rate.max(1) as f64;
    (source / host) * 2f64.powf(clamp_semis(semis) as f64 / 12.0)
}

/// The full voice step with the lane's time settings:
/// `(sample_rate / host_rate) × speed × 2^((pitch_st + semis) / 12)`.
///
/// [`step_for`] with the lane's pitch added to the poly tuning (the sum is
/// clamped there, so the step stays finite and the voice mortal) and the
/// speed factor applied after. A speed of exactly 1.0 multiplies exactly,
/// so Off mode at pitch 0 keeps every v1 step bit for bit.
pub fn time_step_for(
    sample_rate: u32,
    host_rate: f64,
    speed: f64,
    pitch_st: f32,
    semis: f32,
) -> f64 {
    let semis = clamp_semis(pitch_st) + clamp_semis(semis);
    step_for(sample_rate, host_rate, semis) * clamp_speed(speed)
}

/// A time speed factor as a step may use it: within
/// [`SPEED_MIN`]..=[`SPEED_MAX`], NaN → 1.0 (the sample's own speed, as a
/// NaN tuning plays at pitch).
fn clamp_speed(speed: f64) -> f64 {
    if speed.is_nan() {
        1.0
    } else {
        speed.clamp(SPEED_MIN, SPEED_MAX)
    }
}

/// Tuning as a step may use it: finite and within
/// [`SEMIS_MIN`]..=[`SEMIS_MAX`].
///
/// NaN becomes 0.0 — the sample's own pitch — rather than an endpoint,
/// because a lost tuning value should leave the note sounding as written,
/// not eight octaves down (the same choice the model makes for a NaN
/// pan).
fn clamp_semis(semis: f32) -> f32 {
    if semis.is_nan() {
        0.0
    } else {
        semis.clamp(SEMIS_MIN, SEMIS_MAX)
    }
}

/// A lane's volume in decibels as a gain: `10^(volume_db / 20)`.
///
/// Exactly `1.0` at 0 dB — `powf(0.0)` is exact — which is what lets a lane
/// at its default volume carry the exactness contract of the module docs.
/// The dB value is clamped first, NaN included: it reaches the audio thread
/// through a published atomic that a hostile state file may have
/// filled.
pub fn db_to_gain(volume_db: f32) -> f32 {
    10f32.powf(clamp_volume_db(volume_db) / 20.0)
}

/// The constant-power pan law: the gains the left and right outputs get at a
/// pan of `pan` in −1.0..=1.0.
///
/// `angle = (pan + 1) · π/4`, left `cos`, right `sin`, so `l² + r²` is 1 at
/// every position: moving a voice across the image changes where it is, not
/// how loud. Centre is −3 dB a side rather than unity (module docs).
///
/// Both sides are floored at zero, which is not pedantry: f32's `FRAC_PI_2`
/// is not π/2, so the cosine at a hard right pan lands on −4.4e−8 instead of
/// on zero — a phase-inverted whisper 147 dB down. Removing it costs one
/// `max` and makes a hard pan mean *silent* on the far side, which is what a
/// hard pan is for. A NaN pan is centred, not thrown to an edge
/// ([`clamp_pan`]).
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let angle = (clamp_pan(pan) + 1.0) * core::f32::consts::FRAC_PI_4;
    (angle.cos().max(0.0), angle.sin().max(0.0))
}

/// A lane volume as [`db_to_gain`] may use it: finite and within
/// [`LANE_VOLUME_DB_MIN`]..=[`LANE_VOLUME_DB_MAX`].
///
/// NaN becomes 0 dB, the lane default (the same rule the model's
/// `set_volume_db` follows), which is also the neutral factor: a lost
/// number must not silence or boost a note it had nothing to do with.
///
/// Public, and the one definition of the rule: `model.rs` re-exports it for
/// [`crate::model::Model::set_volume_db`] rather than writing the clamp a
/// second time, the arrangement [`CHOKE_GROUPS`] and [`LaneMode`] have.
pub fn clamp_volume_db(volume_db: f32) -> f32 {
    if volume_db.is_nan() {
        0.0
    } else {
        volume_db.clamp(LANE_VOLUME_DB_MIN, LANE_VOLUME_DB_MAX)
    }
}

/// A gain factor as the chain may use it: finite and within
/// 0.0..=[`GAIN_MAX`]. For the master volume and PolyVolume, the factors
/// that arrive as gains.
///
/// NaN becomes 1.0, the neutral factor (see [`clamp_volume_db`]). The low
/// end is zero rather than negative: a negative gain is a phase inversion,
/// which is never what a volume control meant.
fn clamp_gain(gain: f32) -> f32 {
    if gain.is_nan() {
        1.0
    } else {
        gain.clamp(0.0, GAIN_MAX)
    }
}

/// A velocity as the chain may use it: finite and within 0.0..=1.0, the
/// range nice-plug documents `NoteOn::velocity` in. NaN becomes 1.0, as in
/// [`clamp_gain`].
fn clamp_velocity(velocity: f32) -> f32 {
    if velocity.is_nan() {
        1.0
    } else {
        velocity.clamp(0.0, 1.0)
    }
}

/// Clamp a choke group to 0..=[`CHOKE_GROUPS`], where 0 is no group.
///
/// Every number crossing into a voice is clamped at the boundary, and this one
/// is no exception even though an out-of-range group would only ever match
/// other voices carrying the same out-of-range group. What it buys is that
/// [`Voice::lane_choke`] always means what the model says a choke group means,
/// so a lane the model clamped to 16 and a snapshot that claimed 200 choke each
/// other instead of forming a private group the GUI cannot show.
///
/// Public for the reason [`clamp_volume_db`] is: `model.rs`'s `set_choke`
/// is this same clamp, not a copy of it.
pub fn clamp_choke(choke: u8) -> u8 {
    choke.min(CHOKE_GROUPS)
}

/// A pan as the law may use it: finite and within −1.0..=1.0.
///
/// NaN becomes 0.0 — centred — which is both the model's rule for a pan it
/// loads and the only safe one here: the `max`-then-`min` order that guards
/// the step math would make a NaN pan hard left.
///
/// Public for the reason [`clamp_volume_db`] is: `model.rs`'s `set_pan` is
/// this same clamp, not a copy of it.
pub fn clamp_pan(pan: f32) -> f32 {
    if pan.is_nan() {
        0.0
    } else {
        pan.clamp(-1.0, 1.0)
    }
}

/// How many whole frames of `sample` can be played: zero if it has no
/// channels, so that the callers' division by the channel count is always
/// safe.
fn playable_frames(sample: &SampleData) -> usize {
    match sample.channels as usize {
        0 => 0,
        channels => sample.frames.len() / channels,
    }
}

/// One channel of one source frame, interpolated: **cubic Hermite
/// (Catmull-Rom)** over the four frames around `position`.
///
/// `frames` is interleaved `channels` wide and `count` frames long.
/// Positions before the first frame and past the last read the edge frame
/// instead ("edge frames clamp to the sample bounds"), so the
/// first and last frames of a sample interpolate against themselves rather
/// than against whatever is next in memory.
///
/// The polynomial is written as `((a·t + b)·t + c)·t + y1`, which is not
/// just Horner's rule being tidy: the constant term *is* `y1`, so a
/// position with no fractional part returns the source frame bit for bit,
/// with no rounding to accumulate. That is what makes an untuned voice
/// reproduce its file exactly.
fn interpolate(
    frames: &[f32],
    channels: usize,
    count: usize,
    channel: usize,
    position: f64,
) -> f32 {
    let index = position.floor();
    let t = (position - index) as f32;
    let base = index as isize;
    let last = count as isize - 1;
    let at = |offset: isize| -> f32 {
        let frame = (base + offset).clamp(0, last) as usize;
        frames[frame * channels + channel]
    };

    let (y0, y1, y2, y3) = (at(-1), at(0), at(1), at(2));
    let a = 0.5 * (-y0 + 3.0 * y1 - 3.0 * y2 + y3);
    let b = 0.5 * (2.0 * y0 - 5.0 * y1 + 4.0 * y2 - y3);
    let c = 0.5 * (-y0 + y2);
    ((a * t + b) * t + c) * t + y1
}

#[cfg(test)]
mod tests {
    use basedrop::Collector;

    use super::*;

    /// A sample the audio thread could have been handed, and the collector
    /// that owns its memory.
    ///
    /// The collector is returned, not dropped, because it has to outlive
    /// every `Shared` made from it — exactly as in the plugin, where it
    /// lives behind the model's writer mutex.
    fn shared_sample(rate: u32, channels: u8, frames: Vec<f32>) -> (Collector, Shared<SampleData>) {
        let collector = Collector::new();
        let sample = SampleData { rate, channels, frames: frames.into_boxed_slice() };
        let shared = Shared::new(&collector.handle(), sample);
        (collector, shared)
    }

    /// A mono sample whose frames are 1.0, 2.0, … so that any frame can be
    /// told from any other by eye in a failure message.
    fn ramp(rate: u32, count: usize) -> (Collector, Shared<SampleData>) {
        shared_sample(rate, 1, (1..=count).map(|n| n as f32).collect())
    }

    /// A note-on with the lane defaults behind it: full velocity, 0 dB, centred
    /// — so, after the pan law, −3 dB a side.
    fn note_on(sample: Shared<SampleData>, semis: f32) -> VoiceStart {
        VoiceStart {
            lane_id: 1,
            note_key: 36,
            channel: 0,
            voice_id: None,
            sample,
            velocity: 1.0,
            semis,
            speed: 1.0,
            pitch_st: 0.0,
            lane_volume_db: 0.0,
            lane_pan: 0.0,
            lane_choke: 0,
            lane_mode: LaneMode::OneShot,
            attack_ms: 0.0,
            release_ms: 0.0,
            output: 0,
            rendered: false,
        }
    }

    /// The same note-on for a lane in gate mode, panned hard left so that the
    /// fade's own shape is what the left channel shows
    /// (see [`unity_note_on`]).
    fn gate_note_on(sample: Shared<SampleData>, semis: f32) -> VoiceStart {
        VoiceStart { lane_mode: LaneMode::Gate, ..unity_note_on(sample, semis) }
    }

    /// A mono sample of constant 1.0 frames: rendered hard left at unity, the
    /// output *is* the gain applied to it, so a fade ramp can be read
    /// straight out of the buffer.
    fn dc(rate: u32, count: usize) -> (Collector, Shared<SampleData>) {
        shared_sample(rate, 1, vec![1.0; count])
    }

    /// The same note-on panned hard left, where the pan law's left gain is
    /// exactly 1.0 and its right gain exactly 0.0.
    ///
    /// The one setting in which the mix multiplies by one, so it is the one
    /// the exactness contracts are written against — they assert equality
    /// with the file itself, which no other gain or pan could survive (module
    /// docs, §"Unity gain" is a pan position).
    fn unity_note_on(sample: Shared<SampleData>, semis: f32) -> VoiceStart {
        VoiceStart { lane_pan: -1.0, ..note_on(sample, semis) }
    }

    /// A note-on for a lane in choke group `group`, panned hard left at
    /// unity so a choke's own ramp can be read out of the left channel.
    ///
    /// One-shot, deliberately: a choke has to cut a voice that ignores
    /// note-off, which is the mode nearly every lane is in.
    fn choked_note_on(sample: Shared<SampleData>, group: u8) -> VoiceStart {
        VoiceStart { lane_choke: group, ..unity_note_on(sample, 0.0) }
    }

    /// A mono sample of silence: a voice that sounds (so it starts, and so
    /// it chokes its group) but adds nothing to the mix.
    ///
    /// It is what lets a choke be measured at all. The choking voice renders
    /// into the same buffer as the voice it is cutting, so a DC choker would
    /// add its own full-gain step to every frame of the ramp it is being
    /// tested against; a silent one leaves the buffer showing the fade and
    /// nothing else.
    fn silence(rate: u32, count: usize) -> (Collector, Shared<SampleData>) {
        shared_sample(rate, 1, vec![0.0; count])
    }

    /// The pan law's centre gain, −3 dB. Expectations multiply by this
    /// rather than spelling out 0.70710677: an f32 multiplication by the
    /// same factor gives the same bits on both sides of an `assert_eq!`.
    fn centre() -> f32 {
        pan_gains(0.0).0
    }

    /// The exactness contract: an untuned voice playing a file at the file's
    /// own rate *is* the file. Not close to it — equal, frame for frame, with
    /// the tail left silent.
    #[test]
    fn an_untuned_voice_at_the_files_rate_reproduces_the_file() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 12];
        let mut right = [0.0f32; 12];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(right, [0.0; 12], "a hard left pan leaves the right side silent");
        assert_eq!(pool.active_count(), 0, "the voice ended with the sample");
    }

    /// The other half of the contract: +12 semitones is twice the speed, so
    /// the voice reads every second frame — and reads it exactly, because
    /// the step is exactly 2.0 and the interpolator is transparent at whole
    /// positions.
    #[test]
    fn twelve_semitones_up_plays_twice_as_fast() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 12.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left, [1.0, 3.0, 5.0, 7.0, 0.0, 0.0, 0.0, 0.0]);
    }

    /// And down an octave is half the speed. The interpolated frames are
    /// the midpoints of a straight line, which Catmull-Rom reproduces
    /// exactly: it is a cubic fitted through the four neighbours, and for
    /// collinear neighbours that cubic *is* the line. Tested from frame 1
    /// onward, where all four neighbours are real frames — at frame 0 the
    /// clamped left neighbour bends the curve on purpose.
    #[test]
    fn twelve_semitones_down_plays_half_as_fast() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, -12.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 6];
        let mut right = [0.0f32; 6];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left[0], 1.0, "frame 0 is still the source frame");
        assert_eq!(left[2..6], [2.0, 2.5, 3.0, 3.5], "midpoints of a ramp are exact");
    }

    /// A 44.1 kHz sample on a 48 kHz host plays slower than its own rate,
    /// by exactly the rate ratio, with no tuning involved.
    #[test]
    fn the_host_rate_resamples_the_voice() {
        assert_eq!(step_for(44_100, 48_000.0, 0.0), 44_100.0 / 48_000.0);
        assert_eq!(step_for(48_000, 48_000.0, 0.0), 1.0, "matching rates are exactly unity");
        assert_eq!(step_for(96_000, 48_000.0, 0.0), 2.0);
        assert_eq!(step_for(48_000, 96_000.0, 0.0), 0.5);
    }

    /// The pitch factors are powers of two taken exactly, and tuning
    /// multiplies the rate ratio rather than replacing it.
    #[test]
    fn tuning_multiplies_the_rate_ratio() {
        assert_eq!(step_for(48_000, 48_000.0, 12.0), 2.0);
        assert_eq!(step_for(48_000, 48_000.0, -12.0), 0.5);
        assert_eq!(step_for(48_000, 48_000.0, 24.0), 4.0);
        assert_eq!(step_for(24_000, 48_000.0, 12.0), 1.0, "an octave up undoes a half-rate file");
        let seven = step_for(48_000, 48_000.0, 7.0);
        assert!((seven - 1.498_307).abs() < 1e-6, "a fifth up is ~1.4983, got {seven}");
    }

    /// Hostile numbers from the host and from per-note expression leave a
    /// finite, positive, *mortal* step — never NaN, which is the one value
    /// that would leave a voice sounding forever.
    #[test]
    fn step_math_survives_hostile_rates_and_tuning() {
        for host_rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY] {
            let step = step_for(48_000, host_rate, 0.0);
            assert!(step.is_finite() && step > 0.0, "host rate {host_rate} gave step {step}");
        }
        for semis in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 10_000.0, -10_000.0] {
            let step = step_for(48_000, 48_000.0, semis);
            assert!(step.is_finite() && step > 0.0, "tuning {semis} gave step {step}");
        }
        assert_eq!(step_for(48_000, 48_000.0, f32::NAN), 1.0, "NaN tuning plays at pitch");
        assert_eq!(
            step_for(48_000, 48_000.0, f32::INFINITY),
            step_for(48_000, 48_000.0, SEMIS_MAX)
        );
        assert_eq!(
            step_for(48_000, 48_000.0, f32::NEG_INFINITY),
            step_for(48_000, 48_000.0, SEMIS_MIN)
        );
        // A sample rate of zero cannot come out of `decode`, but the step
        // divides by nothing and the voice still has to end.
        assert!(step_for(0, 48_000.0, 0.0) > 0.0);
    }

    /// Retuning mid-note changes the speed from where the voice is, not
    /// where it started: the position survives, the step does not.
    #[test]
    fn retuning_keeps_the_position_and_recomputes_the_step() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [1.0, 2.0]);

        for voice in pool.voices_mut() {
            voice.set_semis(12.0, 48_000.0);
            assert_eq!(voice.position, 2.0, "retuning does not seek");
            assert_eq!(voice.step, 2.0);
        }

        let mut left = [0.0f32; 3];
        let mut right = [0.0f32; 3];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [3.0, 5.0, 7.0], "carries on from frame 2, two frames at a time");
    }

    /// The lane's time settings fold into the step: pitch +12 doubles it,
    /// speed 2 doubles it, both together quadruple it, and poly tuning still
    /// adds on top.
    #[test]
    fn set_time_scales_the_step_and_tuning_still_adds() {
        let (_collector, sample) = ramp(48_000, 64);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        let voice = pool.voices_mut().next().expect("started");
        assert_eq!(voice.step, 1.0, "speed 1, pitch 0: v1's step exactly");

        voice.set_time(1.0, 12.0, 48_000.0);
        assert_eq!(voice.step, 2.0, "pitch +12 doubles the step");
        voice.set_time(2.0, 0.0, 48_000.0);
        assert_eq!(voice.step, 2.0, "speed 2 doubles the step");
        voice.set_time(2.0, 12.0, 48_000.0);
        assert_eq!(voice.step, 4.0, "both combine");
        voice.set_time(0.5, -12.0, 48_000.0);
        assert_eq!(voice.step, 0.25, "and the same downwards");

        voice.set_time(1.0, 12.0, 48_000.0);
        voice.set_semis(12.0, 48_000.0);
        assert_eq!(voice.step, 4.0, "poly tuning adds to the lane pitch");
        voice.set_time(2.0, 12.0, 48_000.0);
        assert_eq!(voice.step, 8.0, "and survives a time change");
        voice.set_semis(-12.0, 48_000.0);
        assert_eq!(voice.step, 2.0, "and a retune keeps the time");
        assert_eq!(voice.position, 0.0, "none of it seeks");
    }

    /// A voice can start with its lane's time already applied, at another
    /// sample rate too: the rate conversion multiplies through.
    #[test]
    fn a_voice_starts_at_its_lane_time() {
        let (_collector, sample) = ramp(24_000, 64);
        let start = VoiceStart { speed: 2.0, pitch_st: 12.0, ..unity_note_on(sample, 12.0) };
        let voice = Voice::new(start, 48_000.0, 0);
        assert_eq!(voice.step, 0.5 * 2.0 * 4.0);
        assert_eq!(voice.step, time_step_for(24_000, 48_000.0, 2.0, 12.0, 12.0));
    }

    /// Hostile time values leave a finite, positive step: speed clamps to
    /// the time module's range (NaN plays at speed 1), pitch plus tuning to
    /// the tuning clamp.
    #[test]
    fn hostile_time_values_cannot_make_a_voice_immortal() {
        let hostile = [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -2.0, 1e300];
        for speed in hostile {
            let step = time_step_for(48_000, 48_000.0, speed, 0.0, 0.0);
            assert!(step.is_finite() && step > 0.0, "speed {speed} gave {step}");
        }
        assert_eq!(time_step_for(48_000, 48_000.0, f64::NAN, 0.0, 0.0), 1.0);
        assert_eq!(time_step_for(48_000, 48_000.0, 1e9, 0.0, 0.0), SPEED_MAX);
        assert_eq!(time_step_for(48_000, 48_000.0, 0.0, 0.0, 0.0), SPEED_MIN);
        for pitch in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30] {
            let step = time_step_for(48_000, 48_000.0, 1.0, pitch, 90.0);
            assert!(step.is_finite() && step > 0.0, "pitch {pitch} gave {step}");
            assert!(step <= step_for(48_000, 48_000.0, SEMIS_MAX), "the sum is clamped");
        }

        let (_collector, sample) = ramp(48_000, 4);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        let voice = pool.voices_mut().next().expect("started");
        voice.set_time(f64::NAN, f32::NAN, 48_000.0);
        assert_eq!((voice.speed, voice.pitch_st, voice.step), (1.0, 0.0, 1.0));
    }

    /// Tuning is clamped where it is stored, too, so a voice's own `semis`
    /// always agrees with the step cached beside it.
    #[test]
    fn a_voices_stored_tuning_is_clamped() {
        let (_collector, sample) = ramp(48_000, 4);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample, 5_000.0), 48_000.0).expect("pool is empty");

        let voice = pool.voices().next().expect("one voice");
        assert_eq!(voice.semis, SEMIS_MAX);
        assert_eq!(voice.step, step_for(48_000, 48_000.0, SEMIS_MAX));
    }

    /// Interpolation is transparent at whole positions and smooth between
    /// them, and the edges clamp instead of reading past the sample.
    ///
    /// The last frame is the interesting one: at position 3.5 of a 4-frame
    /// sample, two of the four neighbours are past the end, and a plain
    /// index would read another lane's audio or walk off the slice.
    #[test]
    fn interpolation_is_transparent_at_whole_frames_and_clamps_at_the_edges() {
        let frames = [1.0, 2.0, 3.0, 4.0];
        for (frame, expected) in frames.iter().enumerate() {
            assert_eq!(interpolate(&frames, 1, 4, 0, frame as f64), *expected);
        }
        assert_eq!(interpolate(&frames, 1, 4, 0, 1.5), 2.5);
        let past_the_end = interpolate(&frames, 1, 4, 0, 3.5);
        assert!(past_the_end > 4.0, "the clamped tail holds its value, got {past_the_end}");
        assert!(past_the_end.is_finite());
        // Before the first frame, the clamped left neighbour pulls the
        // curve towards frame 0 rather than towards silence.
        assert!(interpolate(&frames, 1, 4, 0, 0.5) > 1.0);
    }

    /// A stereo sample keeps its sides apart: the left output is the left
    /// channel and nothing else, at the centred pan law's gain.
    #[test]
    fn stereo_channels_stay_on_their_own_side() {
        let frames = vec![1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0];
        let (_collector, sample) = shared_sample(48_000, 2, frames);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);

        let c = centre();
        assert_eq!(left, [1.0 * c, 2.0 * c, 3.0 * c, 4.0 * c]);
        assert_eq!(right, [-c, -2.0 * c, -3.0 * c, -4.0 * c]);
    }

    /// Panning a stereo voice is a **balance**: a hard pan silences the far
    /// side rather than folding it into the near one, so a stereo sample panned
    /// left plays only its left channel.
    #[test]
    fn panning_a_stereo_voice_is_a_balance() {
        let frames = vec![1.0, -1.0, 2.0, -2.0, 3.0, -3.0, 4.0, -4.0];
        let (_collector, sample) = shared_sample(48_000, 2, frames);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "the near side is untouched");
        assert_eq!(right, [0.0; 4], "the far side is gone, not summed in");
    }

    /// Two voices on one sample sum, which is how lanes layer — and the mix is
    /// additive, so whatever was in the buffer before survives.
    #[test]
    fn voices_sum_into_the_buffer_additively() {
        let (_collector, sample) = ramp(48_000, 4);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample.clone(), 0.0), 48_000.0).expect("slot 0");
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("slot 1");

        let mut left = [10.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left, [12.0, 14.0, 16.0, 18.0]);
        assert_eq!(right, [0.0; 4], "both voices are panned hard left");
    }

    /// Rendering one block equals rendering it in halves, bit for bit. The pool
    /// half of that guarantee: a voice's position is the only thing carried
    /// across the seam, and nothing is recomputed per block.
    #[test]
    fn one_block_equals_two_halves() {
        let (_collector, sample) = ramp(44_100, 64);

        let whole = {
            let mut pool = VoicePool::new();
            pool.start(note_on(sample.clone(), 3.0), 48_000.0).expect("pool is empty");
            let mut left = [0.0f32; 32];
            let mut right = [0.0f32; 32];
            pool.render_additive(&mut left, &mut right, 0.75);
            (left, right)
        };

        let halves = {
            let mut pool = VoicePool::new();
            pool.start(note_on(sample, 3.0), 48_000.0).expect("pool is empty");
            let mut left = [0.0f32; 32];
            let mut right = [0.0f32; 32];
            let (left_a, left_b) = left.split_at_mut(16);
            let (right_a, right_b) = right.split_at_mut(16);
            pool.render_additive(left_a, right_a, 0.75);
            pool.render_additive(left_b, right_b, 0.75);
            (left, right)
        };

        assert_eq!(whole.0, halves.0);
        assert_eq!(whole.1, halves.1);
    }

    /// The pool is exactly [`MAX_VOICES`] deep and never grows: the 65th
    /// note-on does not overrun it and is not refused either — it takes the
    /// oldest voice's slot and leaves the voice it took fading in a tail.
    #[test]
    fn the_pool_holds_max_voices_and_steals_for_the_next() {
        let (_collector, sample) = ramp(48_000, 1_000);
        let mut pool = VoicePool::new();
        for voice in 0..MAX_VOICES {
            assert_eq!(pool.start(note_on(sample.clone(), 0.0), 48_000.0), Some(voice));
        }
        assert_eq!(pool.active_count(), MAX_VOICES);

        assert_eq!(
            pool.start(note_on(sample, 0.0), 48_000.0),
            Some(0),
            "the oldest voice's slot, not a refusal"
        );
        assert_eq!(pool.active_count(), MAX_VOICES, "still exactly MAX_VOICES live voices");
        assert_eq!(pool.tail_count(), 1, "and the stolen voice is fading out of the mix");
    }

    /// An ended voice's slot is reusable immediately, and the next voice
    /// takes the lowest free one.
    #[test]
    fn ended_voices_free_their_slots() {
        let (_collector, short) = ramp(48_000, 2);
        let (_long_collector, long) = ramp(48_000, 1_000);
        let mut pool = VoicePool::new();
        assert_eq!(pool.start(note_on(short, 0.0), 48_000.0), Some(0));
        assert_eq!(pool.start(note_on(long.clone(), 0.0), 48_000.0), Some(1));

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(pool.active_count(), 1, "the two-frame voice is gone");

        assert_eq!(pool.start(note_on(long, 0.0), 48_000.0), Some(0), "slot 0 came back");
    }

    /// Ages count up across the pool's whole life, so "oldest" means
    /// oldest, not "lowest slot" — the ordering a steal goes by.
    #[test]
    fn ages_count_up_per_voice_not_per_slot() {
        let (_collector, short) = ramp(48_000, 2);
        let (_long_collector, long) = ramp(48_000, 1_000);
        let mut pool = VoicePool::new();
        pool.start(note_on(short, 0.0), 48_000.0).expect("slot 0");
        pool.start(note_on(long.clone(), 0.0), 48_000.0).expect("slot 1");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        pool.start(note_on(long, 0.0), 48_000.0).expect("slot 0 again");

        let mut ages: Vec<u64> = pool.voices().map(|voice| voice.age).collect();
        ages.sort_unstable();
        assert_eq!(ages, vec![1, 2]);
    }

    /// `clear` silences everything at once, for the host's reset path.
    #[test]
    fn clearing_the_pool_drops_every_voice() {
        let (_collector, sample) = ramp(48_000, 1_000);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample.clone(), 0.0), 48_000.0).expect("slot 0");
        pool.start(note_on(sample, 0.0), 48_000.0).expect("slot 1");
        pool.clear();

        assert_eq!(pool.active_count(), 0);
        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [0.0; 4], "a cleared pool renders silence");
    }

    /// A sample with nothing to play never becomes a voice — the one
    /// `start` rejection that is not about capacity.
    #[test]
    fn a_sample_with_no_audio_starts_no_voice() {
        let (_collector, empty) = shared_sample(48_000, 1, vec![]);
        let (_no_channels_collector, no_channels) = shared_sample(48_000, 0, vec![1.0, 2.0]);
        let mut pool = VoicePool::new();

        assert_eq!(pool.start(note_on(empty, 0.0), 48_000.0), None);
        assert_eq!(pool.start(note_on(no_channels, 0.0), 48_000.0), None);
        assert_eq!(pool.active_count(), 0);
    }

    /// A voice is finished the frame after its last, and not before.
    #[test]
    fn a_voice_is_finished_once_it_passes_the_last_frame() {
        let (_collector, sample) = ramp(48_000, 3);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        pool.render_additive(&mut left, &mut right, 1.0);
        let voice = pool.voices().next().expect("two of three frames played");
        assert_eq!(voice.position, 2.0);
        assert!(!voice.is_finished(), "frame 2 is still to play");

        let mut left = [0.0f32; 1];
        let mut right = [0.0f32; 1];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [3.0]);
        assert_eq!(pool.active_count(), 0);
    }

    /// The gain chain, with nothing left out and nothing extra in: velocity ×
    /// lane volume × PolyVolume × master.
    ///
    /// The expectation is written as that same product in that same order,
    /// so it is bit-identical rather than merely close, and the rendered
    /// block is the product applied to the file.
    #[test]
    fn the_gain_chain_multiplies_velocity_lane_volume_poly_volume_and_master() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        let start =
            VoiceStart { velocity: 0.5, lane_volume_db: -6.0206, ..unity_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");
        for voice in pool.voices_mut() {
            voice.set_poly_volume(0.25);
        }

        let gain = 0.5 * db_to_gain(-6.0206) * 0.25 * 0.5;
        let voice = pool.voices().next().expect("one voice");
        assert_eq!(voice.gain(0.5), gain);
        assert_eq!(voice.channel_gains(0.5), (gain, 0.0), "hard left puts it all on one side");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 0.5);
        assert_eq!(left, [1.0 * gain, 2.0 * gain, 3.0 * gain, 4.0 * gain]);
        assert_eq!(right, [0.0; 4]);
    }

    /// Every factor pulls its weight: change one and the gain changes with
    /// it. A chain that quietly dropped a factor would still pass the
    /// product test above if the dropped factor happened to be 1.0.
    #[test]
    fn every_factor_moves_the_gain() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        let voice = pool.voices_mut().next().expect("one voice");

        assert_eq!(voice.gain(1.0), 1.0, "the lane defaults are unity gain");
        voice.velocity = 0.5;
        assert_eq!(voice.gain(1.0), 0.5, "velocity");
        voice.set_lane_volume_db(-6.0206);
        assert_eq!(voice.gain(1.0), 0.5 * db_to_gain(-6.0206), "lane volume");
        voice.set_lane_volume_db(0.0);
        voice.set_poly_volume(0.25);
        assert_eq!(voice.gain(1.0), 0.125, "PolyVolume");
        assert_eq!(voice.gain(0.5), 0.0625, "master");
    }

    /// Lane volume is decibels, converted by `10^(db/20)`: 0 dB
    /// is exactly unity, −6.0206 dB is half the amplitude, and the ends of
    /// the model's range are where the clamp puts them.
    #[test]
    fn lane_volume_is_decibels() {
        assert_eq!(db_to_gain(0.0), 1.0, "the default volume is exactly unity");
        assert!((db_to_gain(-6.0206) - 0.5).abs() < 1e-6, "got {}", db_to_gain(-6.0206));
        assert!((db_to_gain(6.0206) - 2.0).abs() < 1e-5, "got {}", db_to_gain(6.0206));
        assert!((db_to_gain(LANE_VOLUME_DB_MIN) - 0.001).abs() < 1e-8);
        assert!((db_to_gain(LANE_VOLUME_DB_MAX) - 3.981_072).abs() < 1e-5);
        assert_eq!(db_to_gain(-200.0), db_to_gain(LANE_VOLUME_DB_MIN), "the quiet end clamps");
        assert_eq!(db_to_gain(5_000.0), db_to_gain(LANE_VOLUME_DB_MAX), "the loud end clamps");
        assert_eq!(db_to_gain(f32::NEG_INFINITY), db_to_gain(LANE_VOLUME_DB_MIN));
        assert_eq!(db_to_gain(f32::NAN), 1.0, "a lost volume plays at 0 dB");
    }

    /// A voice's cached gain and the dB it was converted from never
    /// disagree, whatever the engine sets per block — the invariant that
    /// makes caching the conversion safe.
    #[test]
    fn a_lane_volume_change_updates_the_cached_gain() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        let voice = pool.voices_mut().next().expect("one voice");

        for volume_db in [0.0, -6.0206, 12.0, -60.0, 3.5, 5_000.0, f32::NAN, -0.0] {
            voice.set_lane_volume_db(volume_db);
            assert_eq!(voice.lane_volume, db_to_gain(voice.lane_volume_db), "db {volume_db}");
            assert!(voice.lane_volume_db >= LANE_VOLUME_DB_MIN);
            assert!(voice.lane_volume_db <= LANE_VOLUME_DB_MAX);
        }
    }

    /// The pan law is constant power everywhere, exact at the ends and −3 dB
    /// in the middle.
    #[test]
    fn the_pan_law_is_constant_power() {
        assert_eq!(pan_gains(-1.0), (1.0, 0.0), "hard left is exactly unity and silence");
        assert_eq!(pan_gains(1.0), (0.0, 1.0), "hard right is exactly silence and unity");

        let (left, right) = pan_gains(0.0);
        assert_eq!(left, right, "centre is symmetric");
        let half_power = core::f32::consts::FRAC_1_SQRT_2;
        assert!((left - half_power).abs() < 1e-6, "centre is −3 dB, got {left}");

        let mut previous = pan_gains(-1.0);
        for step in 0..=20 {
            let pan = -1.0 + step as f32 / 10.0;
            let (left, right) = pan_gains(pan);
            let power = left * left + right * right;
            assert!((power - 1.0).abs() < 1e-6, "pan {pan} has power {power}");
            assert!(left >= 0.0 && right >= 0.0, "pan {pan} inverts a side: {left}/{right}");
            assert!(left <= previous.0, "pan {pan} raised the left gain");
            assert!(right >= previous.1, "pan {pan} lowered the right gain");
            previous = (left, right);
        }

        assert_eq!(pan_gains(-9.0), pan_gains(-1.0), "the pan clamps");
        assert_eq!(pan_gains(f32::INFINITY), pan_gains(1.0));
        assert_eq!(pan_gains(f32::NAN), pan_gains(0.0), "a lost pan is centred");
    }

    /// Lane pan and PolyPan combine by **clamped sum** rather than by
    /// replacement, and the voice's cached law follows both.
    #[test]
    fn lane_pan_and_poly_pan_combine_by_clamped_sum() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        let voice = pool.voices_mut().next().expect("one voice");

        voice.set_lane_pan(-0.5);
        voice.set_poly_pan(0.5);
        assert_eq!((voice.pan_l, voice.pan_r), pan_gains(0.0), "equal and opposite cancel");

        voice.set_lane_pan(0.5);
        voice.set_poly_pan(0.8);
        assert_eq!((voice.pan_l, voice.pan_r), pan_gains(1.0), "1.3 clamps to hard right");

        voice.set_poly_pan(f32::NAN);
        assert_eq!(voice.poly_pan, 0.0, "a lost PolyPan leaves the lane's own pan");
        assert_eq!((voice.pan_l, voice.pan_r), pan_gains(0.5));

        voice.set_lane_pan(-9.0);
        assert_eq!(voice.lane_pan, -1.0, "the lane's pan is clamped where it is stored");
        assert_eq!((voice.pan_l, voice.pan_r), pan_gains(-1.0));
    }

    /// A mono sample feeds both channels **before** the pan law: centred, both
    /// sides get the same frame at the same −3 dB.
    #[test]
    fn a_mono_sample_feeds_both_channels_before_panning() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);

        let c = centre();
        assert_eq!(left, [1.0 * c, 2.0 * c, 3.0 * c, 4.0 * c]);
        assert_eq!(right, left, "the one channel reaches both sides");
    }

    /// Master volume scales the mix linearly and is clamped like every other
    /// factor on the way in.
    #[test]
    fn master_volume_scales_the_mix_linearly() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 0.25);
        assert_eq!(left, [0.25, 0.5, 0.75, 1.0]);

        let voice = pool.voices().next().expect("the voice is still sounding");
        assert_eq!(voice.gain(f32::NAN), 1.0, "a lost master plays at unity");
        assert_eq!(voice.gain(f32::INFINITY), GAIN_MAX, "and an absurd one stops at the ceiling");
        assert_eq!(voice.gain(-1.0), 0.0, "a negative master is silence, never an inversion");
    }

    /// A [`MasterRamp`] is the line the master gain is walked along:
    /// `start + step * frame`, with both ends guarded and the target landing on
    /// the frame *after* the span — the next block's frame 0, so two blocks in
    /// a row join up.
    #[test]
    fn a_master_ramp_walks_from_one_blocks_gain_to_the_nexts() {
        // A flat ramp is one value for however long it is asked for, and
        // takes the same guard every gain takes.
        let flat = MasterRamp::flat(0.5);
        assert_eq!(flat.at(0), 0.5);
        assert_eq!(flat.at(1_000_000), 0.5, "a flat ramp never drifts");
        assert_eq!(MasterRamp::flat(f32::NAN).at(7), 1.0, "a lost gain plays at unity");
        assert_eq!(MasterRamp::flat(-1.0).at(7), 0.0, "a negative gain is silence");
        assert_eq!(MasterRamp::flat(f32::INFINITY).at(7), GAIN_MAX, "and an absurd one stops");

        // The line itself, in exact f32 steps of a quarter.
        let up = MasterRamp::across(0.0, 1.0, 4);
        assert_eq!([up.at(0), up.at(1), up.at(2), up.at(3)], [0.0, 0.25, 0.5, 0.75]);
        assert_eq!(up.at(4), 1.0, "the target lands on the next block's first frame");

        // And seen from part way in, which is what an event inside a block
        // splits it into: the same line, not a new one.
        let rest = up.from_frame(2);
        assert_eq!([rest.at(0), rest.at(1)], [0.5, 0.75]);
        assert_eq!(rest.at(2), 1.0);

        // Nothing to ramp between, nothing to ramp over: both are flat, so
        // the steady state is exactly as precise as a single scalar was.
        assert_eq!(MasterRamp::across(0.5, 0.5, 64), MasterRamp::flat(0.5));
        assert_eq!(MasterRamp::across(1.0, 0.25, 0), MasterRamp::flat(0.25));

        // Hostile ends are clamped before the step is taken, so the line
        // between them is finite and stays in the range the mix accepts.
        let hostile = MasterRamp::across(f32::NAN, f32::INFINITY, 4);
        assert_eq!(hostile.at(0), 1.0, "NaN came in as unity");
        assert_eq!(hostile.at(4), GAIN_MAX, "and infinity as the ceiling");
        assert_eq!(hostile.at(1_000), GAIN_MAX, "past the span, still in range");
    }

    /// The mix takes the ramp a frame at a time: at unity and a hard pan every
    /// output frame is the file's frame times that frame's own point on the
    /// line.
    #[test]
    fn a_master_ramp_scales_the_mix_frame_by_frame() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        // 1.0 down to 0.5 over eight frames: a step of −0.0625, every value
        // of which is exact in f32, as is every product below.
        pool.render_additive(&mut left, &mut right, MasterRamp::across(1.0, 0.5, 8));

        assert_eq!(left, [1.0, 1.875, 2.625, 3.25, 3.75, 4.125, 4.375, 4.5]);
        assert_eq!(right, [0.0; 8], "a hard left voice leaves the right side silent");
    }

    /// Velocity is a gain factor like the others, clamped where it is
    /// stored so that the chain never has to look at it again.
    #[test]
    fn velocity_is_clamped_when_the_voice_starts() {
        let (_collector, sample) = ramp(48_000, 8);
        for (given, stored) in [(0.5, 0.5), (2.0, 1.0), (-1.0, 0.0), (f32::NAN, 1.0)] {
            let mut pool = VoicePool::new();
            let start = VoiceStart { velocity: given, ..unity_note_on(sample.clone(), 0.0) };
            pool.start(start, 48_000.0).expect("pool is empty");
            let voice = pool.voices().next().expect("one voice");
            assert_eq!(voice.velocity, stored, "velocity {given}");
            assert_eq!(voice.gain(1.0), stored, "velocity {given} reaches the gain");
        }
    }

    /// A silent voice is still a voice: zero gain must not stop its position
    /// advancing, or a note at −60 dB into a muted master would sound
    /// forever and hold its slot against every note after it.
    #[test]
    fn a_silent_voice_still_ends_on_time() {
        let (_collector, sample) = ramp(48_000, 4);
        let mut pool = VoicePool::new();
        let start = VoiceStart { velocity: 0.0, ..unity_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 0.0);

        assert_eq!(left, [0.0; 4], "silent");
        assert_eq!(pool.active_count(), 0, "and finished, on the frame it should be");
    }

    /// Nothing a host or a hostile state file can say to the gain chain puts
    /// a NaN or an infinity into the output buffer — the one kind of damage
    /// a mix cannot recover from, since it spreads to every voice summed
    /// after it and stays until the host reloads the plugin.
    #[test]
    fn hostile_gains_and_pans_cannot_poison_the_mix() {
        let (_collector, sample) = ramp(48_000, 8);
        let hostile = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30];
        for velocity in hostile {
            for volume_db in hostile {
                for pan in hostile {
                    for master in hostile {
                        let mut pool = VoicePool::new();
                        let start = VoiceStart {
                            velocity,
                            lane_volume_db: volume_db,
                            lane_pan: pan,
                            ..note_on(sample.clone(), 0.0)
                        };
                        pool.start(start, 48_000.0).expect("pool is empty");
                        for voice in pool.voices_mut() {
                            voice.set_poly_volume(volume_db);
                            voice.set_poly_pan(pan);
                        }

                        let mut left = [0.0f32; 4];
                        let mut right = [0.0f32; 4];
                        pool.render_additive(&mut left, &mut right, master);
                        for (l, r) in left.iter().zip(right.iter()) {
                            assert!(
                                l.is_finite() && r.is_finite(),
                                "{l}/{r} from {velocity}/{volume_db}/{pan}/{master}"
                            );
                            // Four frames of a ramp at the chain's ceiling
                            // are nowhere near this; bounded is the point.
                            assert!(
                                l.abs() < 1_024.0 && r.abs() < 1_024.0,
                                "{l}/{r} from {velocity}/{volume_db}/{pan}/{master}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// A lane plays one-shots unless it says otherwise, and a voice carries the
    /// mode its lane had at the note-on.
    #[test]
    fn a_lane_is_one_shot_unless_it_says_otherwise() {
        assert_eq!(LaneMode::default(), LaneMode::OneShot);

        let (_collector, sample) = ramp(48_000, 4);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample.clone(), 0.0), 48_000.0).expect("slot 0");
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("slot 1");

        let modes: Vec<LaneMode> = pool.voices().map(|voice| voice.lane_mode).collect();
        assert_eq!(modes, vec![LaneMode::Gate, LaneMode::OneShot]);
        assert!(pool.voices().all(|voice| !voice.is_fading()), "a new voice is not fading");
    }

    /// The headline rule of the note events: **a one-shot ignores
    /// note-off**. It plays the whole file however short the note was, which
    /// is the difference between a sample player and a synth — a kick
    /// triggered by a one-tick step is a whole kick.
    #[test]
    fn a_one_shot_voice_ignores_note_off_and_plays_to_the_end() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(pool.release(0, 36, None), 0, "a one-shot answers no note-off");
        assert!(pool.voices().all(|voice| !voice.is_fading()), "and starts no fade");

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(
            left,
            [3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0],
            "the rest of the file, exactly as if no note-off had arrived"
        );
        assert_eq!(pool.active_count(), 0, "it ended with the sample, not with the note");
    }

    /// A gate voice ends with its note instead: note-off starts the fade,
    /// the fade is a straight line from full gain to silence, and the voice
    /// is gone on the frame the line reaches zero — not one frame later.
    #[test]
    fn a_gate_voice_fades_out_at_note_off_within_the_fades_length() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, frames * 4);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        assert_eq!(pool.release(0, 36, None), 1, "the gate voice answers the note-off");
        assert!(pool.voices().all(Voice::is_fading));

        let mut left = vec![0.0f32; frames + 1];
        let mut right = vec![0.0f32; frames + 1];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left[0], 1.0, "the ramp starts at full gain: no step on the way in");
        for (index, rendered) in left[..frames].iter().enumerate() {
            let expected = (frames - index) as f32 / frames as f32;
            assert_eq!(*rendered, expected, "frame {index} of the ramp");
        }
        assert_eq!(left[frames], 0.0, "and the frame after the ramp is silent");
        assert_eq!(right, vec![0.0; frames + 1], "hard left throughout");
        assert_eq!(pool.active_count(), 0, "the voice is gone with sample to spare");
    }

    /// The fade is [`FADE_MS`] long at whatever rate the host runs at, and
    /// stays inside 3–5 ms however the frame count rounds. A
    /// hostile rate still leaves a ramp, because a fade of no frames is the
    /// cut the fade exists to replace.
    #[test]
    fn the_fade_is_four_milliseconds_at_any_host_rate() {
        assert_eq!(FADE_MS, 4.0, "the value picked from the 3–5 ms range");
        assert_eq!(fade_frames_for(48_000.0), 192);
        assert_eq!(fade_frames_for(44_100.0), 177, "176.4 frames rounds up, never down");

        for rate in [8_000.0, 22_050.0, 44_100.0, 48_000.0, 88_200.0, 96_000.0, 192_000.0] {
            let ms = fade_frames_for(rate) as f64 / rate * 1000.0;
            assert!((3.0..=5.0).contains(&ms), "rate {rate} fades for {ms} ms");
        }
        for rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY, 1e300] {
            assert!(fade_frames_for(rate) >= 1, "hostile rate {rate} left no fade");
        }
    }

    /// The anti-click contract, read off a DC sample where every output frame
    /// *is* the gain: no two frames of a release differ by more than one step
    /// of the ramp — not at the step into the fade, not along it, and not at
    /// the step into silence, which is the one a hard stop would turn into a
    /// full-scale click.
    #[test]
    fn a_gate_release_never_steps_by_more_than_one_frame_of_the_ramp() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, frames * 4);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        // Some full-gain audio first, so the seam at the note-off is in the
        // buffer too and not just the ramp.
        let mut left = vec![0.0f32; frames * 2];
        let mut right = vec![0.0f32; frames * 2];
        let (head_l, tail_l) = left.split_at_mut(frames / 2);
        let (head_r, tail_r) = right.split_at_mut(frames / 2);
        pool.render_additive(head_l, head_r, 1.0);
        pool.release(0, 36, None);
        pool.render_additive(tail_l, tail_r, 1.0);

        // One step of the ramp, plus one f32 ulp: two neighbouring ratios are
        // each rounded to f32, so their difference can miss `1 / frames` by
        // that much and no more.
        let step = 1.0 / frames as f32;
        let threshold = step + f32::EPSILON;
        for (index, pair) in left.windows(2).enumerate() {
            let jump = (pair[1] - pair[0]).abs();
            assert!(
                jump <= threshold,
                "frames {index}..{} jump by {jump}, a step is {step}",
                index + 1
            );
            assert!(pair[1] <= pair[0], "frame {index} got louder during a release");
        }
        assert_eq!(left[frames / 2], 1.0, "the note-off itself is not a step");
        assert_eq!(left[frames / 2 + frames], 0.0, "and the ramp is over a fade later");
        assert_eq!(pool.active_count(), 0);
    }

    /// A second note-off during a fade leaves the ramp where it is — as a
    /// second choke does, in
    /// [`choking_a_fading_voice_does_not_restart_its_ramp`]. Restarting it
    /// would jump the gain back to full: the one discontinuity a fade must
    /// never introduce itself.
    #[test]
    fn releasing_a_fading_voice_again_does_not_restart_the_ramp() {
        let frames = fade_frames_for(48_000.0);
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        pool.release(0, 36, None);

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(pool.release(0, 36, None), 1, "it still answers the note-off");
        let voice = pool.voices().next().expect("still fading, not restarted");
        assert_eq!(voice.fade, FadeState::FadingOut { frames_left: frames - 8, frames });

        let mut next_l = [0.0f32; 1];
        let mut next_r = [0.0f32; 1];
        pool.render_additive(&mut next_l, &mut next_r, 1.0);
        assert!(next_l[0] < left[7], "the gain only ever goes down: {} vs {}", next_l[0], left[7]);
    }

    /// A gate voice whose sample runs out mid-fade ends with the sample. The
    /// two end conditions are independent, and whichever comes first wins.
    #[test]
    fn a_gate_voice_whose_sample_runs_out_first_ends_with_the_sample() {
        let (_collector, sample) = ramp(48_000, 4);
        let frames = fade_frames_for(48_000.0) as f32;
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        pool.release(0, 36, None);

        let mut left = [0.0f32; 6];
        let mut right = [0.0f32; 6];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left[0], 1.0, "the first frame of a fade is at full gain");
        assert_eq!(left[3], 4.0 * ((frames - 3.0) / frames), "the fourth is three steps down");
        assert_eq!(left[4..], [0.0, 0.0], "and there is no fifth frame to fade");
        assert_eq!(pool.active_count(), 0);
    }

    /// Note-off reaches the voices the host meant: the ones carrying its voice
    /// id when it sent one, and otherwise the ones on its channel and key.
    /// Several may match at once — lanes layer, and one key can have started a
    /// voice per lane.
    #[test]
    fn a_note_off_matches_by_voice_id_when_it_has_one_else_by_channel_and_key() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        let seven = VoiceStart { voice_id: Some(7), ..gate_note_on(sample.clone(), 0.0) };
        let eight = VoiceStart { voice_id: Some(8), ..gate_note_on(sample.clone(), 0.0) };
        let other_key = VoiceStart { note_key: 40, ..gate_note_on(sample.clone(), 0.0) };
        let other_channel = VoiceStart { channel: 3, ..gate_note_on(sample, 0.0) };
        pool.start(seven, 48_000.0).expect("slot 0");
        pool.start(eight, 48_000.0).expect("slot 1");
        pool.start(other_key, 48_000.0).expect("slot 2");
        pool.start(other_channel, 48_000.0).expect("slot 3");

        assert_eq!(pool.release(0, 36, Some(7)), 1, "an id picks out exactly its own voice");
        let fading: Vec<bool> = pool.voices().map(Voice::is_fading).collect();
        assert_eq!(fading, vec![true, false, false, false]);

        // With no id, channel and key decide — which is both id-carrying
        // voices, the one already fading included.
        assert_eq!(pool.release(0, 36, None), 2, "channel 0 key 36 is two of these voices");
        let fading: Vec<bool> = pool.voices().map(Voice::is_fading).collect();
        assert_eq!(fading, vec![true, true, false, false], "key 40 and channel 3 are other notes");

        assert_eq!(pool.release(3, 36, None), 1, "the channel is part of the match");
        assert_eq!(pool.release(0, 41, None), 0, "and so is the key");
    }

    /// A voice with no id of its own — an audition from the GUI — is still
    /// matched by channel and key by an event that carries one.
    /// An id it was never given cannot identify it, and the alternative is a
    /// held audition no note-off can end.
    #[test]
    fn a_voice_with_no_id_is_still_matched_by_channel_and_key() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        assert_eq!(pool.release(0, 36, Some(7)), 1, "an id it never had cannot exclude it");
        assert_eq!(pool.release(0, 37, Some(7)), 0, "the key still has to agree");
    }

    /// A wildcard in the **event** is match-any: a host's "all notes off" names
    /// no key, no channel and no voice id, and has to reach every voice rather
    /// than none. The two fields are independent, so a wildcard key on a real
    /// channel releases that channel alone and leaves the others sounding.
    #[test]
    fn a_wildcard_key_or_channel_in_an_event_matches_any_voice() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        let other_key = VoiceStart { note_key: 40, ..gate_note_on(sample.clone(), 0.0) };
        let other_channel = VoiceStart { channel: 3, ..gate_note_on(sample.clone(), 0.0) };
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("slot 0: channel 0, key 36");
        pool.start(other_key, 48_000.0).expect("slot 1");
        pool.start(other_channel, 48_000.0).expect("slot 2");

        // Any key, this channel: the channel-3 voice is another note's.
        assert_eq!(pool.release(0, KEY_WILDCARD, None), 2, "both voices on channel 0");
        let fading: Vec<bool> = pool.voices().map(Voice::is_fading).collect();
        assert_eq!(fading, vec![true, true, false], "channel 3 was not asked");

        // Any channel, this key: the channel-3 voice and the channel-0 one
        // already fading, which a second release still counts (it took the
        // event; its ramp is left alone).
        assert_eq!(pool.release(CHANNEL_WILDCARD, 36, None), 2, "key 36 wherever it sounds");
        assert!(pool.voices().all(Voice::is_fading), "and now everything is released");

        // Both at once — the host's panic button — reaches the lot. And the
        // wildcard is still only one value: an event naming a real key has
        // to find that key, wherever it looks for it.
        assert_eq!(pool.release(CHANNEL_WILDCARD, KEY_WILDCARD, None), 3, "every voice");
        assert_eq!(pool.release(CHANNEL_WILDCARD, 41, None), 0, "no voice is on key 41");
    }

    /// A fade crossing a block boundary renders bit-identically to the same
    /// fade inside one block: the ramp is counted in frames rendered, so the
    /// seam is not an event.
    #[test]
    fn a_fade_stitches_exactly_across_a_block_boundary() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, frames * 4);
        let span = frames + 8;

        let render = |halves: bool| {
            let mut pool = VoicePool::new();
            pool.start(gate_note_on(sample.clone(), 0.0), 48_000.0).expect("pool is empty");
            pool.release(0, 36, None);
            let mut left = vec![0.0f32; span];
            let mut right = vec![0.0f32; span];
            if halves {
                let split = frames / 3;
                let (left_a, left_b) = left.split_at_mut(split);
                let (right_a, right_b) = right.split_at_mut(split);
                pool.render_additive(left_a, right_a, 1.0);
                pool.render_additive(left_b, right_b, 1.0);
            } else {
                pool.render_additive(&mut left, &mut right, 1.0);
            }
            (left, right)
        };

        assert_eq!(render(false), render(true));
    }

    /// Attack 0 is v1, bit for bit. The same exact-math render with
    /// `attack_ms: 0.0` spelled out reproduces the file, and the voice carries
    /// no ramp at all.
    #[test]
    fn attack_zero_renders_bit_identically_to_v1() {
        let (_collector, sample) = ramp(48_000, 8);
        let mut pool = VoicePool::new();
        let start = VoiceStart { attack_ms: 0.0, ..unity_note_on(sample.clone(), 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");
        assert_eq!(pool.voices().next().expect("one voice").attack, Ramp::default());

        let mut left = [0.0f32; 12];
        let mut right = [0.0f32; 12];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(right, [0.0; 12]);

        // And at a centred pan, a pitch and a ramping master — every factor
        // of the chain not at one — attack 0 still matches the v1 formula
        // `l * fade * gain_l` exactly.
        let (_collector, long) = ramp(44_100, 64);
        let mut pool = VoicePool::new();
        pool.start(note_on(long.clone(), 3.0), 48_000.0).expect("pool is empty");
        let master = MasterRamp::across(0.5, 0.9, 32);
        let mut left = [0.0f32; 32];
        let mut right = [0.0f32; 32];
        pool.render_additive(&mut left, &mut right, master);

        let mut reference = Voice::new(note_on(long, 3.0), 48_000.0, 0);
        let frames = playable_frames(&reference.sample);
        for frame in 0..32 {
            let (gain_l, gain_r) = reference.channel_gains(master.at(frame));
            let l = interpolate(&reference.sample.frames, 1, frames, 0, reference.position);
            assert_eq!(left[frame], l * 1.0 * gain_l, "left frame {frame}");
            assert_eq!(right[frame], l * 1.0 * gain_r, "right frame {frame}");
            reference.position += reference.step;
        }
    }

    /// An attack is a linear fade-in from silence over `attack_ms` of host
    /// frames — 10 ms at 48 kHz is 480 frames, frame *k* is mixed at `k / 480`,
    /// and frame 480 is at full gain.
    #[test]
    fn a_ten_ms_attack_at_48k_reaches_full_gain_at_frame_480_along_a_line() {
        assert_eq!(attack_frames_for(10.0, 48_000.0), 480);
        let (_collector, sample) = dc(48_000, 2_000);
        let mut pool = VoicePool::new();
        let start = VoiceStart { attack_ms: 10.0, ..unity_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");

        let mut left = vec![0.0f32; 1_000];
        let mut right = vec![0.0f32; 1_000];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(left[0], 0.0, "the attack starts from silence");
        for (frame, &value) in left.iter().enumerate().take(480) {
            assert_eq!(value, frame as f32 / 480.0, "frame {frame} is on the line");
            assert!(value < 1.0, "frame {frame} is still ramping");
        }
        assert!(left[480..].iter().all(|&v| v == 1.0), "full gain from frame 480 on");
        assert!(right.iter().all(|&v| v == 0.0), "hard left");
        // The steps are all one size: a line, not a curve.
        let step = left[1] - left[0];
        for pair in left[..=480].windows(2) {
            assert!((pair[1] - pair[0] - step).abs() < 1e-6);
        }
        assert!(pool.voices().next().expect("still sounding").attack.is_done());
    }

    /// The attack is counted in frames rendered, not blocks, so a voice
    /// mid-ramp renders the same whether its block is whole or cut into
    /// pieces that land anywhere inside the ramp.
    #[test]
    fn an_attack_stitches_exactly_across_block_boundaries() {
        let (_collector, sample) = ramp(44_100, 4_000);
        let render = |pieces: &[usize]| {
            let mut pool = VoicePool::new();
            let start = VoiceStart { attack_ms: 10.0, ..note_on(sample.clone(), 2.0) };
            pool.start(start, 48_000.0).expect("pool is empty");
            let mut left = vec![0.0f32; 1_024];
            let mut right = vec![0.0f32; 1_024];
            let mut at = 0;
            for &len in pieces {
                pool.render_additive(&mut left[at..at + len], &mut right[at..at + len], 0.8);
                at += len;
            }
            assert_eq!(at, 1_024);
            (left, right)
        };
        let whole = render(&[1_024]);
        assert_eq!(whole, render(&[512, 512]));
        assert_eq!(whole, render(&[64; 16]));
        assert_eq!(whole, render(&[1, 239, 240, 1, 479, 64]));
    }

    /// A released note fades from wherever its attack had got to: the two
    /// ramps multiply, so there is no jump at the release and the voice
    /// still ends after its fade.
    #[test]
    fn releasing_mid_attack_fades_from_the_current_gain_without_a_jump() {
        let (_collector, sample) = dc(48_000, 4_000);
        let mut pool = VoicePool::new();
        let start = VoiceStart { attack_ms: 10.0, ..gate_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");
        let mut left = vec![0.0f32; 1_000];
        let mut right = vec![0.0f32; 1_000];
        pool.render_additive(&mut left[..100], &mut right[..100], 1.0);
        pool.release(0, 36, None);
        pool.render_additive(&mut left[100..], &mut right[100..], 1.0);

        let largest = left.windows(2).map(|p| (p[1] - p[0]).abs()).fold(0.0f32, f32::max);
        assert!(largest <= 1.0 / 192.0 + 1e-6, "largest step {largest}");
        let fade = fade_frames_for(48_000.0) as usize;
        assert!(left[100 + fade..].iter().all(|&v| v == 0.0));
        assert_eq!(pool.active_count(), 0);
    }

    /// Every frame-to-frame step of `left` is within one step of a
    /// [`FADE_MS`] ramp at 48 kHz (plus an f32 ulp) — the v1 click
    /// threshold of [`a_gate_release_never_steps_by_more_than_one_frame_of_the_ramp`]
    /// — and the gain never rises.
    fn assert_no_click(left: &[f32]) {
        let threshold = 1.0 / fade_frames_for(48_000.0) as f32 + f32::EPSILON;
        for (index, pair) in left.windows(2).enumerate() {
            let jump = (pair[1] - pair[0]).abs();
            assert!(jump <= threshold, "frames {index}..{} jump by {jump}", index + 1);
            assert!(pair[1] <= pair[0], "frame {index} got louder during a fade");
        }
    }

    /// A gate note-off fades over the lane's release — 100 ms at 48 kHz is 4800
    /// frames along a line, silent on frame 4800 and the voice gone — with no
    /// step above the v1 threshold.
    #[test]
    fn a_hundred_ms_gate_release_fades_over_4800_frames_at_48k() {
        assert_eq!(fade_frames_for_ms(100.0, 48_000.0), 4_800);
        let (_collector, sample) = dc(48_000, 20_000);
        let mut pool = VoicePool::new();
        let start = VoiceStart { release_ms: 100.0, ..gate_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");

        let mut left = vec![0.0f32; 6_000];
        let mut right = vec![0.0f32; 6_000];
        pool.render_additive(&mut left[..100], &mut right[..100], 1.0);
        assert_eq!(pool.release(0, 36, None), 1);
        assert_eq!(
            pool.voices().next().expect("fading").fade,
            FadeState::FadingOut { frames_left: 4_800, frames: 4_800 }
        );
        pool.render_additive(&mut left[100..], &mut right[100..], 1.0);

        assert_eq!(left[100], 1.0, "the note-off itself is not a step");
        for k in 0..4_800 {
            assert_eq!(left[100 + k], (4_800 - k) as f32 / 4_800.0, "frame {k} of the release");
        }
        assert!(left[100 + 4_799] > 0.0, "still sounding on the last frame of the ramp");
        assert!(left[100 + 4_800..].iter().all(|&v| v == 0.0), "silent from frame 4800");
        assert_eq!(pool.active_count(), 0, "and the voice is gone");
        assert_no_click(&left);
    }

    /// Release 0 is the v1 fade, [`FADE_MS`], not a cut — and so is a
    /// release shorter than it, or a hostile one clamped to the lane range.
    #[test]
    fn a_zero_release_fades_over_fade_ms() {
        let fade = fade_frames_for(48_000.0);
        assert_eq!(fade_frames_for_ms(0.0, 48_000.0), fade);
        assert_eq!(fade_frames_for_ms(1.0, 48_000.0), fade, "never shorter than the fade");
        assert_eq!(fade_frames_for_ms(-5.0, 48_000.0), fade);
        assert_eq!(fade_frames_for_ms(f32::NAN, 48_000.0), 14_400, "NaN is the 300 ms default");
        assert_eq!(fade_frames_for_ms(f32::INFINITY, 48_000.0), 240_000, "5 s at most");
        assert_eq!(fade_frames_for_ms(0.0, f64::NAN), fade_frames_for(f64::NAN));

        let (_collector, sample) = dc(48_000, 4_000);
        let mut pool = VoicePool::new();
        let start = VoiceStart { release_ms: 0.0, ..gate_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");
        let mut left = vec![0.0f32; 400];
        let mut right = vec![0.0f32; 400];
        pool.render_additive(&mut left[..8], &mut right[..8], 1.0);
        pool.release(0, 36, None);
        pool.render_additive(&mut left[8..], &mut right[8..], 1.0);

        let fade = fade as usize;
        assert!(left[8 + fade - 1] > 0.0);
        assert!(left[8 + fade..].iter().all(|&v| v == 0.0), "silent a FADE_MS later");
        assert_eq!(pool.active_count(), 0);
        assert_no_click(&left);
    }

    /// A one-shot ignores note-off whatever its release says.
    #[test]
    fn a_one_shot_ignores_its_release() {
        let (_collector, sample) = dc(48_000, 1_000);
        let mut pool = VoicePool::new();
        let start = VoiceStart { release_ms: 100.0, ..unity_note_on(sample, 0.0) };
        pool.start(start, 48_000.0).expect("pool is empty");
        assert_eq!(pool.release(0, 36, None), 0);
        assert_eq!(pool.voices().next().expect("sounding").fade, FadeState::None);
    }

    /// Choke still fades over [`FADE_MS`] on a lane with a long release —
    /// both a voice at full gain and one already part-way down its release,
    /// whose long ramp is cut short from the gain it had reached.
    #[test]
    fn a_choke_still_fades_over_fade_ms_with_a_long_release() {
        let fade = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, 20_000);
        let (_silent_collector, quiet) = silence(48_000, 20_000);
        let gate = |group| VoiceStart {
            release_ms: 100.0,
            lane_mode: LaneMode::Gate,
            ..choked_note_on(sample.clone(), group)
        };

        // At full gain: the choke is a plain FADE_MS ramp.
        let mut pool = VoicePool::new();
        pool.start(gate(1), 48_000.0).expect("pool is empty");
        let mut left = vec![0.0f32; 1_000];
        let mut right = vec![0.0f32; 1_000];
        pool.render_additive(&mut left[..10], &mut right[..10], 1.0);
        pool.start(VoiceStart { lane_id: 2, ..choked_note_on(quiet.clone(), 1) }, 48_000.0);
        pool.render_additive(&mut left[10..], &mut right[10..], 1.0);
        assert!(left[10 + fade - 1] > 0.0);
        assert!(left[10 + fade..].iter().all(|&v| v == 0.0), "choked within FADE_MS");
        assert_no_click(&left);

        // Mid-release (1200 of 4800 frames in, gain 0.75): the choke takes
        // over at 0.75 and is silent a FADE_MS later.
        let mut pool = VoicePool::new();
        pool.start(gate(1), 48_000.0).expect("pool is empty");
        let mut left = vec![0.0f32; 2_000];
        let mut right = vec![0.0f32; 2_000];
        pool.release(0, 36, None);
        pool.render_additive(&mut left[..1_200], &mut right[..1_200], 1.0);
        pool.start(VoiceStart { lane_id: 2, ..choked_note_on(quiet, 1) }, 48_000.0);
        let choked = pool.voices().find(|v| v.lane_id == 1).expect("fading").fade;
        assert_eq!(choked, FadeState::FadingOut { frames_left: 144, frames: 192 });
        pool.render_additive(&mut left[1_200..], &mut right[1_200..], 1.0);
        assert_eq!(left[1_200], 0.75, "no jump where the choke takes over");
        assert!(left[1_200 + 144..].iter().all(|&v| v == 0.0), "silent within FADE_MS");
        assert_no_click(&left);
    }

    /// Steal still fades over [`FADE_MS`] on a lane with a long release:
    /// the stolen voice's tail is gone a FADE_MS after the steal, whether it
    /// was at full gain or releasing.
    #[test]
    fn a_steal_still_fades_over_fade_ms_with_a_long_release() {
        let fade = fade_frames_for(48_000.0);
        let (_collector, sample) = dc(48_000, 20_000);
        let (_silent_collector, quiet) = silence(48_000, 20_000);
        for releasing in [false, true] {
            let mut pool = VoicePool::new();
            let oldest = VoiceStart { release_ms: 100.0, ..gate_note_on(sample.clone(), 0.0) };
            pool.start(oldest, 48_000.0).expect("pool is empty");
            for key in 1..MAX_VOICES {
                let filler = VoiceStart { note_key: key as u8, ..note_on(quiet.clone(), 0.0) };
                pool.start(filler, 48_000.0).expect("room");
            }
            let mut left = vec![0.0f32; 1_000];
            let mut right = vec![0.0f32; 1_000];
            if releasing {
                pool.release(0, 36, None);
            }
            pool.render_additive(&mut left[..480], &mut right[..480], 1.0);
            let thief = VoiceStart { note_key: 100, ..note_on(quiet.clone(), 0.0) };
            pool.start(thief, 48_000.0).expect("steals");
            let tail = pool.slots[MAX_VOICES..].iter().flatten().next().expect("parked");
            assert!(tail.fade.frames_left() <= fade, "releasing {releasing}");
            pool.render_additive(&mut left[480..], &mut right[480..], 1.0);
            let end = 480 + fade as usize;
            assert!(left[end..].iter().all(|&v| v == 0.0), "releasing {releasing}");
            assert_no_click(&left);
        }
    }

    /// Hostile attack times still give a finite ramp that ends: NaN and
    /// negatives are no attack, an infinity is the lane maximum.
    #[test]
    fn a_hostile_attack_is_clamped() {
        assert_eq!(attack_frames_for(f32::NAN, 48_000.0), 0);
        assert_eq!(attack_frames_for(-5.0, 48_000.0), 0);
        assert_eq!(attack_frames_for(f32::INFINITY, 48_000.0), 96_000);
        assert_eq!(attack_frames_for(10.0, f64::NAN), 0);
        assert_eq!(Ramp::attack(0).gain(), 1.0);
        assert_eq!(Ramp::attack(4).gain(), 0.0);
    }

    /// The whole of the choke-group rule: a new voice in group
    /// N fades every sounding voice in group N, whatever lane they came
    /// from, and leaves every other group — and every ungrouped voice —
    /// alone.
    #[test]
    fn a_new_voice_chokes_every_sounding_voice_in_its_group() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        // An open hat in group 1, a tom in group 2, a kick in no group at
        // all — three lanes, no two of them in one group, so starting them
        // chokes nothing.
        for (lane_id, group) in [(1, 1), (2, 2), (3, 0)] {
            let start = VoiceStart { lane_id, ..choked_note_on(sample.clone(), group) };
            pool.start(start, 48_000.0).expect("pool is empty");
        }
        assert!(pool.voices().all(|voice| !voice.is_fading()), "nothing has been choked yet");

        // The pedal hat: a different lane, group 1. It cuts the open hat and
        // nothing else.
        let pedal = VoiceStart { lane_id: 4, ..choked_note_on(sample.clone(), 1) };
        pool.start(pedal, 48_000.0).expect("there is room for a fourth");

        let fading: Vec<(u32, bool)> =
            pool.voices().map(|voice| (voice.lane_id, voice.is_fading())).collect();
        assert_eq!(
            fading,
            vec![(1, true), (2, false), (3, false), (4, false)],
            "group 1 is cut, group 2 and the ungrouped kick play on, the new voice sounds"
        );

        // The closed hat, a third lane in group 1: now *two* earlier voices
        // share the group, and both go — the one still at full gain and the
        // one already fading.
        let closed = VoiceStart { lane_id: 5, ..choked_note_on(sample.clone(), 1) };
        pool.start(closed, 48_000.0).expect("there is room for a fifth");

        let fading: Vec<(u32, bool)> =
            pool.voices().map(|voice| (voice.lane_id, voice.is_fading())).collect();
        assert_eq!(
            fading,
            vec![(1, true), (2, false), (3, false), (4, true), (5, false)],
            "every lane in group 1 is cut, however many there are"
        );
    }

    /// `cut_lane` ("Cut itself") fades every sounding voice of one lane —
    /// one-shots included — whatever group it is in, and leaves every other
    /// lane alone, its group-mates too.
    #[test]
    fn cut_lane_fades_that_lanes_voices_and_no_others() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        for (lane_id, group) in [(1, 0), (2, 3), (1, 0), (3, 3)] {
            let start = VoiceStart { lane_id, ..choked_note_on(sample.clone(), group) };
            pool.start(start, 48_000.0).expect("room");
        }
        assert_eq!(pool.cut_lane(2), 1, "lane 2's one voice, not its group-mate on lane 3");
        assert_eq!(pool.cut_lane(1), 2, "both of lane 1's one-shots");
        assert_eq!(pool.cut_lane(9), 0, "a lane with nothing sounding");

        let fading: Vec<(u32, bool)> =
            pool.voices().map(|voice| (voice.lane_id, voice.is_fading())).collect();
        assert_eq!(fading, vec![(1, true), (2, true), (1, true), (3, false)]);
    }

    /// A cut is an early stop, so it fades over [`FADE_MS`] whatever the
    /// lane's release, and a second cut keeps the ramp the first started.
    #[test]
    fn a_cut_fades_over_fade_ms_and_a_second_cut_keeps_the_ramp() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, frames * 4);
        let mut pool = VoicePool::new();
        let start = VoiceStart { release_ms: 2_000.0, ..choked_note_on(sample, 0) };
        assert_eq!(start.lane_id, 1);
        pool.start(start, 48_000.0).expect("pool is empty");

        assert_eq!(pool.cut_lane(1), 1);
        let mut left = vec![0.0f32; frames / 2];
        let mut right = vec![0.0f32; frames / 2];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(pool.cut_lane(1), 1, "still sounding, still addressed");
        let mut rest_l = vec![0.0f32; frames];
        let mut rest_r = vec![0.0f32; frames];
        pool.render_additive(&mut rest_l, &mut rest_r, 1.0);

        let whole: Vec<f32> = left.iter().chain(&rest_l).copied().collect();
        for (index, rendered) in whole[..frames].iter().enumerate() {
            let expected = (frames - index) as f32 / frames as f32;
            assert_eq!(*rendered, expected, "frame {index} of the one ramp");
        }
        assert!(whole[frames..].iter().all(|&frame| frame == 0.0), "silent after FADE_MS");
        assert_eq!(pool.active_count(), 0);
    }

    /// A lane alone in a group cuts its own earlier hits — the point of putting
    /// one lane in a group, and the thing matching by group rather than by lane
    /// gives for free.
    #[test]
    fn a_lane_alone_in_its_group_chokes_itself() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        for _ in 0..3 {
            pool.start(choked_note_on(sample.clone(), 7), 48_000.0).expect("pool is empty");
        }
        pool.start(choked_note_on(sample.clone(), 7), 48_000.0).expect("pool is empty");

        let fading: Vec<bool> = pool.voices().map(Voice::is_fading).collect();
        assert_eq!(fading, vec![true, true, true, false], "every hit but the newest is on its way");
    }

    /// Group 0 is "no group", not a group everyone shares: an ungrouped
    /// voice neither chokes nor is choked, which has to hold because it is
    /// the lane default.
    #[test]
    fn group_zero_chokes_nothing_and_is_choked_by_nothing() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        for _ in 0..3 {
            pool.start(choked_note_on(sample.clone(), 0), 48_000.0).expect("pool is empty");
        }

        assert_eq!(pool.choke_group(0), 0, "a sweep of group 0 addresses no voice");
        assert!(pool.voices().all(|voice| !voice.is_fading()));

        pool.start(choked_note_on(sample.clone(), 1), 48_000.0).expect("pool is empty");
        assert!(pool.voices().all(|voice| !voice.is_fading()), "a group cannot cut the ungrouped");
    }

    /// A choke cuts a one-shot voice. Note-off does not, and a
    /// drum lane is a one-shot, so a choke that only worked on gate voices
    /// would never fire in a real kit.
    #[test]
    fn a_choke_cuts_a_one_shot_that_would_ignore_a_note_off() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        let start = choked_note_on(sample.clone(), 3);
        assert_eq!(start.lane_mode, LaneMode::OneShot);
        pool.start(start, 48_000.0).expect("pool is empty");

        assert_eq!(pool.release(0, 36, None), 0, "a one-shot ignores the note-off");
        assert_eq!(pool.choke_group(3), 1, "but not the choke");
        assert!(pool.voices().next().expect("still sounding").is_fading());
    }

    /// A note that does not sound chokes nothing: a lane with an unplayable
    /// sample cannot silence the group it is in. The sweep happens on the
    /// way in to a slot, so "the note started" and "the group was cut" are
    /// the same event.
    #[test]
    fn a_note_that_cannot_start_leaves_its_group_sounding() {
        let (_collector, sample) = dc(48_000, 10_000);
        let (_empty_collector, empty) = silence(48_000, 0);
        let mut pool = VoicePool::new();
        pool.start(choked_note_on(sample.clone(), 2), 48_000.0).expect("pool is empty");

        assert!(pool.start(choked_note_on(empty, 2), 48_000.0).is_none(), "nothing to play");
        assert!(pool.voices().all(|voice| !voice.is_fading()), "the group plays on");
    }

    /// A choke group above the model's range is clamped to
    /// [`CHOKE_GROUPS`] rather than forming a private group of its own
    /// (clamp everything at the boundaries). A snapshot claiming group 200
    /// chokes, and is choked by, group 16.
    #[test]
    fn a_hostile_choke_group_is_clamped_into_range() {
        assert_eq!(CHOKE_GROUPS, 16, "0 = none, 1..=16");
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(choked_note_on(sample.clone(), 200), 48_000.0).expect("pool is empty");
        assert_eq!(
            pool.voices().next().expect("sounding").lane_choke,
            CHOKE_GROUPS,
            "stored in range, whatever arrived"
        );

        pool.start(choked_note_on(sample.clone(), CHOKE_GROUPS), 48_000.0).expect("room");
        let fading: Vec<bool> = pool.voices().map(Voice::is_fading).collect();
        assert_eq!(fading, vec![true, false], "group 16 cut what claimed to be group 200");
    }

    /// The choke half of the anti-click contract: a choked voice fades to
    /// **exactly** silence inside its fade, and no two frames of the mix differ
    /// by more than one step of the ramp — least of all the frame the choke
    /// lands on, which a hard stop would turn into a full-scale click.
    ///
    /// Read off a DC sample at unity hard left, where each output frame *is*
    /// the gain applied to it, with the choking voice silent
    /// ([`silence`]) so the buffer shows the ramp alone.
    #[test]
    fn a_choke_fades_to_silence_without_a_click() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, frames * 4);
        let (_silent_collector, silent) = silence(48_000, frames * 4);
        let mut pool = VoicePool::new();
        pool.start(choked_note_on(sample, 1), 48_000.0).expect("pool is empty");

        // Full-gain audio first, so the seam at the choke is in the buffer
        // and not just the ramp that follows it.
        let head = frames / 2;
        let mut left = vec![0.0f32; head + frames + 8];
        let mut right = vec![0.0f32; head + frames + 8];
        let (head_l, tail_l) = left.split_at_mut(head);
        let (head_r, tail_r) = right.split_at_mut(head);
        pool.render_additive(head_l, head_r, 1.0);
        // The choke as it really arrives: a new voice in the same group.
        pool.start(choked_note_on(silent, 1), 48_000.0).expect("there is room for the choker");
        pool.render_additive(tail_l, tail_r, 1.0);

        // One step of the ramp, plus one f32 ulp: two neighbouring ratios
        // are each rounded to f32, so their difference can miss `1 / frames`
        // by that much and no more.
        let step = 1.0 / frames as f32;
        let threshold = step + f32::EPSILON;
        for (index, pair) in left.windows(2).enumerate() {
            let jump = (pair[1] - pair[0]).abs();
            assert!(
                jump <= threshold,
                "frames {index}..{} jump by {jump}, a step is {step}",
                index + 1
            );
            assert!(pair[1] <= pair[0], "frame {index} got louder during a choke");
        }
        assert_eq!(left[head - 1], 1.0, "full gain up to the choke");
        assert_eq!(left[head], 1.0, "the choke itself is not a step: the ramp starts at 1.0");
        for (index, rendered) in left[head..head + frames].iter().enumerate() {
            let expected = (frames - index) as f32 / frames as f32;
            assert_eq!(*rendered, expected, "frame {index} of the choked ramp");
        }
        assert_eq!(left[head + frames..], vec![0.0; 8], "silence within the fade, and after it");
        assert_eq!(right, vec![0.0; head + frames + 8], "hard left throughout");
        assert_eq!(pool.active_count(), 1, "the choked voice freed its slot; the choker sounds");
    }

    /// Choking a voice that is already fading leaves its ramp where it is
    /// ([`Voice::begin_fade`]): a hat choked twice in two blocks must not
    /// jump back to full gain, which is the one discontinuity the fade could
    /// introduce by itself. It is still counted — the choke did address it.
    #[test]
    fn choking_a_fading_voice_does_not_restart_its_ramp() {
        let frames = fade_frames_for(48_000.0);
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(choked_note_on(sample.clone(), 5), 48_000.0).expect("pool is empty");
        pool.choke_group(5);

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        pool.render_additive(&mut left, &mut right, 1.0);

        assert_eq!(pool.choke_group(5), 1, "a fading voice is still in its group");
        let voice = pool.voices().next().expect("still fading, not restarted");
        assert_eq!(voice.fade, FadeState::FadingOut { frames_left: frames - 8, frames });

        let mut next_l = [0.0f32; 1];
        let mut next_r = [0.0f32; 1];
        pool.render_additive(&mut next_l, &mut next_r, 1.0);
        assert!(next_l[0] < left[7], "the gain only ever goes down: {} vs {}", next_l[0], left[7]);
    }

    /// A voice keeps the group it started in: moving a lane into or out of a
    /// group must not reach back into the notes it has already played, the
    /// same rule [`VoiceStart::lane_mode`] follows. The published lane is
    /// read at the note-on, so this is a property of the pool, not of a
    /// setter the pool cannot see.
    #[test]
    fn a_voice_keeps_the_group_it_started_in() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        // One lane, two hits, the lane moved from group 4 to group 6 in
        // between — as the GUI could have done between two blocks.
        pool.start(choked_note_on(sample.clone(), 4), 48_000.0).expect("pool is empty");
        pool.start(choked_note_on(sample.clone(), 6), 48_000.0).expect("pool is empty");

        let fading: Vec<bool> = pool.voices().map(Voice::is_fading).collect();
        assert_eq!(fading, vec![false, false], "different groups: the first hit plays on");
        assert_eq!(pool.choke_group(4), 1, "the first hit is still group 4's");
        assert_eq!(pool.choke_group(6), 1, "and the second is group 6's");
    }

    /// Fill the live slots from `from` on, each voice newer than the last,
    /// so that the next note-on has no choice but to steal.
    ///
    /// `from` is there so a test can plant the voice it cares about in slot
    /// 0 first: that one is then the oldest, and so the first to be stolen.
    fn fill_live_slots(pool: &mut VoicePool, sample: &Shared<SampleData>, from: usize) {
        for slot in from..MAX_VOICES {
            assert_eq!(
                pool.start(note_on(sample.clone(), 0.0), 48_000.0),
                Some(slot),
                "filling slot {slot}"
            );
        }
    }

    /// Stealing takes the oldest voice by [`Voice::age`], not the lowest
    /// slot.
    ///
    /// Slot 3 is given a two-frame voice, so that one slot in the middle of
    /// the pool falls free and is refilled by the pool's *newest* voice.
    /// Four steals then have to walk past it: 0, 1, 2, 4 — which slot order
    /// alone would never produce.
    #[test]
    fn stealing_follows_age_and_not_slot_order() {
        let (_collector, long) = ramp(48_000, 10_000);
        let (_short_collector, short) = ramp(48_000, 2);
        let mut pool = VoicePool::new();
        for slot in 0..MAX_VOICES {
            let sample = if slot == 3 { short.clone() } else { long.clone() };
            assert_eq!(pool.start(note_on(sample, 0.0), 48_000.0), Some(slot));
        }

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(pool.active_count(), MAX_VOICES - 1, "the two-frame voice is gone");
        assert_eq!(pool.start(note_on(long.clone(), 0.0), 48_000.0), Some(3), "slot 3 refilled");

        let stolen: Vec<usize> = (0..4)
            .map(|steal| {
                pool.start(note_on(long.clone(), 0.0), 48_000.0)
                    .unwrap_or_else(|| panic!("steal {steal} sounded nothing"))
            })
            .collect();
        assert_eq!(stolen, vec![0, 1, 2, 4], "oldest first; slot 3 now holds the newest voice");
        assert_eq!(pool.active_count(), MAX_VOICES, "a steal replaces, it does not add");
        assert_eq!(pool.tail_count(), 4, "one tail per steal");
    }

    /// The steal half of the anti-click contract ("every early stop fades"): a
    /// stolen voice goes on rendering its ramp from a tail slot and reaches
    /// **exactly** silence inside its fade, while the voice that took its slot
    /// plays.
    ///
    /// Read off a DC sample at unity hard left, like
    /// [`a_choke_fades_to_silence_without_a_click`], with every other voice
    /// in the pool silent so the buffer shows the stolen voice's ramp alone.
    /// Frame-for-frame equality with the ramp is the stronger claim: it
    /// bounds every jump at one step of it, the seam at the steal included.
    #[test]
    fn a_stolen_voice_fades_to_silence_without_a_click() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, frames * 4);
        let (_silent_collector, silent) = silence(48_000, frames * 4);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        fill_live_slots(&mut pool, &silent, 1);

        // Full-gain audio first, so the seam at the steal is in the buffer
        // and not just the ramp that follows it.
        let head = frames / 2;
        let mut left = vec![0.0f32; head + frames + 8];
        let mut right = vec![0.0f32; head + frames + 8];
        let (head_l, tail_l) = left.split_at_mut(head);
        let (head_r, tail_r) = right.split_at_mut(head);
        pool.render_additive(head_l, head_r, 1.0);
        // The steal as it really arrives: a 65th note on a full pool.
        assert_eq!(pool.start(note_on(silent, 0.0), 48_000.0), Some(0), "the oldest voice's slot");
        pool.render_additive(tail_l, tail_r, 1.0);

        assert_eq!(left[head - 1], 1.0, "full gain up to the steal");
        assert_eq!(left[head], 1.0, "the steal itself is not a step: the ramp starts at 1.0");
        for (index, rendered) in left[head..head + frames].iter().enumerate() {
            let expected = (frames - index) as f32 / frames as f32;
            assert_eq!(*rendered, expected, "frame {index} of the stolen voice's ramp");
        }
        assert_eq!(left[head + frames..], vec![0.0; 8], "silence within the fade, and after it");
        assert_eq!(right, vec![0.0; head + frames + 8], "hard left throughout");
        assert_eq!(pool.tail_count(), 0, "the tail freed its slot when its ramp ended");
        assert_eq!(pool.active_count(), MAX_VOICES, "and the voice that stole it plays on");
    }

    /// Stealing a voice that is already fading carries the ramp it is on
    /// into its tail rather than restarting it ([`Voice::begin_fade`]): a
    /// voice released and stolen eight frames later is eight frames
    /// quieter, not back at full gain — the one discontinuity a fade could
    /// introduce by itself.
    #[test]
    fn stealing_a_fading_voice_carries_its_ramp_into_the_tail() {
        let frames = fade_frames_for(48_000.0);
        let (_collector, sample) = dc(48_000, 10_000);
        let (_silent_collector, silent) = silence(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        assert_eq!(pool.release(0, 36, None), 1, "a gate voice fades at its note-off");

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        pool.render_additive(&mut left, &mut right, 1.0);
        fill_live_slots(&mut pool, &silent, 1);
        pool.start(note_on(silent, 0.0), 48_000.0).expect("the oldest voice, fading or not");
        assert_eq!(pool.tail_count(), 1);

        let mut next_l = [0.0f32; 1];
        let mut next_r = [0.0f32; 1];
        pool.render_additive(&mut next_l, &mut next_r, 1.0);
        let expected = (frames - 8) as f32 / frames as f32;
        assert_eq!(next_l[0], expected, "the ramp went on from where the release left it");
    }

    /// A parked tail is audio and nothing else (module docs, §"Stealing
    /// parks the voice it takes"): the note-off and the choke group of the
    /// note it was playing no longer reach it, so nothing can restart the
    /// ramp the host has already been told is the voice's last.
    #[test]
    fn a_stolen_voice_is_addressed_by_nothing() {
        let frames = fade_frames_for(48_000.0);
        let (_collector, sample) = dc(48_000, 10_000);
        let (_silent_collector, silent) = silence(48_000, 10_000);
        let mut pool = VoicePool::new();
        // A gate voice in choke group 2, on a key of its own so that the
        // note-off below can only be about this one voice.
        let oldest = VoiceStart { note_key: 60, lane_choke: 2, ..gate_note_on(sample, 0.0) };
        pool.start(oldest, 48_000.0).expect("pool is empty");
        fill_live_slots(&mut pool, &silent, 1);
        pool.start(note_on(silent, 0.0), 48_000.0).expect("a steal always sounds");

        let mut first_l = [0.0f32; 8];
        let mut first_r = [0.0f32; 8];
        pool.render_additive(&mut first_l, &mut first_r, 1.0);
        assert_eq!(first_l[0], 1.0, "the stolen voice's ramp starts at full gain");

        assert_eq!(pool.release(0, 60, None), 0, "a note-off cannot find a stolen voice");
        assert_eq!(pool.choke_group(2), 0, "nor can the choke group it was in");
        assert_eq!(pool.active_count(), MAX_VOICES, "a tail is not a live voice");
        assert_eq!(pool.tail_count(), 1, "it is a tail");

        let mut next_l = [0.0f32; 1];
        let mut next_r = [0.0f32; 1];
        pool.render_additive(&mut next_l, &mut next_r, 1.0);
        let expected = (frames - 8) as f32 / frames as f32;
        assert_eq!(next_l[0], expected, "the ramp carried on, unrestarted");
    }

    /// Every voice that ends is recorded for the host
    /// — a one-shot that runs out of sample and a gate voice
    /// whose release reaches silence — with the identity
    /// `NoteEvent::VoiceTerminated` needs, oldest first, and the drain
    /// clears it.
    #[test]
    fn every_ended_voice_is_recorded_in_the_terminated_outbox() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, short) = ramp(48_000, 2);
        let (_long_collector, long) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        let one_shot =
            VoiceStart { note_key: 40, channel: 2, voice_id: Some(7), ..note_on(short, 0.0) };
        let gate =
            VoiceStart { note_key: 41, channel: 3, voice_id: Some(9), ..gate_note_on(long, 0.0) };
        pool.start(one_shot, 48_000.0).expect("pool is empty");
        pool.start(gate, 48_000.0).expect("room for two");
        assert!(pool.terminated().is_empty(), "nothing has ended yet");

        let mut left = vec![0.0f32; 4];
        let mut right = vec![0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        let ran_out = TerminatedVoice { voice_id: Some(7), channel: 2, note_key: 40 };
        assert_eq!(
            pool.terminated().to_vec(),
            vec![ran_out],
            "the one-shot that ran past its last frame"
        );

        assert_eq!(pool.release(3, 41, Some(9)), 1);
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        pool.render_additive(&mut left, &mut right, 1.0);
        let released = TerminatedVoice { voice_id: Some(9), channel: 3, note_key: 41 };
        assert_eq!(
            pool.terminated().to_vec(),
            vec![ran_out, released],
            "oldest first, and the gate voice ended inside its own fade"
        );
        assert_eq!(pool.active_count(), 0);

        pool.clear_terminated();
        assert!(pool.terminated().is_empty(), "a drained outbox is empty");
    }

    /// A stolen voice is reported at the steal — its slot is in use again
    /// at that moment, which is what `NoteEvent::VoiceTerminated` tells the
    /// host — and reported exactly once: the tail finishing its ramp is not
    /// a second ending.
    #[test]
    fn a_stolen_voice_is_reported_once_at_the_steal() {
        let frames = fade_frames_for(48_000.0) as usize;
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        let start = note_on(sample.clone(), 0.0);
        let oldest = VoiceStart { note_key: 52, channel: 1, voice_id: Some(3), ..start };
        pool.start(oldest, 48_000.0).expect("pool is empty");
        fill_live_slots(&mut pool, &sample, 1);
        assert!(pool.terminated().is_empty(), "a full pool has ended nothing");

        pool.start(note_on(sample, 0.0), 48_000.0).expect("a steal always sounds");
        let stolen = TerminatedVoice { voice_id: Some(3), channel: 1, note_key: 52 };
        assert_eq!(
            pool.terminated().to_vec(),
            vec![stolen],
            "reported when its slot was reused, not when its fade ends"
        );

        let mut left = vec![0.0f32; frames * 2];
        let mut right = vec![0.0f32; frames * 2];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(pool.tail_count(), 0, "the ramp is over");
        assert_eq!(pool.terminated().to_vec(), vec![stolen], "and the host is not told twice");
    }

    /// With every tail busy, a steal takes the tail nearest silence
    /// ([`STEAL_TAILS`]) and that voice stops hard — the module's one
    /// click, and the quietest one available.
    ///
    /// The audible tail here is 16 frames further down its ramp than the
    /// [`STEAL_TAILS`] − 1 fresh ones behind it, so it is the one the
    /// eviction has to pick, and its absence from the mix is what the last
    /// render reads.
    #[test]
    fn a_full_tail_region_evicts_the_tail_nearest_silence() {
        let (_collector, sample) = dc(48_000, 10_000);
        let (_silent_collector, silent) = silence(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        fill_live_slots(&mut pool, &silent, 1);

        pool.start(note_on(silent.clone(), 0.0), 48_000.0).expect("the first steal");
        let mut left = [0.0f32; 16];
        let mut right = [0.0f32; 16];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert!(left[15] > 0.0, "the first stolen voice is the one audible tail");

        for steal in 1..STEAL_TAILS {
            pool.start(note_on(silent.clone(), 0.0), 48_000.0)
                .unwrap_or_else(|| panic!("steal {steal} sounded nothing"));
        }
        assert_eq!(pool.tail_count(), STEAL_TAILS, "every tail is busy");

        pool.start(note_on(silent, 0.0), 48_000.0).expect("the steal with nowhere to park");
        assert_eq!(pool.tail_count(), STEAL_TAILS, "the tail region did not grow");
        assert_eq!(
            pool.terminated().len(),
            STEAL_TAILS + 1,
            "one report per steal: the evicted tail was reported at its own"
        );

        let mut after_l = [0.0f32; 16];
        let mut after_r = [0.0f32; 16];
        pool.render_additive(&mut after_l, &mut after_r, 1.0);
        assert_eq!(after_l, [0.0; 16], "the audible tail is the one that was dropped");
    }

    /// A full outbox drops what will not fit and keeps what it has
    /// ([`TERMINATED_CAPACITY`]): the audio thread's only other answer
    /// would be to allocate.
    #[test]
    fn a_full_terminated_outbox_drops_what_will_not_fit() {
        let (_collector, short) = ramp(48_000, 1);
        let mut pool = VoicePool::new();
        let mut left = [0.0f32; 1];
        let mut right = [0.0f32; 1];
        // Two pool-fulls of one-frame voices fill the outbox exactly.
        for round in 0..2u8 {
            for slot in 0..MAX_VOICES {
                let start = VoiceStart { note_key: 36 + round, ..note_on(short.clone(), 0.0) };
                assert_eq!(pool.start(start, 48_000.0), Some(slot));
            }
            pool.render_additive(&mut left, &mut right, 1.0);
        }
        assert_eq!(pool.terminated().len(), TERMINATED_CAPACITY, "exactly full");

        for slot in 0..MAX_VOICES {
            let start = VoiceStart { note_key: 90, ..note_on(short.clone(), 0.0) };
            assert_eq!(pool.start(start, 48_000.0), Some(slot));
        }
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(pool.terminated().len(), TERMINATED_CAPACITY, "and no bigger for trying");
        assert!(
            pool.terminated().iter().all(|ended| ended.note_key != 90),
            "the reports already in hand are the ones kept"
        );
        assert_eq!(pool.active_count(), 0, "every voice ended, reported or not");
    }

    /// The host's reset path takes the tails and the outbox with the voices
    /// ([`VoicePool::clear`]): there is no block left to send a
    /// `VoiceTerminated` in, and the host is throwing away its own voice
    /// bookkeeping too.
    #[test]
    fn a_reset_clears_the_tails_and_the_outbox_with_the_voices() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        fill_live_slots(&mut pool, &sample, 0);
        pool.start(note_on(sample, 0.0), 48_000.0).expect("a steal always sounds");
        assert_eq!(pool.tail_count(), 1);
        assert_eq!(pool.terminated().len(), 1);

        pool.clear();
        assert_eq!(pool.active_count(), 0);
        assert_eq!(pool.tail_count(), 0, "a tail does not survive a reset either");
        assert!(pool.terminated().is_empty(), "nor does an unsent report");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [0.0; 4], "a cleared pool renders silence");
    }

    /// Per-note expression reaches the voices the host meant, by the same rule
    /// note-off uses: the voices carrying its voice id when it sent one, and
    /// otherwise every voice on its channel and key.
    #[test]
    fn an_expression_event_matches_by_voice_id_when_it_has_one_else_by_channel_and_key() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        let seven = VoiceStart { voice_id: Some(7), ..note_on(sample.clone(), 0.0) };
        let eight = VoiceStart { voice_id: Some(8), ..note_on(sample.clone(), 0.0) };
        let other_key = VoiceStart { note_key: 40, ..note_on(sample.clone(), 0.0) };
        let other_channel = VoiceStart { channel: 3, ..note_on(sample, 0.0) };
        pool.start(seven, 48_000.0).expect("slot 0");
        pool.start(eight, 48_000.0).expect("slot 1");
        pool.start(other_key, 48_000.0).expect("slot 2");
        pool.start(other_channel, 48_000.0).expect("slot 3");

        assert_eq!(pool.poly_volume(0, 36, Some(7), 0.25), 1, "an id picks out its own voice");
        let gains: Vec<f32> = pool.voices().map(|voice| voice.poly_volume).collect();
        assert_eq!(gains, vec![0.25, 1.0, 1.0, 1.0]);

        // With no id, channel and key decide — which is both id-carrying
        // voices, the one the id already moved included. Lanes layer, so
        // this is the everyday case: one key, a voice per lane, all moved.
        assert_eq!(pool.poly_volume(0, 36, None, 0.5), 2, "channel 0 key 36 is two of these");
        let gains: Vec<f32> = pool.voices().map(|voice| voice.poly_volume).collect();
        assert_eq!(gains, vec![0.5, 0.5, 1.0, 1.0], "key 40 and channel 3 are other notes");

        assert_eq!(pool.poly_pan(3, 36, None, 0.5), 1, "the channel is part of the match");
        assert_eq!(pool.poly_pan(0, 41, None, 0.5), 0, "and so is the key");
        let pans: Vec<f32> = pool.voices().map(|voice| voice.poly_pan).collect();
        assert_eq!(pans, vec![0.0, 0.0, 0.0, 0.5]);
    }

    /// A voice with no id of its own — an audition from the GUI — still
    /// takes an expression event that carries one, exactly as it
    /// still takes a note-off that does: an id it was never given cannot
    /// identify it.
    #[test]
    fn an_expression_event_reaches_an_id_less_voice_by_channel_and_key() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        assert_eq!(pool.poly_tuning(0, 36, Some(7), 12.0, 48_000.0), 1, "no id to exclude it");
        assert_eq!(pool.voices().next().expect("one voice").step, 2.0);
        assert_eq!(pool.poly_tuning(0, 37, Some(7), 0.0, 48_000.0), 0, "the key still agrees");
        assert_eq!(pool.voices().next().expect("one voice").step, 2.0, "and nothing moved");
    }

    /// Retune-while-playing: a PolyTuning arriving mid-note bends the note from
    /// where it is. The position survives, the speed does not — the routed form
    /// of
    /// [`retuning_keeps_the_position_and_recomputes_the_step`].
    #[test]
    fn retuning_mid_note_through_the_pool_bends_the_note_from_where_it_is() {
        // Long enough that reading it at double speed does not run it out:
        // a voice that has ended is a voice no later event can find.
        let (_collector, sample) = ramp(48_000, 16);
        let mut pool = VoicePool::new();
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [1.0, 2.0], "two frames at the file's own rate");

        assert_eq!(pool.poly_tuning(0, 36, None, 12.0, 48_000.0), 1);
        assert_eq!(pool.voices().next().expect("one voice").position, 2.0, "no seek");

        let mut left = [0.0f32; 3];
        let mut right = [0.0f32; 3];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert_eq!(left, [3.0, 5.0, 7.0], "carries on from frame 2, two frames at a time");

        // And back down again, from wherever that left it.
        assert_eq!(pool.poly_tuning(0, 36, None, 0.0, 48_000.0), 1);
        assert_eq!(pool.voices().next().expect("one voice").step, 1.0);
    }

    /// PolyPan and PolyVolume are heard on the voice they are routed to, in
    /// the mix, and they combine with the lane's own pan and volume rather
    /// than replacing them.
    #[test]
    fn pan_and_volume_expressions_are_heard_on_the_voice_they_reach() {
        let (_collector, sample) = dc(48_000, 64);
        let mut pool = VoicePool::new();
        // Hard left from the lane, so the pan law starts at exactly (1, 0).
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        // One frame at a time into a cleared buffer: `render_additive` adds
        // into what it is given, so a reading of the gains has to start
        // from silence.
        let frame = |pool: &mut VoicePool| {
            let mut left = [0.0f32; 1];
            let mut right = [0.0f32; 1];
            pool.render_additive(&mut left, &mut right, 1.0);
            (left[0], right[0])
        };
        assert_eq!(frame(&mut pool), (1.0, 0.0), "the lane's own pan");

        // +1.0 of PolyPan on a lane panned −1.0 is centre, not hard right.
        assert_eq!(pool.poly_pan(0, 36, None, 1.0), 1);
        assert_eq!(frame(&mut pool), (centre(), centre()), "the sum of the two pans");

        // And PolyVolume multiplies that, it does not stand in for it.
        assert_eq!(pool.poly_volume(0, 36, None, 0.5), 1);
        assert_eq!(frame(&mut pool), (centre() * 0.5, centre() * 0.5));
    }

    /// A fading voice is still being mixed, so an expression event still
    /// reaches it: freezing a released note's pitch and pan mid-fade would
    /// be audible, and bitstep sends expression at the note-on's own timing,
    /// so an event and a fade can share a sample offset.
    #[test]
    fn an_expression_event_still_reaches_a_fading_voice() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(gate_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        assert_eq!(pool.release(0, 36, None), 1);
        assert!(pool.voices().next().expect("one voice").is_fading());

        assert_eq!(pool.poly_volume(0, 36, None, 0.5), 1, "a fade does not hide a voice");
        assert_eq!(pool.poly_tuning(0, 36, None, 12.0, 48_000.0), 1);
        let voice = pool.voices().next().expect("still sounding");
        assert_eq!(voice.poly_volume, 0.5);
        assert_eq!(voice.step, 2.0);
        assert!(voice.is_fading(), "and the ramp it is on was left alone");
    }

    /// A stolen voice finishing its fade in a tail is audio and nothing
    /// else: an expression event addressed to the note it was playing does
    /// not find it (module docs, §"Stealing parks the voice it takes") —
    /// the host has been told it is gone, and its id may already belong to
    /// the note that took its slot.
    #[test]
    fn an_expression_event_never_reaches_a_stolen_tail() {
        let (_collector, sample) = dc(48_000, 10_000);
        let (_silent_collector, silent) = silence(48_000, 10_000);
        let mut pool = VoicePool::new();
        // The oldest voice is the only audible one and the only one on key
        // 36, so what the tail does is all the mix can show.
        pool.start(unity_note_on(sample, 0.0), 48_000.0).expect("pool is empty");
        for slot in 1..MAX_VOICES {
            let start = VoiceStart { note_key: 40, ..note_on(silent.clone(), 0.0) };
            assert_eq!(pool.start(start, 48_000.0), Some(slot), "filling slot {slot}");
        }
        let newest = VoiceStart { note_key: 40, ..note_on(silent, 0.0) };
        pool.start(newest, 48_000.0).expect("the steal takes the oldest voice");
        assert_eq!(pool.tail_count(), 1, "the audible voice is parked in a tail");

        assert_eq!(pool.poly_volume(0, 36, None, 0.0), 0, "no live voice answers to that note");
        let mut left = [0.0f32; 1];
        let mut right = [0.0f32; 1];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert!(left[0] > 0.0, "and the tail still sounds out its own fade");
    }

    /// Expression values are clamped where they enter a voice, like every other
    /// number crossing the boundary: a host or a bug cannot put an infinity in
    /// the mix, an immortal NaN step in a voice, or a pan outside the law's
    /// range.
    #[test]
    fn hostile_expression_values_are_clamped_at_the_voice() {
        let (_collector, sample) = dc(48_000, 10_000);
        let mut pool = VoicePool::new();
        pool.start(note_on(sample, 0.0), 48_000.0).expect("pool is empty");

        assert_eq!(pool.poly_volume(0, 36, None, f32::INFINITY), 1);
        assert_eq!(pool.poly_pan(0, 36, None, -9.0), 1);
        assert_eq!(pool.poly_tuning(0, 36, None, 5_000.0, 48_000.0), 1);
        let voice = pool.voices().next().expect("one voice");
        assert_eq!(voice.poly_volume, GAIN_MAX);
        assert_eq!(voice.poly_pan, -1.0);
        assert_eq!(voice.semis, SEMIS_MAX);
        assert!(voice.step.is_finite() && voice.step > 0.0);

        // NaN is the one that cannot be clamped into range, so each value
        // falls back to its own neutral: unity gain, centre, no shift.
        assert_eq!(pool.poly_volume(0, 36, None, f32::NAN), 1);
        assert_eq!(pool.poly_pan(0, 36, None, f32::NAN), 1);
        assert_eq!(pool.poly_tuning(0, 36, None, f32::NAN, 48_000.0), 1);
        let voice = pool.voices().next().expect("one voice");
        assert_eq!(voice.poly_volume, 1.0);
        assert_eq!(voice.poly_pan, 0.0);
        assert_eq!(voice.semis, 0.0);
        assert_eq!(voice.step, 1.0);

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        pool.render_additive(&mut left, &mut right, 1.0);
        assert!(left.iter().chain(right.iter()).all(|frame| frame.is_finite()));
    }
}
