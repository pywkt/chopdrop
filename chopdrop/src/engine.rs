//! The per-block driver: events in, stereo audio out on up to 16 outputs.
//!
//! [`Engine`] is the layer between the host and the voice pool. It owns a
//! [`VoicePool`], reads the published lane list the writers hand it, turns
//! the note events of a block into starts, releases, chokes and retunes
//! against those lanes, and mixes the result into the block's stereo
//! [`Outputs`], each voice into the pair its lane was routed to. `lib.rs`
//! calls [`run_block`](Engine::run_block) once per `process()` and does
//! nothing else with the audio.
//!
//! Why it is a module of its own rather than the body of `process()`: this
//! is where "which lane answers this note" and "when inside the block does
//! it answer" are decided, and both are worth testing without a host. So it
//! obeys the purity rule — no nice-plug type appears here, the
//! events are a plain enum `lib.rs` fills in, and every test below drives
//! the real engine against a real [`Model`](crate::model::Model). It obeys
//! the real-time rule just as absolutely as [`crate::voices`] does: one
//! lock-free [`PublishedCell::get`], a bounded walk of events and lanes, and
//! no allocation anywhere.
//!
//! # Time inside a block
//!
//! An event does not apply to a block, it applies to a **frame** of one: a
//! note on the second sixteenth of a 512-frame buffer has to start there and
//! not at the top, or every hit in the bar lands on the block grid and the
//! groove is gone. So a block is rendered as the spans between its event
//! offsets — render up to the next event, apply it, render on — which is
//! what makes the engine's output independent of the buffer size the host
//! happens to use. [`run_block`](Engine::run_block) is that loop and little
//! else.
//!
//! Two things the loop is careful about, because the offsets come from the
//! host and are therefore not trusted (clamp at the boundaries):
//!
//! - An offset past the end of the block is applied at its end rather than
//!   dropped: the note was meant to sound, and the block it was meant to
//!   sound in is this one.
//! - An offset before the span already rendered is applied where the cursor
//!   stands, never backwards. nice-plug delivers events in order, and
//!   re-sorting them would need a buffer the real-time rule does not allow;
//!   what this rules out is an event *silently* applying to frames that have
//!   already been written, which would make the output depend on the block
//!   size again.
//!
//! # The master gain is a line
//!
//! The gain arrives once a block — `lib.rs` steps nice-plug's smoother by
//! the whole buffer — but it is applied per frame: the engine remembers
//! what the last block was given and walks from there to this block's value
//! ([`crate::voices::MasterRamp`]). That memory is the engine's only
//! per-block state besides the pool itself, and it is why
//! [`run_block`](Engine::run_block) takes the gain as a target rather than
//! as the gain the block is mixed at. Without it a host automating the
//! volume would put one gain step per buffer into the mix, and a large
//! enough step is a click — the same click every early stop fades to avoid,
//! arriving by a different door.
//!
//! # The "bitstep Sample pad" switch
//!
//! The switch is instance-wide (on for a new instance, off in a project
//! saved with it off), and the audio thread
//! reads it out of the published snapshot as an atomic
//! ([`PublishedLanes::sample_pad_switch`]). Off, a lane plays its sample at
//! the sample's own pitch whichever note triggered it. On, the key is
//! tracked from C3 — `semis = tuning + (key − 60)` — which is what a Drum
//! Machine pad does, and which cancels bitstep's own Sample pad setting
//! exactly, since that sends `tuning = 60 − key`.
//!
//! The offset is folded in **here**, at the two events that carry a tuning
//! (a note-on and a `PolyTuning`), rather than down in the pool:
//! [`crate::voices`] knows one tuning per voice and it is the effective one
//! ([`VoiceStart::semis`]), so there is a single place where a tuning
//! becomes a step and no second one to disagree with it.
//! [`effective_semis`] is that fold, and it is pure.
//!
//! The consequence worth naming: a voice reads the switch once, at the
//! event that tuned it. Flipping the switch while a note rings does not
//! bend that note — it is heard from the next note-on on. That is
//! deliberate: the switch is a setup decision about which note source is
//! driving the plugin, not a performance gesture, and retuning a whole kit
//! mid-hit would be the worse answer. It is also all that a voice's single
//! `tuning_semis` field can represent.
//!
//! # Events the host left open
//!
//! A CLAP host may address a note event to any key or any channel rather
//! than to one — "all notes off", the panic button, a choke of everything —
//! and `lib.rs` maps those onto the out-of-range stand-ins
//! [`crate::voices::KEY_WILDCARD`] / [`crate::voices::CHANNEL_WILDCARD`]
//! (255), because a [`NoteTarget`] carries the plain channel and key a
//! voice carries.
//!
//! The engine reads them as *match any* wherever it matches by channel and
//! key — a note-off, a choke and all three expressions, which is every
//! event that addresses voices already sounding
//! ([`crate::voices::Voice::matches`] is the one predicate, so none of them
//! can read it differently). So a wildcard note-off gates every sounding
//! gate voice, a wildcard choke fades the whole pool, and a wildcard on one
//! field alone still narrows by the other: "all notes off on channel 1"
//! leaves channel 2 ringing.
//!
//! A note-**on** is the exception, and needs no code to be: it starts a
//! voice for every lane whose note is the event's key, and no lane's note
//! can be 255, so a wildcard note-on starts nothing. That is the honest
//! answer — "play some note, any note" names no sound a lane list can make,
//! and a voice that carried 255 would be a voice no later event could
//! address by key.
//!
//! # The audition ring
//!
//! The GUI's play buttons are the one note source that is not the host, and
//! they are pressed on the GUI thread while the audio thread is half way
//! through a block. So they do not call the engine: they push a lane id onto
//! the [`AuditionRing`] the engine owns, and the engine drains it at the top of
//! every block. It is the only shape the real-time rule leaves — a mutex around
//! the pool would put the audio thread behind a thread that is drawing, and a
//! channel would allocate.
//!
//! An audition is a note-on for one lane at that lane's own note: the
//! lane's choke group fires, the switch above pitches it, and the host is
//! told when the voice ends, exactly as if the note had come from bitstep.
//! What it is not is a note-*off*, and a gate lane auditioned therefore
//! plays to the end of its sample rather than hanging — a voice ends when
//! it runs out of sample whatever its mode
//! ([`crate::voices::Voice::render_additive`]).
//!
//! The ring also carries [`AUDITION_STOP`], which fades the sounding
//! audition, and the engine writes back which lane's audition is sounding
//! ([`AuditionRing::sounding`]) so the row's play button can be a stop
//! button while it plays. One audition sounds at a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use crate::model::{OUTPUT_MAIN, OUTPUT_MAX, PublishedCell, PublishedLane, PublishedLanes};
use crate::chop;
use crate::preview::{PreviewChannel, Region};
use crate::time::{self, TempoCell, TimeMode};
use crate::voices::{KEY_WILDCARD, LaneMode, MasterRamp, Voice, VoicePool, VoiceStart};

/// How many stereo outputs a block renders into: Out 1 (the main output) plus
/// the 15 aux outputs, one per value a lane's `output` may hold.
pub const OUTPUTS: usize = OUTPUT_MAX as usize + 1;

/// The stereo outputs one block renders into.
///
/// A stack array of borrows `lib.rs` builds per block from the host's main
/// and aux buffers — nothing in it allocates. Index 0 is the main output,
/// index `n` is Out `n + 1`; `None` is an output the host did not provide
/// (or provided with fewer than two channels, or too few frames). The main
/// output is the block's frame clock: [`Engine::run_block`] renders as many
/// frames as both sides of pair 0 have, and a pair shorter than that is
/// treated as missing.
pub struct Outputs<'a> {
    /// Index 0 is the main output. Each entry is (left, right).
    pub pairs: [Option<(&'a mut [f32], &'a mut [f32])>; OUTPUTS],
}

impl<'a> Outputs<'a> {
    /// Only the main output, as a host with no aux buffers hands over: every
    /// voice lands on `left`/`right` whatever its lane's output.
    pub fn main(left: &'a mut [f32], right: &'a mut [f32]) -> Self {
        let mut pairs = [const { None }; OUTPUTS];
        pairs[usize::from(OUTPUT_MAIN)] = Some((left, right));
        Outputs { pairs }
    }

    /// The pair a voice renders into: its own output when the host gave one
    /// with two channels, else the main output. Never silence — a lane
    /// routed to an output the host left out is heard on Out 1 — unless the
    /// host gave no main output either, which renders no frames at all.
    pub fn pair_for(&mut self, output: u8) -> Option<(&mut [f32], &mut [f32])> {
        let own = usize::from(output);
        let index = if own < OUTPUTS && self.pairs[own].is_some() {
            own
        } else {
            usize::from(OUTPUT_MAIN)
        };
        self.pairs[index].as_mut().map(|(left, right)| (&mut **left, &mut **right))
    }

    /// The block's length: the frames both sides of the main output have,
    /// or 0 without one (shortest buffer wins, as everywhere below).
    fn frames(&self) -> usize {
        match &self.pairs[usize::from(OUTPUT_MAIN)] {
            Some((left, right)) => left.len().min(right.len()),
            None => 0,
        }
    }

    /// Silence every present buffer whole — they belong to the host and
    /// arrive holding whatever was last in them — then drop any pair too
    /// short for a `frames`-frame block, so its voices fall back to the main
    /// output instead of being cut short.
    fn silence_and_fit(&mut self, frames: usize) {
        for slot in &mut self.pairs {
            if let Some((left, right)) = slot {
                left.fill(0.0);
                right.fill(0.0);
                if left.len().min(right.len()) < frames {
                    *slot = None;
                }
            }
        }
    }
}

/// The key the "bitstep Sample pad" switch tracks from: 60, which Bitwig
/// names C3.
///
/// The root note of a Drum Machine pad, and the one bitstep measures its own
/// Sample pad tuning (`60 − key`) against — so adding `key − 60` on top of
/// that tuning lands back on the sample's own pitch for every key.
pub const PAD_ROOT_KEY: u8 = 60;

/// The `(speed, pitch_st)` a lane's voices play at, handed to
/// [`Voice::set_time`] at the note-on and once per block after.
///
/// - Off: speed 1 at the lane's own pitch.
/// - Repitch: `repitch_speed(natural, target)` with no pitch (the field is
///   disabled there), `target` being the lane's length in beats at `tempo`
///   trimmed by its fine; a lane on "as cut" (`length_beats` 0) plays at
///   speed 1, as Off without its pitch.
/// - Stretch: on a render (`rendered`), the speed correction
///   `render_target_secs / target_secs(now)` — the
///   render's own length, which is the target it was made for, measured
///   against the lane's target at `tempo` — so a voice stays the right
///   length while a re-render for a new tempo is on its way. No pitch (the
///   render carries it). The raw chop, and "as cut" (whose render is the
///   chop's own length whatever the tempo), play at speed 1.
///
/// `natural_secs` is the length of the sample the voice plays (its own,
/// [`Voice::natural_secs`], once sounding) and `rendered` whether that is a
/// render ([`Voice::rendered`]) — the voice's, not the lane's, so a voice
/// keeps correcting the render it started on after a newer one lands.
/// Pure maths on two relaxed loads at most: no allocation.
pub fn lane_time(
    lane: &PublishedLane,
    natural_secs: f64,
    rendered: bool,
    tempo: f64,
) -> (f64, f32) {
    match lane.time_mode {
        TimeMode::Off => (1.0, lane.pitch_st()),
        TimeMode::Repitch => match lane_target_secs(lane, tempo) {
            Some(target) => (time::repitch_speed(natural_secs, target), 0.0),
            None => (1.0, 0.0),
        },
        TimeMode::Stretch => match lane_target_secs(lane, tempo) {
            Some(target) if rendered => (time::repitch_speed(natural_secs, target), 0.0),
            _ => (1.0, 0.0),
        },
    }
}

/// The length in seconds a Repitch or Stretch lane's note should last at
/// `tempo`: its length in beats trimmed by its fine. `None` for "as cut"
/// (`length_beats` 0.0 exactly) — and for a non-positive or NaN value,
/// which plays as cut rather than at a clamp.
fn lane_target_secs(lane: &PublishedLane, tempo: f64) -> Option<f64> {
    let beats = f64::from(lane.length_beats);
    if beats.is_nan() || beats <= 0.0 {
        return None;
    }
    Some(time::beats_secs(beats, lane.fine_pct(), tempo))
}

/// The tuning a voice on `key` actually plays at, given the switch.
///
/// `tuning` is what the event carried: 0.0 at a note-on, the host's value at
/// a `PolyTuning`. With the switch off that is the whole answer — the sample
/// plays at its own pitch and only expression moves it. With the switch on
/// the key is tracked from [`PAD_ROOT_KEY`], so a lane answering note 36
/// sounds two octaves below its sample and one answering note 72 an octave
/// above it.
///
/// An add and a branch, called once per tuning event and never per frame.
/// Nothing is clamped here on purpose: the one place a tuning becomes a step
/// clamps and NaN-guards it ([`crate::voices::step_for`]), so folding the
/// offset in cannot be a second opinion about what a hostile tuning of NaN
/// or 1e30 means.
///
/// A wildcard key ([`KEY_WILDCARD`]) tracks nothing: the event said
/// "any key", so there is no key to measure the offset from, and the tuning
/// it carried is the whole answer whatever the switch says. Without that the
/// stand-in would be arithmetic — 255 − 60, a nonsense 195 semitones the
/// clamp would turn into silence — on exactly the wildcard events that
/// reach voices at all.
pub fn effective_semis(tuning: f32, key: u8, sample_pad_switch: bool) -> f32 {
    if sample_pad_switch && key != KEY_WILDCARD {
        tuning + (key as f32 - PAD_ROOT_KEY as f32)
    } else {
        tuning
    }
}

/// How many auditions the ring between the GUI and the audio thread holds.
///
/// 32: one click is one entry and one block drains all of them, so the only
/// way to fill it is to click faster than the host calls `process()` — at
/// which point dropping the surplus clicks is the honest answer, which is
/// why an audition is best-effort.
pub const AUDITION_CAPACITY: usize = 32;

/// The velocity an audition plays at: full.
///
/// The play button is not a performance control. It answers "what is on
/// this lane", so it answers at the velocity the sample was recorded to be
/// heard at; dynamics come from the note source.
pub const AUDITION_VELOCITY: f32 = 1.0;

/// What the ring carries for "stop the audition" ([`AuditionRing::stop`]),
/// and what [`AuditionRing::sounding`] holds for "none". No lane has this
/// id (they count up from 0, one per lane ever added), so an engine that
/// took it for a lane would find none and sound nothing.
pub const AUDITION_STOP: u32 = u32::MAX;

/// The queue the GUI's play buttons push onto and the audio thread drains.
///
/// A hand-written single-producer/single-consumer ring: fixed capacity,
/// atomics only, no lock and no allocation on either side. Small enough
/// that a dependency for it would cost more than it saves, and the real-time
/// rule leaves nothing else — the consumer is a block of audio, which may
/// not wait for a thread that is drawing a window.
///
/// **One producer, one consumer.** The GUI thread calls
/// [`push`](Self::push), the audio thread calls [`pop`](Self::pop), and
/// neither is safe to call from two threads at once. That is not a
/// limitation in practice: there is one editor window and one audio thread,
/// and nothing else has a reason to audition a lane.
///
/// How it stays correct with no lock: the two counters only ever count up,
/// and each is written by one side alone — the producer owns `pushed`, the
/// consumer owns `popped`, and `pushed − popped` is what is waiting. A
/// producer publishes a slot by releasing `pushed` *after* writing it and a
/// consumer reads it after acquiring `pushed`, which is what makes the
/// relaxed slot accesses sound: the store to the slot cannot be seen later
/// than the counter that announces it. The same pair the other way round
/// (`popped` released after the read, acquired before the write) is what
/// stops a producer from overwriting a slot the consumer has not taken yet.
pub struct AuditionRing {
    /// The lane ids, in the order they were pushed, indexed by counter
    /// modulo the capacity.
    slots: [AtomicU32; AUDITION_CAPACITY],
    /// How many auditions have ever been pushed. The producer's counter;
    /// the consumer only reads it.
    pushed: AtomicUsize,
    /// How many have ever been popped. The consumer's counter; the producer
    /// only reads it.
    popped: AtomicUsize,
    /// The lane whose audition is sounding, [`AUDITION_STOP`] for none.
    /// Written by the engine at the end of every block, read by
    /// the GUI to draw that row's play button as a stop button.
    sounding: AtomicU32,
}

impl AuditionRing {
    /// An empty ring.
    pub fn new() -> Self {
        AuditionRing {
            slots: [const { AtomicU32::new(0) }; AUDITION_CAPACITY],
            pushed: AtomicUsize::new(0),
            popped: AtomicUsize::new(0),
            sounding: AtomicU32::new(AUDITION_STOP),
        }
    }

    /// Ask for the sounding audition to stop on the next block: it fades
    /// over the short fade, as a choke does. `false` if the ring was full.
    pub fn stop(&self) -> bool {
        self.push(AUDITION_STOP)
    }

    /// The lane whose audition is sounding, as of the engine's last block.
    pub fn sounding(&self) -> Option<u32> {
        match self.sounding.load(Ordering::Relaxed) {
            AUDITION_STOP => None,
            lane => Some(lane),
        }
    }

    /// The engine's side of [`sounding`](Self::sounding).
    fn set_sounding(&self, lane: Option<u32>) {
        self.sounding.store(lane.unwrap_or(AUDITION_STOP), Ordering::Relaxed);
    }

    /// Ask for `lane_id` to sound on the next block. `false` if the ring was
    /// full and the audition was dropped (an audition is best-effort).
    ///
    /// Called from the GUI thread, in the frame the button was clicked.
    /// Wait-free: a bounded number of atomic operations, whatever the audio
    /// thread is doing.
    pub fn push(&self, lane_id: u32) -> bool {
        // Relaxed: this side is the only writer of `pushed`. `popped` is
        // acquired, so a slot the consumer has just freed is seen as free.
        let pushed = self.pushed.load(Ordering::Relaxed);
        if pushed.wrapping_sub(self.popped.load(Ordering::Acquire)) >= AUDITION_CAPACITY {
            return false;
        }
        self.slots[pushed % AUDITION_CAPACITY].store(lane_id, Ordering::Relaxed);
        // Release: the slot is written before the counter that publishes it.
        self.pushed.store(pushed.wrapping_add(1), Ordering::Release);
        true
    }

    /// The oldest audition waiting, or `None` when there is none.
    ///
    /// Called from the audio thread, once per entry at the top of a block
    /// ([`Engine::run_block`]). Wait-free, for the same reason
    /// [`push`](Self::push) is.
    pub fn pop(&self) -> Option<u32> {
        // The mirror of `push`: relaxed on this side's own counter,
        // acquire on the other side's.
        let popped = self.popped.load(Ordering::Relaxed);
        if popped == self.pushed.load(Ordering::Acquire) {
            return None;
        }
        let lane_id = self.slots[popped % AUDITION_CAPACITY].load(Ordering::Relaxed);
        // Release: the slot is read before the counter that frees it.
        self.popped.store(popped.wrapping_add(1), Ordering::Release);
        Some(lane_id)
    }

    /// How many auditions are waiting: 0..=[`AUDITION_CAPACITY`].
    ///
    /// A snapshot, and so only exactly true to the thread that asked —
    /// enough for the tests below, which is what it is for. Nothing on
    /// either hot path needs it: a block drains until the ring says `None`.
    pub fn pending(&self) -> usize {
        let pushed = self.pushed.load(Ordering::Relaxed);
        pushed.wrapping_sub(self.popped.load(Ordering::Relaxed))
    }
}

impl Default for AuditionRing {
    fn default() -> Self {
        Self::new()
    }
}

/// Which voices a note event is about.
///
/// The three fields every event in [`EngineEvent`] carries, kept together
/// because they travel together: a note-off, a choke and all three
/// expressions ask the pool exactly one question — is this voice the one the
/// host meant — and [`crate::voices::Voice::matches`] is the single answer.
/// By the host's `voice_id` when it sent one, else by channel and key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NoteTarget {
    /// The MIDI channel the event arrived on.
    pub channel: u8,
    /// The key, 0–127 — Bitwig's convention, so 60 is C3.
    pub key: u8,
    /// The host's voice id, when it tracks voices and sent one.
    pub voice_id: Option<i32>,
}

/// One thing a block can ask of the engine.
///
/// A plain enum, deliberately: `lib.rs` maps the host's nice-plug
/// `NoteEvent`s onto it field for field and ignores the kinds
/// that are not here, so everything below this point is testable without a
/// host and without the framework.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EngineEvent {
    /// Start a voice in **every** lane whose note is this key and which has
    /// a sample — lanes layer, so one key may be a stack.
    NoteOn {
        /// Who the note is, and who its later events will name.
        note: NoteTarget,
        /// Velocity, 0.0..=1.0: the first factor of the gain chain.
        velocity: f32,
    },
    /// End the note: matching gate voices fade, one-shots play on
    /// ([`crate::voices::Voice::release`]).
    NoteOff {
        /// Which voices the note-off is about.
        note: NoteTarget,
    },
    /// The host's own choke: fade the voices it names, whatever
    /// mode their lanes are in. Not a lane choke group — that one is applied
    /// by the pool at the note-on which triggered it.
    Choke {
        /// Which voices the host is cutting.
        note: NoteTarget,
    },
    /// Per-note tuning: retune the matching voices, in semitones.
    PolyTuning {
        /// Which voices to retune.
        note: NoteTarget,
        /// Semitones away from the sample's own pitch.
        semis: f32,
    },
    /// Per-note pan: an offset on the lane's own pan, not a replacement.
    PolyPan {
        /// Which voices to pan.
        note: NoteTarget,
        /// The offset, −1.0..=1.0.
        pan: f32,
    },
    /// Per-note volume: a factor in the gain chain beside velocity and the
    /// lane's volume.
    PolyVolume {
        /// Which voices to set the gain of.
        note: NoteTarget,
        /// The gain, about 1.0.
        gain: f32,
    },
}

/// An [`EngineEvent`] and the frame of the block it happens on.
///
/// `timing` is nice-plug's own: frames from the start of the block. It is
/// separate from the event rather than a field of each variant so that the
/// walk in [`Engine::run_block`] can read it without matching, and so that
/// adding an event kind cannot forget it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TimedEvent {
    /// Frames from the start of the block. Offsets past the end of the block
    /// land at its end, and an offset behind the cursor lands at the cursor
    /// (module docs, §Time inside a block).
    pub timing: u32,
    /// What happens there.
    pub event: EngineEvent,
}

impl TimedEvent {
    /// An event at `timing`. A constructor because the two fields read
    /// better at a call site than a struct literal does, and `lib.rs` writes
    /// one per host event it recognises.
    pub fn new(timing: u32, event: EngineEvent) -> Self {
        TimedEvent { timing, event }
    }
}

/// The audio thread's whole state: the voice pool and nothing else.
///
/// Nothing else, because everything else a block needs is given to it —
/// the lanes come from the published cell, the master volume and the host
/// rate come from `lib.rs` as arguments. The engine is created with the
/// plugin instance, lives as long as it, and is touched from the audio
/// thread alone.
pub struct Engine {
    /// The voices. Private: starting and stopping one is a decision about
    /// lanes and events, which is this module's job
    /// ([`pool`](Engine::pool) is the read-only view the tests look at it
    /// through).
    pool: VoicePool,
    /// The GUI's auditions, waiting to be heard.
    ///
    /// Owned by the engine because the engine is the end that consumes
    /// them, and lent to the GUI as a producer by
    /// [`auditions`](Engine::auditions). One `Arc`, allocated with the
    /// instance and cloned on the main thread when the editor is built —
    /// never on this path, where only the pop happens.
    auditions: Arc<AuditionRing>,
    /// The audition sounding: its lane and its voice's start order
    /// ([`Voice::age`], which no other voice shares), so it can be stopped
    /// and reported without being mistaken for a note of the same lane.
    /// One at a time: a new audition fades the last one.
    audition_voice: Option<(u32, u64)>,
    /// Whether any voice started in the block just rendered — the other
    /// half of [`needs_a_frame`](Engine::needs_a_frame), reset at the top of
    /// every [`run_block`](Engine::run_block) and set by
    /// [`start_lane`](Engine::start_lane) beside the activity marker it
    /// belongs with.
    ///
    /// A `bool` and not a count: the counts the GUI reads are the lanes'
    /// own ([`crate::model::PublishedLane::activity`]), and all this one is
    /// asked is whether the editor has a light to draw.
    struck: bool,
    /// The master gain the last rendered block was handed — where the next
    /// block's ramp starts from (module docs, §The master gain is a line).
    ///
    /// The value `lib.rs` passed, not the gain the last frame was mixed at:
    /// the convention is that the block's target is reached at the frame
    /// *after* its last one, which is this block's frame 0, so remembering
    /// the target is what makes two blocks join exactly. Keeping the number
    /// the host drove rather than a clamped copy is harmless — every use of
    /// it goes through [`MasterRamp`], which clamps both ends of every line
    /// it draws, so a hostile value lands in range the same way twice.
    ///
    /// `None` before the first block of a run and after a reset: there is
    /// nothing to join onto then, and a block with nothing behind it is
    /// rendered flat at its own gain.
    master_volume: Option<f32>,
    /// The browser's previews, when the instance has a previewer: `None` in
    /// tests that build a bare engine.
    preview: Option<Arc<PreviewChannel>>,
    /// The preview generation last acted on.
    preview_seen: u64,
    /// The preview playing, outside the pool: it never steals, chokes,
    /// lights a lane or reports a terminated voice.
    preview_voice: Option<PreviewVoice>,
    /// The previous preview, finishing its fade after being replaced or
    /// stopped.
    preview_tail: Option<PreviewVoice>,
    /// The host tempo: the model's cell in the plugin
    /// ([`with_tempo`](Engine::with_tempo)), a cell of its own in tests that
    /// build a bare engine. Read with one relaxed load.
    tempo: Arc<TempoCell>,
}

/// A preview voice and, for the chop editor's previews, the pass of the
/// region it is on.
struct PreviewVoice {
    voice: Voice,
    /// The region this pass plays, clamped into the sample, with its edge
    /// fade in frames; `None` for the browser's whole-sample previews.
    pass: Option<(Region, u64)>,
}

impl PreviewVoice {
    /// A pass over `region` (already clamped, non-empty) from its start.
    fn begin_pass(&mut self, region: Region) {
        let len = region.end - region.start;
        self.voice.position = region.start as f64;
        self.pass = Some((region, chop::edge_fade_frames(self.voice.sample.rate, len)));
    }

    /// Mixes the voice into the block and says whether it still sounds.
    /// `channel` is the live region and loop flag, read at each seam; `None`
    /// (a tail fading out) ends the voice at its seam instead.
    fn render(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        master: MasterRamp,
        channel: Option<&PreviewChannel>,
    ) -> bool {
        if self.pass.is_none() {
            return self.voice.render_additive(out_l, out_r, master);
        }
        let total = self.voice.sample.frame_count() as u64;
        // Looping and the live region no longer holds the position (an edge
        // was dragged past it): jump to the live start at once.
        if let Some(channel) = channel
            && channel.live_looping()
        {
            let live = channel.live_region().clamped(total);
            let at = self.voice.position;
            if live.end > live.start
                && (at < live.start as f64 || at >= live.end as f64)
            {
                self.begin_pass(live);
            }
        }
        let frames = out_l.len().min(out_r.len());
        let mut at = 0;
        loop {
            let Some((pass, fade)) = self.pass else { return false };
            let written = self.voice.render_region_additive(
                &mut out_l[at..frames],
                &mut out_r[at..frames],
                master.from_frame(at),
                pass.start..pass.end,
                fade,
            );
            at += written;
            if self.voice.fade.is_finished() {
                return false;
            }
            if self.voice.position < pass.end as f64 {
                // The block is full and the pass goes on.
                return true;
            }
            // The seam: reload the live region; loop onto it, or end.
            let Some(channel) = channel else { return false };
            let live = channel.live_region().clamped(total);
            if !channel.live_looping() || live.end <= live.start {
                return false;
            }
            // The step's overshoot past the end carries into the next pass,
            // so the loop keeps time at any rate.
            let over = self.voice.position - pass.end as f64;
            self.begin_pass(live);
            if over < (live.end - live.start) as f64 {
                self.voice.position += over;
            }
            if at >= frames {
                return true;
            }
        }
    }
}

/// The lane id a preview voice carries. No lane has it (ids count up from
/// 1), and the preview is never in the pool, where lane ids are matched.
const PREVIEW_LANE: u32 = u32::MAX;

impl Engine {
    /// An engine with no voices sounding and an empty audition ring.
    pub fn new() -> Self {
        Engine {
            pool: VoicePool::new(),
            auditions: Arc::new(AuditionRing::new()),
            audition_voice: None,
            struck: false,
            master_volume: None,
            preview: None,
            preview_seen: 0,
            preview_voice: None,
            preview_tail: None,
            tempo: Arc::new(TempoCell::new()),
        }
    }

    /// This engine, reading the host tempo from `tempo` — the model's cell,
    /// so the engine, the editor and the render worker share one. Main
    /// thread, at construction.
    pub fn with_tempo(mut self, tempo: Arc<TempoCell>) -> Self {
        self.tempo = tempo;
        self
    }

    /// The tempo cell this engine reads.
    pub fn tempo(&self) -> &Arc<TempoCell> {
        &self.tempo
    }

    /// This engine, also playing the browser's previews from `channel`.
    /// Main thread, at construction.
    pub fn with_preview(mut self, channel: Arc<PreviewChannel>) -> Self {
        self.preview_seen = channel.get().generation;
        self.preview = Some(channel);
        self
    }

    /// The preview channel this engine plays, if it was given one.
    pub fn preview(&self) -> Option<&Arc<PreviewChannel>> {
        self.preview.as_ref()
    }

    /// Render one block: apply `events` at their offsets against the
    /// published lanes and mix every sounding voice into its output's pair
    /// of `outputs`.
    ///
    /// The four steps, in order: the snapshot is read **once** through
    /// [`PublishedCell::get`] (so one block renders against one lane list
    /// however many times a writer publishes while it runs), the audition ring
    /// is drained into the top of the block, the events are walked by offset
    /// with the spans between them rendered, and the voices that ended on the
    /// way are left in the pool's terminated outbox for `lib.rs` to send on
    /// ([`pool`](Engine::pool)`().terminated()`,
    /// [`clear_terminated`](Engine::clear_terminated)).
    ///
    /// The buffers are **written, not added to**: they belong to the host and
    /// arrive holding whatever was last in them, so the block is silenced
    /// first and the voices mix into the silence. Every present buffer is
    /// silenced whole; only the frames both sides of the main output have
    /// are rendered ([`Outputs`]).
    ///
    /// Each voice renders into the pair its lane's output named at the
    /// note-on ([`Outputs::pair_for`]: the main output when the host left
    /// that one out). The browser's preview and the GUI's auditions always
    /// land on the main output. The master ramp is applied per voice, so it
    /// shapes every pair alike.
    ///
    /// `host_rate` is the host's sample rate, which every voice's step and fade
    /// length are computed from, and `master_volume` is the block's master gain
    /// — the last factor of every voice's gain chain. Both come from the host
    /// and both are guarded where they are used ([`crate::voices::step_for`],
    /// [`crate::voices::Voice::gain`]), so no value of either can make a voice
    /// immortal or the mix infinite.
    ///
    /// The master gain is a *target*, not the gain the whole block is mixed at
    /// (module docs, §The master gain is a line): the block is rendered on a
    /// line from what the last block was given to this one's value, a per-frame
    /// lerp ([`MasterRamp`]), reaching the target at the frame after the
    /// block's last — the next block's first. A knob drag sounds the same
    /// either way, since nice-plug smooths it over longer than a buffer; what
    /// this is for is the one large automation jump, which a bare per-block
    /// gain would land as a step at a block boundary, which is a click. The
    /// first block of a run has nothing to ramp from and is mixed flat.
    ///
    /// One consequence worth naming, since block stitching is the contract
    /// right beside it: a block whose master gain is *moving* does not stitch —
    /// one 512-frame block spreads the move over 512 frames where two 256-frame
    /// halves would take two targets from the host and spread them over 256
    /// each. That is inherent to a gain the host hands over once a block, and
    /// it is why the stitching tests hold the gain still; everything else in
    /// the block — positions, fades, steals — stitches exactly as before.
    ///
    /// Allocation-free and lock-free, as the real-time rule requires: the
    /// `get` is a refcount bump, the event walk and the lane walk are
    /// bounded, and the mix writes into the pool's own fixed array and the
    /// host's buffers.
    pub fn run_block(
        &mut self,
        published: &PublishedCell,
        events: &[TimedEvent],
        outputs: &mut Outputs<'_>,
        host_rate: f64,
        master_volume: f32,
    ) {
        // Step 1, the snapshot: once, and held for the whole block.
        let lanes = published.get();

        // This block's strikes, counted from nothing: the flag answers for
        // the block being rendered and not for the run
        // ([`Engine::needs_a_frame`]).
        self.struck = false;

        // Shortest buffer wins, as it does one level down
        // ([`crate::voices::Voice::render_additive`]): a host that hands over
        // a short side gets a short block rather than a panic.
        let frames = outputs.frames();
        outputs.silence_and_fit(frames);

        self.sync_lane_controls(&lanes, host_rate);

        // The block's gain as a line rather than a number (module docs, §The
        // master gain is a line), walked a frame at a time by the mix,
        // and this block's target remembered beside it for the next block
        // to start its own line from.
        let master = match self.master_volume {
            Some(previous) => MasterRamp::across(previous, master_volume, frames),
            None => MasterRamp::flat(master_volume),
        };
        self.master_volume = Some(master_volume);

        // Step 2: the GUI's own note-ons, at frame 0 of this block.
        // Bounded by the ring's capacity rather than looping until it is
        // empty, so a GUI thread pushing while this runs cannot keep the
        // audio thread here; whatever it pushes after the bound is reached
        // is heard one block later.
        for _ in 0..AUDITION_CAPACITY {
            match self.auditions.pop() {
                None => break,
                Some(AUDITION_STOP) => self.stop_audition(),
                Some(lane_id) => self.audition(&lanes, lane_id, host_rate),
            }
        }

        // Step 3, the timed walk. `cursor` is the first frame not yet written.
        let mut cursor = 0;
        for timed in events {
            // Clamped into the block and never behind the cursor — module
            // docs, §Time inside a block.
            let at = (timed.timing as usize).min(frames).max(cursor);
            if at > cursor {
                self.pool.render_routed(
                    outputs,
                    cursor..at,
                    // Each piece renders from its own frame 0, so it is
                    // handed the line moved up to where it begins.
                    master.from_frame(cursor),
                );
                cursor = at;
            }
            self.dispatch(&lanes, timed.event, host_rate);
        }
        // The tail of the block after the last event — and the whole of it
        // when there were none, which is the common case.
        self.pool.render_routed(outputs, cursor..frames, master.from_frame(cursor));

        // And the browser's preview over the whole block, after the lanes:
        // it is mixed in, so the order is only bookkeeping. Always on the
        // main output: a preview belongs to no lane.
        if let Some((left, right)) = outputs.pair_for(OUTPUT_MAIN) {
            self.render_preview(&mut left[..frames], &mut right[..frames], master, host_rate);
        }

        // Whether the audition is still sounding, for the row's stop button:
        // not once it has run out (the render drops it), been stopped,
        // choked or cut.
        let sounding = self.audition_voice.filter(|&(_, age)| {
            self.pool
                .voices()
                .any(|voice| voice.age == age && !voice.is_fading())
        });
        self.audition_voice = sounding;
        self.auditions.set_sounding(sounding.map(|(lane, _)| lane));
    }

    /// Picks up a new preview request, if there is one, and mixes the
    /// preview voices into the block.
    ///
    /// A new generation fades out whatever is previewing (into the one tail
    /// slot; an older tail still fading is dropped, which only happens when
    /// requests come faster than 4 ms apart) and starts the new sample from
    /// its first frame: unity velocity, 0 dB, centred, its own pitch.
    ///
    /// A request with a region (the chop editor's) starts at the region's start
    /// instead and plays it with the chop's edge fades; at its end it rereads
    /// the live region and loop flag and loops onto the live region or ends
    /// ([`PreviewVoice::render`]). The voice's frame goes to the channel each
    /// block for the hairline. Allocation-free: one `get`, a `Shared` clone,
    /// and a plain struct.
    fn render_preview(
        &mut self,
        out_l: &mut [f32],
        out_r: &mut [f32],
        master: MasterRamp,
        host_rate: f64,
    ) {
        let Some(channel) = self.preview.as_ref() else { return };
        let state = channel.get();
        if state.generation != self.preview_seen {
            self.preview_seen = state.generation;
            if let Some(mut current) = self.preview_voice.take() {
                current.voice.begin_fade();
                self.preview_tail = Some(current);
            }
            if let Some(sample) = state.sample.as_ref() {
                let total = sample.frame_count() as u64;
                let region = state.region.map(|region| region.clamped(total));
                let start = VoiceStart {
                    lane_id: PREVIEW_LANE,
                    note_key: PAD_ROOT_KEY,
                    channel: 0,
                    voice_id: None,
                    sample: sample.clone(),
                    velocity: 1.0,
                    lane_volume_db: 0.0,
                    lane_pan: 0.0,
                    lane_choke: 0,
                    lane_mode: LaneMode::OneShot,
                    semis: 0.0,
                    speed: 1.0,
                    pitch_st: 0.0,
                    attack_ms: 0.0,
                    release_ms: 0.0,
                    output: OUTPUT_MAIN,
                    rendered: false,
                };
                let mut voice = PreviewVoice { voice: Voice::new(start, host_rate, 0), pass: None };
                match region {
                    // An empty region plays nothing.
                    Some(region) if region.end <= region.start => {}
                    Some(region) => {
                        voice.begin_pass(region);
                        self.preview_voice = Some(voice);
                    }
                    None => self.preview_voice = Some(voice),
                }
            }
        }
        drop(state);

        if let Some(voice) = &mut self.preview_voice
            && !voice.render(out_l, out_r, master, Some(channel))
        {
            self.preview_voice = None;
        }
        if let Some(tail) = &mut self.preview_tail
            && !tail.render(out_l, out_r, master, None)
        {
            self.preview_tail = None;
        }
        if let Some(voice) = &self.preview_voice {
            // The play hairline's frame (whole frames; never past the end).
            let frame = voice.voice.position.max(0.0) as u64;
            channel.set_position(frame.min(voice.voice.sample.frame_count() as u64));
        }
        channel.set_playing(self.preview_voice.is_some() || self.preview_tail.is_some());
    }

    /// The voices, for reading.
    ///
    /// `lib.rs` drains the terminated outbox through it once per block
    /// (`pool().terminated()`, then [`clear_terminated`](Engine::clear_terminated)),
    /// and it is the window the tests look at the pool through: the pool is
    /// the only place a test can see what a block decided (which voices
    /// sound, which fade, which lane each belongs to), and the engine's own
    /// API deliberately says none of that.
    ///
    /// Read-only on purpose. A voice starts or stops because of an event and
    /// a lane, which is a decision this module makes; handing out a mutable
    /// pool would be handing out a second place to make it.
    pub fn pool(&self) -> &VoicePool {
        &self.pool
    }

    /// The audition ring, to push onto.
    ///
    /// The producer end, for the editor's play buttons: `Arc::clone` it when
    /// the editor is built and the GUI can ask for a lane without touching
    /// the engine. Shared rather than moved out because the ring outlives
    /// any one editor window — the host may close and reopen it many times
    /// over one instance, and an audition clicked just before a close is
    /// still heard.
    ///
    /// One producer only ([`AuditionRing`]).
    pub fn auditions(&self) -> &Arc<AuditionRing> {
        &self.auditions
    }

    /// Whether anything at all is making sound: a live voice, or a stolen
    /// one still finishing its fade in the tail region.
    ///
    /// A walk of the pool's slots and nothing else — no allocation and no
    /// lock, since it is asked on the audio thread.
    pub fn is_sounding(&self) -> bool {
        self.pool.active_count() > 0 || self.pool.tail_count() > 0
    }

    /// Whether the editor has anything to draw after this block, and so
    /// what `lib.rs` wakes it on ("continuous repaint while any light is
    /// fading or any voice sounds").
    ///
    /// Two things and not one, because a voice sounding is not the only way
    /// a lane's light comes on. A sample shorter than the host's buffer
    /// starts *and* ends inside one [`run_block`](Engine::run_block) — a
    /// 5 ms click at a 512-frame buffer — so by the time `process()` asks,
    /// [`is_sounding`](Engine::is_sounding) is already false again while the
    /// lane's activity marker has just moved. Waking on the sound alone
    /// would leave exactly those lanes dark: egui repaints when its input
    /// changes, and a note from bitstep is not input, so a frame nobody
    /// asks for is a frame nobody draws. [`struck`](Engine::struck) is that
    /// missing half — a voice *started* in this block, however briefly.
    ///
    /// Both halves are needed: the strike lights the dot on the block the
    /// note arrives in, and the sound keeps the window drawn while a long
    /// one-shot or a held gate runs on. The fade's own remaining frames are
    /// the editor's to ask for ([`crate::editor::activity::Lights::fading`])
    /// — this says nothing about them, and once the sound stops and the
    /// light goes out nothing asks for anything, which is what keeps an
    /// idle instance idle.
    pub fn needs_a_frame(&self) -> bool {
        self.struck || self.is_sounding()
    }

    /// Forget the terminated voices, once the host has been told
    /// ([`VoicePool::clear_terminated`]).
    pub fn clear_terminated(&mut self) {
        self.pool.clear_terminated();
    }

    /// Silence everything at once, without fades, and drop the outbox with
    /// it ([`VoicePool::clear`]).
    ///
    /// For the host's reset and deactivate path, where there is no block left
    /// to fade into and no one listening. Never a musical stop — those all
    /// fade.
    pub fn reset(&mut self) {
        self.pool.clear();
        self.audition_voice = None;
        self.auditions.set_sounding(None);
        // The preview too, and without replaying it: the generation it was
        // started for stays seen.
        self.preview_voice = None;
        self.preview_tail = None;
        if let Some(channel) = &self.preview {
            channel.set_playing(false);
        }
        // Nothing sounding and nothing struck: a reset engine asks the
        // editor for no frames ([`Engine::needs_a_frame`]).
        self.struck = false;
        // And nothing to ramp from: the next block, whenever it comes,
        // starts from silence and has no previous gain to join onto
        // ([`Engine::master_volume`]).
        self.master_volume = None;
    }

    /// Point every sounding voice at its lane's current volume and pan,
    /// once per block.
    ///
    /// This is what the two atomics in [`crate::model::PublishedLane`] are
    /// for: a knob being dragged writes into the snapshot the audio thread
    /// already holds, and a voice that is already sounding hears the move on
    /// the next block without anything being republished. Both setters cache
    /// their conversion behind a comparison
    /// ([`crate::voices::Voice::set_lane_volume_db`]), so a block in which
    /// nothing moved costs two compares a voice and no `powf` at all.
    ///
    /// A voice whose lane is no longer published — deleted, or its sample
    /// cleared — keeps the volume and pan it last had and plays to its end (the
    /// audio outlives the lane that pointed at it). Stolen voices finishing
    /// their fade are not reached either, by the same rule that keeps every
    /// other event off them ([`VoicePool::voices_mut`]).
    ///
    /// The lane lookup is a scan per voice ([`PublishedLanes::lane`]), which
    /// is a bounded 64 × the published lane count and so allowed on this
    /// path; a kit is eight lanes, and nothing here allocates to do better.
    ///
    /// The lane's time settings ride along: [`lane_time`] is re-read here once
    /// per block with the tempo read once, so a pitch drag, a fine trim or a
    /// tempo change is heard on a note already sounding within the next block;
    /// [`Voice::set_time`] compares before it recomputes, so an unchanged block
    /// costs no `powf`.
    fn sync_lane_controls(&mut self, lanes: &PublishedLanes, host_rate: f64) {
        let tempo = self.tempo.get();
        for voice in self.pool.voices_mut() {
            let Some(lane) = lanes.lane(voice.lane_id) else { continue };
            voice.set_lane_volume_db(lane.volume_db());
            voice.set_lane_pan(lane.pan());
            let (speed, pitch_st) = lane_time(lane, voice.natural_secs, voice.rendered, tempo);
            voice.set_time(speed, pitch_st, host_rate);
        }
    }

    /// Apply one event at the frame the walk has reached.
    ///
    /// Every arm but the note-on is a single question put to the pool, which
    /// answers it for every voice the event names — one key can be a stack
    /// of lanes, so "every" rather than "the first" is the whole point
    /// ([`VoicePool::release`] and its siblings explain why they count).
    fn dispatch(&mut self, lanes: &PublishedLanes, event: EngineEvent, host_rate: f64) {
        match event {
            EngineEvent::NoteOn { note, velocity } => {
                self.note_on(lanes, note, velocity, host_rate);
            }
            EngineEvent::NoteOff { note } => {
                self.pool.release(note.channel, note.key, note.voice_id);
            }
            EngineEvent::Choke { note } => self.choke(note),
            EngineEvent::PolyTuning { note, semis } => {
                // The host's tuning plus the switch's key offset: the pool
                // is only ever told effective tunings
                // ([`effective_semis`]).
                let semis = effective_semis(semis, note.key, lanes.sample_pad_switch());
                self.pool.poly_tuning(note.channel, note.key, note.voice_id, semis, host_rate);
            }
            EngineEvent::PolyPan { note, pan } => {
                self.pool.poly_pan(note.channel, note.key, note.voice_id, pan);
            }
            EngineEvent::PolyVolume { note, gain } => {
                self.pool.poly_volume(note.channel, note.key, note.voice_id, gain);
            }
        }
    }

    /// Start a voice in every published lane on this key.
    ///
    /// Every lane, not the first: lanes layer, and two lanes sharing a note
    /// are a stacked sound (a kick and a sub, a snare and its rimshot) which
    /// a note-on has to trigger as one. Only published lanes exist here, and
    /// a lane is published exactly when it has a sample
    /// ([`crate::model::PublishedLane`]) — so "and which has a sample" needs
    /// no test of its own, and a key no lane answers starts nothing at all.
    /// A key the host left open ([`KEY_WILDCARD`]) is one of those
    /// keys, which is why a wildcard note-on is silent without a case of
    /// its own here (module docs, §"Events the host left open").
    ///
    /// What each lane's voice is made of is [`start_lane`](Self::start_lane)
    /// below, which an audition starts its single voice through as well.
    ///
    /// The tuning a voice starts at is the switch's
    /// ([`effective_semis`]): 0.0 — the sample's own pitch — with the switch
    /// off, and `key − 60` with it on, before any `PolyTuning` of the host's
    /// arrives. The switch is read once per note-on rather than once per
    /// lane: it is instance-wide, so every lane stacked on the key is
    /// pitched by the same offset.
    fn note_on(
        &mut self,
        lanes: &PublishedLanes,
        note: NoteTarget,
        velocity: f32,
        host_rate: f64,
    ) {
        let semis = effective_semis(0.0, note.key, lanes.sample_pad_switch());
        for lane in lanes.lanes() {
            if lane.note != note.key {
                continue;
            }
            self.start_lane(lane, note, velocity, semis, lane.output, host_rate);
        }
    }

    /// Sound one lane because the GUI asked, at the frame the drain has
    /// reached — the top of the block.
    ///
    /// A note-on for exactly that lane, found **by id**: a lane sharing its
    /// note with another is auditioned alone, which is the whole point of
    /// the button (the question it asks is what *this* row holds). At the
    /// lane's own note, at [`AUDITION_VELOCITY`], and at the **sample's own
    /// pitch whatever the switch says**. The switch exists to
    /// cancel the retune bitstep's Sample pad puts on its notes; an
    /// audition carries no such retune, so applying the switch here would
    /// pitch a C1 lane two octaves down for nothing — and the sample's own
    /// pitch is exactly what bitstep's Sample pad lanes play through the
    /// switch, so it is also the honest preview. The lane's choke group
    /// fires too, because
    /// [`VoicePool::start`] applies it: auditioning the closed hat cuts the
    /// open one, as playing it would.
    ///
    /// The voice takes the shape of a note the host did not track — channel
    /// 0 and no voice id — so a note-off on that key releases an audition of
    /// a gate lane exactly as it releases a played note, and nothing else
    /// can tell the two apart. An audition nothing ever releases still ends:
    /// a voice is finished when it runs past its sample, whatever its mode.
    ///
    /// A lane id that is not published — the lane was deleted, or its
    /// sample cleared, between the click and this block — sounds nothing.
    /// There is nothing to report from the audio thread and nothing to
    /// report it to.
    ///
    /// One audition sounds at a time: a new one fades the last (the play button
    /// and `S` start and stop it), and the new one is remembered by its voice
    /// so it can be stopped ([`stop_audition`](Self::stop_audition)) and
    /// reported sounding.
    fn audition(&mut self, lanes: &PublishedLanes, lane_id: u32, host_rate: f64) {
        let Some(lane) = lanes.lane(lane_id) else { return };
        self.stop_audition();
        let note = NoteTarget { channel: 0, key: lane.note, voice_id: None };
        // No tuning came with it, so there is nothing for the switch to
        // cancel: the sample's own pitch.
        let semis = 0.0;
        // On the main output whatever the lane's routing: the
        // button is a check of what the row holds, heard where the user is
        // listening even before any aux chain exists.
        let started =
            self.start_lane(lane, note, AUDITION_VELOCITY, semis, OUTPUT_MAIN, host_rate);
        // The voice just started is the youngest in the pool.
        self.audition_voice = started
            .and_then(|_| self.pool.voices().map(|voice| voice.age).max())
            .map(|age| (lane_id, age));
    }

    /// Fade the sounding audition, if there is one, as an early stop (over
    /// the short fade, like a choke). Its lane's other voices — notes the
    /// host played — carry on.
    fn stop_audition(&mut self) {
        let Some((_, age)) = self.audition_voice.take() else { return };
        for voice in self.pool.voices_mut() {
            if voice.age == age {
                voice.begin_fade();
            }
        }
    }

    /// Start one voice: this lane, this note's identity, this tuning, into
    /// this output (the lane's own for a note-on, the main one for an
    /// audition). Returns the pool slot it went into, `None` for a sample
    /// with nothing to play ([`VoicePool::start`]).
    ///
    /// The single place a [`VoiceStart`] is built, so a note-on and an
    /// audition cannot drift apart over what a lane's voice is made of. The
    /// lane's volume, pan, choke group and mode are read here, at the start,
    /// and the voice keeps the last two for its life ([`VoiceStart`]); the
    /// voice gets its **own** handle on the lane's audio (`Shared::clone`, a
    /// refcount bump and no allocation), which is what makes replacing or
    /// clearing that lane's sample mid-note safe. Cut itself goes first
    /// ([`VoicePool::cut_lane`]), then [`VoicePool::start`] does the rest:
    /// the choke group, then a steal if the pool is full.
    ///
    /// Also the one place the lane's activity counter moves: every voice start
    /// is a hit, and every voice start is here, so the lights the GUI draws see
    /// a note-on and an audition alike. A note no lane answers moves nothing,
    /// because it reaches no lane; a note two lanes answer moves both of
    /// theirs.
    fn start_lane(
        &mut self,
        lane: &PublishedLane,
        note: NoteTarget,
        velocity: f32,
        semis: f32,
        output: u8,
        host_rate: f64,
    ) -> Option<usize> {
        // Cut itself: the lane's own voices fade first, so the voice about to
        // start is not among them. Independent of the choke group, which
        // `start` still applies.
        if lane.cut_itself {
            self.pool.cut_lane(lane.id);
        }
        let natural = time::natural_secs(lane.sample.frame_count(), lane.sample.rate);
        let (speed, pitch_st) = lane_time(lane, natural, lane.rendered, self.tempo.get());
        let slot = self.pool.start(
            VoiceStart {
                lane_id: lane.id,
                note_key: note.key,
                channel: note.channel,
                voice_id: note.voice_id,
                sample: lane.sample.clone(),
                velocity,
                lane_volume_db: lane.volume_db(),
                lane_pan: lane.pan(),
                lane_choke: lane.choke,
                lane_mode: lane.mode,
                semis,
                speed,
                pitch_st,
                attack_ms: lane.attack_ms(),
                release_ms: lane.release_ms(),
                output,
                rendered: lane.rendered,
            },
            host_rate,
        );
        // After the start, not before: the marker says a voice is sounding,
        // and `start` is what makes that true. A relaxed add either way —
        // nothing the GUI reads is ordered against it.
        lane.mark_activity();
        // And the same news for the editor, which has to be woken to read
        // that marker at all ([`Engine::needs_a_frame`]).
        self.struck = true;
        slot
    }

    /// Answer the host's choke event: fade every live voice it names.
    ///
    /// Not [`VoicePool::choke_group`], which is the lane feature and is
    /// applied by the pool itself at a note-on — this is the host saying one
    /// particular note is finished, so it is matched like a note-off
    /// ([`crate::voices::Voice::matches`]) and, unlike a note-off, it cuts a
    /// one-shot too. A choke is an early stop, so it fades like every other
    /// one ([`crate::voices::Voice::begin_fade`]), and a voice already
    /// fading keeps the ramp it is on.
    ///
    /// A host's choke of *everything* — a wildcard key, a wildcard channel,
    /// no voice id — therefore fades the whole live pool, since
    /// the predicate reads a wildcard as match-any.
    fn choke(&mut self, note: NoteTarget) {
        for voice in self.pool.voices_mut() {
            if voice.matches(note.channel, note.key, note.voice_id) {
                voice.begin_fade();
            }
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    use crate::model::{Model, SampleRef};
    use crate::time::{Length, NoteLength};
    use crate::sample::SampleData;
    use crate::voices::{
        fade_frames_for, pan_gains, step_for, LaneMode, CHANNEL_WILDCARD, MAX_VOICES,
    };

    /// The host rate every test runs at, and the rate every fixture sample
    /// was recorded at — so the step is exactly 1.0 and the exactness
    /// contracts of the engine apply.
    const RATE: f64 = 48_000.0;

    /// A fresh model with the "bitstep Sample pad" switch **off**, which is
    /// what every test here measures pitch against unless it turns the
    /// switch on itself: a new instance starts with it on
    /// ([`crate::model::SAMPLE_PAD_SWITCH_DEFAULT`]), and on, a note on C1
    /// with no tuning plays two octaves down, which is the switch's
    /// business and not these tests'.
    fn switch_off_model() -> Model {
        let mut model = Model::new();
        model.set_sample_pad_switch(false);
        model
    }

    /// A mono sample whose frames are 1.0, 2.0, … `count`, so any frame can
    /// be told from any other by eye in a failure message
    /// (`voices.rs`'s fixtures do the same).
    fn ramp(count: usize) -> Vec<f32> {
        (1..=count).map(|n| n as f32).collect()
    }

    /// Put `frames` on the lane at `index` as its sample, answering `note`,
    /// and pan the lane **hard left** — where the pan law's gains are
    /// exactly 1.0 and 0.0, the one setting in which the mix multiplies by
    /// one and an equality against the file itself can hold
    /// (`voices.rs`'s module docs, §"Unity gain" is a pan position).
    ///
    /// Returns the lane's id. Writing through the model rather than building
    /// a `PublishedLanes` by hand is deliberate: these tests then cover the
    /// real publishing path the GUI and the loader use.
    fn lane_with(model: &mut Model, index: usize, note: u8, frames: Vec<f32>) -> u32 {
        let id = model.lanes()[index].id;
        let data = SampleData { rate: RATE as u32, channels: 1, frames: frames.into() };
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/kick.wav"), vec![0], data);
        assert!(model.set_sample(id, sample), "lane {index} takes a sample");
        assert!(model.set_note(id, note), "lane {index} takes a note");
        assert!(model.set_pan(id, -1.0), "lane {index} pans hard left");
        id
    }

    /// A model with one lane holding an eight-frame ramp on note 36, and the
    /// id of that lane — the fixture most of these tests want.
    fn one_lane_model(frames: usize) -> (Model, u32) {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 36, ramp(frames));
        (model, id)
    }

    /// Which voices an event with no host voice id is about: channel 0 and
    /// this key, the shape a host that does not track voices sends.
    fn target(key: u8) -> NoteTarget {
        NoteTarget { channel: 0, key, voice_id: None }
    }

    /// A full-velocity note-on at `timing`.
    fn note_on(timing: u32, key: u8) -> TimedEvent {
        TimedEvent::new(timing, EngineEvent::NoteOn { note: target(key), velocity: 1.0 })
    }

    /// A note-off at `timing` for the note [`note_on`] started.
    fn note_off(timing: u32, key: u8) -> TimedEvent {
        TimedEvent::new(timing, EngineEvent::NoteOff { note: target(key) })
    }

    /// The pan law's centre gain, −3 dB. Expectations multiply by this
    /// rather than spelling the number out, so both sides of an assertion
    /// are the same f32 multiplication.
    fn centre() -> f32 {
        pan_gains(0.0).0
    }

    // ---- The timed walk and the mix ------------------------------------

    /// The engine's first contract: an untuned one-shot at the file's own rate,
    /// through the whole engine, **is** the file. Not close to it — equal,
    /// frame for frame, with the rest of the block silent.
    #[test]
    fn an_untuned_one_shot_through_the_engine_reproduces_the_file() {
        let (model, _id) = one_lane_model(8);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 12];
        let mut right = [0.0f32; 12];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(right, [0.0; 12], "a hard left lane leaves the right side silent");
        assert_eq!(engine.pool().active_count(), 0, "the one-shot ended with its sample");
    }

    /// A note-on applies at its own frame of the block, not at the top of it
    /// (module docs, §Time inside a block): the frames before the offset are
    /// untouched and the sample starts exactly on it.
    #[test]
    fn a_note_on_sounds_from_its_own_sample_offset() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(3, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [0.0, 0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 0.0]);
    }

    /// The other half of the contract ("block stitching must be exact"): one
    /// block equals two halves, bit for bit, with an event inside each half and
    /// a voice sounding across the seam.
    ///
    /// Tuned by a fraction of a semitone on purpose, so the interpolator is
    /// doing real work at fractional positions on both sides of the
    /// boundary — the case a position kept in f32, or a fade counted in
    /// blocks rather than frames, would quietly get wrong.
    #[test]
    fn one_block_equals_two_halves_with_an_event_in_each() {
        let (model, _id) = one_lane_model(40);
        let tuning = |timing| {
            TimedEvent::new(timing, EngineEvent::PolyTuning { note: target(36), semis: 3.5 })
        };

        let mut engine = Engine::new();
        let mut whole_l = [0.0f32; 16];
        let mut whole_r = [0.0f32; 16];
        engine.run_block(
            model.published(),
            &[note_on(2, 36), tuning(11)],
            &mut Outputs::main(&mut whole_l, &mut whole_r),
            RATE,
            1.0,
        );

        let mut halves = Engine::new();
        let mut first_l = [0.0f32; 8];
        let mut first_r = [0.0f32; 8];
        let first = [note_on(2, 36)];
        halves.run_block(
            model.published(),
            &first,
            &mut Outputs::main(&mut first_l, &mut first_r),
            RATE,
            1.0,
        );
        let mut second_l = [0.0f32; 8];
        let mut second_r = [0.0f32; 8];
        halves.run_block(
            model.published(),
            &[tuning(3)],
            &mut Outputs::main(&mut second_l, &mut second_r),
            RATE,
            1.0,
        );

        assert_eq!(whole_l[..8], first_l, "the first half of the block");
        assert_eq!(whole_l[8..], second_l, "the second half, across the seam");
        assert_eq!(whole_r[..8], first_r);
        assert_eq!(whole_r[8..], second_r);
    }

    /// Lanes layer: two lanes on one note are two voices, and the mix is
    /// their sum at their own gains — nothing is divided by the number of
    /// voices.
    #[test]
    fn two_lanes_on_one_note_sum_into_the_mix() {
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 36, ramp(4));
        lane_with(&mut model, 1, 36, vec![10.0; 4]);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 6];
        let mut right = [0.0f32; 6];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [11.0, 12.0, 13.0, 14.0, 0.0, 0.0]);
        assert_eq!(right, [0.0; 6]);
    }

    /// The output buffer belongs to the host and arrives holding whatever
    /// was last in it, so the engine writes the block rather than adding to
    /// it: a block in which nothing sounds is silence, not the host's
    /// leftovers.
    #[test]
    fn the_engine_writes_over_whatever_the_host_left_in_the_buffers() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.5f32; 6];
        let mut right = [-0.5f32; 6];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [0.0; 6], "no note, no sound");
        assert_eq!(right, [0.0; 6]);
    }

    /// A note no lane answers starts nothing at all, and neither does a
    /// note on a lane with no sample — a lane without one is not published,
    /// so the engine never sees it.
    #[test]
    fn a_note_no_lane_answers_starts_nothing() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 6];
        let mut right = [0.0f32; 6];
        // 37 is the second default lane's note, and that lane is empty.
        engine.run_block(
            model.published(),
            &[note_on(0, 37)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 0);
        assert_eq!(left, [0.0; 6]);
    }

    /// Several events sharing one offset are all applied before the frame
    /// they share is rendered, in the order they arrived — which is what
    /// makes bitstep's own ordering work, since it sends the expression
    /// events at the note-on's own timing.
    #[test]
    fn events_at_one_offset_all_apply_before_the_frame_they_share() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                note_on(0, 36),
                TimedEvent::new(0, EngineEvent::PolyVolume { note: target(36), gain: 0.5 }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [0.5, 1.0, 1.5, 2.0], "the first frame is already at half gain");
    }

    /// An offset past the end of the block lands at its end rather than
    /// being dropped: the note sounds from the next block instead of never
    /// (module docs, §Time inside a block).
    #[test]
    fn an_event_past_the_end_of_the_block_lands_at_the_end_of_it() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(999, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [0.0; 4], "nothing sounded inside this block");
        assert_eq!(engine.pool().active_count(), 1, "but the voice is waiting for the next");

        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "and it starts from the sample's first frame");
    }

    /// An event behind the frames already rendered is applied where the
    /// cursor stands, never backwards over audio that is already written
    /// (module docs, §Time inside a block). Hosts deliver events in order;
    /// this pins what happens if one does not.
    #[test]
    fn an_out_of_order_event_is_applied_at_the_cursor_and_not_backwards() {
        let (mut model, id) = one_lane_model(4);
        // Stacked hits on one lane: the v1 behaviour, so the lane keeps its
        // earlier voices rather than cutting them.
        assert!(model.set_cut_itself(id, false));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(4, 36), note_on(1, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        // Both notes start at frame 4: the first at its own offset, the
        // second where the walk had already reached.
        assert_eq!(left, [0.0, 0.0, 0.0, 0.0, 2.0, 4.0, 6.0, 8.0]);
    }

    /// A block with no frames in it still takes its events: a host may ask
    /// for zero frames, and the note it sends with them is not lost.
    #[test]
    fn a_block_of_no_frames_still_takes_its_events() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left: [f32; 0] = [];
        let mut right: [f32; 0] = [];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 1);
    }

    /// A short side makes a short block rather than a panic: the frames both
    /// buffers have are rendered, and all of both is silenced first.
    #[test]
    fn a_mismatched_pair_of_buffers_renders_the_frames_they_share() {
        let (model, _id) = one_lane_model(8);
        let mut engine = Engine::new();

        let mut left = [9.0f32; 6];
        let mut right = [9.0f32; 2];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 0.0, 0.0, 0.0, 0.0], "two frames rendered, the rest silenced");
        assert_eq!(right, [0.0; 2]);
    }

    // ---- The lanes the voices are driven against -----------------------

    /// The whole point of the published atomics: a lane's volume moved while
    /// a note is sounding is heard on the next block, without the lane list
    /// being rebuilt.
    #[test]
    fn a_lane_volume_moved_between_blocks_is_heard_on_the_next_one() {
        let (mut model, id) = one_lane_model(8);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(left, [1.0, 2.0, 3.0, 4.0]);

        // −6.0206 dB is a half, but the assertion does not need the number:
        // the gain the voice must now be at is the lane's own conversion.
        assert!(model.set_volume_db(id, -6.0));
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let gain = crate::voices::db_to_gain(-6.0);
        assert_eq!(left, [5.0 * gain, 6.0 * gain, 7.0 * gain, 8.0 * gain]);
    }

    /// And the same for pan: moved mid-note, heard on the next block. The
    /// lane starts hard left, so the move is visible as the right side
    /// arriving.
    #[test]
    fn a_lane_pan_moved_between_blocks_is_heard_on_the_next_one() {
        let (mut model, id) = one_lane_model(8);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(right, [0.0; 4], "hard left to begin with");

        assert!(model.set_pan(id, 0.0));
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [5.0 * centre(), 6.0 * centre(), 7.0 * centre(), 8.0 * centre()]);
        assert_eq!(right, left, "centred, both sides at the pan law's centre gain");
    }

    /// The lane's published attack reaches the voice at its note-on: 10 ms at
    /// 48 kHz fades the first 480 frames in from silence, and frame 480 plays
    /// at the file's own value.
    #[test]
    fn a_lanes_attack_fades_its_voices_in() {
        let (mut model, id) = one_lane_model(1_000);
        assert!(model.set_attack_ms(id, 10.0));
        let mut engine = Engine::new();

        let mut left = vec![0.0f32; 512];
        let mut right = vec![0.0f32; 512];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[0], 0.0, "from silence");
        assert_eq!(left[240], 241.0 * (240.0 / 480.0), "halfway up the line");
        assert_eq!(left[480], 481.0, "full gain at frame 480");
        assert_eq!(left[511], 512.0);
    }

    /// The lane's published release reaches a gate voice at its note-on: 100 ms
    /// at 48 kHz fades the note-off over 4800 frames, silent from frame 4800 on
    /// and the voice reported gone.
    #[test]
    fn a_lanes_release_sets_its_gate_fade() {
        let (mut model, id) = one_lane_model(8_000);
        assert!(model.set_mode(id, LaneMode::Gate));
        assert!(model.set_release_ms(id, 100.0));
        let mut engine = Engine::new();

        let mut left = vec![0.0f32; 4_808];
        let mut right = vec![0.0f32; 4_808];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_off(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[0], 1.0, "the note-off is not a step");
        assert_eq!(left[2_400], 2_401.0 * 0.5, "halfway down the release");
        assert!(left[4_799] > 0.0, "still sounding on the ramp's last frame");
        assert_eq!(left[4_800..], [0.0; 8], "silent from frame 4800");
        assert_eq!(engine.pool().active_count(), 0);
        assert_eq!(engine.pool().terminated().len(), 1);
    }

    /// v1 projects stay the same: a Gate lane loaded from a **version 1**
    /// project releases over v1's fixed fade — the note-off is finished
    /// [`FADE_MS`](crate::voices::FADE_MS) after it lands, not the 300 ms a v2
    /// lane missing the field would get.
    #[test]
    fn a_v1_loaded_gate_lane_fades_out_fade_ms_after_its_note_off() {
        let json = r#"{"version":1,"lanes":[{"name":"Hat","note":36,"mode":"gate"}]}"#;
        let snapshot: crate::state::Snapshot = serde_json::from_str(json).expect("v1 parses");
        let mut model = switch_off_model();
        let pending = model.load_snapshot(snapshot);
        assert!(pending.is_empty(), "no sample to load");
        lane_with(&mut model, 0, 36, ramp(48_000));
        let lane = &model.lanes()[0];
        assert_eq!((lane.mode, lane.release_ms), (LaneMode::Gate, 0.0), "a v1 gate lane");
        assert!(!lane.cut_itself);

        let off_at = 100usize;
        let fade_ms_frames = (crate::voices::FADE_MS as f64 / 1000.0 * RATE).ceil() as usize;
        assert_eq!(fade_ms_frames, 192, "4 ms at 48 kHz");
        let end = off_at + fade_ms_frames;
        let mut engine = Engine::new();
        let mut left = vec![0.0f32; end + 64];
        let mut right = vec![0.0f32; end + 64];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_off(off_at as u32, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[off_at - 1], off_at as f32, "full level up to the note-off");
        assert!(left[end - 1] > 0.0, "still on the ramp's last frame");
        assert!(left[end..].iter().all(|&s| s == 0.0), "silent FADE_MS after the note-off");
        assert_eq!(engine.pool().active_count(), 0, "the voice is gone with its fade");
        assert_eq!(engine.pool().terminated().len(), 1);
    }

    /// A voice outlives the lane it came from: deleting the lane mid-note
    /// republishes a list without it, and the note goes on to the end of the
    /// audio it is holding open.
    #[test]
    fn a_voice_whose_lane_is_deleted_plays_on_to_the_end_of_its_sample() {
        let (mut model, id) = one_lane_model(8);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(model.delete_lane(id));
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [5.0, 6.0, 7.0, 8.0], "at the volume and pan it last had");
        assert_eq!(engine.pool().active_count(), 0, "and it ends with its sample, as ever");
    }

    /// The master volume is the last factor of the chain and scales the
    /// whole block linearly.
    ///
    /// Flat here because this is the engine's first block: there is no
    /// previous gain to ramp from, so the block is mixed at the one value
    /// ([`a_master_gain_jump_ramps_across_the_block_it_lands_in`] is the
    /// case where there is).
    #[test]
    fn master_volume_scales_the_block_linearly() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            0.5,
        );

        assert_eq!(left, [0.5, 1.0, 1.5, 2.0]);
    }

    /// The master gain as a line (module docs, §The master gain is a
    /// line): a block is rendered on a line from what the last block
    /// was given to this one's value, one frame at a time.
    ///
    /// Read off a hard-left lane at unity, where an output frame is the
    /// file's frame times that frame's own gain, and pinned against the
    /// line computed by hand — 1.0 down to 0.5 over eight frames is a step
    /// of −0.0625, and every value and every product below is exact in f32.
    #[test]
    fn a_master_gain_jump_ramps_across_the_block_it_lands_in() {
        let (model, _id) = one_lane_model(16);
        let mut engine = Engine::new();

        // A block at full gain with nothing in it, to leave 1.0 behind as
        // the gain the next block ramps from.
        let mut quiet_l = [0.0f32; 8];
        let mut quiet_r = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut quiet_l, &mut quiet_r),
            RATE,
            1.0,
        );

        // And the jump: the host drops the volume to 0.5 between blocks.
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            0.5,
        );

        let gains = [1.0f32, 0.9375, 0.875, 0.8125, 0.75, 0.6875, 0.625, 0.5625];
        let expected: Vec<f32> = (1..=8).map(|n| n as f32).zip(gains).map(|(f, g)| f * g).collect();
        assert_eq!(expected, [1.0, 1.875, 2.625, 3.25, 3.75, 4.125, 4.375, 4.5], "by hand");
        assert_eq!(left.to_vec(), expected, "the block is mixed along the line");
        assert_eq!(right, [0.0; 8], "a hard left lane leaves the right side silent");

        // The target lands on the frame after the block's last, which is the
        // next block's first: the following block, asked for the same 0.5,
        // is flat at it.
        let mut next_l = [0.0f32; 8];
        let mut next_r = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut next_l, &mut next_r),
            RATE,
            0.5,
        );

        let voiced: Vec<f32> = (9..=16).map(|n| n as f32 * 0.5).collect();
        assert_eq!(next_l.to_vec(), voiced, "the ramp arrived and stopped");
    }

    /// The point of the ramp: the largest jump a host can automate — full gain
    /// to silence between two blocks — is a slope and not a step, at the block
    /// boundary included.
    ///
    /// Read off a DC lane at a hard pan, where every output frame *is* the
    /// gain, so a voice's anti-click threshold can be applied to the
    /// output directly, exactly as `voices.rs` applies it to a fade.
    #[test]
    fn a_master_gain_jump_never_steps_by_more_than_one_frame_of_the_ramp() {
        const FRAMES: usize = 64;
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 36, vec![1.0; FRAMES * 4]);
        let mut engine = Engine::new();

        let mut first_l = [0.0f32; FRAMES];
        let mut first_r = [0.0f32; FRAMES];
        let note = [note_on(0, 36)];
        engine.run_block(
            model.published(),
            &note,
            &mut Outputs::main(&mut first_l, &mut first_r),
            RATE,
            1.0,
        );
        assert_eq!(first_l, [1.0; FRAMES], "a DC sample at unity is the gain itself");

        let mut second_l = [0.0f32; FRAMES];
        let mut second_r = [0.0f32; FRAMES];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut second_l, &mut second_r),
            RATE,
            0.0,
        );

        assert_eq!(second_l[0], 1.0, "the boundary itself is not a step");
        assert_eq!(second_l[FRAMES - 1], 1.0 / FRAMES as f32, "and the fall is nearly over");

        // One step of the ramp, plus one f32 ulp — the allowance
        // `voices.rs` makes for a fade, for the same rounding reason.
        let step = 1.0 / FRAMES as f32;
        let threshold = step + f32::EPSILON;
        let block: Vec<f32> = first_l.iter().chain(second_l.iter()).copied().collect();
        for (index, pair) in block.windows(2).enumerate() {
            let jump = (pair[1] - pair[0]).abs();
            assert!(
                jump <= threshold,
                "frames {index}..{} jump by {jump}, a step is {step}",
                index + 1
            );
            assert!(pair[1] <= pair[0], "frame {index} got louder on the way down");
        }
    }

    /// An event inside the block splits the mix in two, and the ramp carries
    /// on across the split rather than restarting at it
    /// ([`crate::voices::MasterRamp::from_frame`]) — restarting would put
    /// back exactly the step the ramp is here to remove.
    ///
    /// Against a block with no event in it, because the gain at a frame
    /// cannot depend on where the block was cut: the two outputs must be
    /// equal, bit for bit.
    #[test]
    fn an_event_inside_the_block_does_not_restart_the_ramp() {
        let (model, _id) = one_lane_model(16);
        // PolyVolumes at the gain a voice already has: events the walk has
        // to split the block at, and which change nothing it mixes. Two of
        // them, so that the block is cut into three pieces and a piece
        // starts at a frame that is neither the block's first nor the one
        // the tail begins at.
        let no_op = |timing| {
            TimedEvent::new(timing, EngineEvent::PolyVolume { note: target(36), gain: 1.0 })
        };

        let mut split = Engine::new();
        let mut split_l = [0.0f32; 8];
        let mut split_r = [0.0f32; 8];
        split.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut [0.0; 8], &mut [0.0; 8]),
            RATE,
            1.0,
        );
        split.run_block(
            model.published(),
            &[note_on(0, 36), no_op(3), no_op(6)],
            &mut Outputs::main(&mut split_l, &mut split_r),
            RATE,
            0.5,
        );

        let mut whole = Engine::new();
        let mut whole_l = [0.0f32; 8];
        let mut whole_r = [0.0f32; 8];
        whole.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut [0.0; 8], &mut [0.0; 8]),
            RATE,
            1.0,
        );
        whole.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut whole_l, &mut whole_r),
            RATE,
            0.5,
        );

        assert_eq!(split_l, whole_l, "the event moved the gain");
        assert_eq!(split_r, whole_r);
    }

    /// A reset leaves no gain to ramp from: the block after it starts from
    /// silence, so there is nothing for its gain to join onto and it is mixed
    /// flat.
    #[test]
    fn a_reset_forgets_the_gain_the_next_block_would_ramp_from() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut [0.0; 8], &mut [0.0; 8]),
            RATE,
            1.0,
        );
        engine.reset();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            0.5,
        );

        assert_eq!(left, [0.5, 1.0, 1.5, 2.0], "flat at the new gain, not a ramp from 1.0");
    }

    /// A lane's choke group is applied by the note-on that triggers it, at
    /// that event's own offset: the earlier voice is fading from there, not
    /// from the top of the block.
    ///
    /// The ramp's own first frame is still at full gain — entering a fade
    /// must not step the gain ([`crate::voices::FadeState::gain`]) — so what
    /// marks the choke is where the fall *begins*, one frame on.
    #[test]
    fn a_note_on_chokes_its_group_at_its_own_offset() {
        let mut model = switch_off_model();
        let open = lane_with(&mut model, 0, 36, vec![1.0; 64]);
        let closed = lane_with(&mut model, 1, 37, vec![0.0; 64]);
        assert!(model.set_choke(open, 1));
        assert!(model.set_choke(closed, 1));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_on(4, 37)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[..5], [1.0; 5], "full gain up to and including the choke's own frame");
        assert!(
            left[5] < 1.0 && left.windows(2).all(|pair| pair[1] <= pair[0]),
            "and falling from there: {left:?}"
        );
    }

    // ---- Cut itself ----------------------------------------------------

    /// A second hit on a cut-itself lane fades the first voice out over
    /// `FADE_MS` and no more, without a click, and with the lane in no choke
    /// group at all — cut itself is not a group.
    ///
    /// Read off a DC lane hard left at unity, where the new voice adds
    /// exactly 1.0 from its own frame on, so what is left over is the first
    /// voice's ramp and nothing else.
    #[test]
    fn a_second_hit_on_a_cut_itself_lane_fades_the_first_without_a_click() {
        let frames = fade_frames_for(RATE) as usize;
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 36, vec![1.0; frames * 4]);
        let published = model.published().get();
        let lane = published.lane(id).expect("published");
        assert!(lane.cut_itself, "a new lane cuts itself");
        assert_eq!(lane.choke, 0, "and is in no choke group");
        let mut engine = Engine::new();

        let hit = 16;
        let mut left = vec![0.0f32; hit + frames + 8];
        let mut right = vec![0.0f32; hit + frames + 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_on(hit as u32, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[..hit], vec![1.0; hit][..], "the first hit alone, at full gain");
        let first: Vec<f32> = left[hit..].iter().map(|both| both - 1.0).collect();
        assert_eq!(first[0], 1.0, "the cut is not a step: the ramp starts at full gain");
        let step = 1.0 / frames as f32;
        let threshold = step + 4.0 * f32::EPSILON;
        for (index, pair) in first.windows(2).enumerate() {
            let jump = pair[0] - pair[1];
            assert!(
                (0.0..=threshold).contains(&jump),
                "frames {index}..{} of the cut move by {jump}, a step is {step}",
                index + 1
            );
        }
        assert_eq!(first[frames..], [0.0; 8], "the first hit is silent after FADE_MS");
        assert_eq!(engine.pool().active_count(), 1, "only the second hit is left");
        assert_eq!(engine.pool().terminated().len(), 1, "and the cut one was reported ended");
    }

    /// With cut itself off, a second hit stacks on the first: both sound,
    /// the v1 behaviour every v1 project's lanes keep.
    #[test]
    fn with_cut_itself_off_both_hits_sound() {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 36, vec![1.0; 4096]);
        assert!(model.set_cut_itself(id, false));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 512];
        let mut right = [0.0f32; 512];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_on(8, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[..8], [1.0; 8]);
        assert_eq!(left[8..], [2.0; 504], "both hits at full gain, to the end of the block");
        assert_eq!(engine.pool().active_count(), 2);
        assert!(engine.pool().voices().all(|voice| !voice.is_fading()));
    }

    /// An audition is a hit like a note-on: it cuts the lane's sounding voice
    /// too.
    #[test]
    fn an_audition_of_a_cut_itself_lane_cuts_its_sounding_voice() {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 36, vec![1.0; 4096]);
        let mut engine = Engine::new();
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(engine.auditions().push(id));
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fading: Vec<bool> = engine.pool().voices().map(|voice| voice.is_fading()).collect();
        assert_eq!(fading, vec![true, false], "the played hit fades, the audition sounds");
    }

    /// Cut itself and a choke group both run: a cut-itself lane in a group cuts
    /// its own earlier hit *and* the group's other lanes, a group-mate without
    /// cut itself still chokes it, and a lane in another group is left alone. A
    /// voice both sweeps reach fades once, over `FADE_MS`.
    #[test]
    fn cut_itself_works_alongside_a_choke_group() {
        let frames = fade_frames_for(RATE) as usize;
        let mut model = switch_off_model();
        let closed = lane_with(&mut model, 0, 36, vec![1.0; frames * 8]);
        let open = lane_with(&mut model, 1, 37, vec![1.0; frames * 8]);
        let tom = lane_with(&mut model, 2, 38, vec![1.0; frames * 8]);
        assert!(model.set_choke(closed, 1));
        assert!(model.set_choke(open, 1));
        assert!(model.set_cut_itself(open, false));
        assert!(model.set_choke(tom, 2));
        let mut engine = Engine::new();
        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];

        let lanes_fading = |engine: &Engine| -> Vec<(u32, bool)> {
            engine.pool().voices().map(|voice| (voice.lane_id, voice.is_fading())).collect()
        };
        engine.run_block(
            model.published(),
            &[note_on(0, 38), note_on(0, 37), note_on(1, 36), note_on(2, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(
            lanes_fading(&engine),
            vec![(tom, false), (open, true), (closed, true), (closed, false)],
            "the closed hat cut the open one and its own first hit; the tom plays on"
        );

        // The open hat has cut itself off but is in the group: it chokes
        // the closed hat's sounding hit all the same.
        engine.run_block(
            model.published(),
            &[note_on(0, 37)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        let sounding: Vec<(u32, bool)> =
            lanes_fading(&engine).into_iter().filter(|&(_, fading)| !fading).collect();
        assert_eq!(sounding, vec![(tom, false), (open, false)]);

        // Every faded voice is gone within FADE_MS of its cut, however many
        // sweeps reached it.
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(lanes_fading(&engine), vec![(tom, false), (open, false)]);
    }

    // ---- The event kinds -----------------------------------------------

    /// Note-off is the whole of gate versus one-shot: a gate lane's voice
    /// starts fading, a one-shot's ignores it and plays on.
    #[test]
    fn a_note_off_gates_a_gate_lane_and_leaves_a_one_shot_playing() {
        let mut model = switch_off_model();
        let gate = lane_with(&mut model, 0, 36, vec![1.0; 2048]);
        lane_with(&mut model, 1, 37, vec![1.0; 2048]);
        assert!(model.set_mode(gate, LaneMode::Gate));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_on(0, 37), note_off(1, 36), note_off(1, 37)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voices: Vec<bool> = engine.pool().voices().map(|voice| voice.is_fading()).collect();
        assert_eq!(voices, [true, false], "the gate voice fades, the one-shot does not");
    }

    /// The host's own choke cuts the voices it names whatever mode they are
    /// in — including a one-shot, which a note-off would not touch.
    #[test]
    fn a_host_choke_fades_the_one_shot_a_note_off_would_not() {
        let (model, _id) = one_lane_model(2048);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), TimedEvent::new(1, EngineEvent::Choke { note: target(36) })],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(engine.pool().voices().all(|voice| voice.is_fading()), "the one-shot is cut");
    }

    /// A choke the host sends for a note nothing is playing leaves the rest
    /// of the mix alone: it is matched like a note-off, not applied to
    /// everything sounding.
    #[test]
    fn a_host_choke_for_another_note_leaves_the_voices_alone() {
        let (model, _id) = one_lane_model(2048);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), TimedEvent::new(1, EngineEvent::Choke { note: target(48) })],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(engine.pool().voices().all(|voice| !voice.is_fading()));
    }

    /// The three expression events reach the voice they name and move the three
    /// things they are for — here by the host's voice id, which is how a host
    /// that tracks voices addresses one of several notes on a key.
    #[test]
    fn the_expression_events_reach_the_voice_their_id_names() {
        let (model, _id) = one_lane_model(64);
        let mut engine = Engine::new();
        let note = NoteTarget { channel: 0, key: 36, voice_id: Some(7) };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                TimedEvent::new(0, EngineEvent::NoteOn { note, velocity: 1.0 }),
                TimedEvent::new(0, EngineEvent::PolyTuning { note, semis: 12.0 }),
                TimedEvent::new(0, EngineEvent::PolyPan { note, pan: 1.0 }),
                TimedEvent::new(0, EngineEvent::PolyVolume { note, gain: 0.5 }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("the note started");
        assert_eq!(voice.semis, 12.0, "tuned an octave up");
        assert_eq!(voice.poly_pan, 1.0, "panned right of its lane's hard left, so centred");
        assert_eq!(voice.poly_volume, 0.5);
        // Centred by the sum of the lane's −1.0 and the event's +1.0, at
        // half gain, reading every second frame of the ramp.
        assert_eq!(left, [0.5 * centre(), 1.5 * centre(), 2.5 * centre(), 3.5 * centre()]);
        assert_eq!(right, left);
    }

    /// An expression event whose id names no voice moves nothing: a stale
    /// id from the host must not land on whichever note happens to share
    /// its key.
    #[test]
    fn an_expression_event_for_an_unknown_id_moves_nothing() {
        let (model, _id) = one_lane_model(64);
        let mut engine = Engine::new();
        let started = NoteTarget { channel: 0, key: 36, voice_id: Some(1) };
        let stale = NoteTarget { channel: 0, key: 36, voice_id: Some(2) };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                TimedEvent::new(0, EngineEvent::NoteOn { note: started, velocity: 1.0 }),
                TimedEvent::new(0, EngineEvent::PolyVolume { note: stale, gain: 0.0 }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "the sounding voice is untouched");
    }

    /// "Start a voice for **every** lane whose note == key": every
    /// lane on the key answers and no other lane does, and each voice it
    /// starts carries the note's identity beside its own lane's settings.
    #[test]
    fn a_note_on_starts_a_voice_for_every_lane_on_that_key_and_no_other() {
        let mut model = switch_off_model();
        let first = lane_with(&mut model, 0, 36, ramp(64));
        let second = lane_with(&mut model, 1, 36, ramp(64));
        lane_with(&mut model, 2, 37, ramp(64));
        assert!(model.set_choke(second, 3));
        assert!(model.set_mode(second, LaneMode::Gate));
        let mut engine = Engine::new();
        let note = NoteTarget { channel: 9, key: 36, voice_id: Some(4) };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[TimedEvent::new(0, EngineEvent::NoteOn { note, velocity: 1.0 })],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let answered: Vec<u32> = engine.pool().voices().map(|voice| voice.lane_id).collect();
        assert_eq!(answered, [first, second], "the two lanes on the key, and not the third");
        for voice in engine.pool().voices() {
            assert_eq!(voice.channel, 9, "the channel the note arrived on");
            assert_eq!(voice.note_key, 36);
            assert_eq!(voice.voice_id, Some(4), "and the id its later events will name");
        }
        let layered = engine.pool().voices().nth(1).expect("the second lane answered");
        assert_eq!(layered.lane_choke, 3, "each voice keeps its own lane's group");
        assert_eq!(layered.lane_mode, LaneMode::Gate, "and its own lane's mode");
    }

    /// "And which has a sample", through the path a user takes to take one
    /// away: clearing a lane's sample unpublishes the lane, so its note starts
    /// nothing — and nothing panics for the want of audio to play.
    #[test]
    fn a_note_on_for_a_lane_whose_sample_was_cleared_starts_nothing() {
        let (mut model, id) = one_lane_model(8);
        let mut engine = Engine::new();

        assert!(model.clear_sample(id));

        let mut left = [0.0f32; 6];
        let mut right = [0.0f32; 6];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 0);
        assert_eq!(left, [0.0; 6]);
    }

    /// The note-on's velocity is the first factor of the voice's gain chain,
    /// so a half-velocity hit of a lane at unity is the file at half scale.
    #[test]
    fn the_velocity_of_a_note_on_scales_the_voice_it_starts() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[TimedEvent::new(0, EngineEvent::NoteOn { note: target(36), velocity: 0.5 })],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [0.5, 1.0, 1.5, 2.0]);
    }

    /// One note-off ends the whole stack a key started: two gate lanes on
    /// one note are two voices, and releasing only the first of them would
    /// leave the second hanging for ever ([`VoicePool::release`]).
    #[test]
    fn a_note_off_releases_every_gate_voice_the_key_started() {
        let mut model = switch_off_model();
        let first = lane_with(&mut model, 0, 36, vec![1.0; 2048]);
        let second = lane_with(&mut model, 1, 36, vec![1.0; 2048]);
        assert!(model.set_mode(first, LaneMode::Gate));
        assert!(model.set_mode(second, LaneMode::Gate));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_off(1, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 2, "both lanes answered the note");
        assert!(engine.pool().voices().all(|voice| voice.is_fading()), "and both are released");
    }

    /// A note-off with no voice id **is** its channel and key
    /// ([`crate::voices::Voice::matches`]): one arriving on another channel
    /// is about another note and must not end this one.
    #[test]
    fn a_note_off_on_another_channel_releases_nothing() {
        let (mut model, id) = one_lane_model(2048);
        assert!(model.set_mode(id, LaneMode::Gate));
        let mut engine = Engine::new();
        let held = NoteTarget { channel: 1, key: 36, voice_id: None };
        let other = NoteTarget { channel: 2, key: 36, voice_id: None };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                TimedEvent::new(0, EngineEvent::NoteOn { note: held, velocity: 1.0 }),
                TimedEvent::new(1, EngineEvent::NoteOff { note: other }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(engine.pool().voices().all(|voice| !voice.is_fading()), "still held");
    }

    /// The whole arc of a gate note-off: the release fades over
    /// [`FADE_MS`](crate::voices::FADE_MS) frames rather than cutting, the
    /// voice is gone when the ramp reaches silence, and the host is told.
    #[test]
    fn a_released_gate_voice_fades_out_and_is_reported_when_it_lands() {
        let (mut model, id) = one_lane_model(4096);
        assert!(model.set_mode(id, LaneMode::Gate));
        // Release 0 is the v1 gate fade, FADE_MS.
        assert!(model.set_release_ms(id, 0.0));
        let fade = fade_frames_for(RATE) as usize;
        let mut engine = Engine::new();

        // The sample is a ramp, so the fade is pinned against the voice's
        // own output rather than a constant: what matters is that it falls
        // to nothing over the ramp's length and ends on exact silence.
        let mut left = vec![0.0f32; fade + 8];
        let mut right = vec![0.0f32; fade + 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_off(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left[0], 1.0, "entering the fade does not step the gain");
        assert!(left[fade - 1] > 0.0, "and the last frame of the ramp still sounds");
        assert_eq!(left[fade..], [0.0; 8], "silent from the frame the ramp lands on");
        assert_eq!(engine.pool().active_count(), 0, "the voice is gone with its ramp");
        assert_eq!(engine.pool().terminated().len(), 1, "and the host is told it ended");
    }

    /// A host choke carrying a voice id cuts that voice and leaves the other
    /// note on the same key sounding — the same matching rule as a note-off
    /// ([`crate::voices::Voice::matches`]), which is the only way two voices
    /// on one key can be told apart.
    #[test]
    fn a_host_choke_with_an_id_cuts_only_the_voice_it_names() {
        let (model, _id) = one_lane_model(2048);
        let mut engine = Engine::new();
        let first = NoteTarget { channel: 0, key: 36, voice_id: Some(1) };
        let second = NoteTarget { channel: 0, key: 36, voice_id: Some(2) };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                TimedEvent::new(0, EngineEvent::NoteOn { note: first, velocity: 1.0 }),
                TimedEvent::new(0, EngineEvent::NoteOn { note: second, velocity: 1.0 }),
                TimedEvent::new(1, EngineEvent::Choke { note: first }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fading: Vec<bool> = engine.pool().voices().map(|voice| voice.is_fading()).collect();
        assert_eq!(fading, [true, false], "the choked voice only");
    }

    /// A choke arriving on a voice that is already fading leaves the ramp it
    /// is on alone ([`crate::voices::Voice::begin_fade`]): restarting the
    /// ramp would jump the gain back to full, which is the click the fade
    /// exists to avoid.
    #[test]
    fn a_host_choke_leaves_an_already_fading_voices_ramp_alone() {
        let (mut model, id) = one_lane_model(4096);
        assert!(model.set_mode(id, LaneMode::Gate));
        // Release 0 is the v1 gate fade, FADE_MS.
        assert!(model.set_release_ms(id, 0.0));
        let fade = fade_frames_for(RATE);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), note_off(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        let left_before = engine.pool().voices().next().expect("still fading").fade.frames_left();
        assert_eq!(left_before, fade - 8, "eight frames of the ramp are spent");

        // A block of no frames: the choke applies, nothing renders, so what
        // the ramp is at afterwards is the choke's doing and nothing else.
        let mut none_l: [f32; 0] = [];
        let mut none_r: [f32; 0] = [];
        engine.run_block(
            model.published(),
            &[TimedEvent::new(0, EngineEvent::Choke { note: target(36) })],
            &mut Outputs::main(&mut none_l, &mut none_r),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("the choke did not end it early");
        assert_eq!(voice.fade.frames_left(), left_before, "the same ramp, not a new one");
    }

    /// Per-note expression with no voice id reaches every voice on its
    /// channel and key: a host that does not track voices still
    /// moves the whole stack a key started.
    #[test]
    fn expression_with_no_id_reaches_every_voice_on_the_channel_and_key() {
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 36, ramp(64));
        lane_with(&mut model, 1, 36, ramp(64));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                note_on(0, 36),
                TimedEvent::new(0, EngineEvent::PolyVolume { note: target(36), gain: 0.5 }),
                TimedEvent::new(0, EngineEvent::PolyTuning { note: target(36), semis: 12.0 }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 2);
        assert!(
            engine
                .pool()
                .voices()
                .all(|voice| voice.poly_volume == 0.5 && voice.semis == 12.0),
            "both voices of the stack took the expression"
        );
    }

    /// And an expression event on another channel, with no id to go by, is
    /// about another note: it moves nothing here.
    #[test]
    fn an_expression_event_on_another_channel_moves_nothing() {
        let (model, _id) = one_lane_model(64);
        let mut engine = Engine::new();
        let sounding = NoteTarget { channel: 1, key: 36, voice_id: None };
        let elsewhere = NoteTarget { channel: 2, key: 36, voice_id: None };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                TimedEvent::new(0, EngineEvent::NoteOn { note: sounding, velocity: 1.0 }),
                TimedEvent::new(0, EngineEvent::PolyVolume { note: elsewhere, gain: 0.0 }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "the sounding voice is untouched");
    }

    // ---- Events the host left open -------------------------------------

    /// The target a host addresses to everything: any channel, any key, no
    /// voice id — a panic button, an "all notes off", a choke of
    /// the lot. `lib.rs` builds exactly this out of `Channel::Wildcard` and
    /// `Key::Wildcard`.
    fn everything() -> NoteTarget {
        NoteTarget { channel: CHANNEL_WILDCARD, key: KEY_WILDCARD, voice_id: None }
    }

    /// A wildcard note-off gates **every** sounding gate voice, whatever key or
    /// channel it is on — the host asking for all notes off must not leave a
    /// held kit ringing. It is still a note-off, so the one-shot beside them
    /// plays on.
    #[test]
    fn a_wildcard_note_off_gates_every_sounding_gate_voice() {
        let mut model = switch_off_model();
        let low = lane_with(&mut model, 0, 36, vec![1.0; 4096]);
        let high = lane_with(&mut model, 1, 48, vec![1.0; 4096]);
        let one_shot = lane_with(&mut model, 2, 60, vec![1.0; 4096]);
        assert!(model.set_mode(low, LaneMode::Gate));
        assert!(model.set_mode(high, LaneMode::Gate));
        let mut engine = Engine::new();
        // Channel 5 for one of them, so the wildcard channel is doing work
        // too and not merely agreeing with 0.
        let on_five = NoteTarget { channel: 5, key: 48, voice_id: None };

        // 64-frame blocks, not the usual 512: a fade is `FADE_MS` (192
        // frames at this rate), so a longer block would see a voice start
        // its fade and finish it inside the same call.
        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        engine.run_block(
            model.published(),
            &[
                note_on(0, 36),
                TimedEvent::new(0, EngineEvent::NoteOn { note: on_five, velocity: 1.0 }),
                note_on(0, 60),
                TimedEvent::new(1, EngineEvent::NoteOff { note: everything() }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fading: Vec<(u32, bool)> =
            engine.pool().voices().map(|voice| (voice.lane_id, voice.is_fading())).collect();
        assert_eq!(
            fading,
            [(low, true), (high, true), (one_shot, false)],
            "both gates released — one of them on another channel — and the one-shot plays on"
        );
    }

    /// A wildcard choke fades everything sounding, one-shots included
    /// a choke is the host saying those notes are finished, and
    /// "those notes" here is all of them.
    #[test]
    fn a_wildcard_choke_fades_everything_sounding() {
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 36, vec![1.0; 4096]);
        lane_with(&mut model, 1, 48, vec![1.0; 4096]);
        let mut engine = Engine::new();
        let on_five = NoteTarget { channel: 5, key: 48, voice_id: None };

        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        engine.run_block(
            model.published(),
            &[
                note_on(0, 36),
                TimedEvent::new(0, EngineEvent::NoteOn { note: on_five, velocity: 1.0 }),
                TimedEvent::new(1, EngineEvent::Choke { note: everything() }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 2, "both are still being mixed");
        assert!(
            engine.pool().voices().all(|voice| voice.is_fading()),
            "and both are on their way out"
        );
    }

    /// A wildcard on one field alone still narrows by the other: "all notes
    /// off on channel 1" is a wildcard key on a real channel, and channel 2
    /// goes on sounding.
    #[test]
    fn a_wildcard_key_on_one_channel_leaves_the_other_channel_sounding() {
        let (mut model, id) = one_lane_model(4096);
        assert!(model.set_mode(id, LaneMode::Gate));
        let mut engine = Engine::new();
        let one = NoteTarget { channel: 1, key: 36, voice_id: None };
        let two = NoteTarget { channel: 2, key: 36, voice_id: None };
        let all_of_one = NoteTarget { channel: 1, key: KEY_WILDCARD, voice_id: None };

        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        engine.run_block(
            model.published(),
            &[
                TimedEvent::new(0, EngineEvent::NoteOn { note: one, velocity: 1.0 }),
                TimedEvent::new(0, EngineEvent::NoteOn { note: two, velocity: 1.0 }),
                TimedEvent::new(1, EngineEvent::NoteOff { note: all_of_one }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fading: Vec<(u8, bool)> =
            engine.pool().voices().map(|voice| (voice.channel, voice.is_fading())).collect();
        assert_eq!(fading, [(1, true), (2, false)], "channel 1 released, channel 2 held");
    }

    /// Per-note expression addressed to every key reaches every voice, not
    /// just the ones on one key: the expressions ask the same
    /// predicate a note-off does.
    #[test]
    fn a_wildcard_expression_reaches_every_voice_whatever_key_it_is_on() {
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 36, ramp(64));
        lane_with(&mut model, 1, 48, ramp(64));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[
                note_on(0, 36),
                note_on(0, 48),
                TimedEvent::new(0, EngineEvent::PolyVolume { note: everything(), gain: 0.5 }),
                TimedEvent::new(0, EngineEvent::PolyPan { note: everything(), pan: 1.0 }),
            ],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 2);
        assert!(
            engine
                .pool()
                .voices()
                .all(|voice| voice.poly_volume == 0.5 && voice.poly_pan == 1.0),
            "both voices took the expression, on two different keys"
        );
    }

    /// And a wildcard note-**on** starts nothing: no lane answers
    /// note 255, so the one event kind a wildcard cannot mean anything for
    /// is silent — no voice, no activity, nothing in the mix.
    #[test]
    fn a_wildcard_note_on_starts_nothing() {
        let (model, id) = one_lane_model(8);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[TimedEvent::new(0, EngineEvent::NoteOn { note: everything(), velocity: 1.0 })],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 0, "no lane answers a key the host left open");
        assert_eq!(left, [0.0; 8], "and the block is silent");
        assert_eq!(activity(&model, id), 0, "nothing was struck, so no light flashes");
    }

    /// A tuning event that names no key tracks no key, switch on or off
    /// ([`effective_semis`]): there is nothing to measure the offset from,
    /// so the tuning the event carried is the whole answer. The arithmetic
    /// alternative would be 255 − 60 — 195 semitones, which the clamp turns
    /// into silence — on every voice the pool is holding.
    #[test]
    fn a_wildcard_tuning_tracks_no_key_even_with_the_switch_on() {
        assert_eq!(effective_semis(0.0, KEY_WILDCARD, true), 0.0, "no key, no offset");
        assert_eq!(effective_semis(5.0, KEY_WILDCARD, true), 5.0, "the event's own tuning");
        assert_eq!(effective_semis(5.0, KEY_WILDCARD, false), 5.0, "and the same with it off");

        let (mut model, _id) = one_lane_model(64);
        model.set_sample_pad_switch(true);
        let mut engine = Engine::new();
        let tuning = EngineEvent::PolyTuning { note: everything(), semis: 5.0 };

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36), TimedEvent::new(0, tuning)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("the note started");
        assert_eq!(voice.semis, 5.0, "the tuning as sent, not 5 + 195");
        assert_eq!(voice.step, step_for(RATE as u32, RATE, 5.0), "and a step it can finish on");
    }

    // ---- The "bitstep Sample pad" switch -------------------------------

    /// The fold with the switch off: the event's own tuning and nothing else,
    /// whichever key it arrived on.
    #[test]
    fn with_the_switch_off_the_tuning_is_the_events_own() {
        for key in [0, 36, 59, 60, 61, 72, 127] {
            assert_eq!(effective_semis(0.0, key, false), 0.0, "key {key}");
            assert_eq!(effective_semis(7.5, key, false), 7.5, "key {key}");
            assert_eq!(effective_semis(-24.0, key, false), -24.0, "key {key}");
        }
    }

    /// And with it on: the key tracked from C3, with C3 itself still the
    /// sample's own pitch, and expression adding on top of the offset rather
    /// than replacing it.
    #[test]
    fn with_the_switch_on_the_key_is_tracked_from_c3() {
        assert_eq!(effective_semis(0.0, PAD_ROOT_KEY, true), 0.0, "C3 plays at pitch");
        assert_eq!(effective_semis(0.0, 36, true), -24.0, "C1 is two octaves down");
        assert_eq!(effective_semis(0.0, 72, true), 12.0, "C4 is an octave up");
        assert_eq!(effective_semis(0.0, 0, true), -60.0, "the bottom of the keyboard");
        assert_eq!(effective_semis(0.0, 127, true), 67.0, "and the top");
        assert_eq!(effective_semis(3.0, 36, true), -21.0, "a tuning adds to the offset");
    }

    /// Why the switch exists: it cancels bitstep's Sample pad tuning of
    /// `60 − key` **exactly**, on every one of the 128 keys. Nearly would not
    /// do — a kit a few cents out on every pad is a broken kit.
    #[test]
    fn the_switch_cancels_bitsteps_sample_pad_tuning_for_every_key() {
        for key in 0..=127u8 {
            let bitstep = PAD_ROOT_KEY as f32 - key as f32;
            let semis = effective_semis(bitstep, key, true);
            assert_eq!(semis, 0.0, "key {key}");
            assert_eq!(step_for(RATE as u32, RATE, semis), 1.0, "key {key} plays at unity");
        }
    }

    /// Through the engine with the switch off — the default — a lane on
    /// note 36 plays its sample at the sample's pitch: the step is unity and
    /// the block is the file.
    #[test]
    fn a_note_on_with_the_switch_off_starts_at_the_samples_own_pitch() {
        let (model, _id) = one_lane_model(64);
        assert!(!model.sample_pad_switch(), "the switch is off by default");
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("the note started");
        assert_eq!(voice.semis, 0.0, "note 36 is not tracked with the switch off");
        assert_eq!(voice.step, 1.0);
        assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "the file, frame for frame");
    }

    /// The same lane with the switch on is pitched down two octaves, and the
    /// step says so: a quarter of a source frame per output frame.
    #[test]
    fn a_note_on_with_the_switch_on_tracks_the_key_down() {
        let (mut model, _id) = one_lane_model(64);
        model.set_sample_pad_switch(true);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("the note started");
        assert_eq!(voice.semis, -24.0, "note 36 is two octaves below C3");
        assert_eq!(voice.step, step_for(RATE as u32, RATE, -24.0));
        assert_eq!(voice.step, 0.25, "a quarter of a source frame per output frame");
    }

    /// And up, in the mix rather than in a field: a lane answering note 72
    /// reads every second frame of its sample — the engine's +12 exact-math
    /// case, driven by the key instead of by an expression.
    #[test]
    fn the_switch_pitches_a_high_key_up_an_octave_in_the_mix() {
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 72, ramp(16));
        model.set_sample_pad_switch(true);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 72)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 3.0, 5.0, 7.0], "every second frame of the ramp");
        assert_eq!(right, [0.0; 4], "the lane is still hard left");
    }

    /// The switch's contract end to end: switch on and bitstep driving, a
    /// lane plays its sample at its own pitch on whatever note the user put
    /// it on, because the `60 − key` bitstep sends at the note-on and the
    /// `key − 60` the switch adds cancel. The block is the file again.
    #[test]
    fn the_switch_and_bitsteps_tuning_cancel_through_the_engine() {
        for key in [0, 36, 48, 59, 60, 61, 72, 127] {
            let mut model = switch_off_model();
            lane_with(&mut model, 0, key, ramp(8));
            model.set_sample_pad_switch(true);
            let mut engine = Engine::new();
            // bitstep sends the tuning right after the note-on, at the same
            // timing, so no frame is rendered in between.
            let bitstep = EngineEvent::PolyTuning {
                note: target(key),
                semis: PAD_ROOT_KEY as f32 - key as f32,
            };

            // Half the ramp, so the voice is still sounding to be asked
            // what pitch it settled on.
            let mut left = [0.0f32; 4];
            let mut right = [0.0f32; 4];
            engine.run_block(
                model.published(),
                &[note_on(0, key), TimedEvent::new(0, bitstep)],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );

            let voice = engine.pool().voices().next().expect("the note started");
            assert_eq!(voice.semis, 0.0, "key {key} ends up at the sample's pitch");
            assert_eq!(voice.step, 1.0, "key {key} plays at unity");
            assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "key {key} is the file, frame for frame");
        }
    }

    /// A voice reads the switch once, at the event that tuned it (module
    /// docs, on the switch): flipping it under a ringing note leaves that
    /// note's pitch exactly where it was, and is heard from the next note-on.
    #[test]
    fn flipping_the_switch_leaves_a_ringing_voice_alone() {
        let (mut model, _id) = one_lane_model(64);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(left, [1.0, 2.0, 3.0, 4.0]);

        model.set_sample_pad_switch(true);
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(left, [5.0, 6.0, 7.0, 8.0], "the ringing note kept its pitch");

        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(engine.pool().active_count(), 2, "the first note is still ringing");
        let mut semis: Vec<f32> = engine.pool().voices().map(|voice| voice.semis).collect();
        semis.sort_by(f32::total_cmp);
        assert_eq!(semis, [-24.0, 0.0], "only the note started under the switch is tracked");
    }

    /// The switch adds to a hostile tuning without rescuing it and without
    /// making it worse: the clamp and the NaN guard stay where the step is
    /// computed ([`crate::voices::step_for`]), so whatever the host sends,
    /// the voice is left at a finite, forward-moving rate and the block
    /// holds finite samples.
    #[test]
    fn the_switch_on_top_of_a_hostile_tuning_still_gives_a_sane_step() {
        for semis in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1e30, -1e30] {
            let (mut model, _id) = one_lane_model(4096);
            model.set_sample_pad_switch(true);
            let mut engine = Engine::new();

            let mut left = [0.0f32; 4];
            let mut right = [0.0f32; 4];
            engine.run_block(
                model.published(),
                &[
                    note_on(0, 36),
                    TimedEvent::new(0, EngineEvent::PolyTuning { note: target(36), semis }),
                ],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );

            let voice = engine.pool().voices().next().expect("the note started");
            let step = voice.step;
            assert!(step.is_finite() && step > 0.0, "tuning {semis} gave step {step}");
            assert!(left.iter().all(|frame| frame.is_finite()), "tuning {semis} mixed finite");
        }
    }

    // ---- The audition ring ---------------------------------------------

    /// The ring is a queue: what the GUI puts in comes out once each, oldest
    /// first, and an empty one says so rather than inventing an entry.
    #[test]
    fn the_ring_hands_auditions_back_oldest_first() {
        let ring = AuditionRing::new();
        assert_eq!(ring.pop(), None, "nothing queued, nothing to play");
        assert_eq!(ring.pending(), 0);

        for id in [7, 3, 7] {
            assert!(ring.push(id), "lane {id} is queued");
        }

        assert_eq!(ring.pending(), 3);
        assert_eq!(ring.pop(), Some(7));
        assert_eq!(ring.pop(), Some(3));
        assert_eq!(ring.pop(), Some(7), "clicking one lane twice is two auditions");
        assert_eq!(ring.pop(), None);
        assert_eq!(ring.pending(), 0);
    }

    /// A full ring drops the push rather than blocking or growing
    /// (an audition is best-effort) — and frees the slot again the
    /// moment the audio thread has taken one, which is what makes it a ring
    /// rather than a bucket.
    #[test]
    fn a_full_ring_drops_the_push_and_takes_more_once_drained() {
        let ring = AuditionRing::new();
        for id in 0..AUDITION_CAPACITY as u32 {
            assert!(ring.push(id), "click {id} fits");
        }

        assert_eq!(ring.pending(), AUDITION_CAPACITY);
        assert!(!ring.push(99), "the click past the capacity is dropped");
        assert_eq!(ring.pending(), AUDITION_CAPACITY, "and nothing already queued with it");

        assert_eq!(ring.pop(), Some(0), "the audio thread takes the oldest");
        assert!(ring.push(99), "one taken, one more fits");

        let rest: Vec<u32> = std::iter::from_fn(|| ring.pop()).collect();
        let expected: Vec<u32> = (1..AUDITION_CAPACITY as u32).chain([99]).collect();
        assert_eq!(rest, expected, "in order, with the late click last");
    }

    /// And it wraps: pushing and popping many times past the capacity keeps
    /// the order and loses nothing, because the counters only count up and
    /// the slot an entry lands in is the counter modulo the capacity.
    #[test]
    fn the_ring_wraps_without_losing_an_entry() {
        let ring = AuditionRing::new();
        for id in 0..AUDITION_CAPACITY as u32 * 5 {
            assert!(ring.push(id));
            assert!(ring.push(id + 1000));
            assert_eq!(ring.pop(), Some(id));
            assert_eq!(ring.pop(), Some(id + 1000));
        }
        assert_eq!(ring.pending(), 0);
    }

    /// The claim the whole type rests on, under two real threads: one
    /// pushing and one popping at the same time hand every entry over
    /// exactly once and in order, with no lock between them.
    ///
    /// Deterministic in what it asserts — the order of 1000 known ids — and
    /// bounded in time by a deadline rather than by a spin count, so a
    /// broken ring fails the test instead of hanging the suite. The full
    /// GUI-against-audio-thread stress test is `tests/stress.rs`; this is the
    /// ring alone.
    #[test]
    fn the_ring_carries_every_entry_across_two_threads_in_order() {
        const SENT: u32 = 1000;
        let ring = Arc::new(AuditionRing::new());
        let producer_ring = Arc::clone(&ring);

        let producer = std::thread::spawn(move || {
            for id in 0..SENT {
                // A full ring drops a click on the GUI thread; here the
                // point is that none is lost, so this one waits for room.
                while !producer_ring.push(id) {
                    std::thread::yield_now();
                }
            }
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut received: Vec<u32> = Vec::with_capacity(SENT as usize);
        while received.len() < SENT as usize {
            match ring.pop() {
                Some(id) => received.push(id),
                None => {
                    assert!(std::time::Instant::now() < deadline, "the ring stopped delivering");
                    std::thread::yield_now();
                }
            }
        }

        producer.join().expect("the producer finished");
        let expected: Vec<u32> = (0..SENT).collect();
        assert_eq!(received, expected, "every entry once, in order");
        assert_eq!(ring.pop(), None, "and nothing left behind");
    }

    /// The audition ring's own contract, and the block's step 2: push an
    /// audition, hear the lane. From the top of the block, where the drain
    /// puts it — so the block is the file again, exactly as a note-on at
    /// offset 0 makes it.
    #[test]
    fn an_audition_renders_its_lane_from_the_top_of_the_block() {
        let (model, id) = one_lane_model(8);
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id), "the GUI queues the click");

        let mut left = [0.0f32; 12];
        let mut right = [0.0f32; 12];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(right, [0.0; 12], "a hard left lane leaves the right side silent");
        assert_eq!(engine.auditions().pending(), 0, "and the block drained the ring");
    }

    /// The ring reports which lane's audition is sounding, and `stop` fades
    /// it: the play button is play and stop.
    /// Starting it again starts it from the top.
    #[test]
    fn an_audition_is_reported_sounding_until_stopped_or_finished() {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 36, ramp(64));
        let mut engine = Engine::new();
        let block = |engine: &mut Engine| {
            let mut left = [0.0f32; 4];
            let mut right = [0.0f32; 4];
            let mut outputs = Outputs::main(&mut left, &mut right);
            engine.run_block(model.published(), &[], &mut outputs, RATE, 1.0);
            left
        };
        assert_eq!(engine.auditions().sounding(), None);
        assert!(engine.auditions().push(id));
        let first = block(&mut engine);
        assert_eq!(engine.auditions().sounding(), Some(id), "sounding after its first block");

        // Stopped: it fades, and is reported stopped at once.
        assert!(engine.auditions().stop());
        block(&mut engine);
        assert_eq!(engine.auditions().sounding(), None);
        assert!(engine.pool().voices().all(|voice| voice.is_fading()), "fading out, not cut");

        // Started again, once the fade is over: from the top, the same
        // frames as the first time.
        for _ in 0..1000 {
            if engine.pool().active_count() == 0 {
                break;
            }
            block(&mut engine);
        }
        assert_eq!(engine.pool().active_count(), 0, "the stopped audition faded out");
        assert!(engine.auditions().push(id));
        assert_eq!(block(&mut engine), first, "from the start again");
        assert_eq!(engine.auditions().sounding(), Some(id));

        // Left alone: no longer sounding once the sample has run out.
        for _ in 0..20 {
            block(&mut engine);
        }
        assert_eq!(engine.auditions().sounding(), None, "ran out");

        // A stop with nothing sounding does nothing.
        assert!(engine.auditions().stop());
        block(&mut engine);
        assert_eq!(engine.auditions().sounding(), None);
    }

    /// One audition at a time: a new one fades the last, whichever lane it
    /// was on, while notes the host plays on that lane carry on.
    #[test]
    fn a_new_audition_fades_the_last_but_not_played_notes() {
        let mut model = switch_off_model();
        let first = lane_with(&mut model, 0, 36, ramp(64));
        let second = lane_with(&mut model, 1, 38, ramp(64));
        let mut engine = Engine::new();
        let block = |engine: &mut Engine, events: &[TimedEvent]| {
            let mut left = [0.0f32; 4];
            let mut right = [0.0f32; 4];
            let mut outputs = Outputs::main(&mut left, &mut right);
            engine.run_block(model.published(), events, &mut outputs, RATE, 1.0);
        };
        assert!(engine.auditions().push(first));
        block(&mut engine, &[note_on(0, 36)]);
        assert_eq!(engine.pool().active_count(), 2, "the audition and the played note");

        assert!(engine.auditions().push(second));
        block(&mut engine, &[]);
        assert_eq!(engine.auditions().sounding(), Some(second));
        let fading: Vec<(u32, bool)> =
            engine.pool().voices().map(|voice| (voice.lane_id, voice.is_fading())).collect();
        let faded = fading.iter().filter(|(_, fading)| *fading).count();
        assert_eq!(faded, 1, "only the first audition fades: {fading:?}");
        assert!(fading.contains(&(first, false)), "the played note on that lane carries on");
        assert!(fading.contains(&(second, false)), "the new audition sounds");
    }

    /// An audition is one lane's, found by id: the other lane on the same
    /// note stays silent, which is the one thing a note-on on that note
    /// cannot do (every lane on a key answers it).
    #[test]
    fn an_audition_sounds_only_the_lane_it_names() {
        let mut model = switch_off_model();
        let _first = lane_with(&mut model, 0, 36, ramp(64));
        // Ten times the ramp, so which lane is in the mix is unmistakable,
        // and long enough that the voice is still there to be counted.
        let tens: Vec<f32> = ramp(64).iter().map(|frame| frame * 10.0).collect();
        let second = lane_with(&mut model, 1, 36, tens);
        let mut engine = Engine::new();
        assert!(engine.auditions().push(second));

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [10.0, 20.0, 30.0, 40.0], "the lane that was clicked, alone");
        let answered: Vec<u32> = engine.pool().voices().map(|voice| voice.lane_id).collect();
        assert_eq!(answered, [second], "and the lane sharing its note started nothing");
    }

    /// What the click becomes: a note-on for the lane's own
    /// note, at full velocity, carrying the lane's settings like any other
    /// voice, in the shape of a note no host tracked.
    #[test]
    fn an_audition_is_a_note_on_for_the_lanes_own_note() {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 44, ramp(2048));
        assert!(model.set_mode(id, LaneMode::Gate));
        assert!(model.set_choke(id, 2));
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("the audition started a voice");
        assert_eq!(voice.lane_id, id);
        assert_eq!(voice.note_key, 44, "the lane's own note");
        assert_eq!(voice.channel, 0, "the channel a host that sends none would use");
        assert_eq!(voice.voice_id, None, "and no voice id: no host issued one");
        assert_eq!(voice.velocity, AUDITION_VELOCITY, "full velocity");
        assert_eq!(voice.lane_choke, 2, "its lane's group, as at any note-on");
        assert_eq!(voice.lane_mode, LaneMode::Gate, "and its lane's mode");
        assert_eq!(voice.semis, 0.0, "the switch is off, so the sample's own pitch");
    }

    /// One block drains the whole ring however full it is: every click
    /// queued is a voice, and nothing is left over for the next block.
    #[test]
    fn one_block_drains_a_full_ring() {
        let (model, id) = one_lane_model(2048);
        let mut engine = Engine::new();
        for _ in 0..AUDITION_CAPACITY {
            assert!(engine.auditions().push(id));
        }

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), AUDITION_CAPACITY, "every click was heard");
        assert_eq!(engine.auditions().pending(), 0);
    }

    /// The lane's choke group fires at an audition as it does at a note-on
    /// including against the lane's own earlier voices: two
    /// clicks on a grouped lane in one block, and the first is fading.
    #[test]
    fn an_audition_applies_the_lanes_choke_group() {
        let (mut model, id) = one_lane_model(2048);
        assert!(model.set_choke(id, 1));
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));
        assert!(engine.auditions().push(id));

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fading: Vec<bool> = engine.pool().voices().map(|voice| voice.is_fading()).collect();
        assert_eq!(fading, [true, false], "the older click is cut by the newer one");
    }

    /// An audition ignores the switch: with it on, a lane on note
    /// 72 still auditions at the sample's own pitch, frame by frame — the
    /// pitch a bitstep Sample pad lane plays it at through the switch. A
    /// switch that moved the play button's pitch would make clicking it
    /// sound like a change to the sample.
    #[test]
    fn an_audition_plays_the_sample_at_its_own_pitch_whatever_the_switch() {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 72, ramp(16));
        model.set_sample_pad_switch(true);
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0], "the ramp frame by frame, not an octave up");
    }

    /// An audition of a gate lane plays to the end of its sample rather than
    /// hanging (module docs, §The audition ring): nothing sends the note-off
    /// a gate voice waits for, and a voice is finished when it runs past its
    /// sample whatever its mode.
    #[test]
    fn an_audition_of_a_gate_lane_plays_to_the_end_of_the_sample() {
        let mut model = switch_off_model();
        let id = lane_with(&mut model, 0, 36, ramp(8));
        assert!(model.set_mode(id, LaneMode::Gate));
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));

        let mut left = [0.0f32; 12];
        let mut right = [0.0f32; 12];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0, 0.0, 0.0]);
        assert_eq!(engine.pool().active_count(), 0, "it ended with its sample, not never");
    }

    /// And a host note-off on that key does reach it, because an audition is
    /// deliberately indistinguishable from a note the host did not track:
    /// the voice fades from where the note-off landed.
    #[test]
    fn a_note_off_releases_an_audition_of_a_gate_lane() {
        let (mut model, id) = one_lane_model(2048);
        assert!(model.set_mode(id, LaneMode::Gate));
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_off(2, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let voice = engine.pool().voices().next().expect("fading, not gone");
        assert!(voice.is_fading(), "the note-off found the audition");
    }

    /// An audition for a lane that is not published sounds nothing: the lane
    /// may have been deleted, or had its sample cleared, between the click
    /// and this block. The entry is consumed all the same — a dropped
    /// audition must not wedge the ring.
    #[test]
    fn an_audition_of_an_unpublished_lane_sounds_nothing() {
        let (mut model, id) = one_lane_model(8);
        assert!(model.clear_sample(id), "the lane loses its sample, and its place in the mix");
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));
        assert!(engine.auditions().push(u32::MAX), "and one for an id that never existed");

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(left, [0.0; 4]);
        assert_eq!(engine.pool().active_count(), 0, "no lane, no voice");
        assert_eq!(engine.auditions().pending(), 0, "both entries were drained");
    }

    // ---- The activity markers ------------------------------------------

    /// What a lane's activity marker reads as right now, through the
    /// published snapshot the GUI would read it through.
    ///
    /// Panics if the lane is not published, which in these tests means the
    /// fixture is wrong rather than that the lane is quiet.
    fn activity(model: &Model, id: u32) -> u32 {
        let published = model.published().get();
        published.lane(id).expect("the lane is published").activity()
    }

    /// The activity marker's own contract: the marker moves when the lane
    /// plays. Once per voice start — and not at all for a block that only
    /// goes on rendering a voice already sounding, which is what makes the
    /// GUI's light a hit and not a level.
    #[test]
    fn a_voice_start_moves_its_lanes_activity_marker() {
        let (model, id) = one_lane_model(64);
        let mut engine = Engine::new();
        assert_eq!(activity(&model, id), 0, "nothing has played yet");

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        let struck = activity(&model, id);
        assert_ne!(struck, 0, "the note-on moved it");

        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(engine.pool().active_count(), 1, "the voice is still sounding");
        assert_eq!(activity(&model, id), struck, "but no new voice started, so no new hit");

        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_ne!(activity(&model, id), struck, "and the next hit moves it again");
    }

    /// The marker belongs to the lane that played, not to the note: a key
    /// two lanes answer lights both of them (every lane on the key starts
    /// a voice), and leaves the lane on another key dark.
    #[test]
    fn a_note_moves_the_marker_of_every_lane_that_answered_it() {
        let mut model = switch_off_model();
        let first = lane_with(&mut model, 0, 36, ramp(64));
        let second = lane_with(&mut model, 1, 36, ramp(64));
        let elsewhere = lane_with(&mut model, 2, 44, ramp(64));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_ne!(activity(&model, first), 0, "the first lane on the key");
        assert_ne!(activity(&model, second), 0, "and the second, layered with it");
        assert_eq!(activity(&model, elsewhere), 0, "the lane on another note stayed dark");
    }

    /// A note no lane answers starts no voice, so it moves nothing: the
    /// lights follow voices, not the note stream.
    #[test]
    fn a_note_no_lane_answers_moves_no_marker() {
        let (model, id) = one_lane_model(64);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 44)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().active_count(), 0, "no voice");
        assert_eq!(activity(&model, id), 0, "no hit");
    }

    /// An audition is a voice start like any other, so the play button lights
    /// the lane it plays — one of the two paths through
    /// [`Engine::start_lane`], and the reason the marker is bumped there
    /// rather than in the note-on.
    #[test]
    fn an_audition_moves_its_lanes_activity_marker() {
        let mut model = switch_off_model();
        let clicked = lane_with(&mut model, 0, 36, ramp(64));
        let sharing = lane_with(&mut model, 1, 36, ramp(64));
        let mut engine = Engine::new();
        assert!(engine.auditions().push(clicked));

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_ne!(activity(&model, clicked), 0, "the lane whose button was pressed");
        assert_eq!(activity(&model, sharing), 0, "the lane sharing its note was not played");
    }

    /// A choke group does not light the lanes it silences, only the lane that
    /// struck: the voice that ends was not started, and a light that flashed on
    /// being cut would tell the user the opposite of what happened.
    #[test]
    fn a_choke_lights_the_lane_that_struck_and_not_the_lanes_it_cuts() {
        let mut model = switch_off_model();
        let early = lane_with(&mut model, 0, 36, ramp(2048));
        let late = lane_with(&mut model, 1, 44, ramp(2048));
        for id in [early, late] {
            assert!(model.set_choke(id, 1), "one group holds them both");
        }
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        let struck = activity(&model, early);

        engine.run_block(
            model.published(),
            &[note_on(0, 44)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(activity(&model, early), struck, "the choked lane did not light again");
        assert_ne!(activity(&model, late), 0, "the lane that chokes it did");
    }

    /// What `lib.rs` wakes the editor on, block by block
    /// ([`Engine::is_sounding`]): true from the block a voice starts in until
    /// the block it ends in, and false either side of that — an instance
    /// between two hits asks for no frames at all, which is what keeps a
    /// closed-eyed editor idle.
    #[test]
    fn an_engine_is_sounding_while_a_voice_is_and_no_longer() {
        let (model, _id) = one_lane_model(24);
        let mut engine = Engine::new();
        assert!(!engine.is_sounding(), "a fresh engine has nothing in it");

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert!(engine.is_sounding(), "a one-shot three blocks long");

        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert!(engine.is_sounding(), "still going");

        // The block the sample runs out in, and then one after it.
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(engine.pool().active_count(), 0, "the one-shot ended with its sample");
        assert!(!engine.is_sounding(), "and the editor stops being woken");
    }

    /// The bug the strike half of [`Engine::needs_a_frame`] exists for: a
    /// sample shorter than the host's buffer starts *and* ends inside one
    /// block, so the lane's activity marker moves while nothing is left
    /// sounding to ask the editor for the frame that would draw it.
    ///
    /// Without the strike the dot of every short sample — a click, a tight
    /// closed hat — would stay dark in a window nobody was touching, which
    /// is the one thing the activity lights are for.
    #[test]
    fn a_sample_shorter_than_the_block_still_asks_for_a_frame() {
        // Twenty-four frames against a 512-frame block: half a millisecond
        // of sound at a buffer Bitwig really uses.
        let (model, id) = one_lane_model(24);
        let mut engine = Engine::new();
        let mut left = [0.0f32; 512];
        let mut right = [0.0f32; 512];

        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_ne!(activity(&model, id), 0, "the lane played, so its dot has to light");
        assert!(!engine.is_sounding(), "and was over before the block was");
        assert!(engine.needs_a_frame(), "the editor still has to be woken to draw it");

        // The flag is this block's, not the run's: the next block struck
        // nothing and nothing is sounding, so nothing is asked for.
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert!(!engine.needs_a_frame(), "an instance between two hits asks for no frames");
    }

    /// A lane struck by a voice that outlives its block asks for frames by
    /// both halves of [`Engine::needs_a_frame`] in turn: the strike on the
    /// block the note lands in, the sound on every block after it.
    #[test]
    fn a_long_voice_asks_by_its_strike_and_then_by_its_sound() {
        // Long against the block below it, so the voice is still going
        // several blocks after the one it started in.
        let (model, _id) = one_lane_model(4096);
        let mut engine = Engine::new();
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        assert!(!engine.needs_a_frame(), "a fresh engine asks for nothing");

        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert!(engine.needs_a_frame(), "struck, and still sounding");

        // Blocks the voice runs on through, striking nothing: the sound is
        // what asks now, and the strike flag has already been put down.
        for block in 0..3 {
            engine.run_block(
                model.published(),
                &[],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
            assert!(engine.is_sounding(), "block {block} is still sounding");
            assert!(engine.needs_a_frame(), "block {block}");
        }

        // And on past the end of the sample: nothing struck and nothing
        // sounding, so the editor goes back to being reactive. Bounded by
        // the blocks the sample can possibly last rather than by counting
        // them out, so the sample's length and the block's stay free to
        // change.
        for _ in 0..4096 / left.len() + 2 {
            engine.run_block(
                model.published(),
                &[],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
        }
        assert!(!engine.is_sounding(), "the one-shot ran out");
        assert!(!engine.needs_a_frame(), "the editor goes back to being reactive");
    }

    /// An audition from the GUI is a voice start like any other, so it wakes
    /// the editor the same way — which is what makes a click on a play
    /// button light its own dot.
    #[test]
    fn an_audition_asks_for_a_frame_too() {
        let (model, id) = one_lane_model(24);
        let mut engine = Engine::new();
        let mut left = [0.0f32; 512];
        let mut right = [0.0f32; 512];

        assert!(engine.auditions().push(id), "the ring takes the click");
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_ne!(activity(&model, id), 0, "the audition started a voice");
        assert!(engine.needs_a_frame(), "so the dot gets a frame to light on");
    }

    /// [`Engine::reset`] leaves an engine that asks for nothing: the host
    /// deactivated it, so there is no window to draw and nothing to draw in
    /// one.
    #[test]
    fn a_reset_engine_asks_for_no_frames() {
        let (model, _id) = one_lane_model(4096);
        let mut engine = Engine::new();
        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert!(engine.needs_a_frame());

        engine.reset();

        assert!(!engine.needs_a_frame());
    }

    /// A stolen voice still fading out is sound too, so it still asks for
    /// frames: the editor is drawn while the tail of the voice it took is
    /// audible ([`Engine::is_sounding`] counts the pool's tail region).
    #[test]
    fn a_fading_steal_is_still_sounding() {
        let (mut model, id) = one_lane_model(4096);
        // Stacked hits on one lane: the v1 behaviour, so the lane keeps its
        // earlier voices rather than cutting them.
        assert!(model.set_cut_itself(id, false));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        // One key, one voice id: the second note-on takes the first voice's
        // slot and parks what it took in the tail region to fade.
        for _ in 0..=MAX_VOICES {
            let hit = [note_on(0, 36)];
            engine.run_block(
                model.published(),
                &hit,
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
        }

        assert!(engine.pool().tail_count() > 0, "a steal parked a fade");
        assert!(engine.is_sounding());
    }

    // ---- What goes back to the host ------------------------------------

    /// Every voice that ends is left in the terminated outbox for `lib.rs` to
    /// send on as `VoiceTerminated`, and clearing it is the engine's other half
    /// of that handshake.
    #[test]
    fn an_ended_voice_waits_in_the_terminated_outbox_for_the_host() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();
        let note = NoteTarget { channel: 2, key: 36, voice_id: Some(5) };

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        engine.run_block(
            model.published(),
            &[TimedEvent::new(0, EngineEvent::NoteOn { note, velocity: 1.0 })],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert_eq!(engine.pool().terminated().len(), 1, "the one-shot ran out inside the block");
        let ended = engine.pool().terminated()[0];
        assert_eq!(ended.voice_id, Some(5));
        assert_eq!(ended.channel, 2);
        assert_eq!(ended.note_key, 36);

        engine.clear_terminated();
        assert!(engine.pool().terminated().is_empty());
    }

    /// The pool's limit is the engine's limit: the 65th simultaneous note
    /// steals rather than allocating, and the engine keeps rendering through
    /// it.
    #[test]
    fn a_block_of_more_notes_than_the_pool_holds_steals_rather_than_grows() {
        let (mut model, id) = one_lane_model(4096);
        // Stacked hits on one lane: the v1 behaviour, so the lane keeps its
        // earlier voices rather than cutting them.
        assert!(model.set_cut_itself(id, false));
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        for key in 0..MAX_VOICES + 4 {
            let note = NoteTarget { channel: 0, key: 36, voice_id: Some(key as i32) };
            engine.run_block(
                model.published(),
                &[TimedEvent::new(0, EngineEvent::NoteOn { note, velocity: 1.0 })],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
        }

        assert_eq!(engine.pool().active_count(), MAX_VOICES);
        assert_eq!(engine.pool().terminated().len(), 4, "four of them were stolen and reported");
    }

    /// The host's reset path silences everything at once and takes the
    /// outbox with it.
    #[test]
    fn a_reset_silences_the_engine_and_empties_the_outbox() {
        let (model, _id) = one_lane_model(2048);
        let mut engine = Engine::new();

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(engine.pool().active_count(), 1);

        engine.reset();
        assert_eq!(engine.pool().active_count(), 0);
        assert!(engine.pool().terminated().is_empty());

        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(left, [0.0; 4], "and the block after it is silent");
    }

    /// The outbox is the engine's half of a handshake, not a per-block
    /// buffer: it holds every voice that has ended, oldest first, until
    /// `lib.rs` has sent them and says so
    /// ([`clear_terminated`](Engine::clear_terminated)). A block that cannot
    /// send them all must not lose the rest.
    #[test]
    fn the_terminated_outbox_holds_across_blocks_until_it_is_drained() {
        let (model, _id) = one_lane_model(4);
        let mut engine = Engine::new();
        let hit = |id| NoteTarget { channel: 0, key: 36, voice_id: Some(id) };

        let mut left = [0.0f32; 8];
        let mut right = [0.0f32; 8];
        for id in 1..=2 {
            engine.run_block(
                model.published(),
                &[TimedEvent::new(0, EngineEvent::NoteOn { note: hit(id), velocity: 1.0 })],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
        }

        let ended: Vec<Option<i32>> =
            engine.pool().terminated().iter().map(|voice| voice.voice_id).collect();
        assert_eq!(ended, [Some(1), Some(2)], "two blocks' worth, oldest first");

        engine.clear_terminated();
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert!(
            engine.pool().terminated().is_empty(),
            "and a block that ends nothing adds nothing"
        );
    }


    // ---- The browser's preview -----------------------------------------

    /// A channel on `model`'s collector, and an engine reading it.
    fn previewing(model: &Model) -> (Arc<PreviewChannel>, Engine) {
        let channel = Arc::new(PreviewChannel::new(model.handle()));
        let engine = Engine::new().with_preview(Arc::clone(&channel));
        (channel, engine)
    }

    /// Ask `channel` to preview `frames` (a 48 kHz mono sample), as
    /// generation `generation`; `None` asks for silence.
    fn preview(model: &Model, channel: &PreviewChannel, generation: u64, frames: Option<Vec<f32>>) {
        let sample = frames.map(|frames| {
            let data = SampleData { rate: RATE as u32, channels: 1, frames: frames.into() };
            basedrop::Shared::new(model.handle(), data)
        });
        let state =
            crate::preview::PreviewState { generation, sample, region: None, looping: false };
        channel.publish(model.handle(), state);
    }

    #[test]
    fn a_preview_plays_its_sample_centred_at_unity() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(ramp(4)));

        let mut left = [0.0f32; 6];
        let mut right = [0.0f32; 6];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let c = centre();
        assert_eq!(left, [c, 2.0 * c, 3.0 * c, 4.0 * c, 0.0, 0.0]);
        assert_eq!(right, left);
        assert!(!channel.is_playing(), "a four-frame preview is over inside the block");
    }

    #[test]
    fn a_preview_is_not_a_lane_voice() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(ramp(1000)));

        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(channel.is_playing());
        assert_eq!(engine.pool().active_count(), 0, "not in the pool");
        assert!(engine.pool().terminated().is_empty(), "nothing reported to the host");
    }

    #[test]
    fn the_same_preview_carries_on_across_blocks_rather_than_restarting() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(ramp(8)));

        let (mut l1, mut r1) = ([0.0f32; 4], [0.0f32; 4]);
        let (mut l2, mut r2) = ([0.0f32; 4], [0.0f32; 4]);
        engine.run_block(model.published(), &[], &mut Outputs::main(&mut l1, &mut r1), RATE, 1.0);
        engine.run_block(model.published(), &[], &mut Outputs::main(&mut l2, &mut r2), RATE, 1.0);

        let c = centre();
        assert_eq!(l1, [c, 2.0 * c, 3.0 * c, 4.0 * c]);
        assert_eq!(l2, [5.0 * c, 6.0 * c, 7.0 * c, 8.0 * c]);
    }

    #[test]
    fn a_stopped_preview_fades_to_silence_within_its_fade() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(vec![1.0; 48_000]));

        let mut left = vec![0.0f32; 256];
        let mut right = vec![0.0f32; 256];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        preview(&model, &channel, 2, None);
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fade = fade_frames_for(RATE) as usize;
        assert!(left[0] > 0.0 && left[0] <= centre(), "the fade starts at full level");
        assert!(left[fade..].iter().all(|&x| x == 0.0), "silent once the fade is over");
        let steepest = left.windows(2).map(|w| (w[0] - w[1]).abs()).fold(0.0f32, f32::max);
        assert!(steepest < 0.01, "no click: steepest step {steepest}");
        assert!(!channel.is_playing());
    }

    #[test]
    fn a_new_preview_replaces_the_old_one_from_its_first_frame() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(vec![1.0; 48_000]));

        let mut left = vec![0.0f32; 512];
        let mut right = vec![0.0f32; 512];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        preview(&model, &channel, 2, Some(vec![0.5; 48_000]));
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        let fade = fade_frames_for(RATE) as usize;
        let c = centre();
        // The old one fading out over the new one, then the new one alone.
        assert!(left[0] > 0.5 * c, "the old preview is still fading in at frame 0");
        assert!(left[fade..].iter().all(|&x| x == 0.5 * c), "then only the new one");
    }

    #[test]
    fn a_reset_silences_the_preview_and_does_not_replay_it() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(vec![1.0; 48_000]));

        let mut left = [0.0f32; 64];
        let mut right = [0.0f32; 64];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        engine.reset();
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );

        assert!(left.iter().all(|&x| x == 0.0));
        assert!(!channel.is_playing());
    }

    #[test]
    fn the_preview_follows_the_master_volume() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(ramp(2)));

        let mut left = [0.0f32; 2];
        let mut right = [0.0f32; 2];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            0.5,
        );

        let c = centre();
        assert_eq!(left, [c * 0.5, 2.0 * c * 0.5]);
    }

    // ---- Routing to outputs --------------------------------------------

    /// One stereo pair of buffers per output, `frames` long, holding
    /// garbage (9.0) the way a host's buffers arrive.
    fn host_buffers(frames: usize) -> Vec<(Vec<f32>, Vec<f32>)> {
        (0..OUTPUTS).map(|_| (vec![9.0; frames], vec![9.0; frames])).collect()
    }

    /// `Outputs` over the buffers at the indexes in `present`; the rest are
    /// outputs the host did not provide.
    fn routed<'a>(buffers: &'a mut [(Vec<f32>, Vec<f32>)], present: &[usize]) -> Outputs<'a> {
        let mut outputs = Outputs { pairs: [const { None }; OUTPUTS] };
        for (index, (left, right)) in buffers.iter_mut().enumerate() {
            if present.contains(&index) {
                outputs.pairs[index] = Some((left.as_mut_slice(), right.as_mut_slice()));
            }
        }
        outputs
    }

    /// Every output index, as a host that connected all sixteen gives.
    fn every_output() -> Vec<usize> {
        (0..OUTPUTS).collect()
    }

    #[test]
    fn a_lane_on_output_3_renders_only_into_pair_3() {
        let (mut model, id) = one_lane_model(8);
        assert!(model.set_output(id, 3));
        let mut engine = Engine::new();

        let mut buffers = host_buffers(12);
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut routed(&mut buffers, &every_output()),
            RATE,
            1.0,
        );

        for (index, (left, right)) in buffers.iter().enumerate() {
            if index == 3 {
                assert_eq!(left[..], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0, 0.0, 0.0]);
            } else {
                assert_eq!(left[..], [0.0; 12], "pair {index} stays silent");
            }
            assert_eq!(right[..], [0.0; 12], "pair {index}: a hard left lane");
        }
    }

    /// A voice keeps the output it started on: moving its lane mid-note
    /// does not move it (same rule as choke and mode).
    #[test]
    fn a_sounding_voice_keeps_its_output_when_the_lane_moves() {
        let (mut model, id) = one_lane_model(8);
        assert!(model.set_output(id, 3));
        let mut engine = Engine::new();
        let mut buffers = host_buffers(4);
        let present = every_output();
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut routed(&mut buffers, &present),
            RATE,
            1.0,
        );

        assert!(model.set_output(id, 7));
        engine.run_block(model.published(), &[], &mut routed(&mut buffers, &present), RATE, 1.0);

        assert_eq!(buffers[3].0[..], [5.0, 6.0, 7.0, 8.0], "still on output 3");
        assert_eq!(buffers[7].0[..], [0.0; 4]);
    }

    #[test]
    fn an_output_the_host_did_not_provide_falls_back_to_the_main_pair() {
        let (mut model, id) = one_lane_model(8);
        assert!(model.set_output(id, 5));
        let mut engine = Engine::new();

        // Out 1 and Out 4 only: output 5 (Out 6) is missing.
        let mut buffers = host_buffers(8);
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut routed(&mut buffers, &[0, 3]),
            RATE,
            1.0,
        );

        assert_eq!(buffers[0].0[..], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], "on main");
        assert_eq!(buffers[3].0[..], [0.0; 8], "a present pair is silenced, not written");
        assert_eq!(buffers[5].0[..], [9.0; 8], "a missing pair is never touched");
    }

    /// An aux buffer shorter than the main one is treated as missing: its
    /// voice is heard whole on the main output rather than cut short.
    #[test]
    fn a_pair_shorter_than_the_main_output_falls_back_to_it() {
        let (mut model, id) = one_lane_model(8);
        assert!(model.set_output(id, 2));
        let mut engine = Engine::new();

        let mut buffers = host_buffers(8);
        buffers[2].0.truncate(4);
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut routed(&mut buffers, &every_output()),
            RATE,
            1.0,
        );

        assert_eq!(buffers[0].0[..], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(buffers[2].0[..], [0.0; 4], "silenced, and nothing written");
    }

    #[test]
    fn pair_for_maps_outputs_and_falls_back() {
        let mut buffers = host_buffers(2);
        buffers[4].0[0] = 4.0;
        let mut outputs = routed(&mut buffers, &[0, 4]);
        assert_eq!(outputs.pair_for(4).expect("present").0[0], 4.0);
        assert_eq!(outputs.pair_for(1).expect("main").0[0], 9.0);
        assert_eq!(outputs.pair_for(200).expect("main").0[0], 9.0, "out of range is main");

        let mut none = Outputs { pairs: [const { None }; OUTPUTS] };
        assert!(none.pair_for(0).is_none(), "no main output, nothing to render into");
    }

    #[test]
    fn an_audition_lands_on_the_main_pair_whatever_the_lanes_output() {
        let (mut model, id) = one_lane_model(8);
        assert!(model.set_output(id, 3));
        let mut engine = Engine::new();
        assert!(engine.auditions().push(id));

        let mut buffers = host_buffers(8);
        engine.run_block(
            model.published(),
            &[],
            &mut routed(&mut buffers, &every_output()),
            RATE,
            1.0,
        );

        assert_eq!(buffers[0].0[..], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(buffers[3].0[..], [0.0; 8]);
    }

    #[test]
    fn a_preview_lands_on_the_main_pair() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        preview(&model, &channel, 1, Some(ramp(4)));

        let mut buffers = host_buffers(4);
        engine.run_block(
            model.published(),
            &[],
            &mut routed(&mut buffers, &every_output()),
            RATE,
            1.0,
        );

        let c = centre();
        assert_eq!(buffers[0].0[..], [c, 2.0 * c, 3.0 * c, 4.0 * c]);
        for (index, (left, right)) in buffers.iter().enumerate().skip(1) {
            assert_eq!((&left[..], &right[..]), (&[0.0; 4][..], &[0.0; 4][..]), "pair {index}");
        }
    }

    /// The master ramp shapes every pair alike: a lane on Out 5 renders,
    /// under a moving master gain, exactly what the same lane renders on the
    /// main output.
    #[test]
    fn the_master_ramp_applies_per_pair() {
        let mut model = switch_off_model();
        lane_with(&mut model, 0, 36, ramp(16));
        let aux = lane_with(&mut model, 1, 37, ramp(16));
        assert!(model.set_output(aux, 4));
        let mut engine = Engine::new();

        let mut buffers = host_buffers(8);
        let present = every_output();
        let notes = [note_on(0, 36), note_on(0, 37)];
        engine.run_block(model.published(), &notes, &mut routed(&mut buffers, &present), RATE, 1.0);
        engine.run_block(model.published(), &[], &mut routed(&mut buffers, &present), RATE, 0.25);

        let main = &buffers[0].0;
        assert_eq!(main[0], 9.0, "frame 0 of the ramp is still the last block's gain");
        assert!(main[7] < 16.0 * 0.5, "and the line has moved down by the end: {main:?}");
        assert_eq!(buffers[4].0, *main, "Out 5 is ramped exactly as Out 1");
    }

    /// Block stitching holds per pair: one 16-frame block and two 8-frame
    /// halves, with notes landing inside both halves on three outputs,
    /// write the same frames into every pair.
    #[test]
    fn stitching_holds_per_pair() {
        let mut model = switch_off_model();
        let main = lane_with(&mut model, 0, 36, ramp(6));
        let two = lane_with(&mut model, 1, 37, ramp(6));
        let nine = lane_with(&mut model, 2, 38, ramp(20));
        assert!(model.set_output(main, 0));
        assert!(model.set_output(two, 2));
        assert!(model.set_output(nine, 9));
        let present = every_output();

        let mut whole = Engine::new();
        let mut whole_buffers = host_buffers(16);
        let events = [note_on(3, 36), note_on(5, 38), note_on(11, 37)];
        whole.run_block(
            model.published(),
            &events,
            &mut routed(&mut whole_buffers, &present),
            RATE,
            1.0,
        );

        let mut halves = Engine::new();
        let mut first = host_buffers(8);
        let mut second = host_buffers(8);
        halves.run_block(
            model.published(),
            &[note_on(3, 36), note_on(5, 38)],
            &mut routed(&mut first, &present),
            RATE,
            1.0,
        );
        halves.run_block(
            model.published(),
            &[note_on(3, 37)],
            &mut routed(&mut second, &present),
            RATE,
            1.0,
        );

        for index in 0..OUTPUTS {
            let joined: Vec<f32> = first[index].0.iter().chain(&second[index].0).copied().collect();
            assert_eq!(joined, whole_buffers[index].0, "pair {index} stitches");
        }
        assert_ne!(whole_buffers[9].0, vec![0.0; 16], "and the aux pairs did get sound");
        assert_ne!(whole_buffers[2].0, vec![0.0; 16]);
    }

    // ---- The chop editor's preview -------------------------------------

    /// A 48 kHz mono [`ramp`] of `count` frames, as the chop editor's source.
    fn chop_source(model: &Model, count: usize) -> basedrop::Shared<SampleData> {
        let data = SampleData { rate: RATE as u32, channels: 1, frames: ramp(count).into() };
        basedrop::Shared::new(model.handle(), data)
    }

    /// Publish `region` of `source` as generation `generation`, live atomics
    /// first, as `Previewer::play_region` does.
    fn preview_region(
        model: &Model,
        channel: &PreviewChannel,
        generation: u64,
        source: &basedrop::Shared<SampleData>,
        region: Region,
        looping: bool,
    ) {
        channel.set_live_region(region);
        channel.set_live_looping(looping);
        let state = crate::preview::PreviewState {
            generation,
            sample: Some(source.clone()),
            region: Some(region),
            looping,
        };
        channel.publish(model.handle(), state);
    }

    /// One block of `frames` frames, left channel only (the preview is
    /// centred, so right is the same).
    fn preview_block(model: &Model, engine: &mut Engine, frames: usize) -> Vec<f32> {
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        engine.run_block(
            model.published(),
            &[],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        assert_eq!(left, right, "centred");
        left
    }

    /// What the lane would play for `[start, end)` — `chop::cut` — at the
    /// preview's centred gain.
    fn cut_at_centre(source: &SampleData, start: u64, end: u64) -> Vec<f32> {
        let c = centre();
        chop::cut(source, start, end).frames.iter().map(|x| x * c).collect()
    }

    #[test]
    fn a_region_preview_plays_start_to_end_once_with_the_cut_fades() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        let source = chop_source(&model, 1000);
        preview_region(&model, &channel, 1, &source, Region { start: 100, end: 300 }, false);

        let left = preview_block(&model, &mut engine, 512);
        let expected = cut_at_centre(&source, 100, 300);
        // Starts at `start` (the ramp's frame 100 is 101.0, faded to 0 at
        // first), and its fade shape is exactly the cut's.
        assert_eq!(&left[..200], &expected[..], "the region, faded as `cut` fades it");
        assert_eq!(left[0], 0.0, "the fade-in starts from silence");
        assert_eq!(left[199], 0.0, "and the fade-out ends there");
        assert_eq!(left[100], 201.0 * centre(), "the middle is untouched");
        assert!(left[200..].iter().all(|&x| x == 0.0), "ends at `end`");
        assert!(!channel.is_playing(), "once");
        assert!(preview_block(&model, &mut engine, 512).iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_region_preview_reports_its_position() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        let source = chop_source(&model, 1000);
        preview_region(&model, &channel, 1, &source, Region { start: 100, end: 900 }, false);
        preview_block(&model, &mut engine, 64);
        assert_eq!(channel.position(), 164);
        preview_block(&model, &mut engine, 64);
        assert_eq!(channel.position(), 228);
    }

    #[test]
    fn a_looping_preview_wraps_at_the_live_end() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        let source = chop_source(&model, 1000);
        preview_region(&model, &channel, 1, &source, Region { start: 100, end: 300 }, true);
        let pass = cut_at_centre(&source, 100, 300);

        // No crossfade at the seam: pass after pass of the cut, seams inside
        // the block.
        let left = preview_block(&model, &mut engine, 512);
        assert_eq!(&left[..200], &pass[..]);
        assert_eq!(&left[200..400], &pass[..]);
        assert_eq!(&left[400..], &pass[..112]);

        // Edges moved around the position (212): this pass runs to its own
        // end, the next plays the live region.
        channel.set_live_region(Region { start: 150, end: 250 });
        let left = preview_block(&model, &mut engine, 288);
        let live = cut_at_centre(&source, 150, 250);
        assert_eq!(&left[..88], &pass[112..], "the pass in flight finishes");
        assert_eq!(&left[88..188], &live[..], "then the live region");
        assert_eq!(&left[188..], &live[..], "and again");

        // Looping turned off: the voice ends at the next seam.
        channel.set_live_looping(false);
        let left = preview_block(&model, &mut engine, 256);
        assert_eq!(&left[..100], &live[..]);
        assert!(left[100..].iter().all(|&x| x == 0.0));
        assert!(!channel.is_playing());
    }

    #[test]
    fn a_looping_preview_jumps_when_the_position_leaves_the_live_region() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        let source = chop_source(&model, 1000);
        preview_region(&model, &channel, 1, &source, Region { start: 100, end: 300 }, true);
        preview_block(&model, &mut engine, 50);
        assert_eq!(channel.position(), 150);

        // The end dragged in before the position: jump to the live start.
        channel.set_live_region(Region { start: 400, end: 500 });
        let left = preview_block(&model, &mut engine, 150);
        let live = cut_at_centre(&source, 400, 500);
        assert_eq!(&left[..100], &live[..], "at once, from the live start");
        assert_eq!(&left[100..], &live[..50]);

        // Not looping, the live region is not obeyed until the seam.
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        let source = chop_source(&model, 1000);
        preview_region(&model, &channel, 1, &source, Region { start: 100, end: 300 }, false);
        preview_block(&model, &mut engine, 50);
        channel.set_live_region(Region { start: 400, end: 500 });
        let left = preview_block(&model, &mut engine, 200);
        assert_eq!(&left[..150], &cut_at_centre(&source, 100, 300)[50..]);
        assert!(left[150..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn a_region_is_clamped_into_the_sample_and_an_empty_one_is_silent() {
        let model = switch_off_model();
        let (channel, mut engine) = previewing(&model);
        let source = chop_source(&model, 1000);
        preview_region(&model, &channel, 1, &source, Region { start: 900, end: 5000 }, false);
        let left = preview_block(&model, &mut engine, 128);
        assert_eq!(&left[..100], &cut_at_centre(&source, 900, 1000)[..]);
        assert!(left[100..].iter().all(|&x| x == 0.0));

        preview_region(&model, &channel, 2, &source, Region { start: 500, end: 500 }, true);
        preview_block(&model, &mut engine, 512);
        assert!(preview_block(&model, &mut engine, 64).iter().all(|&x| x == 0.0));
        assert!(!channel.is_playing());
    }

    // ---- Lane time -----------------------------------------------------

    /// An Off-mode lane plays at its own pitch, and a pitch drag reaches a
    /// voice already sounding within the next block; poly tuning still adds.
    #[test]
    fn an_off_lane_plays_at_its_pitch_and_hears_a_drag_mid_note() {
        let (mut model, id) = one_lane_model(4_096);
        assert!(model.set_pitch_st(id, 12.0));
        let mut engine = Engine::new();
        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        engine.run_block(
            model.published(),
            &[note_on(0, 36)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        let voice = engine.pool().voices().next().expect("the note started");
        assert_eq!(voice.step, 2.0, "pitch +12 at the note-on");

        assert!(model.set_pitch_st(id, -12.0));
        let tuning = EngineEvent::PolyTuning { note: target(36), semis: 24.0 };
        engine.run_block(
            model.published(),
            &[TimedEvent::new(0, tuning)],
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        let voice = engine.pool().voices().next().expect("still sounding");
        assert_eq!(voice.step, 2.0, "−12 lane pitch + 24 tuning, picked up mid-note");
    }

    /// The lane's pitch is Off mode's: a Repitch or Stretch lane ignores it
    /// (its field is disabled there); a Repitch lane on "as cut" (the
    /// default length) and a Stretch lane play at speed 1.
    #[test]
    fn lane_time_takes_the_pitch_only_in_off_mode() {
        let (mut model, id) = one_lane_model(64);
        assert!(model.set_pitch_st(id, 7.0));
        let lane = |model: &Model| {
            lane_time(model.published().get().lane(id).expect("lane"), 0.5, false, 120.0)
        };
        assert_eq!(lane(&model), (1.0, 7.0));
        assert!(model.set_time_mode(id, TimeMode::Repitch));
        assert_eq!(lane(&model), (1.0, 0.0));
        assert!(model.set_time_mode(id, TimeMode::Stretch));
        assert_eq!(lane(&model), (1.0, 0.0));
    }

    /// One beat at 120 BPM in frames at [`RATE`]: half a second.
    const BEAT_AT_120: usize = 24_000;

    /// A model with one Repitch lane on a one-beat chop (at 120 BPM),
    /// synced to a quarter note, and its id.
    fn repitch_model() -> (Model, u32) {
        let (mut model, id) = one_lane_model(BEAT_AT_120);
        assert!(model.set_time_mode(id, TimeMode::Repitch));
        assert!(model.set_length(id, Length::Beats(NoteLength::Quarter)));
        (model, id)
    }

    /// Run one `frames`-long block with `events`, returning its left channel.
    fn time_block(
        model: &Model,
        engine: &mut Engine,
        events: &[TimedEvent],
        frames: usize,
    ) -> Vec<f32> {
        let mut left = vec![0.0f32; frames];
        let mut right = vec![0.0f32; frames];
        engine.run_block(
            model.published(),
            events,
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        left
    }

    /// The first sounding voice's `(speed, step)`.
    fn speed_and_step(engine: &Engine) -> (f64, f64) {
        let voice = engine.pool().voices().next().expect("a voice sounding");
        (voice.speed, voice.step)
    }

    /// A one-beat chop synced to 1/4 plays as it is at 120 BPM and at half
    /// speed at 60 BPM, the lane's pitch ignored either way.
    #[test]
    fn a_one_beat_repitch_chop_plays_at_1_at_120_and_half_at_60() {
        let (mut model, id) = repitch_model();
        assert!(model.set_pitch_st(id, 7.0));

        let mut engine = Engine::new();
        assert_eq!(engine.tempo().get(), 120.0);
        let out = time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0), "120 BPM: speed 1");
        assert_eq!(out, [1.0, 2.0, 3.0, 4.0], "the chop as it is");

        let mut slow = Engine::new();
        slow.tempo().set(60.0);
        let out = time_block(&model, &mut slow, &[note_on(0, 36)], 4);
        assert_eq!(speed_and_step(&slow), (0.5, 0.5), "60 BPM: speed 0.5");
        // Every frame lasts two: whole positions land on frames 0 and 2 (the
        // half positions are the interpolator's business).
        assert_eq!((out[0], out[2]), (1.0, 2.0), "every frame twice as long: {out:?}");
    }

    /// A tempo change, a fine trim or a length change reaches a note already
    /// sounding within the next block, without moving its position.
    #[test]
    fn a_tempo_change_mid_note_changes_the_step_in_the_next_block() {
        let (mut model, id) = repitch_model();
        let mut engine = Engine::new();
        time_block(&model, &mut engine, &[note_on(0, 36)], 8);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0));

        engine.tempo().set(60.0);
        let out = time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (0.5, 0.5), "the very next block");
        assert_eq!((out[0], out[2]), (9.0, 10.0), "from where the voice was: {out:?}");

        engine.tempo().set(240.0);
        time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (2.0, 2.0), "and back faster");

        // 1/2 at 240 BPM is a beat at 120: speed 1 again.
        assert!(model.set_length(id, Length::Beats(NoteLength::Half)));
        time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0), "a length change");

        // Fine +10 % makes the target longer, the speed slower.
        assert!(model.set_fine_pct(id, 10.0));
        time_block(&model, &mut engine, &[], 4);
        let (speed, _) = speed_and_step(&engine);
        assert!((speed - 1.0 / 1.1).abs() < 1e-6, "fine +10 % → speed 1/1.1, got {speed}");
    }

    /// A model with one Stretch lane on a one-beat chop (at 120 BPM),
    /// synced to a quarter note, and its id. No render has landed yet.
    fn stretch_model() -> (Model, u32) {
        let (mut model, id) = one_lane_model(BEAT_AT_120);
        assert!(model.set_time_mode(id, TimeMode::Stretch));
        assert!(model.set_length(id, Length::Beats(NoteLength::Quarter)));
        (model, id)
    }

    /// Land a `frames`-long render (a ramp at [`RATE`]) for the lane's
    /// newest request, as the render worker would.
    fn land_render(model: &mut Model, id: u32, frames: usize) {
        assert!(model.request_render(id));
        let generation = model.lane(id).expect("lane").render_generation;
        let data = SampleData { rate: RATE as u32, channels: 1, frames: ramp(frames).into() };
        assert!(model.set_render(id, generation, data), "the render lands");
    }

    /// The speed correction: a render made for 120 BPM, played at 60 BPM, runs
    /// at speed 0.5 so the note still lasts its quarter; once the render for
    /// 60 BPM lands, new notes play it at speed 1, while the note already
    /// sounding keeps its own render and its 0.5.
    #[test]
    fn a_120_bpm_render_at_60_plays_at_half_until_the_new_render_lands() {
        let (mut model, id) = stretch_model();
        land_render(&mut model, id, BEAT_AT_120);

        let mut engine = Engine::new();
        engine.tempo().set(60.0);
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        assert_eq!(speed_and_step(&engine), (0.5, 0.5), "the 120 BPM render at 60 BPM");
        // And it stays corrected block after block.
        time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (0.5, 0.5), "a later block");

        land_render(&mut model, id, 2 * BEAT_AT_120);
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        let mut voices: Vec<_> = engine.pool().voices().map(|v| (v.age, v)).collect();
        voices.sort_by_key(|&(age, _)| age);
        let [(_, old), (_, new)] = voices[..] else { panic!("two voices: {}", voices.len()) };
        assert_eq!(new.sample.frame_count(), 2 * BEAT_AT_120, "the new note plays the new render");
        assert_eq!((new.speed, new.step), (1.0, 1.0), "a render for this tempo: speed 1");
        assert_eq!(old.sample.frame_count(), BEAT_AT_120, "the old note keeps its render");
        assert_eq!((old.speed, old.step), (0.5, 0.5), "and its correction");
    }

    /// A tempo change reaches a sounding Stretch voice within the next
    /// block, as in Repitch, measured against the voice's own render.
    #[test]
    fn a_tempo_change_corrects_a_sounding_stretch_voice_in_the_next_block() {
        let (mut model, id) = stretch_model();
        land_render(&mut model, id, BEAT_AT_120);
        let mut engine = Engine::new();
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0), "120 BPM: the render as made");

        engine.tempo().set(240.0);
        time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (2.0, 2.0), "the very next block");
    }

    /// A Stretch lane with no render yet plays its raw chop at speed 1
    /// whatever the tempo, and "as cut" plays its render at speed 1 (the
    /// render is the chop's own length); the lane's pitch is not applied.
    #[test]
    fn a_raw_chop_and_as_cut_play_at_speed_1_in_stretch() {
        let (mut model, id) = stretch_model();
        assert!(model.set_pitch_st(id, 7.0));
        let mut engine = Engine::new();
        engine.tempo().set(60.0);
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        let voice = engine.pool().voices().next().expect("a voice");
        assert!(!voice.rendered, "the raw chop");
        assert_eq!(speed_and_step(&engine), (1.0, 1.0), "raw chop: speed 1");

        assert!(model.set_length(id, Length::AsCut));
        land_render(&mut model, id, BEAT_AT_120);
        let mut engine = Engine::new();
        engine.tempo().set(60.0);
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        assert!(engine.pool().voices().next().expect("a voice").rendered, "the render");
        assert_eq!(speed_and_step(&engine), (1.0, 1.0), "as cut: speed 1");
    }

    /// "As cut" in Repitch is Off without its pitch: speed 1 at any tempo,
    /// at the note-on and mid-note.
    #[test]
    fn a_repitch_lane_as_cut_plays_at_speed_1() {
        let (mut model, id) = repitch_model();
        assert!(model.set_length(id, Length::AsCut));
        assert!(model.set_pitch_st(id, -5.0));
        let mut engine = Engine::new();
        engine.tempo().set(73.0);
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0));
        engine.tempo().set(200.0);
        time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0));
    }

    /// A voice is timed by its own sample: swapping the lane's sample for a
    /// longer one mid-note leaves the sounding voice's speed alone.
    #[test]
    fn a_repitch_voice_keeps_its_own_samples_length() {
        let (mut model, id) = repitch_model();
        let mut engine = Engine::new();
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        lane_with(&mut model, 0, 36, ramp(2 * BEAT_AT_120));
        let mode = model.published().get().lane(id).map(|l| l.time_mode);
        assert_eq!(mode, Some(TimeMode::Repitch), "a new sample keeps the lane's mode");
        time_block(&model, &mut engine, &[], 4);
        assert_eq!(speed_and_step(&engine), (1.0, 1.0), "the old voice, untouched");
        time_block(&model, &mut engine, &[note_on(0, 36)], 4);
        let speeds: Vec<f64> = engine.pool().voices().map(|v| v.speed).collect();
        assert!(speeds.contains(&2.0), "the new note on the 2-beat sample: {speeds:?}");
    }

    /// Block stitching holds for a Repitch lane at a steady tempo whose
    /// speed is fractional: one block equals two halves, bit for bit.
    #[test]
    fn a_repitch_lane_stitches_at_a_steady_tempo() {
        let (model, _id) = repitch_model();
        let mut whole = Engine::new();
        whole.tempo().set(97.0);
        let joined_whole = time_block(&model, &mut whole, &[note_on(3, 36)], 64);

        let mut halves = Engine::new();
        halves.tempo().set(97.0);
        let mut joined = time_block(&model, &mut halves, &[note_on(3, 36)], 32);
        joined.extend(time_block(&model, &mut halves, &[], 32));

        let (speed, _) = speed_and_step(&halves);
        assert!(speed != 1.0 && speed.fract() != 0.0, "a fractional speed: {speed}");
        assert_eq!(joined_whole, joined);
    }

    /// One stretch of a stitching run: the host tempo for it, lane 1's fine
    /// trim, and its events, timed from the stretch's own start.
    type Span<'a> = (f64, f32, &'a [TimedEvent]);

    /// Frames in each [`Span`].
    const STRETCH_FRAMES: usize = 64;

    /// Render `stretches` back to back through a fresh engine, cutting each
    /// one into blocks of at most `block` frames — the tempo stored and the
    /// fine set only at a stretch's start, as a host's transport and the
    /// GUI can only change them between blocks. Returns the left channel
    /// and, per stretch, the speeds of the voices sounding at its end.
    fn render_stretches(
        model: &mut Model,
        fine_lane: u32,
        stretches: &[Span],
        block: usize,
    ) -> (Vec<f32>, Vec<Vec<f64>>) {
        let mut engine = Engine::new();
        let mut out = Vec::new();
        let mut speeds = Vec::new();
        for &(bpm, fine, events) in stretches {
            engine.tempo().set(bpm);
            assert!(model.set_fine_pct(fine_lane, fine));
            let mut start = 0;
            while start < STRETCH_FRAMES {
                let end = (start + block).min(STRETCH_FRAMES);
                let inside: Vec<TimedEvent> = events
                    .iter()
                    .filter(|e| (start..end).contains(&(e.timing as usize)))
                    .map(|e| TimedEvent::new(e.timing - start as u32, e.event))
                    .collect();
                out.extend(time_block(model, &mut engine, &inside, end - start));
                start = end;
            }
            speeds.push(engine.pool().voices().map(|v| v.speed).collect());
        }
        (out, speeds)
    }

    /// Block stitching holds across tempo changes: with the tempo and a fine
    /// trim moving only between blocks, as they do from a host, a run cut
    /// into 64-frame blocks equals the same run cut into 32-frame and into
    /// uneven 13-frame blocks, bit for bit — Repitch voices re-timed mid-note
    /// at every change, notes starting inside later blocks, an Off lane's
    /// pitch sounding alongside.
    #[test]
    fn repitch_lanes_stitch_across_tempo_changes() {
        let (mut model, _quarter) = repitch_model();
        let eighth = lane_with(&mut model, 1, 37, ramp(30_000));
        assert!(model.set_time_mode(eighth, TimeMode::Repitch));
        assert!(model.set_length(eighth, Length::Beats(NoteLength::EighthTriplet)));
        let off = lane_with(&mut model, 2, 38, ramp(30_000));
        assert!(model.set_pitch_st(off, 3.5));

        let first = [note_on(0, 36), note_on(5, 37), note_on(40, 38)];
        let second = [note_on(17, 36)];
        let third = [note_on(33, 37), note_off(50, 36)];
        let stretches: [Span; 4] = [
            (97.0, 0.0, &first),
            (140.0, 3.7, &second),
            (61.5, -8.2, &third),
            (203.0, 12.5, &[]),
        ];

        let (whole, speeds) = render_stretches(&mut model, eighth, &stretches, STRETCH_FRAMES);
        let (halves, _) = render_stretches(&mut model, eighth, &stretches, STRETCH_FRAMES / 2);
        let (uneven, _) = render_stretches(&mut model, eighth, &stretches, 13);

        // The run has to re-time voices at every change, at fractional
        // speeds, or equal outputs prove little.
        for pair in speeds.windows(2) {
            assert!(pair[0] != pair[1], "the speeds moved at the change: {speeds:?}");
        }
        assert!(
            speeds.iter().flatten().any(|s| s.fract() != 0.0),
            "fractional speeds: {speeds:?}"
        );
        assert!(whole.iter().any(|&x| x != 0.0), "the run is audible");
        assert_eq!(whole, halves, "64-frame blocks equal 32-frame ones");
        assert_eq!(whole, uneven, "and uneven 13-frame ones");
    }

    /// The other side of the contract: a tempo change *is* heard at the
    /// block it arrives in, not smeared over a later one. Two runs alike but
    /// for the second stretch's tempo are bit-identical up to the change
    /// and differ from its first frame on.
    #[test]
    fn a_tempo_change_is_heard_from_the_block_it_arrives_in() {
        let (mut model, id) = repitch_model();
        let first = [note_on(0, 36)];
        let steady: [Span; 2] = [(120.0, 0.0, &first), (120.0, 0.0, &[])];
        let changed: [Span; 2] = [(120.0, 0.0, &first), (90.0, 0.0, &[])];

        let (a, _) = render_stretches(&mut model, id, &steady, 16);
        let (b, _) = render_stretches(&mut model, id, &changed, 16);

        assert_eq!(a[..STRETCH_FRAMES], b[..STRETCH_FRAMES], "the same up to the change");
        // Frame 64 of the voice is position 64 either way; from the next
        // frame on, the slower step lands between frames.
        assert_eq!(a[STRETCH_FRAMES], b[STRETCH_FRAMES], "the boundary frame is shared");
        assert_ne!(a[STRETCH_FRAMES + 1], b[STRETCH_FRAMES + 1], "then the new speed");
    }
}
