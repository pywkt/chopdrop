//! The lane list: what a lane is, and the eight a fresh instance starts with.
//!
//! `model.rs` owns every lane and every way a lane changes. The GUI, the
//! background loader and a project load all reach the lanes through this
//! module's writer API; the audio thread never does — it reads an immutable
//! snapshot the writers publish for it. Pure module: nothing here imports a
//! nice-plug type, so the whole of it is unit-tested without the framework.
//!
//! # What is here
//!
//! The shapes and the starting state: [`Lane`], [`SampleRef`], the limits
//! every writer will clamp against, and [`default_lanes`] — the eight empty
//! lanes on notes 36–43 that a new instance opens with, pinned by a test
//! because every default a user sees without touching anything is a promise
//! the GUI, the docs and a reloaded project all repeat.
//!
//! And the writer API: [`Model`] holds the lanes and every operation that
//! changes one — add, delete, duplicate, rename, reorder, the per-lane
//! setters and the instance switch — each clamping what it is handed, so a
//! GUI gesture, a loader result and a project file all reach a
//! lane field through the same check. Lanes are named by [`Lane::id`]
//! throughout, so a writer call for a lane that is gone lands nowhere
//! instead of on its neighbour.
//!
//! And publishing: the model owns the [`PublishedCell`] the audio thread
//! reads its lanes out of once per block, the `basedrop::Collector` that
//! frees the snapshots the swaps retire, and the volume, pan and switch
//! atomics that let a knob drag reach the audio thread without building a
//! list at all. Every writer here ends in one of two tails: publish a fresh
//! list and collect, or store an atomic and collect — so garbage is
//! reclaimed by whoever made it, with the editor open or closed. The one
//! thing a writer that is not the GUI needs on top of that is a way to say
//! it has written: [`Model::set_repaint_hook`] and
//! [`Model::request_repaint`], which is how a sample the loader decoded on
//! its own thread appears without the pointer moving.
//!
//! And undo: the model keeps the state as of the last committed
//! gesture, so the writers above need to know nothing about history and a
//! knob dragged for a second is one step back rather than sixty.
//! [`Model::commit`] is what the editor and the loader call when a gesture
//! finishes, and [`Model::undo`] / [`Model::redo`] walk the bounded stacks
//! of states it builds.
//!
//! And the project trip: [`Model::snapshot`] and [`Model::load_snapshot`],
//! which are [`Model::replace_lanes`] with the project snapshot in front of
//! it, plus the embedding-limit accounting — what a save would embed
//! ([`Model::embedded_bytes`]) — which [`crate::state::embed_warning`] is
//! the threshold for — and the note a lane holding a sample too big to embed
//! carries ([`Lane::embed_note`]).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use basedrop::{Collector, Handle, Shared, SharedCell};

use crate::chop::{self, ChopEditor, ChopOrigin, Peaks, Selection, View};
use crate::sample::SampleData;
use crate::state::{
    ChopEditorSnapshot, ChopRestore, LANE_PALETTE, PendingSample, Restore, Snapshot,
};
// The project's embedding limits live in `state.rs`, beside the save
// that applies them; this module only counts against them
// ([`Model::embedded_bytes`]), and the editor imports the threshold from there.

// The limits the writer API clamps against. Those that govern playback are
// defined in `voices.rs`, beside it; this module re-exports the ones its
// writers clamp with rather than declaring a second copy that could drift
// (see each constant's docs there). `MAX_VOICES` sizes the pool and no
// writer, so it is imported from `voices.rs` alone.
pub use crate::voices::{CHOKE_GROUPS, LANE_VOLUME_DB_MAX, LANE_VOLUME_DB_MIN, LaneMode};
// And the clamps those ranges imply, for the same reason: the setter that
// stores a lane's volume, pan or choke group and the voice that plays it
// agree by construction, because they call the one function.
pub use crate::voices::{clamp_choke, clamp_pan, clamp_volume_db};
// The v2 time types live in the pure `time.rs`, re-exported here as
// `LaneMode` is from `voices.rs`.
pub use crate::time::{Length, NoteLength, TimeMode};
use crate::time::TempoCell;

/// The most lanes an instance may hold: 128.
///
/// A ceiling rather than a capacity — the lane list is a `Vec` that grows as
/// lanes are added — so its job is to stop a hostile or corrupt project file
/// from asking for unbounded allocation (a project load drops the excess;
/// `add_lane` refuses silently at the limit). 128 is one lane per MIDI note,
/// which is as many as a note source can address at all, and sixteen times
/// what a fresh instance offers.
pub const MAX_LANES: u32 = 128;

/// The longest a lane name may be, in **characters**: 64.
///
/// Counted in characters and not in bytes because the limit exists for the
/// GUI's sake — a lane row has only so much width — and because a user
/// naming a lane in Japanese should get the same 64 of them an English name
/// gets. [`Lane::new`] and `rename` trim on a character boundary, so no
/// truncation can ever cut a multi-byte character in half.
pub const MAX_NAME_LEN: usize = 64;

/// How loud a lane plays when nothing has touched its volume: 0 dB,
/// unchanged.
///
/// Also where a NaN volume lands: the lane clamps send a lost number to the
/// neutral factor rather than to silence, so a bad value from a project file
/// cannot quietly mute a lane.
pub const LANE_VOLUME_DB_DEFAULT: f32 = 0.0;

/// The leftmost a lane may be panned: hard left.
pub const LANE_PAN_MIN: f32 = -1.0;
/// The rightmost a lane may be panned: hard right.
pub const LANE_PAN_MAX: f32 = 1.0;
/// Where a lane sits when nothing has touched its pan: centred — and where a
/// NaN pan lands, for the reason [`LANE_VOLUME_DB_DEFAULT`] gives.
pub const LANE_PAN_DEFAULT: f32 = 0.0;

/// The choke group of a lane that chokes nothing and is choked by nothing:
/// 0, which is not a group at all. Groups are 1..=[`CHOKE_GROUPS`].
pub const CHOKE_NONE: u8 = 0;

/// The shortest a lane's attack may be: 0 ms, an instant start — v1's
/// behaviour, bit for bit.
pub const ATTACK_MS_MIN: f32 = 0.0;
/// The longest a lane's attack may be: 2000 ms.
pub const ATTACK_MS_MAX: f32 = 2000.0;
/// A new lane's attack, and where a NaN attack lands: 0 ms.
pub const ATTACK_MS_DEFAULT: f32 = 0.0;

/// The shortest a lane's Gate-mode release may be: 0 ms (which still plays
/// the fixed anti-click fade).
pub const RELEASE_MS_MIN: f32 = 0.0;
/// The longest a lane's release may be: 5000 ms.
pub const RELEASE_MS_MAX: f32 = 5000.0;
/// A new lane's release, and where a NaN release lands: 300 ms.
pub const RELEASE_MS_DEFAULT: f32 = 300.0;
/// Every lane's release when a **version 1** project loads: 0 ms, which is
/// the fixed [`crate::voices::FADE_MS`] fade v1's gate lanes always had — so
/// a v1 project sounds the same. A v2 file that omits the field still gets
/// [`RELEASE_MS_DEFAULT`].
pub const RELEASE_MS_V1: f32 = 0.0;

/// Whether a lane made in v2 cuts itself off at each new note-on: yes.
///
/// A v1 project's lanes load with `false` instead (v1 projects stay the
/// same), which is why this is the *new-lane* default and not the
/// snapshot fallback.
pub const CUT_ITSELF_DEFAULT: bool = true;

/// The highest output index a lane may render into: 15, so outputs 0..=15
/// are Out 1 (main) … Out 16.
pub const OUTPUT_MAX: u8 = 15;
/// A new lane's output: 0, the main output — and where an out-of-range
/// output lands, so a bad file can never leave a lane silent.
pub const OUTPUT_MAIN: u8 = 0;

/// The lowest a lane's fine tempo trim may be: −10 %.
pub const FINE_MIN: f32 = -10.0;
/// The highest a lane's fine tempo trim may be: +10 %.
pub const FINE_MAX: f32 = 10.0;
/// A new lane's fine trim, and where a NaN one lands: 0 %.
pub const FINE_DEFAULT: f32 = 0.0;

/// The lowest a lane's pitch may be: −24 semitones.
pub const PITCH_MIN: f32 = -24.0;
/// The highest a lane's pitch may be: +24 semitones.
pub const PITCH_MAX: f32 = 24.0;
/// A new lane's pitch, and where a NaN one lands: 0 semitones.
pub const PITCH_DEFAULT: f32 = 0.0;

/// How many lanes a new instance opens with: 16 (since
/// 2026-11-16; v1 and v2 until then opened with 8).
///
/// The first eight pair one to one with bitstep's eight default lanes: on
/// the same note, in the same colour. The other eight carry on up
/// the chromatic scale and round the palette, so a kit bigger than one
/// bitstep instance has its lanes waiting too. Only a fresh instance: a
/// saved project keeps the lanes it was saved with.
pub const DEFAULT_LANE_COUNT: u32 = 16;

/// The note the first default lane answers to: 36, which Bitwig names C1
/// (60 = C3, the convention everywhere in this plugin). The default lanes
/// walk up chromatically from here, so the sixteenth is on 51 (D#2).
///
/// The same octave a Drum Machine's first pads sit on, and the one bitstep's
/// default lanes send on.
pub const DEFAULT_FIRST_NOTE: u8 = 36;

/// Whether the "bitstep Sample pad" switch starts on: yes, for a **new**
/// instance (since 2026-11-16).
///
/// chopdrop is played by bitstep, and bitstep's lanes start with Sample pad
/// on, which sends every note retuned by `60 − key`. With the switch on that
/// retune is cancelled, so bitstep's default lanes play each sample at its
/// own pitch out of the box. A project saved with the switch off keeps it
/// off, and a snapshot that omits the field loads it off (`state.rs`): only
/// a fresh instance starts on. The switch is instance-wide model state,
/// saved with the project, and lives beside the lanes in the model proper.
pub const SAMPLE_PAD_SWITCH_DEFAULT: bool = true;

/// How many gestures the model can step back over: 100.
///
/// Bounded because an undo entry holds a clone of the lane list, and a lane's
/// clone keeps its sample alive: without a limit, a session of editing would
/// be a session of never handing back a single decoded file. A hundred is
/// more gestures than a user retraces by hand, and past it the *oldest* entry
/// goes rather than the newest being refused — the recent past is the part
/// anyone undoes.
pub const UNDO_DEPTH: usize = 100;

// A default lane list that could not be created by `add_lane` would be a
// state the writer API cannot reach, and `MAX_LANES` is the one limit a
// fresh instance must already satisfy.
const _: () = assert!(DEFAULT_LANE_COUNT <= MAX_LANES);
// The default notes have to exist: 36 + 15 = 51, far below 127. Said here
// so that raising either constant cannot silently wrap the progression.
const _: () = assert!(DEFAULT_FIRST_NOTE as u32 + DEFAULT_LANE_COUNT - 1 <= 127);

/// The sample a lane holds: the file as it was read, and the audio decoded
/// out of it.
///
/// Both halves are `basedrop::Shared`, and both are needed:
///
/// - [`bytes`](Self::bytes) is the original file, which is what gets saved
///   into the project (the embedded copy is the truth, not the path), and
/// - [`data`](Self::data) is the decoded audio, which is what the audio
///   thread plays.
///
/// `Shared` rather than `Arc` because a sounding voice holds its own clone
/// of `data`: replacing or clearing a lane's sample mid-note cannot stop or
/// glitch the notes already playing the old one, and the last clone to go is
/// freed by the collector on the writer or editor side — never on the audio
/// thread.
///
/// Cloning a `SampleRef` copies two refcount bumps and two short strings: a
/// snapshot, an undo entry or a duplicated lane all share the one copy of
/// the audio.
#[derive(Clone)]
pub struct SampleRef {
    /// Where the file was loaded from, for the GUI to show and for "reload
    /// from file" to re-read.
    ///
    /// A `String` and not a `PathBuf` because this is what the snapshot
    /// stores; [`SampleRef::new`] converts lossily, so a path that is not
    /// valid UTF-8 survives as something displayable rather than as bytes no
    /// project file could hold. The audio does not depend on it: the lane
    /// plays [`data`](Self::data), which came from
    /// [`bytes`](Self::bytes), whatever the path says.
    pub path: String,
    /// The last component of [`path`](Self::path) — what the lane row and
    /// the error messages name the sample by, because a lane is three
    /// hundred pixels wide and a path is not.
    pub file_name: String,
    /// The file exactly as it was read, before decoding: what a save embeds
    /// (base64 of these bytes) and what a project load decodes again, so
    /// that a restored lane plays the audio the project was saved with even
    /// if the file on disk has since changed or gone.
    ///
    /// `Shared<Box<[u8]>>` and not `Shared<[u8]>`: basedrop 0.1.3's
    /// `Shared<T>` is sized-only, so the slice needs a box around it. One
    /// extra indirection on a path nothing hot reads.
    pub bytes: Shared<Box<[u8]>>,
    /// The decoded audio the voices read.
    pub data: Shared<SampleData>,
    /// Where a chop lane's sample was cut from, for the row's hover text
    /// ([`chop::format_origin`]) and "open source in editor"; `None` for a
    /// sample loaded any other way ([`SampleRef::new`]). Saved as
    /// [`crate::state::OriginSnapshot`].
    pub origin: Option<ChopOrigin>,
}

impl SampleRef {
    /// A sample reference over a file's bytes and the audio decoded from
    /// them, both handed to the collector that will eventually free them.
    ///
    /// [`file_name`](Self::file_name) is derived here rather than passed in,
    /// so it cannot disagree with [`path`](Self::path). A path with no final
    /// component (`/`, or an empty string) keeps the whole path as its name:
    /// a label that is odd is better than one that is blank, and a sample
    /// loaded from such a path is a case only a corrupt project file
    /// produces.
    ///
    /// The `bytes` `Vec` is boxed to its exact length: these live for as
    /// long as the lane holds the sample, and up to 64 MiB of them
    /// ([`crate::sample::MAX_SAMPLE_BYTES`]), so spare capacity from the
    /// read is not worth keeping.
    pub fn new(handle: &Handle, path: &Path, bytes: Vec<u8>, data: SampleData) -> Self {
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());

        Self {
            path: path.to_string_lossy().into_owned(),
            file_name,
            bytes: Shared::new(handle, bytes.into_boxed_slice()),
            data: Shared::new(handle, data),
            origin: None,
        }
    }

    /// [`SampleRef::new`] for a chop: the same sample, remembering the
    /// source and frames it was cut from.
    pub fn with_origin(
        handle: &Handle,
        path: &Path,
        bytes: Vec<u8>,
        data: SampleData,
        origin: Option<ChopOrigin>,
    ) -> Self {
        Self { origin, ..Self::new(handle, path, bytes, data) }
    }

    /// The origin line for a chop lane — its sample-name hover and the lane
    /// panel's read-only line: [`chop::format_origin`] over the source's file
    /// name, in the sample's own rate (a cut keeps the source's rate, so its
    /// frames are the source's frames). `None` for a sample with no origin.
    pub fn origin_text(&self) -> Option<String> {
        let origin = self.origin.as_ref()?;
        let path = Path::new(&origin.source_path);
        let name = path
            .file_name()
            .map_or_else(|| origin.source_path.clone(), |name| name.to_string_lossy().into_owned());
        Some(chop::format_origin(origin, self.data.rate, &name))
    }

    /// How many bytes a save would embed for this sample, before base64.
    ///
    /// The quantity the embedding limits account for: the per-file cap and
    /// the running embedded total are both counted in original file bytes,
    /// not in decoded audio.
    pub fn byte_len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether a save carries this sample's bytes inside the project, or
    /// only its path: embedded up to [`crate::sample::MAX_SAMPLE_BYTES`],
    /// path-only above it.
    ///
    /// The one place that decision is made. The save reads it
    /// ([`crate::state::SampleSnapshot`] writes `bytes_b64: None` when it is
    /// false), the running total skips what it excludes
    /// ([`Model::embedded_bytes`]) and the lane's note comes from it
    /// ([`Lane::embed_note`]) — three answers that have to be the same
    /// answer, because a project that said it embedded a sample and did not
    /// would come back silent without explaining why.
    ///
    /// Inclusive at the cap: a file of exactly the maximum size is one the
    /// loader accepts, so it is one a project keeps.
    ///
    /// In this version nothing can answer false — the loader refuses an
    /// over-cap file before it ever becomes a `SampleRef` — but the cap is
    /// written to be raised, and the day the two numbers differ this is
    /// what tells the project and the user the same story.
    pub fn embeds(&self) -> bool {
        self.byte_len() <= crate::sample::MAX_SAMPLE_BYTES
    }
}

// `basedrop::Shared` has no `Debug`, and a lane printed in a failing test
// should say which sample it holds without dumping sixty-four megabytes of
// audio at anyone. Sizes, not contents.
impl std::fmt::Debug for SampleRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleRef")
            .field("path", &self.path)
            .field("file_name", &self.file_name)
            .field("bytes", &self.byte_len())
            .field("rate", &self.data.rate)
            .field("channels", &self.data.channels)
            .field("frames", &self.data.frame_count())
            .field("origin", &self.origin)
            .finish()
    }
}

/// One lane: one sample, one note, and how it plays.
///
/// Every field is public, and every field that has a valid range is clamped
/// into it by the only two kinds of code that writes one — [`Lane::new`] and
/// the writer API — because the values arrive from the GUI, from host
/// automation and from project files, and the rule at every boundary is to
/// clamp rather than to trust or to panic.
///
/// A lane's position in the model's `Vec` is its display order and nothing
/// more: reordering lanes does not change which note any of them answers to.
#[derive(Clone, Debug)]
pub struct Lane {
    /// Identifies this lane for as long as the instance lives, and is never
    /// reused once the lane is gone.
    ///
    /// What every writer call and every GUI gesture names a lane by, rather
    /// than its index: a lane dragged to a new position, or one whose
    /// neighbour was deleted, is still the same lane, and an in-flight
    /// loader result that arrives for a lane that has since been deleted
    /// must land nowhere instead of on whoever took its place.
    ///
    /// Not saved: a project load assigns fresh ids.
    pub id: u32,
    /// What the lane is called: up to [`MAX_NAME_LEN`] characters.
    pub name: String,
    /// The note that plays this lane, 0–127 (Bitwig names 60 as C3).
    ///
    /// Several lanes may share a note — they layer, and one note-on starts a
    /// voice in each of them. That is a feature, not a collision to resolve.
    pub note: u8,
    /// The lane's colour as `0xRRGGBB`; only the low 24 bits are ever read.
    pub colour: u32,
    /// The lane's own volume in dB, [`LANE_VOLUME_DB_MIN`]..=
    /// [`LANE_VOLUME_DB_MAX`], default [`LANE_VOLUME_DB_DEFAULT`].
    ///
    /// Decibels and not a gain because that is what the GUI shows and what a
    /// project stores; `voices.rs` converts once per voice.
    pub volume_db: f32,
    /// Where the lane sits in the stereo field, [`LANE_PAN_MIN`]..=
    /// [`LANE_PAN_MAX`], default [`LANE_PAN_DEFAULT`] (centred).
    pub pan: f32,
    /// The lane's choke group: [`CHOKE_NONE`] for no group, otherwise
    /// 1..=[`CHOKE_GROUPS`].
    ///
    /// A new voice in a group silences every sounding voice in the same
    /// group, including earlier voices of this lane.
    pub choke: u8,
    /// Whether the lane's voices end with their sample ([`LaneMode::OneShot`])
    /// or with the note ([`LaneMode::Gate`]).
    pub mode: LaneMode,
    /// The fade-in from note-on, [`ATTACK_MS_MIN`]..=[`ATTACK_MS_MAX`] ms,
    /// default [`ATTACK_MS_DEFAULT`].
    pub attack_ms: f32,
    /// The Gate-mode fade after note-off, [`RELEASE_MS_MIN`]..=
    /// [`RELEASE_MS_MAX`] ms, default [`RELEASE_MS_DEFAULT`]. OneShot
    /// ignores it.
    pub release_ms: f32,
    /// Whether a note-on for this lane fades out its own sounding voices
    /// first: [`CUT_ITSELF_DEFAULT`] for new lanes, `false` for v1 lanes.
    pub cut_itself: bool,
    /// The output the lane renders into: [`OUTPUT_MAIN`] (Out 1) ..=
    /// [`OUTPUT_MAX`] (Out 16).
    pub output: u8,
    /// How the lane's length follows the tempo; [`TimeMode::Off`] by default.
    pub time_mode: TimeMode,
    /// The length a synced lane plays for; [`Length::AsCut`] by default.
    pub length: Length,
    /// The fine tempo trim in percent, [`FINE_MIN`]..=[`FINE_MAX`], stored to
    /// 0.1 steps; default [`FINE_DEFAULT`].
    pub fine_pct: f32,
    /// The pitch in semitones, [`PITCH_MIN`]..=[`PITCH_MAX`], stored to two
    /// decimals (cents); default [`PITCH_DEFAULT`].
    pub pitch_st: f32,
    /// The sample, once one has been loaded. `None` is an empty lane: it
    /// takes up a row, keeps its settings, and plays nothing.
    pub sample: Option<SampleRef>,
    /// The last thing that went wrong for this lane, in the words the user
    /// sees — a file that would not decode, a sample a project could not
    /// restore. Cleared by a load that works.
    ///
    /// Saved state keeps no errors: a message describes this session's
    /// attempt, and the next project load makes its own.
    pub error: Option<String>,
    /// Whether a loader job for this lane is still running, so the GUI can
    /// say "loading…".
    ///
    /// Neither saved nor published to the audio thread: it is a fact about
    /// this moment in this session, and the audio thread has nothing to do
    /// with it — a lane that is loading simply has no sample yet.
    pub loading: bool,
    /// The newest time-stretch render that landed for this lane, or `None`
    /// before the first.
    ///
    /// Session state, like [`error`](Self::error): never saved (renders are
    /// rebuilt on load), never part of an undo step, and carried across an
    /// undo by [`Model::undo`] rather than reverted — a render is a cache
    /// of the lane's settings, not one of them. Only published while the
    /// lane is in [`TimeMode::Stretch`]; leaving Stretch or changing the
    /// sample drops it.
    pub render: Option<Render>,
    /// The generation of the newest render request for this lane.
    ///
    /// Bumped by every request ([`Model::request_render`]) and every change
    /// that makes an in-flight render useless, so that a result for an
    /// older generation is recognised as stale and ignored
    /// ([`Model::set_render`]). Never goes backwards — an undo keeps the
    /// live lane's value.
    pub render_generation: u64,
    /// Whether a render for [`render_generation`](Self::render_generation)
    /// is still to land, so the GUI can say "rendering…".
    pub render_pending: bool,
    /// Why the last render failed, in the words the user sees ("render
    /// failed"); cleared by a new request or a render that lands.
    pub render_error: Option<String>,
}

/// A time-stretched copy of a lane's chop, made by the render worker.
#[derive(Clone)]
pub struct Render {
    /// The rendered audio: the chop's rate and channel count, stretched to
    /// the lane's target length and shifted by its pitch.
    pub data: Shared<SampleData>,
    /// How long the render plays at speed 1, in seconds — the target it was
    /// made for, and what the engine's speed correction divides by.
    pub target_secs: f64,
    /// The request generation this render answered.
    pub generation: u64,
}

// `Shared` has no `Debug`, and the audio is not what a failing test wants to
// read anyway.
impl std::fmt::Debug for Render {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Render")
            .field("frames", &self.data.frame_count())
            .field("target_secs", &self.target_secs)
            .field("generation", &self.generation)
            .finish()
    }
}

impl Lane {
    /// An empty lane with the given identity: no sample, no error, not
    /// loading, 0 dB, centred, no choke group, one-shot.
    ///
    /// The single constructor, and therefore the single place a bad value
    /// could enter a lane, so it clamps what it can: the name is trimmed to
    /// [`MAX_NAME_LEN`] characters on a character boundary and the colour is
    /// masked to its low 24 bits. `note` needs no clamp — a `u8` is already
    /// 0–127 or above it only in a way no caller can produce, as the writer
    /// API's `set_note` clamps the one path (a project file) that can.
    pub fn new(id: u32, name: &str, note: u8, colour: u32) -> Self {
        Self {
            id,
            name: truncate_name(name),
            note: clamp_note(note),
            colour: clamp_colour(colour),
            volume_db: LANE_VOLUME_DB_DEFAULT,
            pan: LANE_PAN_DEFAULT,
            choke: CHOKE_NONE,
            mode: LaneMode::OneShot,
            attack_ms: ATTACK_MS_DEFAULT,
            release_ms: RELEASE_MS_DEFAULT,
            cut_itself: CUT_ITSELF_DEFAULT,
            output: OUTPUT_MAIN,
            time_mode: TimeMode::Off,
            length: Length::AsCut,
            fine_pct: FINE_DEFAULT,
            pitch_st: PITCH_DEFAULT,
            sample: None,
            error: None,
            loading: false,
            render: None,
            render_generation: 0,
            render_pending: false,
            render_error: None,
        }
    }

    /// The audio a new voice of this lane plays: the newest render while the
    /// lane is in [`TimeMode::Stretch`] and one has landed, the chop
    /// otherwise.
    pub fn playing_data(&self) -> Option<&Shared<SampleData>> {
        let sample = self.sample.as_ref()?;
        match (&self.render, self.time_mode) {
            (Some(render), TimeMode::Stretch) => Some(&render.data),
            _ => Some(&sample.data),
        }
    }

    /// Whether a new voice of this lane plays its render rather than its
    /// chop: the lane is in [`TimeMode::Stretch`], has a sample and a render
    /// has landed — the case where [`playing_data`](Self::playing_data)
    /// hands out the render.
    pub fn plays_render(&self) -> bool {
        self.sample.is_some() && self.render.is_some() && self.time_mode == TimeMode::Stretch
    }

    /// Clamp every field with a range into it, leaving the rest alone.
    ///
    /// For lanes that were built somewhere other than [`Lane::new`] — a
    /// project file's, or an undo entry's — and so for the one writer that
    /// takes such lanes, [`Model::replace_lanes`]. A setter clamps its own
    /// one value; this clamps all of them at once, out of the same
    /// functions, so a hostile snapshot cannot reach a lane field by a route
    /// that checks less than the GUI's route does.
    pub fn clamp(&mut self) {
        self.name = truncate_name(&self.name);
        self.note = clamp_note(self.note);
        self.colour = clamp_colour(self.colour);
        self.volume_db = clamp_volume_db(self.volume_db);
        self.pan = clamp_pan(self.pan);
        self.choke = clamp_choke(self.choke);
        self.attack_ms = clamp_attack_ms(self.attack_ms);
        self.release_ms = clamp_release_ms(self.release_ms);
        self.output = clamp_output(self.output);
        self.fine_pct = clamp_fine_pct(self.fine_pct);
        self.pitch_st = clamp_pitch_st(self.pitch_st);
    }

    /// The note this lane carries about what a save will do with its
    /// sample: [`crate::state::SAMPLE_NOT_EMBEDDED`] when the sample is
    /// over the embedding cap and the project will hold only its path,
    /// `None` otherwise.
    ///
    /// Derived from the sample rather than stored on the lane, so it cannot
    /// drift from the save that produced it and no writer has to remember to
    /// put it there. Not [`error`](Self::error) either: the lane is working,
    /// and the GUI shows this *beside* the file name rather than instead of
    /// it, which an error would replace.
    pub fn embed_note(&self) -> Option<&'static str> {
        match &self.sample {
            Some(sample) if !sample.embeds() => Some(crate::state::SAMPLE_NOT_EMBEDDED),
            _ => None,
        }
    }
}

/// A float clamped into `min..=max`, NaN to `default` (`max`-then-`min`
/// order, bitstep's pattern: an infinity lands on the nearer limit).
fn clamp_or(value: f32, min: f32, max: f32, default: f32) -> f32 {
    if value.is_nan() { default } else { value.max(min).min(max) }
}

/// An attack time as a lane may hold it: [`ATTACK_MS_MIN`]..=
/// [`ATTACK_MS_MAX`] ms, NaN to [`ATTACK_MS_DEFAULT`].
pub fn clamp_attack_ms(ms: f32) -> f32 {
    clamp_or(ms, ATTACK_MS_MIN, ATTACK_MS_MAX, ATTACK_MS_DEFAULT)
}

/// A release time as a lane may hold it: [`RELEASE_MS_MIN`]..=
/// [`RELEASE_MS_MAX`] ms, NaN to [`RELEASE_MS_DEFAULT`].
pub fn clamp_release_ms(ms: f32) -> f32 {
    clamp_or(ms, RELEASE_MS_MIN, RELEASE_MS_MAX, RELEASE_MS_DEFAULT)
}

/// An output index as a lane may hold it: anything above [`OUTPUT_MAX`]
/// goes to [`OUTPUT_MAIN`] rather than to the top output, so a bad value
/// is heard on the main output and never routed somewhere silent.
pub fn clamp_output(output: u8) -> u8 {
    if output > OUTPUT_MAX { OUTPUT_MAIN } else { output }
}

/// A fine trim as a lane may hold it: [`FINE_MIN`]..=[`FINE_MAX`] %, to
/// 0.1 steps, NaN to [`FINE_DEFAULT`].
pub fn clamp_fine_pct(pct: f32) -> f32 {
    let pct = clamp_or(pct, FINE_MIN, FINE_MAX, FINE_DEFAULT);
    (pct * 10.0).round() / 10.0
}

/// A pitch as a lane may hold it: [`PITCH_MIN`]..=[`PITCH_MAX`] semitones,
/// to two decimals, NaN to [`PITCH_DEFAULT`].
pub fn clamp_pitch_st(st: f32) -> f32 {
    let st = clamp_or(st, PITCH_MIN, PITCH_MAX, PITCH_DEFAULT);
    (st * 100.0).round() / 100.0
}

/// A lane name cut to [`MAX_NAME_LEN`] characters, never mid-character.
///
/// Counting characters and taking a prefix of them, rather than slicing
/// bytes, is what makes a 64-character Japanese name keep all 64 of them and
/// a byte-truncated multi-byte name impossible — slicing a `String` at a
/// byte index that is not a character boundary panics, and a lane name is
/// user input.
pub(crate) fn truncate_name(name: &str) -> String {
    name.chars().take(MAX_NAME_LEN).collect()
}

/// The lanes a fresh instance opens with: [`DEFAULT_LANE_COUNT`] (16) empty
/// ones named "Lane 1" … "Lane 16", on notes 36–51, in palette order and
/// round it again past its tenth colour.
///
/// Ids are handed out from 0 in creation order, so these hold 0..16 and the
/// first lane added afterwards gets 16 (the model owns the counter).
///
/// Lanes a user has to fill rather than ones they have to add: dropping
/// samples onto a new instance is the thing this plugin exists to make
/// quick, and the notes they land on are the ones bitstep sends.
pub fn default_lanes() -> Vec<Lane> {
    (0..DEFAULT_LANE_COUNT)
        .map(|index| {
            Lane::new(
                index,
                &format!("Lane {}", index + 1),
                DEFAULT_FIRST_NOTE + index as u8,
                // Round the palette past its end, as `+ lane` goes.
                LANE_PALETTE[index as usize % LANE_PALETTE.len()],
            )
        })
        .collect()
}

/// One lane as the audio thread sees it.
///
/// Only lanes that have a sample are published: a lane with nothing in it
/// cannot start a voice, so an entry for it would be one the engine always
/// skips. Everything here is immutable except the atomics — see
/// [`volume_db`](Self::volume_db), [`attack_ms`](Self::attack_ms) and
/// [`activity`](Self::activity) — because changing anything else means a
/// whole new snapshot.
///
/// The fields a voice copies at its note-on (the note, the choke group, the
/// mode, the sample) are plain values: a sounding voice keeps what it
/// started with, so there would be nothing to gain from making them
/// changeable in place.
pub struct PublishedLane {
    /// Which lane this is ([`Lane::id`]): what a writer's atomic store and
    /// the GUI's activity lights look an entry up by.
    pub id: u32,
    /// The note that starts a voice in this lane, 0–127. Several published
    /// lanes may share one — they layer.
    pub note: u8,
    /// The lane's choke group: [`CHOKE_NONE`] or 1..=[`CHOKE_GROUPS`].
    pub choke: u8,
    /// Whether this lane's voices end with the sample or with the note.
    pub mode: LaneMode,
    /// The decoded audio a new voice clones its own handle from, so that
    /// replacing this lane's sample cannot pull the audio out from under a
    /// note already playing it: the newest render for a Stretch lane that
    /// has one, the chop otherwise ([`Lane::playing_data`]).
    pub sample: Shared<SampleData>,
    /// Whether [`sample`](Self::sample) is the lane's time-stretch render
    /// rather than its chop ([`Lane::plays_render`]): a new voice is
    /// speed-corrected against the tempo only then. Structural: a render
    /// landing republishes.
    pub rendered: bool,
    /// Whether a note-on for this lane fades out the lane's own sounding
    /// voices. Structural: a toggle, not a drag, so changing it republishes.
    pub cut_itself: bool,
    /// The output this lane's voices render into, 0 ([`OUTPUT_MAIN`]) to
    /// [`OUTPUT_MAX`]. Structural.
    pub output: u8,
    /// How the lane's length follows the tempo. Structural.
    pub time_mode: TimeMode,
    /// The lane's [`Length`] in beats: 0.0 for [`Length::AsCut`], else
    /// [`NoteLength::beats`]. Structural.
    pub length_beats: f32,
    /// The lane's volume in dB, as the bits of an `f32`.
    ///
    /// An atomic and not a plain field because volume is continuous: a
    /// knob drag writes it sixty times a second, and those writes should
    /// land in the snapshot the audio thread already has rather than build
    /// a new lane list each time.
    volume_db: AtomicU32,
    /// The lane's pan as the bits of an `f32`, atomic for the reason
    /// [`volume_db`](Self::volume_db) is.
    pan: AtomicU32,
    /// The lane's attack in ms as `f32` bits — atomic, like the next three,
    /// because a panel drag changes it and must not republish per frame.
    attack_ms: AtomicU32,
    /// The lane's Gate-mode release in ms, as `f32` bits.
    release_ms: AtomicU32,
    /// The lane's fine tempo trim in percent, as `f32` bits.
    fine_pct: AtomicU32,
    /// The lane's pitch in semitones, as `f32` bits.
    pitch_st: AtomicU32,
    /// How many voices this lane has started, counted up by the audio
    /// thread for the GUI's activity lights — see
    /// [`activity`](Self::activity).
    activity: AtomicU32,
}

impl PublishedLane {
    /// The published form of a lane, or `None` if it has no sample and so
    /// nothing for the audio thread to play.
    ///
    /// `previous` is the snapshot this one replaces, and the lane's activity
    /// counter is carried over from it: the count is the GUI's clock for the
    /// lights, so letting it restart at zero whenever a writer republishes
    /// would flash every light in the list the moment a user changed one
    /// lane's note.
    fn of(lane: &Lane, previous: Option<&PublishedLanes>) -> Option<Self> {
        let sample = lane.sample.as_ref()?;
        let activity = previous
            .and_then(|previous| previous.lane(lane.id))
            .map_or(0, PublishedLane::activity);
        Some(Self {
            id: lane.id,
            note: lane.note,
            choke: lane.choke,
            mode: lane.mode,
            sample: lane.playing_data().unwrap_or(&sample.data).clone(),
            rendered: lane.plays_render(),
            cut_itself: lane.cut_itself,
            output: lane.output,
            time_mode: lane.time_mode,
            length_beats: length_beats(lane.length),
            volume_db: AtomicU32::new(lane.volume_db.to_bits()),
            pan: AtomicU32::new(lane.pan.to_bits()),
            attack_ms: AtomicU32::new(lane.attack_ms.to_bits()),
            release_ms: AtomicU32::new(lane.release_ms.to_bits()),
            fine_pct: AtomicU32::new(lane.fine_pct.to_bits()),
            pitch_st: AtomicU32::new(lane.pitch_st.to_bits()),
            activity: AtomicU32::new(activity),
        })
    }

    /// The lane's volume in dB, [`LANE_VOLUME_DB_MIN`]..=
    /// [`LANE_VOLUME_DB_MAX`] — read on the audio thread when a voice
    /// starts.
    ///
    /// Relaxed, like every atomic on this path: the value stands alone,
    /// there is nothing for it to be ordered against, and a note that
    /// starts one block before a knob's last move is inaudible.
    pub fn volume_db(&self) -> f32 {
        load_f32(&self.volume_db)
    }

    /// The lane's pan, [`LANE_PAN_MIN`]..=[`LANE_PAN_MAX`], read like
    /// [`volume_db`](Self::volume_db).
    pub fn pan(&self) -> f32 {
        load_f32(&self.pan)
    }

    /// The lane's attack in ms, [`ATTACK_MS_MIN`]..=[`ATTACK_MS_MAX`] — read
    /// once at voice start. Relaxed, as [`volume_db`](Self::volume_db) is.
    pub fn attack_ms(&self) -> f32 {
        load_f32(&self.attack_ms)
    }

    /// The lane's Gate-mode release in ms,
    /// [`RELEASE_MS_MIN`]..=[`RELEASE_MS_MAX`], read like
    /// [`attack_ms`](Self::attack_ms).
    pub fn release_ms(&self) -> f32 {
        load_f32(&self.release_ms)
    }

    /// The lane's fine tempo trim in percent, [`FINE_MIN`]..=[`FINE_MAX`],
    /// read like [`attack_ms`](Self::attack_ms).
    pub fn fine_pct(&self) -> f32 {
        load_f32(&self.fine_pct)
    }

    /// The lane's pitch in semitones, [`PITCH_MIN`]..=[`PITCH_MAX`], read
    /// like [`attack_ms`](Self::attack_ms).
    pub fn pitch_st(&self) -> f32 {
        load_f32(&self.pitch_st)
    }

    /// How many voices this lane has started since the instance opened, for
    /// the GUI to light the lane's colour dot with.
    ///
    /// Not a number anything should read as a quantity: the GUI keeps the
    /// value it last saw and compares, so all that matters is that it
    /// *changes* when the lane plays. Which is also why the wrap at
    /// `u32::MAX` is no concern — a lane struck sixty times a second would
    /// take two years to reach it, and the GUI would see the wrap as one
    /// more hit.
    ///
    /// Relaxed, like every atomic on this path
    /// ([`volume_db`](Self::volume_db)): a light that brightens one frame
    /// late is a light that brightened.
    pub fn activity(&self) -> u32 {
        self.activity.load(Ordering::Relaxed)
    }

    /// Count a voice this lane has just started — the engine's one write
    /// here, from [`crate::engine::Engine::run_block`].
    ///
    /// Wrapping, and for the reason [`activity`](Self::activity) gives: the
    /// counter is a marker that moved, not a tally anyone adds up. An
    /// atomic add and nothing else, which is what lets the audio thread do
    /// it (the real-time rule: no allocation, no lock).
    pub fn mark_activity(&self) {
        self.activity.fetch_add(1, Ordering::Relaxed);
    }

    /// Store a volume the writer has already clamped
    /// ([`Model::set_volume_db`] is the only caller).
    fn set_volume_db(&self, volume_db: f32) {
        store_f32(&self.volume_db, volume_db);
    }

    /// Store a pan the writer has already clamped
    /// ([`Model::set_pan`] is the only caller).
    fn set_pan(&self, pan: f32) {
        store_f32(&self.pan, pan);
    }

    /// Store an attack [`Model::set_attack_ms`] has already clamped.
    fn set_attack_ms(&self, ms: f32) {
        store_f32(&self.attack_ms, ms);
    }

    /// Store a release [`Model::set_release_ms`] has already clamped.
    fn set_release_ms(&self, ms: f32) {
        store_f32(&self.release_ms, ms);
    }

    /// Store a fine trim [`Model::set_fine_pct`] has already clamped.
    fn set_fine_pct(&self, pct: f32) {
        store_f32(&self.fine_pct, pct);
    }

    /// Store a pitch [`Model::set_pitch_st`] has already clamped.
    fn set_pitch_st(&self, st: f32) {
        store_f32(&self.pitch_st, st);
    }
}

/// A [`Length`] as the beats [`PublishedLane::length_beats`] carries: 0.0
/// for [`Length::AsCut`] (no musical length — the chop plays as it is).
fn length_beats(length: Length) -> f32 {
    match length {
        Length::AsCut => 0.0,
        Length::Beats(note) => note.beats() as f32,
    }
}

/// Everything the audio thread needs from the model, as one immutable
/// snapshot.
///
/// Built by a writer, swapped into the [`PublishedCell`] and read once per
/// block by the engine. Immutable is the whole point: the audio thread takes
/// no lock and allocates nothing, and the list it is holding cannot change
/// under it halfway through a block — a writer builds the next one instead,
/// and the one it replaces lives until its last reader lets go
/// ([`Model::collect`] frees it then, on a writer or the editor thread,
/// never on the audio thread).
///
/// The exceptions are the atomics, which exist so that the controls a user
/// drags do not rebuild anything: a lane's volume, pan, attack, release,
/// fine and pitch ([`PublishedLane`]) and the "bitstep Sample pad" switch.
pub struct PublishedLanes {
    /// The playable lanes in display order — the lanes of the model that
    /// have a sample, and only those.
    lanes: Vec<PublishedLane>,
    /// The "bitstep Sample pad" switch.
    sample_pad_switch: AtomicBool,
}

impl PublishedLanes {
    /// The snapshot for these lanes and this switch, replacing `previous`.
    ///
    /// Allocates (a `Vec` and, around it, a `Shared`): a writer-thread
    /// job, which is exactly why the audio thread is given a finished
    /// snapshot rather than anything it has to assemble itself.
    ///
    /// `previous` is only read for what has to outlive a republish — the
    /// lanes' activity counters ([`PublishedLane::of`]). `None` where there
    /// is nothing to carry: the first snapshot of an instance, and the
    /// empty one a closing instance leaves behind. A bump the audio thread
    /// lands between the read here and the swap below is lost, which costs
    /// one light one flash at the same instant its user was editing the
    /// lane — the alternative is a counter outside the snapshot, which is a
    /// second lifetime to manage for a cosmetic guarantee.
    fn build(lanes: &[Lane], sample_pad_switch: bool, previous: Option<&PublishedLanes>) -> Self {
        Self {
            lanes: lanes.iter().filter_map(|lane| PublishedLane::of(lane, previous)).collect(),
            sample_pad_switch: AtomicBool::new(sample_pad_switch),
        }
    }

    /// The playable lanes in display order: what a note-on walks to start a
    /// voice in every lane on its key. Lanes without a sample are not here
    /// at all.
    pub fn lanes(&self) -> &[PublishedLane] {
        &self.lanes
    }

    /// The published lane with this id, or `None` if it has no sample or no
    /// longer exists.
    ///
    /// A scan, which is what the writers' atomic stores and the GUI's
    /// lights want (one lane, by id, off the audio thread); the engine
    /// matches notes by walking [`lanes`](Self::lanes) instead.
    pub fn lane(&self, id: u32) -> Option<&PublishedLane> {
        self.lanes.iter().find(|lane| lane.id == id)
    }

    /// Whether the "bitstep Sample pad" switch is on, read per note-on on
    /// the audio thread.
    pub fn sample_pad_switch(&self) -> bool {
        self.sample_pad_switch.load(Ordering::Relaxed)
    }

    /// Flip the switch in place ([`Model::set_sample_pad_switch`] is the
    /// only caller): an instance-wide toggle is no reason to rebuild the
    /// lane list.
    fn set_sample_pad_switch(&self, on: bool) {
        self.sample_pad_switch.store(on, Ordering::Relaxed);
    }
}

// `Shared` has no `Debug` (and 64 MiB of audio has no business in a test
// failure), so the published side prints itself: the lane fields as they
// are, the sample as its shape.
impl std::fmt::Debug for PublishedLane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedLane")
            .field("id", &self.id)
            .field("note", &self.note)
            .field("choke", &self.choke)
            .field("mode", &self.mode)
            .field("volume_db", &self.volume_db())
            .field("pan", &self.pan())
            .field("cut_itself", &self.cut_itself)
            .field("output", &self.output)
            .field("time_mode", &self.time_mode)
            .field("length_beats", &self.length_beats)
            .field("rendered", &self.rendered)
            .field("attack_ms", &self.attack_ms())
            .field("release_ms", &self.release_ms())
            .field("fine_pct", &self.fine_pct())
            .field("pitch_st", &self.pitch_st())
            .field("activity", &self.activity())
            .field("rate", &self.sample.rate)
            .field("channels", &self.sample.channels)
            .field("frames", &self.sample.frame_count())
            .finish()
    }
}

impl std::fmt::Debug for PublishedLanes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublishedLanes")
            .field("lanes", &self.lanes)
            .field("sample_pad_switch", &self.sample_pad_switch())
            .finish()
    }
}

/// The cell the audio thread reads the published lanes out of.
///
/// Shared through an `Arc` rather than reached through the model, because
/// the model sits behind the writer mutex and the audio thread must never
/// take it: a block starts with one [`get`](Self::get) through a clone of
/// this `Arc`, and the audio thread touches nothing else of the model's.
///
/// `basedrop::SharedCell` is what makes that safe. The swap is lock-free,
/// a reader's `get` only bumps a refcount, and the snapshot it hands back
/// stays alive as long as the reader holds it even after a writer has
/// replaced it — so the writer never waits on the audio thread and the
/// audio thread never frees anything.
pub struct PublishedCell {
    cell: SharedCell<PublishedLanes>,
}

impl PublishedCell {
    /// The current snapshot: called **once per block** on the audio thread,
    /// and by the writers that only need to store an atomic into it.
    ///
    /// Takes no lock and allocates nothing, as the real-time rule
    /// requires. The handle it returns keeps that snapshot alive for as
    /// long as it is held, so one block renders against one list however
    /// many times the GUI publishes while it runs.
    pub fn get(&self) -> Shared<PublishedLanes> {
        self.cell.get()
    }
}

/// The editor's wake-up call, as the model holds it.
///
/// A plain closure rather than nice-plug-egui's `RepaintNotifier` itself,
/// because this module imports no framework type: [`crate::editor::create`]
/// wraps the notifier it already holds in one of these, and the model's
/// tests pass a counter instead.
///
/// `Send + Sync` because the writer that rings it is whichever thread was
/// writing — the loader's worker, in practice — and the ring must not
/// block: it is made under the writer mutex, and the notifier the editor
/// installs is one relaxed atomic store.
pub type RepaintHook = Arc<dyn Fn() + Send + Sync>;

/// The model as an undo entry: the lanes and the instance switch.
///
/// The persistent part of the model and nothing else — the fields a project
/// snapshot holds, which is what lets an undo step be the same kind of
/// change as a project load. It keeps whole [`Lane`]s rather than a second
/// list of lane fields, so that a field added to a lane cannot be forgotten
/// here, and [`Model::capture`] blanks the ones that are not persistent
/// ([`Lane::error`], [`Lane::loading`] and the render state, which
/// `apply_state` carries over from the live lane): those describe this
/// moment of this session — a decode still running, a message about a file
/// that would not open — and undoing a rename has no business reviving
/// either of them.
///
/// Cheap to make and cheap to hold: the strings are short and a
/// [`SampleRef`] is two refcount bumps, not a copy of any audio. What an
/// entry does do is keep that audio *alive* — which is precisely why undoing
/// a cleared lane brings its sample back without decoding anything, and why
/// the stacks are bounded ([`UNDO_DEPTH`]).
#[derive(Clone, Debug)]
struct ModelState {
    /// The lanes in display order, ids included (see [`Model::undo`]: an
    /// undo entry comes from this instance, so its ids are this instance's
    /// and are restored rather than reassigned).
    lanes: Vec<Lane>,
    /// The "bitstep Sample pad" switch as it was.
    sample_pad_switch: bool,
    /// The chop editor's selection as it was, and which source it was on
    /// ([`ChopMark`]) — so Ctrl+Z steps back over selection gestures in the
    /// one history with everything else (the user's, 2026-11-16).
    chop: ChopMark,
}

/// The chop editor's selection in an undo entry: the selection, and the
/// source it was made on.
///
/// The source is named by [`Model::chop_serial`], which every open, landing
/// and close of a source moves on. A selection only means something on the
/// source it was made on, so an entry from another source neither counts as
/// a change ([`chop_matches`]) nor is restored ([`Model::apply_state`]) —
/// and opening a source is not a step to undo, it moves the baseline's mark
/// along instead ([`Model::rebase_chop`]).
#[derive(Clone, Copy, Debug, PartialEq)]
struct ChopMark {
    serial: u64,
    selection: Option<Selection>,
}

/// The lane list, and the only way anything changes it.
///
/// One `Model` per plugin instance, holding the lanes a user sees plus the
/// instance-wide "bitstep Sample pad" switch. Every mutation in the plugin
/// is a method here — the GUI's gestures, the background loader's results
/// and a project load all write through these — so there is exactly one
/// place where a value from outside is clamped, and exactly one place that
/// hands the result to the audio thread (every writer either publishes a
/// fresh snapshot or stores an atomic into the one that is published, and
/// then collects).
///
/// Lanes are named by [`Lane::id`], never by index: an id survives a
/// reorder, the deletion of a neighbour and the wait for a decode, so a
/// gesture or a loader result that arrives late lands on the lane it meant
/// or on no lane at all. Every method naming a lane that is gone does
/// nothing and says so in what it returns — nothing here panics, and
/// nothing here guesses at which lane was meant.
///
/// None of those writers touches the undo stacks. History is kept
/// by gesture, not by mutation: whoever finished a gesture calls
/// [`commit`](Model::commit), and the model works out from the state it
/// kept whether anything actually changed.
pub struct Model {
    /// The lanes in display order (order is display only — a reorder moves
    /// no notes).
    lanes: Vec<Lane>,
    /// The id the next lane created will take. Only ever increases, so an
    /// id is never reused within the instance.
    next_id: u32,
    /// The note [`Model::add_lane`] counted up from last, so that adding
    /// lanes walks up the keyboard one note per lane the way the eight
    /// defaults do.
    last_added_note: u8,
    /// The "bitstep Sample pad" switch: instance-wide, saved with the
    /// project, default off.
    sample_pad_switch: bool,
    /// The state as of the last committed gesture.
    ///
    /// What [`Model::commit`] compares the lanes against to decide whether
    /// a gesture changed anything, and therefore the entry it pushes when
    /// one did. Kept up to date by every commit, undo and redo, so the
    /// writers in between can stay ignorant of history.
    baseline: ModelState,
    /// The states to step back through, oldest first: at most
    /// [`UNDO_DEPTH`] of them, the newest at the back.
    undo_stack: VecDeque<ModelState>,
    /// The states undoing has stepped out of, to step forward into again.
    /// Cleared by any new edit, and never deeper than the undo
    /// stack it is fed from.
    redo_stack: VecDeque<ModelState>,
    /// Where the audio thread reads its lanes from, and where every writer
    /// here leaves them.
    ///
    /// An `Arc`, because this is the one part of the model that is reached
    /// from outside the writer mutex: the plugin clones it once at setup and
    /// the audio thread holds that clone for the life of the instance.
    published: Arc<PublishedCell>,
    /// What every published snapshot, every lane sample and every sample's
    /// file bytes are allocated against — and the only thing that frees
    /// them.
    ///
    /// It lives here, inside the model, so that it is behind the writer
    /// mutex with everything else: a loader result, a GUI gesture, a
    /// project load and the editor's per-frame collect are then serialized
    /// by the one lock, and the audio thread — which must never free
    /// anything — does not hold it.
    collector: Collector,
    /// A handle on [`collector`](Self::collector) to allocate against:
    /// every snapshot published below comes from it, and so does every
    /// [`SampleRef`] the loader builds ([`Model::handle`]).
    handle: Handle,
    /// How a writer that is not the GUI asks the window to draw itself
    /// again ([`Model::set_repaint_hook`], [`Model::request_repaint`]).
    ///
    /// `None` until an editor is built, and left in place when its window
    /// closes: ringing it then is a store to an atomic nobody reads, which
    /// is cheaper than teaching every writer whether a window is open.
    repaint_hook: Option<RepaintHook>,
    /// The chop editor's state: a workbench, not the project's content, so
    /// never published to the audio thread, and outside undo but for its
    /// selection ([`ChopMark`]).
    chop: ChopEditor,
    /// Which source the chop editor holds, as far as undo is concerned
    /// ([`ChopMark`]): moved on by every open, landing and close.
    chop_serial: u64,
    /// The host tempo: the instance's one [`TempoCell`], outside the writer
    /// mutex like [`Self::published`] — `process()` stores into it and the
    /// engine, the editor and the render worker read it through clones of
    /// this `Arc`.
    tempo: Arc<TempoCell>,
}

// `Collector` and `SharedCell` have no `Debug`, and nothing in either is
// what a failing test wants to read anyway; the lanes are, and so is the
// size of what the collector is still holding.
impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model")
            .field("lanes", &self.lanes)
            .field("next_id", &self.next_id)
            .field("last_added_note", &self.last_added_note)
            .field("sample_pad_switch", &self.sample_pad_switch)
            .field("undo_depth", &self.undo_stack.len())
            .field("redo_depth", &self.redo_stack.len())
            .field("published", &*self.published.get())
            .field("allocs", &self.alloc_count())
            // The hook is a closure, which has nothing to print; whether
            // an editor has installed one is the readable part.
            .field("repaint_hook", &self.repaint_hook.is_some())
            .field("chop", &self.chop)
            .finish()
    }
}

impl Drop for Model {
    /// Let the lanes, the history and the published snapshot go, then give
    /// the collector one last run, so closing a plugin instance hands its
    /// decoded audio back — up to [`crate::sample::MAX_SAMPLE_BYTES`] per
    /// lane — instead of leaving it to the process.
    ///
    /// The undo stacks go first and for the same reason the lanes do: an
    /// entry holds a [`SampleRef`], so a collect that left a hundred of them
    /// standing would reclaim none of the audio they name.
    ///
    /// Best effort by nature: a snapshot the audio thread or the editor is
    /// still holding cannot be freed from here, and basedrop keeps the few
    /// bytes of its own queue alive while any handle to the collector
    /// exists (`Collector::try_cleanup` wants no handles and no
    /// allocations, which an `Arc` the audio side still holds cannot
    /// promise). What this does reclaim reliably is the audio of an
    /// instance nothing is playing any more, which is all of the size.
    fn drop(&mut self) {
        self.lanes.clear();
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.baseline.lanes.clear();
        // The open source's audio is the collector's to free too.
        self.chop.source = None;
        let empty = PublishedLanes::build(&self.lanes, self.sample_pad_switch, None);
        self.published.cell.set(Shared::new(&self.handle, empty));
        self.collector.collect();
    }
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    /// A fresh instance's model: the eight default lanes
    /// ([`default_lanes`]) and the Sample pad switch off.
    ///
    /// Brings its own `basedrop::Collector` — one per instance, as the
    /// samples and snapshots it frees belong to one instance — and
    /// publishes straight away, so the audio thread has a snapshot to read
    /// from the first block even if nothing has been written yet. That
    /// first snapshot is empty: eight lanes with no samples have nothing
    /// to play.
    pub fn new() -> Self {
        let lanes = default_lanes();
        let collector = Collector::new();
        let handle = collector.handle();
        let published = PublishedLanes::build(&lanes, SAMPLE_PAD_SWITCH_DEFAULT, None);

        Self {
            next_id: lanes.len() as u32,
            last_added_note: note_to_continue_from(&lanes),
            // The baseline a first gesture is measured against: the state a
            // user opened. A plain clone is already what `capture` would
            // make of it — a default lane carries no message and no pending
            // decode.
            baseline: ModelState {
                lanes: lanes.clone(),
                sample_pad_switch: SAMPLE_PAD_SWITCH_DEFAULT,
                chop: ChopMark { serial: 0, selection: None },
            },
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            lanes,
            sample_pad_switch: SAMPLE_PAD_SWITCH_DEFAULT,
            published: Arc::new(PublishedCell {
                cell: SharedCell::new(Shared::new(&handle, published)),
            }),
            collector,
            handle,
            // Nothing to wake until the host opens the window; a headless
            // restore is the case this has to be fine for.
            repaint_hook: None,
            chop: ChopEditor::default(),
            chop_serial: 0,
            tempo: Arc::new(TempoCell::new()),
        }
    }

    /// The cell the audio thread reads its lanes out of: clone the `Arc`
    /// once when the instance is built and keep it outside the writer
    /// mutex.
    pub fn published(&self) -> &Arc<PublishedCell> {
        &self.published
    }

    /// The instance's tempo cell: clone the `Arc` once when the instance is
    /// built, as with [`published`](Self::published), and read it without
    /// the lock.
    pub fn tempo(&self) -> &Arc<TempoCell> {
        &self.tempo
    }

    /// The collector handle this instance's samples are allocated against,
    /// for the loader to build its [`SampleRef`]s with.
    ///
    /// Every allocation that the audio thread might end up holding has to
    /// come from here, or nothing would ever free it on the right side.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Free the snapshots and samples whose last reader has let go.
    ///
    /// Every writer below ends in one of these, so garbage is reclaimed by
    /// whoever makes it and a headless project restore of sixty samples
    /// cannot pile up sixty retired lane lists (writers collect after
    /// mutating, and this works with the editor closed). The editor also
    /// calls it once per frame, which is what finally frees a sample the
    /// audio thread was still playing when a writer replaced it.
    ///
    /// `&mut self`, which is basedrop's own requirement, and is the writer
    /// mutex doing its job: collection happens on exactly the threads that
    /// are allowed to free things.
    pub fn collect(&mut self) {
        self.collector.collect();
    }

    /// Install the way a writer wakes the window: called once by
    /// [`crate::editor::create`] with nice-plug-egui's `RepaintNotifier`
    /// behind it.
    ///
    /// One hook, replaced rather than added to, because there is one window
    /// per instance; an editor built for a second time over the same model
    /// (the host may close and reopen the window as often as it likes)
    /// installs the same notifier again.
    pub fn set_repaint_hook(&mut self, hook: RepaintHook) {
        self.repaint_hook = Some(hook);
    }

    /// Ask the window for a frame, if an editor ever installed a way to.
    ///
    /// For the writers a user is not watching: the loader applying a
    /// decoded sample writes from its own thread, and without this the lane
    /// would sit on "loading…" until the next input event moved the window
    /// — a dropped sample unseen on a window nobody is touching.
    ///
    /// Three things this is deliberately not. It is **not** for the GUI's
    /// own writers: egui is already drawing the frame that made the change.
    /// It is **not** reachable from the audio thread, which holds only
    /// [`published`](Self::published) and rings the same notifier itself
    /// (`crate::Chopdrop::process`). And it is **no** guarantee of a frame:
    /// with no window open there is nobody to draw one, which is why
    /// ringing has to be safe rather than meaningful.
    pub fn request_repaint(&self) {
        if let Some(hook) = &self.repaint_hook {
            hook();
        }
    }

    /// How many basedrop allocations this instance is still holding: the
    /// published snapshots, the lane samples and their file bytes.
    ///
    /// For the tests here and the concurrency stress test, which need to see
    /// that the collector really drains.
    pub fn alloc_count(&self) -> usize {
        self.collector.alloc_count()
    }

    /// The lanes in display order, for the GUI to draw and for a save to
    /// walk. Read-only: changing one means calling a writer method.
    pub fn lanes(&self) -> &[Lane] {
        &self.lanes
    }

    /// The lane with this id, or `None` if it has been deleted.
    ///
    /// The writer side's read-by-id, and so the tests' way of asking what a
    /// gesture did. The window does not use it: it draws rows and names
    /// them by position ([`lanes`](Self::lanes) plus
    /// [`index_of`](Self::index_of) when a gesture hands it an id), and the
    /// audio thread reads the published copy
    /// ([`PublishedLanes::lane`]) instead. Kept as the one place that turns
    /// an id into a lane without a caller spelling the scan out: every
    /// writer below needs it, and so does anything holding an id across a
    /// reorder.
    pub fn lane(&self, id: u32) -> Option<&Lane> {
        self.index_of(id).map(|index| &self.lanes[index])
    }

    /// Where the lane with this id sits in display order, for the GUI's
    /// drag-to-reorder (which thinks in positions but names lanes by id).
    pub fn index_of(&self, id: u32) -> Option<usize> {
        self.lanes.iter().position(|lane| lane.id == id)
    }

    /// Whether the "bitstep Sample pad" switch is on.
    pub fn sample_pad_switch(&self) -> bool {
        self.sample_pad_switch
    }

    /// How many bytes a save would embed right now: the original file bytes
    /// of every lane's sample, leaving out the ones too big to embed
    /// ([`SampleRef::embeds`]) because a save leaves those out too.
    ///
    /// Pre-base64 and pre-zstd, which is the figure
    /// [`crate::state::EMBED_WARN_BYTES`] is set in: what the state costs on
    /// the way out depends on the compressor, and this is the quantity the user can act on — the
    /// samples they have loaded.
    ///
    /// Counted over the lanes each time it is asked rather than kept in a
    /// field the writers add to and subtract from: at most [`MAX_LANES`]
    /// loads and additions, which no caller of this is in a hurry about
    /// (the editor asks once a frame, off the audio thread), and a total
    /// that is recomputed is a total that cannot be left stale by a writer
    /// that forgot it.
    ///
    /// Lanes sharing one sample are counted once per lane, because a save
    /// writes the bytes once per lane: duplicating a lane really does
    /// double what the project carries, even though it does not double what
    /// this session holds in memory.
    pub fn embedded_bytes(&self) -> usize {
        self.lanes
            .iter()
            .filter_map(|lane| lane.sample.as_ref())
            .filter(|sample| sample.embeds())
            .map(SampleRef::byte_len)
            .sum()
    }

    /// Everything this instance would save: the lanes with their settings,
    /// each lane's sample as the original file's bytes, and the switch.
    ///
    /// A read, not a writer — the host asks for it on its own schedule
    /// (through the plugin's `#[persist]` field) and it changes nothing.
    /// What it costs is the base64 of every embedded sample, so it is
    /// main-thread work and nothing on the audio path may call it.
    ///
    /// The session-only halves of a lane — its message and its "loading…"
    /// flag — are left out by [`Snapshot::of`]: a save describes the lanes,
    /// not this moment's attempt at a file.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            chop_editor: ChopEditorSnapshot::of(&self.chop),
            ..Snapshot::of(&self.lanes, self.sample_pad_switch)
        }
    }

    /// Add an empty lane at the end of the list, returning its id — or
    /// `None` if the list is already [`MAX_LANES`] long, which is refused
    /// silently (there is nothing a user could usefully do about it, and
    /// 128 lanes is every note a note source can send).
    ///
    /// The new lane continues the three progressions of the defaults:
    ///
    /// - **Name**: "Lane N" with N one past the current count, so a fresh
    ///   instance's first added lane is "Lane 9". Existing lanes are never
    ///   renumbered, so adding after a delete can repeat a name — a name is
    ///   a label, not an identity ([`Lane::id`] is that), and silently
    ///   renaming lanes a user may have named themselves would be worse.
    /// - **Note**: one above the last note *added*, clamped at 127, so
    ///   lanes keep walking up the keyboard even if the user has since
    ///   retuned one of them, and so that deleting lanes does not hand the
    ///   next one a note that is already taken. At 127 further lanes all
    ///   land on 127: lanes may share a note (they layer), so this is a
    ///   pile-up and not a failure. Only a wholesale replace
    ///   ([`Model::replace_lanes`]) restarts the count.
    /// - **Colour**: the next palette entry, wrapping after all ten
    ///   ([`LANE_PALETTE`]), taken from the lane count so that the ninth
    ///   lane gets the ninth colour.
    pub fn add_lane(&mut self) -> Option<u32> {
        if self.lanes.len() as u32 >= MAX_LANES {
            return None;
        }

        let name = format!("Lane {}", self.lanes.len() + 1);
        let id = self.push_new_lane(&name, None);
        self.republish();
        Some(id)
    }

    /// Append a lane named `name` holding `sample`, on the next note and
    /// colour of [`Model::add_lane`]'s progression, and return its id. The
    /// caller checks [`MAX_LANES`] and publishes.
    fn push_new_lane(&mut self, name: &str, sample: Option<SampleRef>) -> u32 {
        let id = self.take_id();
        let note = clamp_note(self.last_added_note.saturating_add(1));
        let colour = LANE_PALETTE[self.lanes.len() % LANE_PALETTE.len()];

        self.last_added_note = note;
        self.lanes.push(Lane { sample, ..Lane::new(id, name, note, colour) });
        id
    }

    /// Delete the lane with this id; `false` if there was no such lane.
    ///
    /// Deleting the last lane is allowed — an empty list is a legitimate
    /// state the GUI offers its way out of (the space below the list adds
    /// lanes). Voices already sounding from the deleted lane hold their own
    /// handle on its sample and play on.
    pub fn delete_lane(&mut self, id: u32) -> bool {
        match self.index_of(id) {
            Some(index) => {
                self.lanes.remove(index);
                self.republish();
                true
            }
            None => false,
        }
    }

    /// Copy the lane with this id, placing the copy directly below it, and
    /// return the copy's id. `None` if there is no such lane, or if the
    /// list is already [`MAX_LANES`] long.
    ///
    /// The copy keeps every setting **including the note** — two lanes on
    /// one note layer, which is what duplicating a lane is usually for (two
    /// samples, one pad) — and shares the one copy of the sample's bytes
    /// and audio rather than decoding or allocating anything.
    ///
    /// Its name gains " copy", trimmed to [`MAX_NAME_LEN`] characters; a
    /// name already that long therefore duplicates unchanged, which is the
    /// honest outcome of a hard limit on a label.
    ///
    /// What the copy does not inherit is [`Lane::loading`]: the pending job
    /// belongs to the original's id and its result will land there, so a
    /// copy that claimed to be loading would say so forever.
    pub fn duplicate_lane(&mut self, id: u32) -> Option<u32> {
        if self.lanes.len() as u32 >= MAX_LANES {
            return None;
        }

        let index = self.index_of(id)?;
        let mut copy = self.lanes[index].clone();
        copy.id = self.take_id();
        copy.name = truncate_name(&format!("{} copy", copy.name));
        copy.loading = false;
        // A finished render is as good for the copy as for the original; one
        // still in flight will land on the original's id, so ask again.
        if copy.render_pending {
            request_render_of(&mut copy);
        }

        let new_id = copy.id;
        self.lanes.insert(index + 1, copy);
        self.republish();
        Some(new_id)
    }

    /// Rename a lane, trimming to [`MAX_NAME_LEN`] characters on a
    /// character boundary. `false` if there is no such lane.
    pub fn rename(&mut self, id: u32, name: &str) -> bool {
        self.with_unpublished_lane(id, |lane| lane.name = truncate_name(name))
    }

    /// Move the lane at `from_index` to `to_index` in **display order**;
    /// `false` if `from_index` names no lane.
    ///
    /// Nothing but the order changes — notes, colours and samples stay with
    /// their lanes. `to_index` is clamped to the end of the list rather than
    /// rejected, because it arrives from a drag that can finish past the
    /// last row.
    pub fn reorder(&mut self, from_index: usize, to_index: usize) -> bool {
        if from_index >= self.lanes.len() {
            return false;
        }

        let to_index = to_index.min(self.lanes.len() - 1);
        if from_index != to_index {
            let lane = self.lanes.remove(from_index);
            self.lanes.insert(to_index, lane);
            self.republish();
        }
        true
    }

    /// Set which note plays a lane, clamped to 0–127.
    pub fn set_note(&mut self, id: u32, note: u8) -> bool {
        self.with_published_lane(id, |lane| lane.note = clamp_note(note))
    }

    /// Set a lane's colour, keeping only the low 24 bits (`0xRRGGBB`).
    pub fn set_colour(&mut self, id: u32, colour: u32) -> bool {
        self.with_unpublished_lane(id, |lane| lane.colour = clamp_colour(colour))
    }

    /// Set a lane's volume in dB, clamped to
    /// [`LANE_VOLUME_DB_MIN`]..=[`LANE_VOLUME_DB_MAX`] (NaN →
    /// [`LANE_VOLUME_DB_DEFAULT`]).
    ///
    /// Reaches the audio thread through the published lane's atomic rather
    /// than through a new lane list: this is a knob a user drags, and a drag
    /// must not build sixty snapshots a second. The lane keeps the value too
    /// — that is what a save and the next snapshot read.
    pub fn set_volume_db(&mut self, id: u32, volume_db: f32) -> bool {
        let volume_db = clamp_volume_db(volume_db);
        self.with_atomic_lane(
            id,
            |lane| lane.volume_db = volume_db,
            |published| published.set_volume_db(volume_db),
        )
    }

    /// Set a lane's pan, clamped to [`LANE_PAN_MIN`]..=[`LANE_PAN_MAX`]
    /// (NaN → [`LANE_PAN_DEFAULT`], centred). An atomic write, for the
    /// reason [`Model::set_volume_db`] gives.
    pub fn set_pan(&mut self, id: u32, pan: f32) -> bool {
        let pan = clamp_pan(pan);
        self.with_atomic_lane(id, |lane| lane.pan = pan, |published| published.set_pan(pan))
    }

    /// Set a lane's choke group, clamped to [`CHOKE_NONE`]..=[`CHOKE_GROUPS`].
    pub fn set_choke(&mut self, id: u32, choke: u8) -> bool {
        self.with_published_lane(id, |lane| lane.choke = clamp_choke(choke))
    }

    /// Set whether a lane's voices end with the sample or with the note.
    pub fn set_mode(&mut self, id: u32, mode: LaneMode) -> bool {
        self.with_published_lane(id, |lane| lane.mode = mode)
    }

    /// Set a lane's attack (fade-in from note-on) in ms, clamped to
    /// [`ATTACK_MS_MIN`]..=[`ATTACK_MS_MAX`] (NaN → [`ATTACK_MS_DEFAULT`]).
    ///
    /// A continuous value a panel drag changes, so it is not structural: the
    /// lane keeps it and the audio side picks it up through its atomic,
    /// never a new lane list.
    pub fn set_attack_ms(&mut self, id: u32, ms: f32) -> bool {
        let ms = clamp_attack_ms(ms);
        self.with_atomic_lane(id, |lane| lane.attack_ms = ms, |p| p.set_attack_ms(ms))
    }

    /// Set a lane's Gate-mode release in ms, clamped to
    /// [`RELEASE_MS_MIN`]..=[`RELEASE_MS_MAX`] (NaN →
    /// [`RELEASE_MS_DEFAULT`]). Continuous, as [`Model::set_attack_ms`] is.
    pub fn set_release_ms(&mut self, id: u32, ms: f32) -> bool {
        let ms = clamp_release_ms(ms);
        self.with_atomic_lane(id, |lane| lane.release_ms = ms, |p| p.set_release_ms(ms))
    }

    /// Set whether a note-on for the lane fades out its own sounding voices.
    pub fn set_cut_itself(&mut self, id: u32, cut_itself: bool) -> bool {
        self.with_published_lane(id, |lane| lane.cut_itself = cut_itself)
    }

    /// Set the output a lane renders into; anything above [`OUTPUT_MAX`]
    /// lands on [`OUTPUT_MAIN`] (see [`clamp_output`]).
    pub fn set_output(&mut self, id: u32, output: u8) -> bool {
        let output = clamp_output(output);
        self.with_published_lane(id, |lane| lane.output = output)
    }

    /// Set how a lane's length follows the tempo. Every [`TimeMode`] is
    /// valid, so there is nothing to clamp.
    pub fn set_time_mode(&mut self, id: u32, mode: TimeMode) -> bool {
        // Into Stretch: a render is wanted. Out of it: the render is useless
        // (an Off or Repitch lane plays its chop), and so is any in flight.
        self.with_published_lane(id, |lane| {
            lane.time_mode = mode;
            refresh_render(lane);
        })
    }

    /// Set the length a synced lane plays for. Every [`Length`] is valid,
    /// so there is nothing to clamp.
    pub fn set_length(&mut self, id: u32, length: Length) -> bool {
        self.with_published_lane(id, |lane| {
            lane.length = length;
            request_if_stretch(lane);
        })
    }

    /// Set a lane's fine tempo trim in percent, clamped to
    /// [`FINE_MIN`]..=[`FINE_MAX`] and stored to 0.1 steps (NaN →
    /// [`FINE_DEFAULT`]). Continuous, as [`Model::set_attack_ms`] is.
    pub fn set_fine_pct(&mut self, id: u32, pct: f32) -> bool {
        let pct = clamp_fine_pct(pct);
        let edit = |lane: &mut Lane| {
            lane.fine_pct = pct;
            request_if_stretch(lane);
        };
        self.with_atomic_lane(id, edit, |p| p.set_fine_pct(pct))
    }

    /// Set a lane's pitch in semitones, clamped to
    /// [`PITCH_MIN`]..=[`PITCH_MAX`] and stored to cents (NaN →
    /// [`PITCH_DEFAULT`]). Continuous, as [`Model::set_attack_ms`] is.
    pub fn set_pitch_st(&mut self, id: u32, st: f32) -> bool {
        let st = clamp_pitch_st(st);
        let edit = |lane: &mut Lane| {
            lane.pitch_st = st;
            request_if_stretch(lane);
        };
        self.with_atomic_lane(id, edit, |p| p.set_pitch_st(st))
    }

    /// Ask for a fresh render of a Stretch lane: bump its
    /// [`Lane::render_generation`], so any render still in flight lands
    /// stale, and mark it pending (when it has a sample to render) with its
    /// last error cleared. The panel's "render failed — click to retry"
    /// calls this; so do the Stretch lane's length, fine, pitch, time-mode
    /// and sample setters.
    ///
    /// `false` for an unknown id or a lane that is not in Stretch — an Off
    /// or Repitch lane plays its chop and has nothing to render. Nothing is
    /// republished: the lane keeps playing its current render until the new
    /// one lands.
    pub fn request_render(&mut self, id: u32) -> bool {
        let is_stretch = self.lane(id).is_some_and(|lane| lane.time_mode == TimeMode::Stretch);
        is_stretch && self.with_unpublished_lane(id, request_render_of)
    }

    /// Land a render from the worker.
    ///
    /// Ignored — `false` — when the lane is gone, is no longer in Stretch,
    /// has no sample, or `generation` is older than the lane's
    /// [`Lane::render_generation`]: a stale result must not replace a newer
    /// render, nor the chop of a lane that left Stretch. Otherwise the
    /// render becomes the lane's and is published (new notes play it;
    /// sounding voices keep the `Shared` they hold); the lane stops being
    /// pending only if this answers its newest request.
    ///
    /// Never an undo step: render state is outside what the undo history
    /// stores and compares, so the landing is in effect rebased — it
    /// neither commits nor swallows a gesture still in progress (which a
    /// literal `rebase()` here would do, folding the drag into the
    /// baseline), and an undo carries the render over rather than
    /// reverting it (see `apply_state`).
    pub fn set_render(&mut self, id: u32, generation: u64, data: SampleData) -> bool {
        let Some(index) = self.index_of(id) else {
            return false;
        };
        let lane = &self.lanes[index];
        let live = lane.time_mode == TimeMode::Stretch && lane.sample.is_some();
        if !live || generation < lane.render_generation {
            return false;
        }

        let target_secs = data.frame_count() as f64 / f64::from(data.rate.max(1));
        let render = Render { data: Shared::new(&self.handle, data), target_secs, generation };
        let lane = &mut self.lanes[index];
        lane.render = Some(render);
        if generation == lane.render_generation {
            lane.render_pending = false;
            lane.render_error = None;
        }
        self.republish();
        true
    }

    /// Report a render that failed. The lane keeps its previous render, or
    /// its chop, stops being pending and shows `message`. Ignored, as in
    /// [`set_render`](Self::set_render), for a stale generation or a lane
    /// that is gone or not in Stretch.
    pub fn set_render_error(&mut self, id: u32, generation: u64, message: &str) -> bool {
        let current = self.lane(id).is_some_and(|lane| {
            lane.time_mode == TimeMode::Stretch && generation >= lane.render_generation
        });
        current
            && self.with_unpublished_lane(id, |lane| {
                lane.render_pending = false;
                lane.render_error = Some(message.to_owned());
            })
    }

    /// "Give each its own output": walk the lanes named in `ids` in
    /// **display order** (not the order of `ids`, not id order) and give
    /// each the lowest output in `1..=`[`OUTPUT_MAX`] that no other lane
    /// uses. A lane already alone on an aux output keeps it. Returns how
    /// many lanes could **not** be placed — no free output left — which keep
    /// their current output.
    ///
    /// Ids that name no lane are ignored, and a repeat counts once. Like the
    /// other setters this is one gesture: the caller commits once after it,
    /// so the whole assignment is a single undo step. Publishes once, and
    /// only if some lane moved.
    pub fn assign_own_outputs(&mut self, ids: &[u32]) -> usize {
        let mut users = [0usize; OUTPUT_MAX as usize + 1];
        for lane in &self.lanes {
            users[lane.output as usize] += 1;
        }

        let mut unplaced = 0;
        let mut moved = false;
        for lane in self.lanes.iter_mut().filter(|lane| ids.contains(&lane.id)) {
            let current = lane.output as usize;
            if current != OUTPUT_MAIN as usize && users[current] == 1 {
                continue;
            }
            match (1..users.len()).find(|&output| users[output] == 0) {
                Some(output) => {
                    users[current] -= 1;
                    users[output] += 1;
                    lane.output = output as u8;
                    moved = true;
                }
                None => unplaced += 1,
            }
        }

        if moved {
            self.republish();
        }
        unplaced
    }

    /// Give a lane its sample: a load that worked, so the lane is no longer
    /// loading and whatever went wrong before no longer applies.
    ///
    /// Called by the loader worker itself for a drop, a reload-from-file or
    /// a restored project — which is why a late result for a deleted lane
    /// simply returns `false` here instead of reviving or mis-landing on
    /// anything.
    pub fn set_sample(&mut self, id: u32, sample: SampleRef) -> bool {
        self.with_published_lane(id, |lane| {
            lane.sample = Some(sample);
            lane.error = None;
            lane.loading = false;
            // A render of the old chop is the wrong audio.
            lane.render = None;
            refresh_render(lane);
        })
    }

    /// Empty a lane: no sample, no message, not loading. The lane itself
    /// stays, with its name, note and settings — that is the difference
    /// between clearing a lane and deleting it.
    pub fn clear_sample(&mut self, id: u32) -> bool {
        self.with_published_lane(id, |lane| {
            lane.sample = None;
            lane.error = None;
            lane.loading = false;
            lane.render = None;
            refresh_render(lane);
        })
    }

    /// Record what went wrong for this lane, in the words the user sees, and
    /// end its loading state — a message is how a job finishes when it does
    /// not finish with a sample.
    ///
    /// The lane keeps whatever sample it had: a failed reload-from-file
    /// leaves the sample that is already playing alone.
    pub fn set_error(&mut self, id: u32, message: &str) -> bool {
        self.with_unpublished_lane(id, |lane| {
            lane.error = Some(message.to_owned());
            lane.loading = false;
        })
    }

    /// Mark a lane as waiting for a decode, or not (set when the job is
    /// queued, cleared by its result). Neither saved nor published to the
    /// audio thread.
    pub fn set_loading(&mut self, id: u32, loading: bool) -> bool {
        self.with_unpublished_lane(id, |lane| lane.loading = loading)
    }

    /// Cut `selection` from the open source onto a new lane and return its
    /// id.
    ///
    /// The lane plays [`chop::cut`]'s frames directly; its bytes (what a
    /// save embeds) are those frames as a 24-bit WAV
    /// ([`crate::sample::encode_wav24`]), so a restored project decodes to
    /// the same audio within 24-bit rounding. The sample keeps the source's
    /// path and a [`ChopOrigin`] with the clamped frames. The name is
    /// [`chop::chop_name`] counting the lanes already cut from this source;
    /// note and colour follow [`Model::add_lane`]'s progression and every
    /// other setting is a new lane's default ([`Lane::new`]).
    ///
    /// `None` — and nothing changed — with no source open, a selection that
    /// is empty once clamped to the source, or the list at [`MAX_LANES`].
    ///
    /// One undo step once the caller commits the gesture, like any writer
    /// ([`Model::commit`]).
    pub fn add_chop_lane(&mut self, selection: Selection) -> Option<u32> {
        if self.lanes.len() as u32 >= MAX_LANES {
            return None;
        }
        let sample = self.chop_sample(selection)?;
        let source_path = &sample.path;
        let existing = self
            .lanes
            .iter()
            .filter(|lane| {
                lane.sample
                    .as_ref()
                    .and_then(|sample| sample.origin.as_ref())
                    .is_some_and(|origin| &origin.source_path == source_path)
            })
            .count();
        let name = chop::chop_name(source_path, existing);

        let id = self.push_new_lane(&name, Some(sample));
        self.republish();
        Some(id)
    }

    /// Cut `selection` from the open source onto an existing lane, as
    /// [`Model::add_chop_lane`] builds it, replacing that lane's sample: the
    /// lane keeps its name, note, colour and every setting. `false` — and
    /// nothing changed — for no such lane, no source open or an empty
    /// selection. One undo step once the caller commits.
    pub fn replace_with_chop(&mut self, id: u32, selection: Selection) -> bool {
        if self.index_of(id).is_none() {
            return false;
        }
        match self.chop_sample(selection) {
            Some(sample) => self.set_sample(id, sample),
            None => false,
        }
    }

    /// The chop of `selection` from the open source as a lane's sample, or
    /// `None` with no source or an empty selection once clamped.
    fn chop_sample(&self, selection: Selection) -> Option<SampleRef> {
        let source = self.chop.source.as_ref()?;
        let end = selection.end.min(source.total());
        let start = selection.start.min(end);
        if start == end {
            return None;
        }
        let data = chop::cut(&source.data, start, end);
        let bytes = crate::sample::encode_wav24(&data);
        let origin =
            ChopOrigin { source_path: source.path.clone(), start_frame: start, end_frame: end };
        Some(SampleRef::with_origin(
            self.handle(),
            Path::new(&source.path),
            bytes,
            data,
            Some(origin),
        ))
    }

    /// The chop editor's state, to draw from.
    pub fn chop(&self) -> &ChopEditor {
        &self.chop
    }

    /// Start opening a chop-editor source: remember the path, drop any open
    /// source, its selection and message, and say "loading…". The loader job
    /// itself is queued by [`crate::loader::Loader::open_source`], which
    /// calls this under the same lock. Not an undo step; publishes nothing.
    pub fn open_source(&mut self, path: &str) {
        let editor = &mut self.chop;
        editor.source = None;
        editor.source_path = Some(path.to_owned());
        editor.loading = true;
        editor.error = None;
        editor.selection = None;
        editor.pending_selection = None;
        editor.pending_view = None;
        editor.view = View::fit(0, editor.view_width_px);
        self.rebase_chop();
        self.collector.collect();
    }

    /// A new source (or none) in the chop editor: its selection is not an
    /// edit of the last one's, so the undo baseline's [`ChopMark`] moves on
    /// to it rather than the change becoming a step.
    fn rebase_chop(&mut self) {
        self.chop_serial += 1;
        self.baseline.chop = self.chop_mark();
    }

    /// The chop editor's selection, marked with its source, as an undo
    /// entry holds it.
    fn chop_mark(&self) -> ChopMark {
        ChopMark { serial: self.chop_serial, selection: self.chop.selection }
    }

    /// Ask for `[start, end)` to be selected (and framed) when the source
    /// being opened lands — the lane menu's "open source in editor" and a
    /// restored project. Ignored with no source path.
    pub fn request_selection(&mut self, start: u64, end: u64) -> bool {
        if self.chop.source_path.is_none() {
            return false;
        }
        self.chop.pending_selection = Some((start, end));
        true
    }

    /// The loader's result for a source: its audio and the peaks built on
    /// the loader thread. Lands only if `path` is still the one loading (a
    /// close or another open since drops it). The view fits the source,
    /// or frames a pending selection, which becomes the selection.
    pub fn set_source(&mut self, path: &str, data: SampleData, peaks: Peaks) -> bool {
        if !self.chop.loading || self.chop.source_path.as_deref() != Some(path) {
            return false;
        }
        let source = chop::Source {
            path: path.to_owned(),
            data: Shared::new(&self.handle, data),
            peaks: Arc::new(peaks),
        };
        let total = source.total();
        let editor = &mut self.chop;
        let width = editor.view_width_px;
        editor.selection = editor.pending_selection.take().map(|(start, end)| {
            let mut selection = Selection { start, end, active: chop::Edge::Start };
            selection.clamp(total);
            selection
        });
        editor.view = match (editor.pending_view.take(), editor.selection) {
            (Some(view), _) => view.clamped(total, width),
            (None, Some(selection)) => View::around(selection.start, selection.end, total, width),
            (None, None) => View::fit(total, width),
        };
        editor.source = Some(source);
        editor.loading = false;
        editor.error = None;
        self.rebase_chop();
        self.collector.collect();
        true
    }

    /// The loader could not open `path`: show `message` (the path stays,
    /// for the strip to name). Ignored unless `path` is still loading.
    ///
    /// A pending selection and view stay too, inert, so saving the project
    /// while its source is missing still remembers them; the next open or
    /// close drops them.
    pub fn source_failed(&mut self, path: &str, message: &str) -> bool {
        if !self.chop.loading || self.chop.source_path.as_deref() != Some(path) {
            return false;
        }
        let editor = &mut self.chop;
        editor.loading = false;
        editor.error = Some(message.to_owned());
        true
    }

    /// Close the source: no audio, path, message, selection or pending
    /// load (a result still in flight is dropped when it lands). Snap and
    /// the strip's size stay.
    pub fn close_source(&mut self) {
        let editor = &mut self.chop;
        editor.source = None;
        editor.source_path = None;
        editor.loading = false;
        editor.error = None;
        editor.selection = None;
        editor.pending_selection = None;
        editor.pending_view = None;
        editor.looping = false;
        editor.view = View::fit(0, editor.view_width_px);
        self.rebase_chop();
        self.collector.collect();
    }

    /// Set (clamped to the open source) or clear the selection. With no
    /// source there is nothing to select: `false`, and it stays `None`.
    pub fn set_selection(&mut self, selection: Option<Selection>) -> bool {
        let total = self.chop.total();
        if self.chop.source.is_none() {
            self.chop.selection = None;
            return selection.is_none();
        }
        self.chop.selection = selection.map(|mut selection| {
            selection.clamp(total);
            selection
        });
        true
    }

    /// Set the main view as drawn `width_px` wide, clamped to the source.
    pub fn set_view(&mut self, view: View, width_px: f32) {
        let width = if width_px.is_finite() && width_px >= 1.0 { width_px } else { 1.0 };
        let total = self.chop.total();
        self.chop.view_width_px = width;
        self.chop.view = view.clamped(total, width);
    }

    /// Snap edges to zero crossings, or not.
    pub fn set_snap(&mut self, on: bool) {
        self.chop.snap = on;
    }

    /// Loop the preview, or not.
    pub fn set_looping(&mut self, on: bool) {
        self.chop.looping = on;
    }

    /// The strip's collapsed flag and expanded height (clamped).
    pub fn set_strip(&mut self, collapsed: bool, height_px: f32) {
        self.chop.collapsed = collapsed;
        self.chop.height_px = chop::clamp_strip_height(height_px);
    }

    /// Turn the instance-wide "bitstep Sample pad" switch on or off.
    ///
    /// Another atomic in the published snapshot: a toggle is no reason to
    /// rebuild the lane list, and the audio thread reads it per note-on.
    pub fn set_sample_pad_switch(&mut self, on: bool) {
        self.sample_pad_switch = on;
        self.published.get().set_sample_pad_switch(on);
        self.collect();
    }

    /// Replace the whole model — the path a project load and an undo both
    /// take, and the one writer that accepts lanes built elsewhere, so it
    /// re-clamps every field of every one of them.
    ///
    /// What it does to the lanes it is given:
    ///
    /// - More than [`MAX_LANES`] of them: the excess is dropped, so a
    ///   corrupt file cannot ask for unbounded lanes.
    /// - Each gets a **fresh id** from this instance's counter, because ids
    ///   are per-instance and never saved; an id from another session (or
    ///   another lane of this one) must not be taken at its word.
    /// - Each is clamped field by field ([`Lane::clamp`]).
    /// - None is loading: no job of this instance's can be outstanding for
    ///   an id that has only just been handed out. Any message they carry
    ///   is kept — a load reports its own failures through it.
    ///
    /// The note progression continues from the highest note in the new
    /// list: after a wholesale replace there is no "last lane added" to
    /// count from, and the highest note is where a user would expect the
    /// next one to go. An empty list restarts at
    /// [`DEFAULT_FIRST_NOTE`].
    ///
    /// And the undo history goes: its entries describe lanes this
    /// instance no longer has, under ids it has already counted past, so
    /// stepping "back" into them would restore a state from the project
    /// that was replaced. The lanes that arrive are the new baseline.
    pub fn replace_lanes(&mut self, mut lanes: Vec<Lane>, sample_pad_switch: bool) {
        lanes.truncate(MAX_LANES as usize);

        for lane in &mut lanes {
            lane.id = self.take_id();
            lane.loading = false;
            lane.clamp();
            // Renders are never saved: rebuild them.
            lane.render = None;
            lane.render_generation = 0;
            lane.render_pending = false;
            lane.render_error = None;
            request_if_stretch(lane);
        }

        self.last_added_note = note_to_continue_from(&lanes);
        self.lanes = lanes;
        self.sample_pad_switch = sample_pad_switch;
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.baseline = self.capture();
        self.republish();
    }

    /// Load a project: replace the lanes with the snapshot's and report the
    /// samples that still have to be decoded, each paired with the **fresh
    /// id** of the lane it belongs to.
    ///
    /// The lanes land first and whole — clamped, published, playable — and
    /// the samples arrive afterwards, one `set_sample` at a time, as the
    /// loader works through what this returns. That is the order
    /// the host needs: `set` on a persistent field happens while the host
    /// waits, and decoding a project's worth of audio there would hang it.
    ///
    /// The ids are what make the hand-off safe: the loader's results come
    /// back to lanes by id, so a user who deletes or replaces a restored
    /// lane while its sample is still decoding gets a result that lands
    /// nowhere rather than on the wrong lane. A caller that wants the GUI to
    /// say "loading…" for them marks them itself ([`Model::set_loading`]) —
    /// this reports the work, it does not queue it.
    ///
    /// Lanes whose sample was already beyond restoring arrive carrying
    /// [`crate::state::SAMPLE_RESTORE_FAILED`] and appear in no pair here.
    pub fn load_snapshot(&mut self, snapshot: Snapshot) -> Vec<(u32, PendingSample)> {
        let Restore { lanes, sample_pad_switch, samples, chop_editor } = snapshot.into_restore();
        self.replace_lanes(lanes, sample_pad_switch);
        self.restore_chop(chop_editor);

        self.lanes
            .iter()
            .zip(samples)
            .filter_map(|(lane, pending)| pending.map(|pending| (lane.id, pending)))
            .collect()
    }

    /// Put the chop editor back as a project saved it (lanes first, so this
    /// runs after [`Model::replace_lanes`]): any open source is closed, the
    /// strip settings apply at once, and a saved source path is left
    /// "loading…" with its view and selection pending — the job itself is
    /// [`crate::loader::Loader::reopen_source`]'s, which the caller sends
    /// under the same lock. A file with no editor loads a fresh one.
    fn restore_chop(&mut self, saved: Option<ChopRestore>) {
        self.close_source();
        let saved = saved.unwrap_or_else(|| ChopEditorSnapshot::default().into_restore());
        self.set_snap(saved.snap);
        self.set_strip(saved.collapsed, saved.height_px);
        if let Some(path) = &saved.source_path {
            self.open_source(path);
            self.chop.pending_selection = saved.selection;
            self.chop.pending_view = saved.view;
        }
    }

    /// Mark the end of a gesture: if anything has changed since the last
    /// commit, the state from before those changes becomes an undo step.
    /// Reports whether it did.
    ///
    /// Called by whoever finished the gesture — the editor at the release of
    /// a drag, or straight after a click; the loader when its result lands —
    /// and never by the setters themselves. That is what makes a knob
    /// dragged across a second one step back instead of sixty: the writers
    /// in between change lanes, and only this marks a place a user can
    /// return to.
    ///
    /// Because the model keeps the committed state rather than a list of
    /// what each writer did, a gesture that ends where it started — a drag
    /// returned to the same value, a rename to the same name, a reorder
    /// dropped back in place — adds no step at all, and a caller that
    /// commits after every gesture regardless does not have to know which
    /// of them changed something.
    ///
    /// A step added clears the redo stack: the future redo would have
    /// replayed is not the one this edit leads to.
    pub fn commit(&mut self) -> bool {
        if self.is_committed() {
            return false;
        }

        let current = self.capture();
        let previous = std::mem::replace(&mut self.baseline, current);
        self.push_undo(previous);
        self.redo_stack.clear();
        // The entries this just dropped — the redone future, and the
        // oldest step if the stack was full — may have held the last
        // handle on a sample.
        self.collect();
        true
    }

    /// Fold what has changed into the state an undo returns to, without
    /// making a step of it (history is kept per gesture, and this is for
    /// changes that are not gestures).
    ///
    /// The loader's restore results are what need it: a project load
    /// replaces the lanes and then, lane by lane as the decodes finish,
    /// gives them their samples. None of that is an edit a user could want
    /// back — opening a project is not a gesture, and the samples arriving
    /// are the project, not a change to it. But they do change the lanes, so
    /// without this the next real gesture's [`commit`](Model::commit) would
    /// push a baseline from before they landed, and undoing a rename would
    /// take a restored sample off its lane with it.
    ///
    /// Not for anything the GUI does: a writer whose change a user made
    /// ends in `commit`, which is what makes it undoable.
    pub fn rebase(&mut self) {
        self.baseline = self.capture();
    }

    /// Step back over one gesture, reporting whether there was one to step
    /// back over.
    ///
    /// Commits first, so an edit nobody marked the end of is still
    /// undoable: the state on screen becomes a step and this undo lands on
    /// the last committed one. (That commit clears the redo stack, as any
    /// new edit does.)
    ///
    /// The step itself is a wholesale replace and a publish, the way a
    /// project load is ([`Model::replace_lanes`]) — with three differences,
    /// all because an undo entry came from this very instance instead of
    /// from a file:
    ///
    /// - lane **ids are kept**, not reassigned, so the GUI's selection and
    ///   a loader job already in flight still name the lane they meant;
    /// - a lane that is still here keeps its current [`Lane::error`] and
    ///   [`Lane::loading`], which describe this session and not the state
    ///   being restored; and
    /// - nothing is re-clamped, because every value in the entry went
    ///   through a clamp on its way into the model in the first place.
    pub fn undo(&mut self) -> bool {
        self.commit();
        // An entry that would change nothing now — a selection step on a
        // source since closed — is dropped rather than spending a keypress.
        while self.undo_stack.back().is_some_and(|state| self.matches_now(state)) {
            self.undo_stack.pop_back();
        }
        match self.undo_stack.pop_back() {
            Some(previous) => {
                let current = self.capture();
                self.redo_stack.push_back(current);
                self.apply_state(previous);
                true
            }
            None => false,
        }
    }

    /// Step forward into a gesture that was undone, reporting whether there
    /// was one.
    ///
    /// Commits first for the same reason [`Model::undo`] does, which is
    /// also how "a new edit clears the redo stack" is enforced at the last
    /// moment: if there is an uncommitted edit, that commit empties the
    /// redo stack and this returns `false` — there is no forward any more.
    pub fn redo(&mut self) -> bool {
        self.commit();
        while self.redo_stack.back().is_some_and(|state| self.matches_now(state)) {
            self.redo_stack.pop_back();
        }
        match self.redo_stack.pop_back() {
            Some(next) => {
                let current = self.capture();
                self.push_undo(current);
                self.apply_state(next);
                true
            }
            None => false,
        }
    }

    /// Whether [`Model::undo`] would do something: a committed step, or an
    /// edit since the last commit that it would commit itself.
    ///
    /// The model's history read surface, together with
    /// [`can_redo`](Self::can_redo), [`undo_depth`](Self::undo_depth) and
    /// [`redo_depth`](Self::redo_depth): what the tests state their
    /// expectations about the undo stack through. **No caller in the
    /// window** — the undo/redo shortcuts simply ask for the step and
    /// [`Model::undo`] reports whether there was one to take, and the menu
    /// has no undo entry to grey. Cheap enough to be asked every frame if
    /// one ever wants to: a field-by-field comparison of the lanes,
    /// allocating nothing.
    pub fn can_undo(&self) -> bool {
        !self.undo_stack.is_empty() || !self.is_committed()
    }

    /// Whether [`Model::redo`] would do something: a step that was undone,
    /// and no edit since to have thrown it away.
    ///
    /// Part of the history read surface described on
    /// [`can_undo`](Self::can_undo), and asked only by the tests.
    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty() && self.is_committed()
    }

    /// How many steps back there are to take — what the tests count the
    /// stack with (see [`can_undo`](Self::can_undo) on why the window does
    /// not ask).
    pub fn undo_depth(&self) -> usize {
        self.undo_stack.len()
    }

    /// How many steps forward there are to take, the counterpart of
    /// [`undo_depth`](Self::undo_depth).
    pub fn redo_depth(&self) -> usize {
        self.redo_stack.len()
    }

    /// Whether the lanes are still exactly the state of the last commit —
    /// that is, whether the gesture in progress (if any) has changed
    /// anything yet.
    fn is_committed(&self) -> bool {
        self.matches_now(&self.baseline)
    }

    /// Whether `state` is the model as it is now, as far as undo can tell.
    fn matches_now(&self, state: &ModelState) -> bool {
        state_matches(&self.lanes, self.sample_pad_switch, state)
            && chop_matches(&self.chop_mark(), &state.chop)
    }

    /// The current state as an undo entry: the lanes with their
    /// session-only fields blanked (see [`ModelState`]), and the switch.
    fn capture(&self) -> ModelState {
        let lanes = self
            .lanes
            .iter()
            .map(|lane| Lane {
                error: None,
                loading: false,
                // Render state is a cache, not a setting: an entry holding
                // render audio would only keep it alive.
                render: None,
                render_generation: 0,
                render_pending: false,
                render_error: None,
                ..lane.clone()
            })
            .collect();

        ModelState { lanes, sample_pad_switch: self.sample_pad_switch, chop: self.chop_mark() }
    }

    /// Push an undo step, dropping the oldest if that would take the stack
    /// past [`UNDO_DEPTH`].
    fn push_undo(&mut self, state: ModelState) {
        self.undo_stack.push_back(state);
        if self.undo_stack.len() > UNDO_DEPTH {
            self.undo_stack.pop_front();
        }
    }

    /// Make an entry the model's state again and publish it: the second
    /// half of an undo or a redo, described at [`Model::undo`].
    ///
    /// The restored lanes become the new baseline, so the gesture that
    /// follows is measured against where the user has just arrived rather
    /// than against where they were before stepping.
    fn apply_state(&mut self, state: ModelState) {
        let ModelState { mut lanes, sample_pad_switch, chop } = state;

        // A lane that is still here keeps what this session knows about it:
        // the message of a load that failed, and a decode that is still
        // running and will report to this id.
        for lane in &mut lanes {
            if let Some(live) = self.lanes.iter().find(|live| live.id == lane.id) {
                lane.error = live.error.clone();
                lane.loading = live.loading;
                // Renders are not undone: the live render stays, and its
                // generation never goes backwards (an in-flight result must
                // still be recognised for what it is). Only settings that
                // change what the render should be ask for a new one.
                lane.render = live.render.clone();
                lane.render_generation = live.render_generation;
                lane.render_pending = live.render_pending;
                lane.render_error = live.render_error.clone();
                if !render_inputs_match(lane, live) {
                    if !same_sample(lane, live) {
                        lane.render = None;
                    }
                    refresh_render(lane);
                }
            } else {
                // A deleted lane coming back: its render went with it.
                request_if_stretch(lane);
            }
        }

        self.last_added_note = note_to_continue_from(&lanes);
        self.lanes = lanes;
        self.sample_pad_switch = sample_pad_switch;
        // The selection comes back only onto the source it was made on.
        if chop.serial == self.chop_serial && self.chop.source.is_some() {
            self.set_selection(chop.selection);
        }
        self.baseline = self.capture();
        self.republish();
    }

    /// The id for a lane about to be created. Only ever counts up, so no id
    /// is reused while the instance lives — a late loader result cannot
    /// land on the lane that took its victim's place.
    ///
    /// Saturating rather than wrapping or panicking: reaching `u32::MAX`
    /// would take four billion add-lane gestures in one session, and a
    /// plugin that panicked on the last of them would be worse than one
    /// whose last two lanes shared an id.
    fn take_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Run `edit` on the lane with this id, reporting whether there was
    /// one. The shape of every setter: name a lane, change one thing, say
    /// nothing happened if the lane is gone.
    ///
    /// Publishes nothing and collects nothing — the three wrappers below
    /// are what writers call, and each of them has a tail.
    fn with_lane(&mut self, id: u32, edit: impl FnOnce(&mut Lane)) -> bool {
        match self.index_of(id) {
            Some(index) => {
                edit(&mut self.lanes[index]);
                true
            }
            None => false,
        }
    }

    /// Change something the audio thread reads out of the snapshot — a
    /// note, a choke group, a mode, a sample — and publish the new list
    /// (these are the structural changes).
    fn with_published_lane(&mut self, id: u32, edit: impl FnOnce(&mut Lane)) -> bool {
        let changed = self.with_lane(id, edit);
        if changed {
            self.republish();
        }
        changed
    }

    /// Change a lane's continuous value (volume, pan, attack, release,
    /// fine, pitch): the value goes to the lane, which is
    /// what a save and the next snapshot read, and into the published
    /// lane's atomic, which is what the audio thread reads — no new list.
    ///
    /// A lane with no sample is not in the snapshot, so there is no atomic
    /// to write; the value waits in the lane and the publish that gives it
    /// a sample carries it over.
    fn with_atomic_lane(
        &mut self,
        id: u32,
        edit: impl FnOnce(&mut Lane),
        store: impl FnOnce(&PublishedLane),
    ) -> bool {
        let changed = self.with_lane(id, edit);
        if changed {
            let published = self.published.get();
            if let Some(lane) = published.lane(id) {
                store(lane);
            }
            drop(published);
            self.collect();
        }
        changed
    }

    /// Change something the audio thread never sees — a name, a colour, a
    /// message, the loading flag — so there is no snapshot to build.
    ///
    /// Collects all the same: a writer is the only thing that frees
    /// anything, and renaming a lane is as good a moment as any to hand
    /// back a sample whose last reader has finished with it.
    fn with_unpublished_lane(&mut self, id: u32, edit: impl FnOnce(&mut Lane)) -> bool {
        let changed = self.with_lane(id, edit);
        if changed {
            self.collect();
        }
        changed
    }

    /// Build the snapshot the lanes now describe, swap it in, and reclaim
    /// what the swap retired: the tail of every structural writer.
    ///
    /// Publishes unconditionally, even where the new list must equal the
    /// old one (adding an empty lane, say, or renaming nothing into
    /// nothing): one rule for every structural writer is worth more than
    /// the few microseconds a comparison would save on a gesture a user
    /// makes by hand.
    fn republish(&mut self) {
        let previous = self.published.get();
        let lanes = PublishedLanes::build(&self.lanes, self.sample_pad_switch, Some(&previous));
        // Before the collect below, so that the snapshot this one retires
        // is reclaimable rather than held open by the handle used to read
        // its activity counters.
        drop(previous);
        self.published.cell.set(Shared::new(&self.handle, lanes));
        self.collect();
    }
}

/// The model as everything that writes to it holds it: one `Model` behind
/// one mutex (writers are serialized by a mutex on the model; the audio
/// thread never takes it).
///
/// Three threads reach for it and the lock is what orders them: the GUI
/// thread for a user's gestures, the loader's worker thread for the samples
/// it has finished decoding and the host's main thread for a project load.
/// The audio thread is not among them — it reads [`Model::published`]'s
/// cell, which needs no lock, and that is the whole reason this mutex can be
/// an ordinary blocking one.
///
/// It also puts the `basedrop::Collector` behind the lock, which is the
/// point: freeing happens on writer threads, never on the audio thread,
/// and every writer collects after mutating.
/// Bring a lane's render state in line with its time mode after a change
/// that invalidates any render: a Stretch lane asks for a new one; any
/// other lane drops its render and, if one was in flight, makes it stale.
fn refresh_render(lane: &mut Lane) {
    if lane.time_mode == TimeMode::Stretch {
        request_render_of(lane);
    } else {
        if lane.render_pending || lane.render.is_some() {
            lane.render_generation = lane.render_generation.saturating_add(1);
        }
        lane.render = None;
        lane.render_pending = false;
        lane.render_error = None;
    }
}

/// [`request_render_of`] for a Stretch lane; any other lane is left alone.
fn request_if_stretch(lane: &mut Lane) {
    if lane.time_mode == TimeMode::Stretch {
        request_render_of(lane);
    }
}

/// The body of [`Model::request_render`]: a new generation, pending when
/// there is a sample to render, no error.
fn request_render_of(lane: &mut Lane) {
    lane.render_generation = lane.render_generation.saturating_add(1);
    lane.render_pending = lane.sample.is_some();
    lane.render_error = None;
}

/// Whether two states of one lane want the same render: the same chop, time
/// mode, length, fine and pitch.
fn render_inputs_match(lane: &Lane, other: &Lane) -> bool {
    same_sample(lane, other)
        && lane.time_mode == other.time_mode
        && lane.length == other.length
        && lane.fine_pct == other.fine_pct
        && lane.pitch_st == other.pitch_st
}

/// Whether two lanes hold the same sample (by identity, as undo compares).
fn same_sample(lane: &Lane, other: &Lane) -> bool {
    sample_matches(lane.sample.as_ref(), other.sample.as_ref())
}

pub type SharedModel = Arc<Mutex<Model>>;

/// Take the writer lock, recovering it if a writer panicked while holding
/// it.
///
/// A poisoned lock here is not a reason to bring anything else down: the
/// model's invariants are restored by the setters themselves (every one of
/// them clamps what it writes), nothing in the lanes can be half-written —
/// a panicking writer leaves whole lanes behind — and the two alternatives
/// are both worse. `unwrap` would turn one panic in the editor into a
/// second one on the loader thread, where it would take the worker out and
/// leave every later drop silently loading forever; refusing to write would
/// leave a plugin instance that has lost its editor unable to load a sample
/// for the rest of the session.
pub fn lock_model(model: &Mutex<Model>) -> MutexGuard<'_, Model> {
    model.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The note [`Model::add_lane`] should count up from, given a lane list
/// that has just become the model's: the highest note in it, or one below
/// [`DEFAULT_FIRST_NOTE`] when the list is empty, so that the first lane
/// added after loading an empty project lands on C1 exactly as a fresh
/// instance's first lane does.
fn note_to_continue_from(lanes: &[Lane]) -> u8 {
    let before_the_first = DEFAULT_FIRST_NOTE.saturating_sub(1);
    lanes.iter().map(|lane| lane.note).max().unwrap_or(before_the_first)
}

/// Whether these lanes and this switch are the state an undo entry holds:
/// the comparison that decides whether a gesture changed anything (one
/// entry per gesture, and none for a gesture that ended where it started).
///
/// Lane by lane and in order, so a reorder counts as a change. Taking the
/// live lanes by reference rather than a [`Model::capture`] of them is what
/// lets [`Model::can_undo`] be asked every frame without allocating.
/// Whether the chop selection `now` is the one an undo entry holds: the same
/// span on the same source. An entry from another source matches whatever
/// is selected now — its selection means nothing here ([`ChopMark`]). Which
/// edge is active is not compared: picking an edge is no edit.
fn chop_matches(now: &ChopMark, stored: &ChopMark) -> bool {
    let span = |mark: &ChopMark| mark.selection.map(|selection| (selection.start, selection.end));
    now.serial != stored.serial || span(now) == span(stored)
}

fn state_matches(lanes: &[Lane], sample_pad_switch: bool, state: &ModelState) -> bool {
    sample_pad_switch == state.sample_pad_switch
        && lanes.len() == state.lanes.len()
        && lanes.iter().zip(&state.lanes).all(|(lane, stored)| lane_matches(lane, stored))
}

/// Whether a lane is the lane an undo entry holds — its persistent fields
/// and its id.
///
/// [`Lane::error`] and [`Lane::loading`] are skipped, and have to be: an
/// entry carries neither (see [`ModelState`]), so comparing them would make
/// a lane that merely has a message look like an edit, and a committed
/// gesture look uncommitted forever.
///
/// Volume, pan and the v2 continuous settings (attack, release, fine,
/// pitch) compare as numbers — the question is whether the value a save
/// would write has changed — and all have been through a clamp, so none can
/// be a NaN that compares unequal to itself.
///
/// Samples compare by **identity**: the same decoded audio and the same file
/// bytes, not an equal copy of them. That is what the writers produce (a
/// lane holds the one [`SampleRef`] until something replaces it), and it is
/// also the honest answer for a reload-from-file that happened to read
/// identical bytes — the lane really does play a different allocation now.
fn lane_matches(lane: &Lane, stored: &Lane) -> bool {
    lane.id == stored.id
        && lane.name == stored.name
        && lane.note == stored.note
        && lane.colour == stored.colour
        && lane.volume_db == stored.volume_db
        && lane.pan == stored.pan
        && lane.choke == stored.choke
        && lane.mode == stored.mode
        && lane.attack_ms == stored.attack_ms
        && lane.release_ms == stored.release_ms
        && lane.cut_itself == stored.cut_itself
        && lane.output == stored.output
        && lane.time_mode == stored.time_mode
        && lane.length == stored.length
        && lane.fine_pct == stored.fine_pct
        && lane.pitch_st == stored.pitch_st
        && sample_matches(lane.sample.as_ref(), stored.sample.as_ref())
}

/// Whether two lanes' samples are the same sample: both empty, or both the
/// one allocation (see [`lane_matches`]).
fn sample_matches(sample: Option<&SampleRef>, stored: Option<&SampleRef>) -> bool {
    match (sample, stored) {
        (None, None) => true,
        (Some(sample), Some(stored)) => {
            std::ptr::eq(&*sample.data, &*stored.data)
                && std::ptr::eq(&*sample.bytes, &*stored.bytes)
        }
        (Some(_), None) | (None, Some(_)) => false,
    }
}

/// The `f32` a published atomic holds, read relaxed.
///
/// Volume and pan travel as `f32` bits in an `AtomicU32` because there is
/// no `AtomicF32`: the bits round-trip exactly, including a NaN that no
/// clamp here ever stores.
fn load_f32(cell: &AtomicU32) -> f32 {
    f32::from_bits(cell.load(Ordering::Relaxed))
}

/// Store an `f32` in a published atomic, relaxed. The callers clamp first,
/// so what lands here is always in range.
fn store_f32(cell: &AtomicU32, value: f32) {
    cell.store(value.to_bits(), Ordering::Relaxed);
}

/// A note in range: 0–127, the whole of MIDI.
pub(crate) fn clamp_note(note: u8) -> u8 {
    note.min(127)
}

/// A colour with only its `0xRRGGBB` bits: anything in the top byte (an
/// alpha a project file invented, say) is dropped rather than drawn.
pub(crate) fn clamp_colour(colour: u32) -> u32 {
    colour & 0x00FF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::EMBED_WARN_BYTES;
    use crate::voices::MAX_VOICES;

    /// The default state, pinned field by field.
    ///
    /// Every one of these is something a user sees on a fresh instance
    /// without touching anything, and something a bitstep lane list is
    /// expected to line up with one to one — so each is a deliberate value,
    /// not whatever the code happened to produce.
    #[test]
    fn a_fresh_instance_opens_with_sixteen_empty_lanes() {
        let lanes = default_lanes();
        assert_eq!(lanes.len(), DEFAULT_LANE_COUNT as usize);
        assert_eq!(lanes.len(), 16, "sixteen");

        let first_eight = [
            0xE2_6E_6E, 0xE2_9A_5E, 0xE2_C9_5E, 0xA8_C9_6E, // red … green
            0x6E_C9_8A, 0x6E_C2_C9, 0x6E_96_D6, 0x8E_7E_D6, // green … violet
        ];
        // The rest go on round the palette: its last two, then from the top.
        let expected_colours: Vec<u32> = first_eight
            .iter()
            .copied()
            .chain(LANE_PALETTE[8..].iter().copied())
            .chain(LANE_PALETTE[..6].iter().copied())
            .collect();

        for (index, lane) in lanes.iter().enumerate() {
            assert_eq!(lane.id, index as u32, "lane {index}: ids run from 0 in order");
            assert_eq!(lane.name, format!("Lane {}", index + 1), "lane {index}: name");
            assert_eq!(lane.note, 36 + index as u8, "lane {index}: notes 36..=51, C1 upward");
            assert_eq!(lane.colour, expected_colours[index], "lane {index}: palette colour");
            assert_eq!(lane.volume_db, 0.0, "lane {index}: unchanged volume");
            assert_eq!(lane.pan, 0.0, "lane {index}: centred");
            assert_eq!(lane.choke, 0, "lane {index}: in no choke group");
            assert_eq!(lane.mode, LaneMode::OneShot, "lane {index}: one-shot");
            assert!(lane.sample.is_none(), "lane {index}: empty");
            assert!(lane.error.is_none(), "lane {index}: nothing has gone wrong yet");
            assert!(!lane.loading, "lane {index}: no loader job pending");
            assert_v2_defaults(lane, &format!("default lane {index}"));
        }

        assert_eq!(lanes[7].note, 43, "the eighth lane is on G1");
        assert_eq!(lanes.last().unwrap().note, 51, "the sixteenth lane is on D#2");

        // The ninth default: the instance switch, which is on, so a fresh
        // instance cancels bitstep's default Sample pad retune and bitstep's
        // default lanes play every sample at its own pitch. A `const` block
        // because the value is known at compile time — clippy rejects a
        // runtime assertion on a constant, and it is no weaker a pin.
        const { assert!(SAMPLE_PAD_SWITCH_DEFAULT, "the Sample pad switch starts on") };
    }

    /// The v2 settings every new lane starts with.
    fn assert_v2_defaults(lane: &Lane, what: &str) {
        assert_eq!(lane.mode, LaneMode::OneShot, "{what}: still one-shot");
        assert_eq!(lane.attack_ms, 0.0, "{what}: attack 0 ms");
        assert_eq!(lane.release_ms, 300.0, "{what}: release 300 ms");
        assert!(lane.cut_itself, "{what}: a new lane cuts itself");
        assert_eq!(lane.output, 0, "{what}: on the main output");
        assert_eq!(lane.time_mode, TimeMode::Off, "{what}: time off");
        assert_eq!(lane.length, Length::AsCut, "{what}: length as cut");
        assert_eq!(lane.fine_pct, 0.0, "{what}: fine 0 %");
        assert_eq!(lane.pitch_st, 0.0, "{what}: pitch 0 st");
    }

    #[test]
    fn a_new_lane_has_the_v2_defaults() {
        assert_v2_defaults(&Lane::new(5, "New", 60, 0), "Lane::new");
    }

    #[test]
    fn the_v2_limits_are_the_specs_limits() {
        assert_eq!((ATTACK_MS_MIN, ATTACK_MS_MAX), (0.0, 2000.0));
        assert_eq!((RELEASE_MS_MIN, RELEASE_MS_MAX), (0.0, 5000.0));
        assert_eq!(OUTPUT_MAX, 15, "16 outputs: 0..=15");
        assert_eq!((FINE_MIN, FINE_MAX), (-10.0, 10.0));
        assert_eq!((PITCH_MIN, PITCH_MAX), (-24.0, 24.0));
    }

    /// A fresh lane with the v2 numeric fields overwritten, then clamped.
    fn clamped(attack: f32, release: f32, output: u8, fine: f32, pitch: f32) -> Lane {
        let mut lane = Lane::new(0, "Hostile", 36, 0);
        lane.attack_ms = attack;
        lane.release_ms = release;
        lane.output = output;
        lane.fine_pct = fine;
        lane.pitch_st = pitch;
        lane.clamp();
        lane
    }

    #[test]
    fn clamp_pins_the_envelope_times_into_range() {
        let low = clamped(-1.0, -1.0, 0, 0.0, 0.0);
        assert_eq!((low.attack_ms, low.release_ms), (0.0, 0.0), "-1 ms → 0 ms");
        let high = clamped(2001.0, 2001.0, 0, 0.0, 0.0);
        assert_eq!(high.attack_ms, 2000.0, "2001 ms attack → 2000 ms");
        assert_eq!(high.release_ms, 2001.0, "2001 ms is a valid release");
        assert_eq!(clamped(0.0, 5001.0, 0, 0.0, 0.0).release_ms, 5000.0);
        let inf = clamped(f32::INFINITY, f32::NEG_INFINITY, 0, 0.0, 0.0);
        assert_eq!((inf.attack_ms, inf.release_ms), (2000.0, 0.0), "infinities: nearer limit");
    }

    #[test]
    fn clamp_sends_nan_to_the_defaults() {
        let lane = clamped(f32::NAN, f32::NAN, 0, f32::NAN, f32::NAN);
        assert_eq!(lane.attack_ms, ATTACK_MS_DEFAULT);
        assert_eq!(lane.release_ms, RELEASE_MS_DEFAULT, "NaN release → 300, not 0");
        assert_eq!(lane.fine_pct, FINE_DEFAULT);
        assert_eq!(lane.pitch_st, PITCH_DEFAULT);
    }

    #[test]
    fn clamp_sends_an_unknown_output_to_main() {
        assert_eq!(clamped(0.0, 300.0, 15, 0.0, 0.0).output, 15, "Out 16 is valid");
        assert_eq!(clamped(0.0, 300.0, 16, 0.0, 0.0).output, 0, "16 → main, not 15");
        assert_eq!(clamped(0.0, 300.0, u8::MAX, 0.0, 0.0).output, 0);
    }

    #[test]
    fn clamp_keeps_fine_in_range_and_to_tenths() {
        assert_eq!(clamped(0.0, 300.0, 0, 10.04, 0.0).fine_pct, 10.0, "10.04 → 10.0");
        assert_eq!(clamped(0.0, 300.0, 0, -11.0, 0.0).fine_pct, -10.0);
        assert_eq!(clamp_fine_pct(0.14), 0.1, "0.14 → 0.1");
        assert_eq!(clamp_fine_pct(0.16), 0.2, "0.16 → 0.2");
        assert_eq!(clamp_fine_pct(-3.27), -3.3);
        assert_eq!(clamp_fine_pct(0.1), 0.1, "a tenth stays a tenth");
    }

    #[test]
    fn clamp_keeps_pitch_in_range_and_to_cents() {
        assert_eq!(clamped(0.0, 300.0, 0, 0.0, -24.001).pitch_st, -24.0, "-24.001 → -24");
        assert_eq!(clamped(0.0, 300.0, 0, 0.0, 24.5).pitch_st, 24.0);
        assert_eq!(clamp_pitch_st(1.234), 1.23, "two decimals");
        assert_eq!(clamp_pitch_st(-7.006), -7.01);
        assert_eq!(clamp_pitch_st(0.5), 0.5);
    }

    /// Every v2 setter clamps its value on the way in.
    #[test]
    fn the_v2_setters_clamp() {
        let mut model = Model::new();
        assert!(model.set_attack_ms(0, 2500.0));
        assert!(model.set_release_ms(0, -5.0));
        assert!(model.set_output(0, 16));
        assert!(model.set_fine_pct(0, 3.27));
        assert!(model.set_pitch_st(0, 30.0));
        let lane = model.lane(0).unwrap();
        assert_eq!(lane.attack_ms, ATTACK_MS_MAX, "2500 ms attack → 2000 ms");
        assert_eq!(lane.release_ms, RELEASE_MS_MIN, "-5 ms release → 0 ms");
        assert_eq!(lane.output, OUTPUT_MAIN, "Out 17 → main");
        assert_eq!(lane.fine_pct, 3.3, "fine to tenths");
        assert_eq!(lane.pitch_st, PITCH_MAX, "30 st → 24 st");

        assert!(model.set_attack_ms(1, f32::NAN));
        assert!(model.set_release_ms(1, f32::NAN));
        assert!(model.set_fine_pct(1, f32::NAN));
        assert!(model.set_pitch_st(1, f32::NAN));
        assert!(model.set_output(1, 15));
        let lane = model.lane(1).unwrap();
        assert_eq!(lane.attack_ms, ATTACK_MS_DEFAULT, "NaN → default");
        assert_eq!(lane.release_ms, RELEASE_MS_DEFAULT);
        assert_eq!(lane.fine_pct, FINE_DEFAULT);
        assert_eq!(lane.pitch_st, PITCH_DEFAULT);
        assert_eq!(lane.output, OUTPUT_MAX, "Out 16 is valid");
    }

    /// Every v2 setter reports an unknown lane and changes nothing.
    #[test]
    fn the_v2_setters_refuse_an_unknown_lane() {
        let mut model = Model::new();
        let missing = 999;
        assert!(!model.set_attack_ms(missing, 10.0));
        assert!(!model.set_release_ms(missing, 10.0));
        assert!(!model.set_cut_itself(missing, false));
        assert!(!model.set_output(missing, 3));
        assert!(!model.set_time_mode(missing, TimeMode::Stretch));
        assert!(!model.set_length(missing, Length::Beats(NoteLength::Bar)));
        assert!(!model.set_fine_pct(missing, 1.0));
        assert!(!model.set_pitch_st(missing, 1.0));
        assert!(!model.can_undo(), "nothing changed, so nothing to undo");
    }

    /// Each v2 setter, committed, is one undo step that restores the old
    /// value — and only that value.
    #[test]
    fn each_v2_setter_is_one_undo_step() {
        type Check = (&'static str, fn(&mut Model) -> bool, fn(&Lane) -> bool);
        let checks: [Check; 8] = [
            ("attack", |m| m.set_attack_ms(0, 50.0), |l| l.attack_ms == ATTACK_MS_DEFAULT),
            ("release", |m| m.set_release_ms(0, 1000.0), |l| {
                l.release_ms == RELEASE_MS_DEFAULT
            }),
            ("cut itself", |m| m.set_cut_itself(0, false), |l| l.cut_itself),
            ("output", |m| m.set_output(0, 4), |l| l.output == OUTPUT_MAIN),
            ("time mode", |m| m.set_time_mode(0, TimeMode::Repitch), |l| {
                l.time_mode == TimeMode::Off
            }),
            ("length", |m| m.set_length(0, Length::Beats(NoteLength::Half)), |l| {
                l.length == Length::AsCut
            }),
            ("fine", |m| m.set_fine_pct(0, -2.5), |l| l.fine_pct == FINE_DEFAULT),
            ("pitch", |m| m.set_pitch_st(0, 7.0), |l| l.pitch_st == PITCH_DEFAULT),
        ];
        for (what, set, is_old) in checks {
            let mut model = Model::new();
            assert!(set(&mut model), "{what}: lane 0 exists");
            assert!(!is_old(model.lane(0).unwrap()), "{what}: the setter changed it");
            assert!(model.commit(), "{what}: the change is a step");
            assert_eq!(model.undo_depth(), 1, "{what}: exactly one step");

            assert!(model.undo(), "{what}: undo");
            assert!(is_old(model.lane(0).unwrap()), "{what}: undo restores the old value");
            assert!(!model.can_undo(), "{what}: and that was the only step");
            assert!(model.redo(), "{what}: redo");
            assert!(!is_old(model.lane(0).unwrap()), "{what}: redo brings it back");
        }
    }

    /// A model of `count` lanes, all on the main output, and their ids in
    /// display order.
    fn model_with_lanes(count: usize) -> (Model, Vec<u32>) {
        let mut model = Model::new();
        while model.lanes().len() < count {
            model.add_lane().expect("under MAX_LANES");
        }
        while model.lanes().len() > count {
            let last = model.lanes().last().unwrap().id;
            assert!(model.delete_lane(last));
        }
        model.rebase();
        let ids = model.lanes().iter().map(|lane| lane.id).collect();
        (model, ids)
    }

    fn outputs(model: &Model) -> Vec<u8> {
        model.lanes().iter().map(|lane| lane.output).collect()
    }

    /// 17 lanes, 15 aux outputs: the first 15 (display order) land on
    /// Out 2–16, the last two stay on main and are reported.
    #[test]
    fn assigning_own_outputs_places_fifteen_and_reports_the_rest() {
        let (mut model, ids) = model_with_lanes(17);
        assert_eq!(model.assign_own_outputs(&ids), 2);
        let mut expected: Vec<u8> = (1..=OUTPUT_MAX).collect();
        expected.extend([OUTPUT_MAIN, OUTPUT_MAIN]);
        assert_eq!(outputs(&model), expected);
    }

    /// A lane already alone on Out 5 keeps it; the others fill around it.
    #[test]
    fn a_lane_alone_on_an_aux_output_keeps_it() {
        let (mut model, ids) = model_with_lanes(17);
        let on_five = ids[10];
        assert!(model.set_output(on_five, 4));
        assert_eq!(model.assign_own_outputs(&ids), 2);

        assert_eq!(model.lane(on_five).unwrap().output, 4, "Out 5 kept");
        let mut expected: Vec<u8> = vec![1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 4, 12, 13, 14, 15];
        expected.extend([OUTPUT_MAIN, OUTPUT_MAIN]);
        assert_eq!(outputs(&model), expected);
    }

    /// Two selected lanes sharing an aux output: the first moves, the
    /// second is then alone and keeps it. An unselected lane's output is
    /// taken, never handed out.
    #[test]
    fn a_shared_aux_output_is_not_free_and_unselected_lanes_are_untouched() {
        let (mut model, ids) = model_with_lanes(4);
        model.set_output(ids[0], 1);
        model.set_output(ids[1], 1);
        model.set_output(ids[3], 2);
        assert_eq!(model.assign_own_outputs(&ids[..3]), 0);
        assert_eq!(outputs(&model), vec![3, 1, 4, 2]);
    }

    /// Display order decides, not id order and not the order of `ids`.
    #[test]
    fn assigning_own_outputs_follows_display_order() {
        let (mut model, _) = model_with_lanes(3);
        // Move the last lane (id 2) to the top: display order is 2, 0, 1.
        assert!(model.reorder(2, 0));
        let display: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
        assert_eq!(display, vec![2, 0, 1]);

        assert_eq!(model.assign_own_outputs(&[1, 0, 2]), 0);
        assert_eq!(model.lane(2).unwrap().output, 1);
        assert_eq!(model.lane(0).unwrap().output, 2);
        assert_eq!(model.lane(1).unwrap().output, 3);
    }

    /// Unknown ids are ignored; with nothing to move nothing changes.
    #[test]
    fn assigning_own_outputs_ignores_unknown_ids() {
        let (mut model, _) = model_with_lanes(2);
        assert_eq!(model.assign_own_outputs(&[999, 1000]), 0);
        assert_eq!(model.assign_own_outputs(&[]), 0);
        assert_eq!(outputs(&model), vec![OUTPUT_MAIN, OUTPUT_MAIN]);
        assert!(!model.commit(), "nothing changed, no step");
    }

    /// The whole assignment, committed, is one undo step.
    #[test]
    fn assigning_own_outputs_is_one_undo_step() {
        let (mut model, ids) = model_with_lanes(17);
        let before = outputs(&model);
        model.assign_own_outputs(&ids);
        let after = outputs(&model);
        assert!(model.commit());
        assert_eq!(model.undo_depth(), 1);

        assert!(model.undo());
        assert_eq!(outputs(&model), before);
        assert!(!model.can_undo());
        assert!(model.redo());
        assert_eq!(outputs(&model), after);
    }

    /// A v1 project's lane does not cut itself, unlike a fresh one.
    #[test]
    fn a_v1_snapshot_lane_does_not_cut_itself() {
        // A v1 lane: no `cut_itself` (nor any other v2 field) in the file.
        let v1: Snapshot =
            serde_json::from_str(r#"{"version":1,"lanes":[{"name":"Kick","note":36}]}"#)
                .expect("a v1 snapshot parses");
        let restore = v1.into_restore();
        assert!(default_lanes().iter().all(|lane| lane.cut_itself), "fresh lanes cut");
        assert!(restore.lanes.iter().all(|lane| !lane.cut_itself), "v1 lanes do not");

        let saved = Snapshot::of(&default_lanes(), false).into_restore();
        assert!(saved.lanes.iter().all(|lane| lane.cut_itself), "a v2 save keeps it on");
    }

    /// The default colours are the palette's own, in order and round it
    /// again — the same list the editor's swatches come from, so a lane's
    /// dot and its saved colour cannot drift apart.
    #[test]
    fn the_default_colours_walk_the_palette_in_order() {
        let colours: Vec<u32> = default_lanes().iter().map(|lane| lane.colour).collect();
        let expected: Vec<u32> = LANE_PALETTE.iter().cycle().take(16).copied().collect();
        assert_eq!(colours, expected);
    }

    /// The lane limits, pinned here because the writer API's clamps and a
    /// project load's clamps are both only as good as these numbers.
    #[test]
    fn the_limits_are_the_specs_limits() {
        assert_eq!(MAX_LANES, 128);
        assert_eq!(MAX_NAME_LEN, 64);
        assert_eq!(MAX_VOICES, 64, "declared once, in voices.rs");
        assert_eq!(CHOKE_GROUPS, 16);
        assert_eq!(LANE_VOLUME_DB_MIN, -60.0);
        assert_eq!(LANE_VOLUME_DB_MAX, 12.0);
        assert_eq!((LANE_PAN_MIN, LANE_PAN_MAX), (-1.0, 1.0));
        assert_eq!(UNDO_DEPTH, 100, "the bounded undo depth");
    }

    /// A lane built from hostile numbers still holds valid ones: the
    /// constructor is a boundary, and every boundary clamps.
    #[test]
    fn the_constructor_clamps_what_it_is_given() {
        let lane = Lane::new(7, "kick", 127, 0xFF_12_34_56);
        assert_eq!(lane.colour, 0x12_34_56, "the top byte is masked off, not kept");
        assert_eq!(lane.note, 127);
        assert_eq!(lane.id, 7);
    }

    /// Names are cut at [`MAX_NAME_LEN`] *characters*, on a character
    /// boundary: a 64-character multi-byte name keeps all 64, and a longer
    /// one loses only the tail — never half a character, which would be a
    /// panic in `String` slicing terms.
    #[test]
    fn long_names_are_trimmed_by_character_not_by_byte() {
        let plain = Lane::new(0, &"x".repeat(100), 36, 0);
        assert_eq!(plain.name.chars().count(), MAX_NAME_LEN);

        // Four bytes per character, so a byte-counting trim would cut one in
        // half long before reaching 64 of them.
        let wide = Lane::new(0, &"𝄞".repeat(100), 36, 0);
        assert_eq!(wide.name.chars().count(), MAX_NAME_LEN);
        assert_eq!(wide.name.len(), MAX_NAME_LEN * 4, "all 64 characters, whole");

        let exact = Lane::new(0, &"あ".repeat(MAX_NAME_LEN), 36, 0);
        assert_eq!(exact.name.chars().count(), MAX_NAME_LEN, "exactly 64 is not trimmed");

        let short = Lane::new(0, "snare", 36, 0);
        assert_eq!(short.name, "snare", "a short name is left alone");
    }

    /// A sample reference names itself after its file and reports the bytes
    /// a save would embed — and the audio it carries is the audio it was
    /// given.
    #[test]
    fn a_sample_ref_takes_its_name_from_its_path() {
        let collector = basedrop::Collector::new();
        let data = SampleData { rate: 44_100, channels: 1, frames: vec![0.25, -0.5].into() };
        let path = Path::new("/tmp/kit/Kick 01.wav");
        let sample = SampleRef::new(&collector.handle(), path, vec![1, 2, 3, 4, 5], data);

        assert_eq!(sample.path, "/tmp/kit/Kick 01.wav");
        assert_eq!(sample.file_name, "Kick 01.wav");
        assert_eq!(sample.byte_len(), 5);
        assert_eq!(&sample.bytes[..], &[1, 2, 3, 4, 5]);
        assert_eq!(sample.data.rate, 44_100);
        assert_eq!(sample.data.frame_count(), 2);
        assert_eq!(sample.origin, None, "a sample loaded from a file is no chop");
    }

    /// A chop remembers where it was cut from; everything else is as `new`.
    #[test]
    fn a_sample_ref_with_an_origin_keeps_it() {
        let collector = basedrop::Collector::new();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.5].into() };
        let origin =
            ChopOrigin { source_path: String::from("/s/Eye.wav"), start_frame: 10, end_frame: 20 };
        let path = Path::new("/tmp/chops/Eye 1.wav");
        let sample = SampleRef::with_origin(
            &collector.handle(),
            path,
            vec![7],
            data,
            Some(origin.clone()),
        );

        assert_eq!(sample.origin, Some(origin));
        assert_eq!(sample.file_name, "Eye 1.wav");
        assert_eq!(&sample.bytes[..], &[7]);
    }

    /// The origin line names the source's file and the cut's times in the
    /// sample's rate; a sample with no origin has none.
    #[test]
    fn the_origin_text_is_format_origin_over_the_source_name() {
        let collector = basedrop::Collector::new();
        let data = || SampleData { rate: 48_000, channels: 1, frames: vec![0.5].into() };
        let origin = ChopOrigin {
            source_path: String::from("/songs/Eye.wav"),
            start_frame: 1_445_760,
            end_frame: 1_540_320,
        };
        let path = Path::new("/tmp/chops/Eye 1.wav");
        let sample =
            SampleRef::with_origin(&collector.handle(), path, vec![7], data(), Some(origin));
        assert_eq!(
            sample.origin_text().as_deref(),
            Some("from Eye.wav, 0:30.120\u{2013}0:32.090")
        );
        let plain = SampleRef::new(&collector.handle(), path, vec![7], data());
        assert_eq!(plain.origin_text(), None);
    }

    /// A path with no final component keeps the whole path as its name: odd
    /// beats blank, and nothing panics or comes out empty.
    #[test]
    fn a_sample_ref_from_a_pathless_path_still_has_a_name() {
        let collector = basedrop::Collector::new();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0].into() };

        let root = SampleRef::new(&collector.handle(), Path::new("/"), vec![0], data.clone());
        assert_eq!(root.file_name, "/");

        let empty = SampleRef::new(&collector.handle(), Path::new(""), vec![0], data);
        assert_eq!(empty.file_name, "");
    }

    /// Cloning a lane shares the one copy of the audio rather than
    /// duplicating it — what makes an undo entry and a duplicated lane
    /// cheap, and what lets a sounding voice outlive the lane it came from.
    #[test]
    fn cloning_a_lane_shares_its_sample() {
        let collector = basedrop::Collector::new();
        let data = SampleData { rate: 44_100, channels: 2, frames: vec![0.1, 0.2].into() };
        let mut lane = Lane::new(3, "hat", 42, LANE_PALETTE[6]);
        let path = Path::new("/tmp/hat.wav");
        lane.sample = Some(SampleRef::new(&collector.handle(), path, vec![9], data));

        let copy = lane.clone();
        let original = lane.sample.as_ref().unwrap();
        let copied = copy.sample.as_ref().unwrap();

        assert!(std::ptr::eq(&*original.data, &*copied.data), "one copy of the audio");
        assert!(std::ptr::eq(&*original.bytes, &*copied.bytes), "one copy of the file bytes");
        assert_eq!(copied.file_name, "hat.wav");
    }

    // ---- The writer API -------------------------------------------------

    /// A sample reference over one tiny frame of audio, for the tests that
    /// care only that a lane holds *a* sample.
    /// Every field of every lane as text, for the tests that assert a
    /// whole list rather than one value. [`Lane`] has no `PartialEq` — a
    /// `SampleRef` would have to compare whole audio buffers to get one —
    /// and its `Debug` prints every field, the sample as sizes and rates.
    fn fingerprint(lanes: &[Lane]) -> Vec<String> {
        lanes.iter().map(|lane| format!("{lane:?}")).collect()
    }

    fn a_sample(collector: &basedrop::Collector) -> SampleRef {
        let data = SampleData { rate: 44_100, channels: 1, frames: vec![0.5].into() };
        SampleRef::new(&collector.handle(), Path::new("/tmp/kick.wav"), vec![1, 2, 3], data)
    }

    /// A new model is the default lane list, the default switch, and ids
    /// running from 0 — the state the default-state test pins, reached
    /// through the type the rest of the plugin will actually hold.
    #[test]
    fn a_new_model_holds_the_default_lanes() {
        let model = Model::new();
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize);
        assert_eq!(fingerprint(model.lanes()), fingerprint(&default_lanes()));
        assert_eq!(model.sample_pad_switch(), SAMPLE_PAD_SWITCH_DEFAULT);
        assert_eq!(model.lane(0).map(|lane| lane.name.as_str()), Some("Lane 1"));
        assert_eq!(model.index_of(15), Some(15));
        assert!(model.lane(16).is_none(), "nothing has been added yet");
    }

    /// The three progressions of `add_lane`, on a fresh instance: the
    /// seventeenth lane is "Lane 17" on note 52, in the palette colour its id
    /// (16) wraps to: `LANE_PALETTE[6]`.
    #[test]
    fn adding_a_lane_continues_the_name_note_and_colour_progressions() {
        let mut model = Model::new();

        let id = model.add_lane().expect("room for a seventeenth lane");
        assert_eq!(id, 16, "ids continue from the sixteen defaults");

        let lane = model.lane(id).expect("the lane just added");
        assert_eq!(lane.name, "Lane 17");
        assert_eq!(lane.note, 52, "one above the sixteenth default's 51");
        assert_eq!(lane.colour, LANE_PALETTE[6], "16 wrapped past the ten colours");
        assert!(lane.sample.is_none(), "added lanes are empty");
        assert_eq!(lane.volume_db, LANE_VOLUME_DB_DEFAULT);
        assert_eq!(lane.pan, LANE_PAN_DEFAULT);
        assert_eq!(lane.choke, CHOKE_NONE);
        assert_eq!(lane.mode, LaneMode::OneShot);
        assert_eq!(model.index_of(id), Some(16), "added at the end of the list");
    }

    /// The colour progression wraps after all ten palette entries; the name
    /// and note progressions keep counting. The sixteen defaults already
    /// contain the first wrap, so it is pinned there and again at twenty.
    #[test]
    fn the_colour_progression_wraps_at_ten_and_the_others_do_not() {
        let mut model = Model::new();
        // The defaults themselves wrap: lane 11 (index 10) is palette 0 again.
        assert_eq!(model.lanes()[9].colour, LANE_PALETTE[9]);
        assert_eq!(model.lanes()[10].colour, LANE_PALETTE[0], "wrapped");

        // And the added lanes carry on from there: 16, 17, 18 ... 20.
        let first = model.add_lane().unwrap();
        let second = model.add_lane().unwrap();
        assert_eq!(model.lane(first).unwrap().colour, LANE_PALETTE[6]);
        assert_eq!(model.lane(second).unwrap().colour, LANE_PALETTE[7]);
        for _ in 0..2 {
            model.add_lane().unwrap();
        }
        let twentieth = model.add_lane().unwrap();
        assert_eq!(model.lane(twentieth).unwrap().colour, LANE_PALETTE[0], "wrapped at 20");
        assert_eq!(model.lane(twentieth).unwrap().name, "Lane 21");
        assert_eq!(model.lane(twentieth).unwrap().note, 56);
    }

    /// The note progression stops at 127 instead of wrapping to 0: lanes
    /// pile up on the top note, which layers, rather than silently landing
    /// in the bass.
    #[test]
    fn the_note_progression_clamps_at_127() {
        let mut model = Model::new();
        while model.lanes().last().unwrap().note < 127 {
            model.add_lane().expect("below MAX_LANES all the way to 127");
        }
        assert_eq!(model.lanes().len(), 92, "36..=127 is 92 lanes");

        let piled = model.add_lane().expect("still below MAX_LANES");
        assert_eq!(model.lane(piled).unwrap().note, 127);
        let also = model.add_lane().unwrap();
        assert_eq!(model.lane(also).unwrap().note, 127);
    }

    /// `MAX_LANES` is refused silently: `add_lane` returns `None` and the
    /// list is untouched.
    #[test]
    fn adding_stops_at_max_lanes() {
        let mut model = Model::new();
        while (model.lanes().len() as u32) < MAX_LANES {
            assert!(model.add_lane().is_some(), "room below the limit");
        }
        assert_eq!(model.lanes().len(), MAX_LANES as usize);

        assert!(model.add_lane().is_none(), "refused at the limit");
        assert_eq!(model.lanes().len(), MAX_LANES as usize, "and nothing changed");
    }

    /// Names are never renumbered, so adding after a delete can repeat a
    /// name — deliberate (a name is a label, [`Lane::id`] is the identity),
    /// and pinned so that changing it is a decision rather than an
    /// accident.
    #[test]
    fn names_count_from_the_current_lane_count_and_can_repeat() {
        let mut model = Model::new();
        assert!(model.delete_lane(0), "delete 'Lane 1'");
        assert_eq!(model.lanes()[0].name, "Lane 2", "the rest keep their names");

        let added = model.add_lane().unwrap();
        assert_eq!(model.lane(added).unwrap().name, "Lane 16", "fifteen lanes + 1");
        assert_eq!(model.lanes()[14].name, "Lane 16", "the same name as lane 16 — allowed");
        assert_ne!(model.lanes()[14].id, added, "but not the same lane");
    }

    /// Deleting takes the named lane and nothing else, and says whether
    /// there was one. Ids are never reused afterwards.
    #[test]
    fn deleting_removes_one_lane_and_never_reuses_its_id() {
        let mut model = Model::new();
        assert!(model.delete_lane(3));
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize - 1);
        assert!(model.lane(3).is_none());
        assert_eq!(
            model.lanes().iter().map(|lane| lane.id).collect::<Vec<_>>(),
            (0..DEFAULT_LANE_COUNT).filter(|&id| id != 3).collect::<Vec<_>>(),
            "the others keep their ids and their order"
        );

        assert!(!model.delete_lane(3), "already gone");
        assert!(!model.delete_lane(99), "never existed");
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize - 1);

        let added = model.add_lane().unwrap();
        assert_eq!(added, DEFAULT_LANE_COUNT, "the counter does not go back for the hole at 3");
    }

    /// Every lane can be deleted; the empty list is a state, not a panic.
    /// Adding to it starts the names and colours over, but not the notes —
    /// the note progression only counts up, so a new lane never lands on a
    /// note a deleted one had.
    #[test]
    fn deleting_every_lane_leaves_an_empty_model_that_still_adds() {
        let mut model = Model::new();
        for id in 0..DEFAULT_LANE_COUNT {
            assert!(model.delete_lane(id));
        }
        assert!(model.lanes().is_empty());

        let added = model.add_lane().expect("an empty model still adds");
        let lane = model.lane(added).unwrap();
        assert_eq!(lane.name, "Lane 1");
        assert_eq!(lane.colour, LANE_PALETTE[0]);
        assert_eq!(lane.note, 52, "one past the deleted sixteenth lane's 51");
        assert_eq!(lane.id, DEFAULT_LANE_COUNT, "and an id of its own all the same");
    }

    /// A duplicate sits directly below its original, keeps the note (two
    /// lanes on one note layer), gains " copy", shares the one copy of the
    /// audio, and is not loading.
    #[test]
    fn duplicating_copies_the_lane_below_itself_and_shares_its_sample() {
        let collector = basedrop::Collector::new();
        let mut model = Model::new();
        model.set_volume_db(2, -6.0);
        model.set_pan(2, 0.5);
        model.set_choke(2, 4);
        model.set_mode(2, LaneMode::Gate);
        model.set_sample(2, a_sample(&collector));
        model.set_loading(2, true);

        let copy_id = model.duplicate_lane(2).expect("room for a copy");
        assert_eq!(model.index_of(copy_id), Some(3), "directly below the original");
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize + 1);

        let original = model.lane(2).unwrap().clone();
        let copy = model.lane(copy_id).unwrap();
        assert_eq!(copy.name, "Lane 3 copy");
        assert_eq!(copy.note, original.note, "same note — they layer");
        assert_eq!(copy.colour, original.colour);
        assert_eq!(copy.volume_db, -6.0);
        assert_eq!(copy.pan, 0.5);
        assert_eq!(copy.choke, 4);
        assert_eq!(copy.mode, LaneMode::Gate);
        assert!(!copy.loading, "the pending job belongs to the original's id");
        assert!(original.loading, "which still has it");
        assert!(std::ptr::eq(
            &*copy.sample.as_ref().unwrap().data,
            &*original.sample.as_ref().unwrap().data,
        ));
    }

    /// Duplicating is refused where adding is: an unknown lane, and a full
    /// list.
    #[test]
    fn duplicating_is_refused_for_an_unknown_lane_and_at_max_lanes() {
        let mut model = Model::new();
        assert!(model.duplicate_lane(42).is_none(), "no such lane");
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize);

        while (model.lanes().len() as u32) < MAX_LANES {
            model.add_lane();
        }
        assert!(model.duplicate_lane(0).is_none(), "refused at the limit");
        assert_eq!(model.lanes().len(), MAX_LANES as usize);
    }

    /// A duplicate's " copy" suffix is trimmed to the name limit, which
    /// means a name already at the limit duplicates unchanged.
    #[test]
    fn a_duplicates_suffix_is_trimmed_to_the_name_limit() {
        let mut model = Model::new();
        let long = "x".repeat(MAX_NAME_LEN);
        model.rename(0, &long);

        let copy_id = model.duplicate_lane(0).unwrap();
        assert_eq!(model.lane(copy_id).unwrap().name, long, "no room left for ' copy'");

        model.rename(0, &"y".repeat(MAX_NAME_LEN - 2));
        let copy_id = model.duplicate_lane(0).unwrap();
        let name = &model.lane(copy_id).unwrap().name;
        assert_eq!(name.chars().count(), MAX_NAME_LEN);
        assert!(name.ends_with(" c"), "cut mid-suffix, not mid-character: {name}");
    }

    /// Renaming trims to 64 characters on a character boundary, and reports
    /// an unknown lane rather than panicking.
    #[test]
    fn renaming_trims_to_the_name_limit() {
        let mut model = Model::new();
        assert!(model.rename(1, "Kick"));
        assert_eq!(model.lane(1).unwrap().name, "Kick");

        assert!(model.rename(1, &"あ".repeat(100)));
        let name = &model.lane(1).unwrap().name;
        assert_eq!(name.chars().count(), MAX_NAME_LEN, "64 characters");
        assert_eq!(name.len(), MAX_NAME_LEN * 3, "whole ones, not 64 bytes");

        assert!(!model.rename(99, "nobody"), "no such lane");
    }

    /// Reordering moves a lane in display order and changes nothing else —
    /// in particular no note moves, which is the whole point of the order
    /// being display-only.
    #[test]
    fn reordering_moves_display_order_only() {
        let mut model = Model::new();
        let notes_before: Vec<u8> = model.lanes().iter().map(|lane| lane.note).collect();

        assert!(model.reorder(0, 3));
        assert_eq!(
            model.lanes().iter().map(|lane| lane.id).collect::<Vec<_>>(),
            [1, 2, 3, 0].into_iter().chain(4..DEFAULT_LANE_COUNT).collect::<Vec<_>>()
        );
        assert_eq!(model.lane(0).unwrap().note, 36, "lane 0 kept its note");

        let mut notes_after: Vec<u8> = model.lanes().iter().map(|lane| lane.note).collect();
        notes_after.sort_unstable();
        assert_eq!(notes_after, notes_before, "the same notes, on the same lanes");

        assert!(model.reorder(3, 0), "and back");
        assert_eq!(
            model.lanes().iter().map(|lane| lane.id).collect::<Vec<_>>(),
            (0..DEFAULT_LANE_COUNT).collect::<Vec<_>>()
        );
    }

    /// A drop past the last row lands on the last row; a drag from nowhere
    /// does nothing. Neither is an error a user could see.
    #[test]
    fn reordering_clamps_the_destination_and_ignores_a_bad_source() {
        let mut model = Model::new();

        assert!(model.reorder(0, 99), "dropped below the list");
        assert_eq!(model.lanes().last().unwrap().id, 0, "landed on the last row");

        let past_the_end = DEFAULT_LANE_COUNT as usize;
        assert!(!model.reorder(past_the_end, 0), "no row past the last to drag");
        assert!(!model.reorder(usize::MAX, 0));
        assert_eq!(model.lanes().len(), past_the_end);

        assert!(model.reorder(2, 2), "a drag that goes nowhere is still a drag");
        assert_eq!(model.lanes()[2].id, 3);
    }

    /// Reordering an empty model cannot index out of bounds (the clamp of
    /// `to_index` would underflow if it were computed for an empty list).
    #[test]
    fn reordering_an_empty_model_does_nothing() {
        let mut model = Model::new();
        for id in 0..DEFAULT_LANE_COUNT {
            model.delete_lane(id);
        }
        assert!(!model.reorder(0, 0));
    }

    /// Note, colour and choke clamps, at and past every edge.
    #[test]
    fn the_note_colour_and_choke_setters_clamp() {
        let mut model = Model::new();

        assert!(model.set_note(0, 0));
        assert_eq!(model.lane(0).unwrap().note, 0, "0 is a note");
        model.set_note(0, 127);
        assert_eq!(model.lane(0).unwrap().note, 127);
        model.set_note(0, 200);
        assert_eq!(model.lane(0).unwrap().note, 127, "clamped, not wrapped");

        model.set_colour(0, 0xFF_12_34_56);
        assert_eq!(model.lane(0).unwrap().colour, 0x12_34_56, "the top byte is dropped");
        model.set_colour(0, 0);
        assert_eq!(model.lane(0).unwrap().colour, 0, "black is a colour");

        model.set_choke(0, CHOKE_NONE);
        assert_eq!(model.lane(0).unwrap().choke, 0);
        model.set_choke(0, CHOKE_GROUPS);
        assert_eq!(model.lane(0).unwrap().choke, 16, "the last group");
        model.set_choke(0, 17);
        assert_eq!(model.lane(0).unwrap().choke, CHOKE_GROUPS);
        model.set_choke(0, u8::MAX);
        assert_eq!(model.lane(0).unwrap().choke, CHOKE_GROUPS);
    }

    /// Volume clamps to −60..=+12, and a NaN volume lands on 0 dB — not on
    /// −60, which `max`-then-`min` alone would give and which would mute a
    /// lane for a value that means nothing.
    #[test]
    fn the_volume_setter_clamps_and_sends_nan_to_unity() {
        let mut model = Model::new();

        model.set_volume_db(0, -6.0);
        assert_eq!(model.lane(0).unwrap().volume_db, -6.0);

        model.set_volume_db(0, LANE_VOLUME_DB_MIN);
        assert_eq!(model.lane(0).unwrap().volume_db, -60.0, "the edge is in range");
        model.set_volume_db(0, -1_000.0);
        assert_eq!(model.lane(0).unwrap().volume_db, LANE_VOLUME_DB_MIN);
        model.set_volume_db(0, f32::NEG_INFINITY);
        assert_eq!(model.lane(0).unwrap().volume_db, LANE_VOLUME_DB_MIN);

        model.set_volume_db(0, LANE_VOLUME_DB_MAX);
        assert_eq!(model.lane(0).unwrap().volume_db, 12.0);
        model.set_volume_db(0, 1_000.0);
        assert_eq!(model.lane(0).unwrap().volume_db, LANE_VOLUME_DB_MAX);
        model.set_volume_db(0, f32::INFINITY);
        assert_eq!(model.lane(0).unwrap().volume_db, LANE_VOLUME_DB_MAX);

        model.set_volume_db(0, f32::NAN);
        assert_eq!(
            model.lane(0).unwrap().volume_db,
            LANE_VOLUME_DB_DEFAULT,
            "a lost volume is neutral, never silent"
        );
    }

    /// Pan clamps to −1..=+1, and a NaN pan lands centred rather than hard
    /// left.
    #[test]
    fn the_pan_setter_clamps_and_sends_nan_to_centre() {
        let mut model = Model::new();

        model.set_pan(0, -0.25);
        assert_eq!(model.lane(0).unwrap().pan, -0.25);

        model.set_pan(0, LANE_PAN_MIN);
        assert_eq!(model.lane(0).unwrap().pan, -1.0);
        model.set_pan(0, -5.0);
        assert_eq!(model.lane(0).unwrap().pan, LANE_PAN_MIN);
        model.set_pan(0, f32::NEG_INFINITY);
        assert_eq!(model.lane(0).unwrap().pan, LANE_PAN_MIN);

        model.set_pan(0, LANE_PAN_MAX);
        assert_eq!(model.lane(0).unwrap().pan, 1.0);
        model.set_pan(0, 5.0);
        assert_eq!(model.lane(0).unwrap().pan, LANE_PAN_MAX);
        model.set_pan(0, f32::INFINITY);
        assert_eq!(model.lane(0).unwrap().pan, LANE_PAN_MAX);

        model.set_pan(0, f32::NAN);
        assert_eq!(model.lane(0).unwrap().pan, LANE_PAN_DEFAULT, "centred, not hard left");
    }

    /// The mode setter stores both modes.
    #[test]
    fn the_mode_setter_stores_both_modes() {
        let mut model = Model::new();
        assert!(model.set_mode(0, LaneMode::Gate));
        assert_eq!(model.lane(0).unwrap().mode, LaneMode::Gate);
        assert!(model.set_mode(0, LaneMode::OneShot));
        assert_eq!(model.lane(0).unwrap().mode, LaneMode::OneShot);
    }

    /// A setter for a lane that is gone changes nothing and says so — the
    /// case a late loader result or a stale GUI frame produces.
    #[test]
    fn every_setter_reports_an_unknown_lane_and_changes_nothing() {
        let collector = basedrop::Collector::new();
        let mut model = Model::new();
        let before = model.lanes().to_vec();

        assert!(!model.set_note(99, 60));
        assert!(!model.set_colour(99, 0xFF_FF_FF));
        assert!(!model.set_volume_db(99, -3.0));
        assert!(!model.set_pan(99, 1.0));
        assert!(!model.set_choke(99, 2));
        assert!(!model.set_mode(99, LaneMode::Gate));
        assert!(!model.set_sample(99, a_sample(&collector)));
        assert!(!model.clear_sample(99));
        assert!(!model.set_error(99, "nowhere"));
        assert!(!model.set_loading(99, true));
        assert!(!model.rename(99, "nobody"));

        assert_eq!(fingerprint(model.lanes()), fingerprint(&before), "lanes untouched");
    }

    /// The sample writers: a sample arriving ends the loading state and
    /// clears the last message; clearing a lane empties all three; a
    /// message ends the loading state but keeps the sample that is there.
    #[test]
    fn the_sample_writers_track_loading_and_the_lane_message() {
        let collector = basedrop::Collector::new();
        let mut model = Model::new();

        assert!(model.set_loading(0, true));
        assert!(model.lane(0).unwrap().loading);

        assert!(model.set_error(0, "unsupported audio file"));
        let lane = model.lane(0).unwrap();
        assert_eq!(lane.error.as_deref(), Some("unsupported audio file"));
        assert!(!lane.loading, "a message ends the job");
        assert!(lane.sample.is_none());

        model.set_loading(0, true);
        assert!(model.set_sample(0, a_sample(&collector)));
        let lane = model.lane(0).unwrap();
        assert_eq!(lane.sample.as_ref().unwrap().file_name, "kick.wav");
        assert!(lane.error.is_none(), "a load that worked clears the last failure");
        assert!(!lane.loading);

        // A reload that fails keeps the sample that is playing.
        assert!(model.set_error(0, "can't read file"));
        assert!(model.lane(0).unwrap().sample.is_some(), "the old sample stays");

        assert!(model.clear_sample(0));
        let lane = model.lane(0).unwrap();
        assert!(lane.sample.is_none());
        assert!(lane.error.is_none());
        assert!(!lane.loading);
        assert_eq!(lane.name, "Lane 1", "clearing a lane is not deleting it");
        assert_eq!(lane.note, 36);
    }

    /// The instance switch is writable and starts off.
    #[test]
    fn the_sample_pad_switch_is_writable() {
        let mut model = Model::new();
        assert!(model.sample_pad_switch(), "a new instance starts with it on");
        model.set_sample_pad_switch(false);
        assert!(!model.sample_pad_switch());
        model.set_sample_pad_switch(true);
        assert!(model.sample_pad_switch());
    }

    /// The wholesale replace — the path an undo and a project load take —
    /// re-clamps every field of every lane it is given, because those lanes
    /// were not built by a setter.
    #[test]
    fn replacing_the_lanes_clamps_every_field() {
        let mut model = Model::new();

        let mut hostile = Lane::new(0, "Loud", 36, 0);
        hostile.note = 200;
        hostile.colour = 0xDE_AD_BE_EF;
        hostile.volume_db = f32::NAN;
        hostile.pan = -9.0;
        hostile.choke = 99;
        hostile.name = "n".repeat(500);
        hostile.loading = true;
        hostile.error = Some("saved sample couldn't be restored".to_owned());

        model.replace_lanes(vec![hostile], true);

        assert_eq!(model.lanes().len(), 1);
        let lane = &model.lanes()[0];
        assert_eq!(lane.note, 127);
        assert_eq!(lane.colour, 0xAD_BE_EF);
        assert_eq!(lane.volume_db, LANE_VOLUME_DB_DEFAULT);
        assert_eq!(lane.pan, LANE_PAN_MIN);
        assert_eq!(lane.choke, CHOKE_GROUPS);
        assert_eq!(lane.name.chars().count(), MAX_NAME_LEN);
        assert!(!lane.loading, "no job of this instance's can be outstanding for it");
        assert_eq!(
            lane.error.as_deref(),
            Some("saved sample couldn't be restored"),
            "a load reports its own failures through the message"
        );
        assert!(model.sample_pad_switch(), "the switch comes with the lanes");
    }

    /// Replaced lanes get fresh ids from this instance's counter: ids are
    /// never saved, and one from another session must not be taken at its
    /// word.
    #[test]
    fn replacing_the_lanes_assigns_fresh_ids() {
        let mut model = Model::new();
        let lanes = vec![
            Lane::new(7, "Kick", 36, LANE_PALETTE[0]),
            Lane::new(7, "Snare", 38, LANE_PALETTE[1]),
        ];

        model.replace_lanes(lanes, false);
        let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
        let next = DEFAULT_LANE_COUNT;
        let why = "continuing past the sixteen defaults, no duplicates";
        assert_eq!(ids, vec![next, next + 1], "{why}");
        assert_eq!(model.lane(next).unwrap().name, "Kick");

        let added = model.add_lane().unwrap();
        assert_eq!(added, next + 2);
    }

    /// A replace continues the note progression from the highest note it
    /// loaded, and an empty one restarts at C1.
    #[test]
    fn replacing_the_lanes_resets_the_note_progression() {
        let mut model = Model::new();
        model.replace_lanes(vec![Lane::new(0, "High", 100, 0)], false);

        let added = model.add_lane().unwrap();
        assert_eq!(model.lane(added).unwrap().note, 101, "one above the highest loaded");
        assert_eq!(model.lane(added).unwrap().name, "Lane 2");

        model.replace_lanes(Vec::new(), false);
        assert!(model.lanes().is_empty());
        let added = model.add_lane().unwrap();
        assert_eq!(model.lane(added).unwrap().note, DEFAULT_FIRST_NOTE);
    }

    /// More lanes than `MAX_LANES` in a snapshot: the excess is dropped, so
    /// a corrupt project cannot ask for unbounded lanes.
    #[test]
    fn replacing_the_lanes_drops_the_excess_past_max_lanes() {
        let mut model = Model::new();
        let too_many: Vec<Lane> =
            (0..MAX_LANES + 50).map(|i| Lane::new(i, "Lane", 36, 0)).collect();

        model.replace_lanes(too_many, false);
        assert_eq!(model.lanes().len(), MAX_LANES as usize);
        assert!(model.add_lane().is_none(), "and the model is full");
    }

    /// A replace keeps a lane's sample, shared and not re-decoded — what
    /// makes an undo entry cheap.
    #[test]
    fn replacing_the_lanes_keeps_shared_samples() {
        let collector = basedrop::Collector::new();
        let mut model = Model::new();
        model.set_sample(0, a_sample(&collector));

        let kept = model.lanes().to_vec();
        let before = kept[0].sample.clone().unwrap();
        model.replace_lanes(kept, false);

        let after = model.lanes()[0].sample.as_ref().unwrap();
        assert!(std::ptr::eq(&*before.data, &*after.data), "one copy of the audio");
        assert!(std::ptr::eq(&*before.bytes, &*after.bytes));
    }

    // ---- Publishing -----------------------------------------------------

    /// A sample allocated against the **model's own** collector, so that
    /// the publishing tests can watch it being freed. Stereo and two
    /// frames, to tell the published audio apart from `a_sample`'s.
    fn a_sample_of(model: &Model) -> SampleRef {
        let data = SampleData { rate: 48_000, channels: 2, frames: vec![0.25, -0.25].into() };
        SampleRef::new(model.handle(), Path::new("/tmp/snare.wav"), vec![7, 7], data)
    }

    /// A fresh instance already has a snapshot for the audio thread to
    /// read, and it is empty: sixteen lanes with no samples have nothing to
    /// play. The switch is there from the first block too.
    #[test]
    fn a_new_model_publishes_an_empty_list_and_the_default_switch() {
        let model = Model::new();
        let published = model.published().get();

        assert!(published.lanes().is_empty(), "sixteen empty lanes play nothing");
        assert_eq!(published.sample_pad_switch(), SAMPLE_PAD_SWITCH_DEFAULT);
        assert!(published.lane(0).is_none(), "an empty lane is not published at all");
    }

    /// What a published lane carries: the fields a voice copies at its
    /// note-on, the volume and pan the lane had when the sample arrived,
    /// and the audio itself. Lanes without a sample are not in the list.
    #[test]
    fn only_lanes_with_a_sample_are_published_and_they_carry_their_settings() {
        let mut model = Model::new();
        model.set_volume_db(1, -6.0);
        model.set_pan(1, -0.5);
        model.set_choke(1, 3);
        model.set_mode(1, LaneMode::Gate);

        let sample = a_sample_of(&model);
        assert!(model.set_sample(1, sample));

        let published = model.published().get();
        assert_eq!(published.lanes().len(), 1, "one lane has a sample");

        let lane = &published.lanes()[0];
        assert_eq!(lane.id, 1);
        assert_eq!(lane.note, 37, "lane 2 of the defaults, C#1");
        assert_eq!(lane.choke, 3);
        assert_eq!(lane.mode, LaneMode::Gate);
        assert_eq!(lane.volume_db(), -6.0, "set before the sample arrived, carried over");
        assert_eq!(lane.pan(), -0.5);
        assert_eq!(lane.sample.rate, 48_000);
        assert_eq!(lane.sample.channels, 2);
        assert_eq!(published.lane(1).map(|found| found.id), Some(1), "found by id");
        assert!(published.lane(0).is_none(), "the empty lane above it is not published");
    }

    /// The swap that publishing is built on: a writer publishes a new list
    /// while a reader — the audio thread, mid-block — holds the old one, and
    /// both lists stay whole. The old one keeps its lane, its atomics and the
    /// audio its lane points at, even though the model has since deleted
    /// that lane and collected.
    #[test]
    fn publishing_a_new_list_leaves_a_reader_holding_the_old_one() {
        let mut model = Model::new();
        let first = a_sample_of(&model);
        model.set_sample(0, first);

        // The audio thread's `SharedCell::get` for this block.
        let held = model.published().get();
        assert_eq!(held.lanes().len(), 1);
        assert_eq!(held.lanes()[0].id, 0);

        // Two structural changes while that block is still rendering: a
        // second sample arrives and the first lane is deleted outright.
        let second = a_sample_of(&model);
        model.set_sample(3, second);
        assert!(model.delete_lane(0));
        model.collect();

        let now = model.published().get();
        assert_eq!(now.lanes().len(), 1, "the new list has only the surviving lane");
        assert_eq!(now.lanes()[0].id, 3);
        assert!(!std::ptr::eq(&*held, &*now), "a structural change is a new snapshot");

        // The reader's list is untouched, and its audio is still there to
        // render: this is the use-after-free the collector must not cause.
        assert_eq!(held.lanes().len(), 1);
        assert_eq!(held.lanes()[0].id, 0, "a lane the model no longer has");
        assert_eq!(held.lanes()[0].volume_db(), 0.0);
        assert_eq!(&held.lanes()[0].sample.frames[..], &[0.25, -0.25]);

        // Alive: the two snapshots, the surviving lane's bytes and audio,
        // and the deleted lane's audio, which the reader still holds (its
        // file bytes went the moment the lane did — nothing else had them).
        assert_eq!(model.alloc_count(), 5);

        // Once the reader lets go, the next collect reclaims both.
        drop(held);
        model.collect();
        assert_eq!(model.alloc_count(), 3, "the retired list and its audio are freed");
    }

    /// Volume and pan reach the audio thread through the atomics in the
    /// snapshot it already holds — no new list for a knob drag — and the
    /// clamps still apply on the way.
    #[test]
    fn volume_and_pan_reach_the_audio_thread_without_a_new_list() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        model.set_sample(0, sample);

        let held = model.published().get();
        assert!(model.set_volume_db(0, -12.0));
        assert!(model.set_pan(0, 0.75));

        assert!(std::ptr::eq(&*held, &*model.published().get()), "the same snapshot");
        assert_eq!(held.lanes()[0].volume_db(), -12.0, "changed in place");
        assert_eq!(held.lanes()[0].pan(), 0.75);
        assert_eq!(model.lane(0).unwrap().volume_db, -12.0, "and kept for the next save");
        assert_eq!(model.lane(0).unwrap().pan, 0.75);

        // Hostile values are clamped before they are stored, so nothing
        // the audio thread reads out of an atomic needs checking again.
        model.set_volume_db(0, f32::NAN);
        model.set_pan(0, -4.0);
        assert_eq!(held.lanes()[0].volume_db(), LANE_VOLUME_DB_DEFAULT);
        assert_eq!(held.lanes()[0].pan(), LANE_PAN_MIN);

        model.set_volume_db(0, 99.0);
        assert_eq!(held.lanes()[0].volume_db(), LANE_VOLUME_DB_MAX);
    }

    /// The four continuous lane settings land in the snapshot the audio
    /// thread already holds — the published `Shared` is the same pointer —
    /// and the clamps apply.
    #[test]
    fn continuous_lane_settings_reach_the_audio_thread_without_a_new_list() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        model.set_sample(0, sample);

        let held = model.published().get();
        assert!(model.set_fine_pct(0, 1.25));
        assert!(model.set_attack_ms(0, 30.0));
        assert!(model.set_release_ms(0, 250.0));
        assert!(model.set_pitch_st(0, -3.5));

        assert!(std::ptr::eq(&*held, &*model.published().get()), "the same snapshot");
        let lane = &held.lanes()[0];
        assert_eq!(lane.fine_pct(), clamp_fine_pct(1.25), "changed in place");
        assert_eq!(lane.fine_pct(), model.lane(0).unwrap().fine_pct);
        assert_eq!(lane.attack_ms(), 30.0);
        assert_eq!(lane.release_ms(), 250.0);
        assert_eq!(lane.pitch_st(), -3.5);

        model.set_fine_pct(0, f32::NAN);
        model.set_attack_ms(0, 1e9);
        assert_eq!(lane.fine_pct(), FINE_DEFAULT);
        assert_eq!(lane.attack_ms(), ATTACK_MS_MAX);
        assert!(std::ptr::eq(&*held, &*model.published().get()), "still the same snapshot");
    }

    /// The structural lane settings republish: a new snapshot carrying
    /// them, while a reader keeps the old one with the old values.
    #[test]
    fn structural_lane_settings_publish_a_new_list() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        model.set_sample(0, sample);
        model.set_fine_pct(0, 2.0);

        let held = model.published().get();
        assert_eq!(held.lanes()[0].output, OUTPUT_MAIN);
        assert!(model.set_output(0, 5));
        let now = model.published().get();
        assert!(!std::ptr::eq(&*held, &*now), "a new snapshot");
        assert_eq!(now.lanes()[0].output, 5);
        assert_eq!(held.lanes()[0].output, OUTPUT_MAIN, "the old one is untouched");
        assert_eq!(now.lanes()[0].fine_pct(), model.lane(0).unwrap().fine_pct, "carried");

        let cut = !now.lanes()[0].cut_itself;
        assert!(model.set_cut_itself(0, cut));
        assert!(model.set_time_mode(0, TimeMode::Stretch));
        assert!(model.set_length(0, Length::Beats(NoteLength::Bar)));
        let last = model.published().get();
        assert!(!std::ptr::eq(&*now, &*last));
        assert_eq!(last.lanes()[0].cut_itself, cut);
        assert_eq!(last.lanes()[0].time_mode, TimeMode::Stretch);
        assert_eq!(last.lanes()[0].length_beats, 4.0);
        assert_eq!(last.lanes()[0].output, 5);

        assert!(model.set_length(0, Length::AsCut));
        assert_eq!(model.published().get().lanes()[0].length_beats, 0.0, "as cut");
    }

    /// A lane with no sample is not in the snapshot, so its volume waits in
    /// the lane — and the publish that gives it a sample carries it over.
    #[test]
    fn a_volume_set_on_an_empty_lane_waits_for_its_sample() {
        let mut model = Model::new();
        assert!(model.set_volume_db(2, -24.0));
        assert!(model.published().get().lanes().is_empty(), "nothing to store it in");

        let sample = a_sample_of(&model);
        model.set_sample(2, sample);
        assert_eq!(model.published().get().lanes()[0].volume_db(), -24.0);
    }

    /// The switch is an atomic in the snapshot for the same reason, and the
    /// model keeps its own copy for the next list and for a save.
    #[test]
    fn the_switch_reaches_the_audio_thread_without_a_new_list() {
        let mut model = Model::new();
        let held = model.published().get();

        model.set_sample_pad_switch(true);
        assert!(std::ptr::eq(&*held, &*model.published().get()), "the same snapshot");
        assert!(held.sample_pad_switch(), "the audio thread sees it in place");
        assert!(model.sample_pad_switch());

        // And a structural change carries it into the new list.
        let sample = a_sample_of(&model);
        model.set_sample(0, sample);
        assert!(model.published().get().sample_pad_switch());

        model.set_sample_pad_switch(false);
        assert!(!model.published().get().sample_pad_switch());
    }

    /// Every structural writer publishes: the list the audio thread reads
    /// follows the lanes through a note, a choke group, a mode, a reorder,
    /// a duplicate, a clear, a delete and a wholesale replace.
    #[test]
    fn every_structural_writer_publishes() {
        let mut model = Model::new();
        let first = a_sample_of(&model);
        let second = a_sample_of(&model);
        model.set_sample(0, first);
        model.set_sample(1, second);

        let ids = |model: &Model| -> Vec<u32> {
            model.published().get().lanes().iter().map(|lane| lane.id).collect()
        };
        assert_eq!(ids(&model), vec![0, 1]);

        assert!(model.set_note(0, 60));
        assert_eq!(model.published().get().lanes()[0].note, 60);

        assert!(model.set_choke(0, 16));
        assert_eq!(model.published().get().lanes()[0].choke, 16);

        assert!(model.set_mode(0, LaneMode::Gate));
        assert_eq!(model.published().get().lanes()[0].mode, LaneMode::Gate);

        assert!(model.reorder(0, 1), "display order, which the engine walks in");
        assert_eq!(ids(&model), vec![1, 0]);

        let copy = model.duplicate_lane(0).expect("room for a copy");
        assert_eq!(ids(&model), vec![1, 0, copy], "the copy shares the audio and plays");

        assert!(model.add_lane().is_some());
        assert_eq!(ids(&model), vec![1, 0, copy], "an empty lane publishes nothing");

        assert!(model.clear_sample(0));
        assert_eq!(ids(&model), vec![1, copy]);

        assert!(model.delete_lane(1));
        assert_eq!(ids(&model), vec![copy]);

        let lanes = model.lanes().to_vec();
        model.replace_lanes(lanes, true);
        assert_eq!(model.published().get().lanes().len(), 1, "the replace published too");
        assert!(model.published().get().sample_pad_switch());
    }

    /// The fields the audio thread never sees do not build a snapshot: a
    /// rename, a colour, a lane message and the loading flag all leave the
    /// published list exactly where it was.
    #[test]
    fn an_unpublished_field_does_not_build_a_new_list() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        model.set_sample(0, sample);

        let held = model.published().get();
        assert!(model.rename(0, "kick"));
        assert!(model.set_colour(0, 0xFF_FF_FF));
        assert!(model.set_error(0, "could not reload"));
        assert!(model.set_loading(0, true));

        assert!(std::ptr::eq(&*held, &*model.published().get()), "nothing republished");
        assert!(model.lane(0).unwrap().loading, "the lane changed all the same");
    }

    /// Writers collect, so garbage does not pile up while the editor is
    /// closed — what a headless project restore of many samples depends on.
    #[test]
    fn writers_collect_so_nothing_piles_up_with_the_editor_closed() {
        let mut model = Model::new();

        for _ in 0..50 {
            let sample = a_sample_of(&model);
            model.set_sample(0, sample);
        }

        // Steady state: the published snapshot plus the one sample's file
        // bytes and decoded audio. The forty-nine replaced samples and the
        // snapshots that carried them are gone, with no editor and no
        // explicit collect anywhere in the loop.
        assert_eq!(model.alloc_count(), 3);

        model.clear_sample(0);
        assert_eq!(model.alloc_count(), 1, "only the empty snapshot is left");
    }

    /// The published side crosses threads and the model travels to the
    /// loader's: the bounds `basedrop::SharedCell` and the plugin's
    /// `Arc<Model>` both need.
    #[test]
    fn the_published_side_crosses_threads() {
        fn send_and_sync<T: Send + Sync>() {}
        fn send<T: Send>() {}

        send_and_sync::<PublishedCell>();
        send_and_sync::<PublishedLanes>();
        send::<Model>();
    }

    /// The marker the GUI's activity lights read: zero on a lane that has
    /// not played, and moving every time the audio thread counts a voice
    /// start.
    #[test]
    fn a_lanes_activity_marker_starts_at_zero_and_counts_up() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        assert!(model.set_sample(0, sample));

        let published = model.published().get();
        let lane = published.lane(0).expect("the lane with the sample is published");
        assert_eq!(lane.activity(), 0, "nothing has played it yet");

        lane.mark_activity();
        assert_eq!(lane.activity(), 1);
        lane.mark_activity();
        assert_eq!(lane.activity(), 2, "a second hit is a second value");
    }

    /// A republish carries each lane's marker over
    /// ([`PublishedLane::of`]), because the GUI reads the count as a clock
    /// and not as a quantity: a snapshot that restarted it at zero would
    /// flash every light in the list the moment a user changed one lane's
    /// note. A lane that was not in the old list starts at zero, having
    /// never played.
    #[test]
    fn a_republish_carries_a_lanes_activity_marker_over() {
        let mut model = Model::new();
        let first = a_sample_of(&model);
        assert!(model.set_sample(0, first));

        let hits = 3;
        {
            let published = model.published().get();
            let lane = published.lane(0).expect("published");
            for _ in 0..hits {
                lane.mark_activity();
            }
        }

        // Two structural changes: one to the lane that has been playing,
        // one that brings a second lane into the list.
        assert!(model.set_note(0, 48));
        let second = a_sample_of(&model);
        assert!(model.set_sample(1, second));

        let published = model.published().get();
        assert_eq!(published.lane(0).map(PublishedLane::activity), Some(hits));
        assert_eq!(published.lane(1).map(PublishedLane::activity), Some(0), "never played");
    }

    /// A lane that leaves the published list and comes back has nothing to
    /// carry: its marker starts again at zero, and the light that reads it
    /// sees one change — which is honest, because the sample it is lit for
    /// is a new one.
    #[test]
    fn a_lane_that_loses_its_sample_loses_its_activity_marker_with_it() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        assert!(model.set_sample(0, sample));
        model.published().get().lane(0).expect("published").mark_activity();

        assert!(model.clear_sample(0), "out of the list");
        let replacement = a_sample_of(&model);
        assert!(model.set_sample(0, replacement), "and back into it");

        assert_eq!(model.published().get().lane(0).map(PublishedLane::activity), Some(0));
    }

    /// A counting stand-in for the editor's repaint notifier: what the
    /// hook tests ring, and the shape [`crate::editor::create`] installs
    /// the real one in (a `Send + Sync` closure over shared state).
    fn counting_hook() -> (RepaintHook, Arc<AtomicU32>) {
        let rings = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&rings);
        (
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
            rings,
        )
    }

    /// The repaint hook rings when a writer asks it to, and only then. A
    /// write does not ring it by itself: publishing is for the audio thread,
    /// and most writers are the GUI's own — already drawing the frame that
    /// made the change.
    #[test]
    fn the_repaint_hook_rings_when_a_writer_asks_and_not_before() {
        let mut model = Model::new();
        let (hook, rings) = counting_hook();
        model.set_repaint_hook(hook);

        let sample = a_sample_of(&model);
        assert!(model.set_sample(0, sample));
        assert_eq!(rings.load(Ordering::Relaxed), 0, "a write is not a ring");

        model.request_repaint();
        assert_eq!(rings.load(Ordering::Relaxed), 1);
        model.request_repaint();
        assert_eq!(rings.load(Ordering::Relaxed), 2, "each ask is one ring");
    }

    /// Ringing with no hook installed is safe and does nothing: the state
    /// a headless project restore writes in, before the host has opened a
    /// window (or after it has closed the only one it ever opened).
    #[test]
    fn a_model_with_no_repaint_hook_can_still_be_rung() {
        let mut model = Model::new();

        model.request_repaint();

        // And installing one later does not retroactively ring it.
        let (hook, rings) = counting_hook();
        model.set_repaint_hook(hook);
        assert_eq!(rings.load(Ordering::Relaxed), 0);
    }

    /// One hook, replaced rather than added to: the host may build an
    /// editor over this model more than once, and a second window must not
    /// mean two rings per result.
    #[test]
    fn installing_a_second_repaint_hook_replaces_the_first() {
        let mut model = Model::new();
        let (first, first_rings) = counting_hook();
        let (second, second_rings) = counting_hook();

        model.set_repaint_hook(first);
        model.set_repaint_hook(second);
        model.request_repaint();

        assert_eq!(first_rings.load(Ordering::Relaxed), 0, "the replaced hook is let go");
        assert_eq!(second_rings.load(Ordering::Relaxed), 1);
    }

    // ---- Undo and redo --------------------------------------------------

    /// A model nobody has edited has nowhere to go: both steps report
    /// that, and asking anyway changes nothing.
    #[test]
    fn a_fresh_model_has_no_history() {
        let mut model = Model::new();

        assert!(!model.can_undo());
        assert!(!model.can_redo());
        assert_eq!((model.undo_depth(), model.redo_depth()), (0, 0));

        assert!(!model.undo(), "nothing to undo");
        assert!(!model.redo(), "nothing to redo");
        assert_eq!(fingerprint(model.lanes()), fingerprint(&default_lanes()));
    }

    /// The shape of one gesture: edit, commit, and the state before it is a
    /// step back — which redo then steps forward into again.
    #[test]
    fn a_committed_gesture_steps_back_and_forward_again() {
        let mut model = Model::new();
        assert!(model.set_note(0, 60));
        assert!(model.commit(), "the gesture changed something");
        assert_eq!(model.undo_depth(), 1);
        assert!(model.can_undo());
        assert!(!model.can_redo(), "nothing has been undone yet");

        assert!(model.undo());
        assert_eq!(model.lane(0).unwrap().note, 36, "back to the lane's own note");
        assert_eq!((model.undo_depth(), model.redo_depth()), (0, 1));
        assert!(model.can_redo());

        assert!(model.redo());
        assert_eq!(model.lane(0).unwrap().note, 60, "forward into the edit again");
        assert_eq!((model.undo_depth(), model.redo_depth()), (1, 0));

        assert!(!model.redo(), "and no further");
    }

    /// A knob dragged across a second is **one** step back, not sixty: the
    /// setters keep no history, and the gesture's end is a single commit
    /// (a drag is one entry, committed at release).
    #[test]
    fn a_drag_is_one_step_back_not_sixty() {
        let mut model = Model::new();

        for frame in 0..60 {
            assert!(model.set_volume_db(2, -6.0 + frame as f32 * 0.1));
        }
        assert!(model.commit());

        assert_eq!(model.undo_depth(), 1, "one gesture, one entry");
        assert!(model.undo());
        assert_eq!(model.lane(2).unwrap().volume_db, 0.0, "where the drag started");
    }

    /// A gesture that ended where it started is not an edit: a rename to
    /// the same name, a drag returned to its old value, a reorder dropped
    /// back in place. The caller commits regardless and the model decides.
    #[test]
    fn a_gesture_that_changed_nothing_commits_nothing() {
        let mut model = Model::new();

        assert!(model.rename(0, "Lane 1"), "the lane was there");
        assert!(!model.commit(), "but its name is what it already was");

        assert!(model.set_pan(1, 0.5));
        assert!(model.set_pan(1, LANE_PAN_DEFAULT));
        assert!(!model.commit(), "the drag came home");

        assert!(model.reorder(3, 5));
        assert!(model.reorder(5, 3));
        assert!(!model.commit(), "the row was dropped where it came from");

        assert_eq!(model.undo_depth(), 0);
        assert!(!model.can_undo(), "and so there is still nothing to undo");
    }

    /// A new edit clears the redo stack: the future redo would have
    /// replayed is not the one this edit leads to.
    #[test]
    fn a_new_edit_clears_the_redo_stack() {
        let mut model = Model::new();
        assert!(model.set_choke(0, 3));
        assert!(model.commit());
        assert!(model.undo());
        assert_eq!(model.redo_depth(), 1);

        assert!(model.set_mode(1, LaneMode::Gate));
        assert!(model.commit(), "a different gesture from here");
        assert_eq!(model.redo_depth(), 0, "the choke group is not a future any more");
        assert!(!model.can_redo());
        assert!(!model.redo());

        // And the edit that replaced it is still undoable.
        assert!(model.undo());
        assert_eq!(model.lane(1).unwrap().mode, LaneMode::OneShot);
        assert_eq!(model.lane(0).unwrap().choke, CHOKE_NONE, "the undone edit stayed undone");
    }

    /// An edit nobody committed is still undoable: `undo` commits it first,
    /// so the state on screen is a step and the step lands on the last
    /// committed one.
    #[test]
    fn an_uncommitted_edit_is_still_undoable() {
        let mut model = Model::new();
        assert!(model.rename(0, "kick"));
        assert_eq!(model.undo_depth(), 0, "nothing has been committed");
        assert!(model.can_undo(), "but there is an edit to step back over");

        assert!(model.undo());
        assert_eq!(model.lane(0).unwrap().name, "Lane 1");
        assert!(model.can_redo(), "and forward into it again");
        assert!(model.redo());
        assert_eq!(model.lane(0).unwrap().name, "kick");
    }

    /// An uncommitted edit also makes `redo` a no-op, which is the same
    /// rule from the other side: a new edit clears the redo stack.
    #[test]
    fn an_uncommitted_edit_closes_the_way_forward() {
        let mut model = Model::new();
        assert!(model.set_note(0, 40));
        assert!(model.commit());
        assert!(model.undo());
        assert!(model.can_redo());

        assert!(model.set_colour(2, 0x00_00_FF));
        assert!(!model.can_redo(), "an edit, even an uncommitted one, is a new present");
        assert!(!model.redo());
        assert_eq!(model.lane(0).unwrap().note, 36, "and the undone edit is not back");
    }

    /// The stacks are bounded at [`UNDO_DEPTH`], and it is the **oldest**
    /// step that goes: a hundred and fifty gestures leave exactly the last
    /// hundred to retrace.
    #[test]
    fn the_history_stops_at_the_bounded_depth() {
        let mut model = Model::new();

        for step in 0..150 {
            assert!(model.rename(0, &format!("name {step}")));
            assert!(model.commit());
        }
        assert_eq!(model.undo_depth(), UNDO_DEPTH, "the oldest fifty have gone");

        for step in 0..UNDO_DEPTH {
            assert!(model.undo(), "step {step} of a hundred");
        }
        assert!(!model.undo(), "and there is no hundred and first");

        // Gesture `step` committed the name the one before it left, so the
        // oldest step still held is the state after gesture 49: "name 49".
        assert_eq!(model.lane(0).unwrap().name, "name 49", "as far back as it goes");
        assert_eq!(model.redo_depth(), UNDO_DEPTH, "all hundred are ahead again");
    }

    /// Undoing a delete brings the lane back whole — its id, its settings
    /// and the one copy of its sample — and publishes it, so the audio
    /// thread can play it again.
    #[test]
    fn undoing_a_delete_brings_the_lane_and_its_sample_back() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        assert!(model.set_sample(1, sample));
        assert!(model.set_volume_db(1, -12.0));
        assert!(model.commit());

        let before = model.lane(1).unwrap().sample.clone().unwrap();
        assert!(model.delete_lane(1));
        assert!(model.commit());
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize - 1);
        assert!(model.published().get().lane(1).is_none(), "and nothing to play");

        assert!(model.undo());
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize);
        let back = model.lane(1).unwrap();
        assert_eq!(back.id, 1, "the same lane, not a new one");
        assert_eq!(back.name, "Lane 2");
        assert_eq!(back.note, 37);
        assert_eq!(back.volume_db, -12.0, "with the volume it was deleted at");

        let sample = back.sample.as_ref().unwrap();
        assert!(std::ptr::eq(&*sample.data, &*before.data), "one copy of the audio");
        assert!(std::ptr::eq(&*sample.bytes, &*before.bytes), "and of the file bytes");

        let published = model.published().get();
        let played = published.lane(1).expect("the restored lane is playable again");
        assert_eq!(played.volume_db(), -12.0);
        assert_eq!(&played.sample.frames[..], &[0.25, -0.25]);
    }

    /// Ids survive an undo, because an entry came from this instance and
    /// not from a file: a loader job still in flight for a lane finds that
    /// lane when it reports (unlike [`Model::replace_lanes`], which hands
    /// out fresh ids).
    #[test]
    fn undoing_keeps_the_ids_a_loader_job_is_waiting_on() {
        let mut model = Model::new();
        assert!(model.set_loading(5, true));
        assert!(model.rename(5, "to be filled"));
        assert!(model.commit());
        assert!(model.undo());

        let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
        let defaults: Vec<u32> = (0..DEFAULT_LANE_COUNT).collect();
        assert_eq!(ids, defaults, "every lane is still itself");

        // The job's result lands on the lane that asked for it.
        let sample = a_sample_of(&model);
        assert!(model.set_sample(5, sample), "the loader's lane is still there");
        assert!(model.lane(5).unwrap().sample.is_some());
    }

    /// What an undo entry does not carry, it does not restore: the message
    /// a failed load left and a decode that is still running describe this
    /// session, not the state being stepped back to.
    #[test]
    fn undoing_leaves_this_sessions_message_and_loading_flag_alone() {
        let mut model = Model::new();
        assert!(model.set_note(0, 50));
        assert!(model.commit());

        // After the commit: a drop starts a decode on one lane and a
        // failure leaves a message on another.
        assert!(model.set_loading(0, true));
        assert!(model.set_error(1, "could not read that file"));

        assert!(model.undo());
        assert_eq!(model.lane(0).unwrap().note, 36, "the note stepped back");
        assert!(model.lane(0).unwrap().loading, "the decode is still running");
        assert_eq!(
            model.lane(1).unwrap().error.as_deref(),
            Some("could not read that file"),
            "and the message still applies"
        );

        // Neither of them counted as an edit, either: the step back was
        // over the note, and the stack is empty again.
        assert_eq!(model.undo_depth(), 0);
    }

    /// The instance switch is model state, so it is part of a step too (an
    /// undo entry is the persistent part of the model).
    #[test]
    fn the_switch_steps_back_with_everything_else() {
        // A new instance starts with it on, so the gesture is turning it off.
        let mut model = Model::new();
        model.set_sample_pad_switch(false);
        assert!(model.commit());

        assert!(model.undo());
        assert!(model.sample_pad_switch(), "on again");
        assert!(model.published().get().sample_pad_switch(), "for the audio thread too");

        assert!(model.redo());
        assert!(!model.sample_pad_switch());
        assert!(!model.published().get().sample_pad_switch());
    }

    /// Undoing an add puts the note progression back where it was: the next
    /// lane added lands on the note the undone one did, not past it.
    #[test]
    fn undoing_an_add_puts_the_note_progression_back() {
        let mut model = Model::new();
        let seventeenth = model.add_lane().unwrap();
        assert_eq!(model.lane(seventeenth).unwrap().note, 52);
        assert!(model.commit());

        assert!(model.undo());
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize);

        let again = model.add_lane().unwrap();
        assert_eq!(model.lane(again).unwrap().note, 52, "the next note up from the sixteen left");
        assert_ne!(again, seventeenth, "a new lane all the same: ids are never reused");
    }

    /// A project load is not a step back: `replace_lanes` forgets the
    /// history, because its entries describe the lanes the project
    /// replaced.
    #[test]
    fn replacing_the_lanes_forgets_the_history() {
        let mut model = Model::new();
        assert!(model.rename(0, "kick"));
        assert!(model.commit());
        assert!(model.undo());
        assert_eq!((model.undo_depth(), model.redo_depth()), (0, 1));

        model.replace_lanes(vec![Lane::new(0, "loaded", 60, 0x11_22_33)], true);
        assert_eq!((model.undo_depth(), model.redo_depth()), (0, 0));
        assert!(!model.can_undo(), "and nothing is pending against the old baseline");
        assert!(!model.can_redo());
        assert!(!model.undo());
        assert_eq!(model.lanes().len(), 1, "the loaded project stands");

        // History starts again from what was loaded.
        assert!(model.set_note(model.lanes()[0].id, 62));
        assert!(model.commit());
        assert!(model.undo());
        assert_eq!(model.lanes()[0].note, 60);
    }

    /// A step that falls off the end of the stack lets its sample go: the
    /// bound is there to free memory, not just to cap a `Vec`.
    #[test]
    fn a_step_that_falls_off_the_stack_frees_its_sample() {
        let mut model = Model::new();
        let sample = a_sample_of(&model);
        assert!(model.set_sample(0, sample));
        assert!(model.commit());

        assert!(model.clear_sample(0));
        assert!(model.commit());
        assert_eq!(
            model.alloc_count(), 3,
            "the published snapshot, and the audio and bytes the history holds"
        );

        for step in 0..UNDO_DEPTH {
            assert!(model.rename(0, &format!("name {step}")));
            assert!(model.commit());
        }

        assert_eq!(model.alloc_count(), 1, "only the published snapshot is left");
        assert!(model.undo(), "and the hundred names are still there to retrace");
        assert!(model.lane(0).unwrap().sample.is_none());
    }

    // ---- Saving and loading ---------------------------------------------

    /// What the two ends of a project load owe each other: the lanes land
    /// immediately, under this instance's own fresh ids, and the samples
    /// come back as work for the loader, paired with those ids.
    #[test]
    fn loading_a_snapshot_pairs_fresh_ids_with_the_samples_still_to_decode() {
        let mut saved = Model::new();
        let sample = a_sample_of(&saved);
        let kick = saved.lanes()[0].id;
        assert!(saved.rename(kick, "Kick"));
        assert!(saved.set_sample(kick, sample));
        saved.set_sample_pad_switch(true);

        let mut loaded = Model::new();
        loaded.add_lane(); // so the id counter is already past the defaults
        assert!(loaded.rename(0, "edited before the load"));
        assert!(loaded.commit());

        let pending = loaded.load_snapshot(saved.snapshot());

        let saved_count = DEFAULT_LANE_COUNT as usize;
        assert_eq!(
            loaded.lanes().len(),
            saved_count,
            "the saved lanes, not this instance's seventeen"
        );
        assert_eq!(loaded.lanes()[0].name, "Kick");
        assert!(loaded.sample_pad_switch(), "and the instance switch with them");
        assert_eq!(
            loaded.lanes().iter().map(|lane| lane.id).collect::<Vec<_>>(),
            (DEFAULT_LANE_COUNT + 1..DEFAULT_LANE_COUNT + 1 + DEFAULT_LANE_COUNT)
                .collect::<Vec<_>>(),
            "fresh ids, none of them a saved one"
        );
        assert!(!loaded.can_undo(), "a project load is not a step to undo past");

        assert!(
            loaded.lanes()[0].sample.is_none(),
            "the lane plays once the loader has decoded for it"
        );
        assert!(loaded.published().get().lanes().is_empty(), "so there is nothing to play yet");

        assert_eq!(pending.len(), 1, "one lane had a sample");
        let (id, sample) = &pending[0];
        assert_eq!(*id, loaded.lanes()[0].id, "the lane the loader must report to");
        assert_eq!(sample.path, "/tmp/snare.wav");
        assert_eq!(sample.file_name, "snare.wav");
        assert_eq!(sample.bytes.as_deref(), Some([7u8, 7].as_slice()), "the embedded file");
    }

    /// The sample a load hands back is the one the model then plays: the
    /// pass through `set_sample` is the loader's, and nothing about going
    /// through a snapshot changes what reaches the audio thread.
    #[test]
    fn a_restored_sample_reaches_the_audio_thread_once_the_loader_applies_it() {
        let mut saved = Model::new();
        let sample = a_sample_of(&saved);
        assert!(saved.set_sample(saved.lanes()[1].id, sample));

        let mut loaded = Model::new();
        let pending = loaded.load_snapshot(saved.snapshot());
        let handle = loaded.handle().clone();

        for (id, pending) in pending {
            let bytes = pending.bytes.expect("the save embedded the file");
            let data = SampleData { rate: 48_000, channels: 2, frames: vec![0.25, -0.25].into() };
            let restored = SampleRef::new(&handle, Path::new(&pending.path), bytes, data);
            assert!(loaded.set_sample(id, restored));
        }

        let published = loaded.published().get();
        assert_eq!(published.lanes().len(), 1, "the restored lane is playable");
        assert_eq!(published.lanes()[0].note, 37, "on the note it was saved with");
        assert_eq!(published.lanes()[0].sample.frames.len(), 2, "playing the restored audio");
    }

    // ---- Embedded size --------------------------------------------------

    /// A sample whose file is this many bytes long, allocated against the
    /// model's collector.
    ///
    /// The bytes are zeroed and nothing reads them: every embedding limit
    /// is about how big a file is, so a test of those rules needs the
    /// length and not the contents. `vec![0; n]` of a cap-sized file is
    /// pages the test never touches.
    fn a_sample_of_bytes(model: &Model, path: &str, bytes: usize) -> SampleRef {
        let data = SampleData { rate: 44_100, channels: 1, frames: vec![0.0].into() };
        SampleRef::new(model.handle(), Path::new(path), vec![0; bytes], data)
    }

    /// The running total is every lane's file bytes, and it follows the
    /// lanes: a duplicate adds its copy (the save writes those bytes
    /// again), clearing or deleting a lane takes them off again.
    #[test]
    fn the_embedded_total_counts_every_lanes_file_bytes() {
        let mut model = Model::new();
        assert_eq!(model.embedded_bytes(), 0, "a fresh instance embeds nothing");
        assert!(!crate::state::embed_warning(model.embedded_bytes()));

        let first = model.lanes()[0].id;
        let second = model.lanes()[1].id;
        let kick = a_sample_of_bytes(&model, "/kick.wav", 300);
        let snare = a_sample_of_bytes(&model, "/snare.wav", 45);
        assert!(model.set_sample(first, kick));
        assert!(model.set_sample(second, snare));
        assert_eq!(model.embedded_bytes(), 345, "both files");

        let copy = model.duplicate_lane(first).expect("a duplicate");
        assert_eq!(model.embedded_bytes(), 645, "a save writes the duplicate's bytes again");

        assert!(model.clear_sample(copy));
        assert_eq!(model.embedded_bytes(), 345, "an emptied lane embeds nothing");
        assert!(model.delete_lane(second));
        assert_eq!(model.embedded_bytes(), 300);
        assert!(
            !crate::state::embed_warning(model.embedded_bytes()),
            "300 bytes is a long way from 128 MiB"
        );
    }

    /// A sample over the per-file cap costs the project nothing — the save
    /// keeps its path — and its lane carries the note that says so, while
    /// staying a lane that works: no error, and it still plays.
    #[test]
    fn an_over_cap_sample_notes_its_lane_and_is_left_out_of_the_total() {
        let mut model = Model::new();
        let small = model.lanes()[0].id;
        let big = model.lanes()[1].id;
        let kick = a_sample_of_bytes(&model, "/kick.wav", 128);
        let take = a_sample_of_bytes(&model, "/take.wav", crate::sample::MAX_SAMPLE_BYTES + 1);
        assert!(model.set_sample(small, kick));
        assert!(model.set_sample(big, take));

        assert_eq!(model.embedded_bytes(), 128, "only the sample the project can carry");
        assert_eq!(
            model.lanes()[1].embed_note(),
            Some(crate::state::SAMPLE_NOT_EMBEDDED),
            "the lane says the project will hold its path and not its audio",
        );
        assert_eq!(model.lanes()[0].embed_note(), None, "a sample within the cap says nothing");
        assert_eq!(model.lanes()[1].error, None, "a sample too big to save has not failed");
        assert_eq!(
            model.published().get().lanes().len(),
            2,
            "and it plays — the cap is about the project file, not about playback",
        );

        let saved = model.snapshot();
        let stored = |index: usize| saved.lanes[index].sample.as_ref().expect("a sample");
        assert!(stored(0).bytes_b64.is_some(), "the small file is embedded");
        assert_eq!(stored(1).bytes_b64, None, "the big one is saved path-only");
    }

    /// The warning starts the byte after the threshold, and a file of
    /// exactly the per-file cap is still embedded: two of them are the
    /// threshold itself, which is quiet, and one byte more is not.
    #[test]
    fn the_warning_starts_once_the_total_passes_the_threshold() {
        let mut model = Model::new();
        let at_cap = a_sample_of_bytes(&model, "/loop.wav", crate::sample::MAX_SAMPLE_BYTES);
        assert!(at_cap.embeds(), "a file of exactly the cap is embedded, not path-only");

        let ids: Vec<u32> = model.lanes().iter().take(3).map(|lane| lane.id).collect();
        assert!(model.set_sample(ids[0], at_cap.clone()));
        assert!(model.set_sample(ids[1], at_cap));
        assert_eq!(model.embedded_bytes(), EMBED_WARN_BYTES, "two cap-sized files exactly");
        assert!(
            !crate::state::embed_warning(model.embedded_bytes()),
            "sitting on the threshold is quiet"
        );
        assert_eq!(model.lanes()[0].embed_note(), None, "neither lane is over the per-file cap");

        let tick = a_sample_of_bytes(&model, "/tick.wav", 1);
        assert!(model.set_sample(ids[2], tick));
        assert_eq!(model.embedded_bytes(), EMBED_WARN_BYTES + 1);
        assert!(
            crate::state::embed_warning(model.embedded_bytes()),
            "one byte past the threshold warns"
        );
    }

    /// A model with a source landed directly (the loader's own path is
    /// tested in `loader.rs`): `frames` mono frames at 48 kHz.
    fn with_source(frames: usize) -> Model {
        let mut model = Model::new();
        let data = SampleData {
            rate: 48_000,
            channels: 1,
            frames: (0..frames).map(|n| (n as f32 * 0.01).sin()).collect(),
        };
        let peaks = Peaks::build(&data);
        model.open_source("/songs/Eye.wav");
        assert!(model.set_source("/songs/Eye.wav", data, peaks));
        model
    }

    #[test]
    fn a_fresh_chop_editor_is_empty_with_snap_on() {
        let model = Model::new();
        let editor = model.chop();
        assert!(editor.source.is_none());
        assert_eq!(editor.source_path, None);
        assert!(!editor.loading);
        assert_eq!(editor.error, None);
        assert_eq!(editor.selection, None);
        assert!(editor.snap);
        assert!(!editor.looping);
        assert!(!editor.collapsed);
        assert_eq!(editor.height_px, chop::STRIP_HEIGHT);
    }

    /// `open_source` says "loading…" for the path and drops what was open.
    #[test]
    fn open_source_marks_loading_and_drops_the_old_source() {
        let mut model = with_source(1000);
        model.set_selection(Some(Selection { start: 10, end: 20, active: chop::Edge::End }));

        model.open_source("/songs/Next.wav");

        let editor = model.chop();
        assert!(editor.loading);
        assert_eq!(editor.source_path.as_deref(), Some("/songs/Next.wav"));
        assert!(editor.source.is_none());
        assert_eq!(editor.selection, None);
    }

    /// The selection `[start, end)`, end edge active.
    fn span(start: u64, end: u64) -> Option<Selection> {
        Some(Selection { start, end, active: chop::Edge::End })
    }

    /// The selection's frames, for comparing without the active edge.
    fn selected(model: &Model) -> Option<(u64, u64)> {
        model.chop().selection.map(|selection| (selection.start, selection.end))
    }

    /// Selection gestures are undo steps in the one history with the
    /// lanes': committed, they step back and forward, in order with a lane
    /// edit between them.
    #[test]
    fn a_committed_selection_change_is_an_undo_step() {
        let mut model = with_source(1000);
        assert!(!model.can_undo(), "a source landing is no step");

        model.set_selection(span(100, 200));
        assert!(model.commit());
        model.set_selection(span(100, 400));
        assert!(model.commit());
        let id = model.lanes()[0].id;
        model.set_volume_db(id, -6.0);
        assert!(model.commit());
        model.set_selection(span(300, 400));
        assert!(model.commit());
        assert_eq!(model.undo_depth(), 4);

        assert!(model.undo());
        assert_eq!(selected(&model), Some((100, 400)));
        assert!(model.undo());
        assert_eq!(model.lanes()[0].volume_db, 0.0, "the lane edit, in its turn");
        assert_eq!(selected(&model), Some((100, 400)));
        assert!(model.undo());
        assert_eq!(selected(&model), Some((100, 200)));
        assert!(model.undo());
        assert_eq!(selected(&model), None);
        assert!(!model.undo());

        assert!(model.redo());
        assert!(model.redo());
        assert_eq!(selected(&model), Some((100, 400)));
        assert!(model.redo());
        assert!(model.redo());
        assert_eq!(selected(&model), Some((300, 400)));
        assert_eq!(model.lanes()[0].volume_db, -6.0);
    }

    /// Picking the other edge is not an edit; a gesture that ends on the
    /// span it started from is no step.
    #[test]
    fn the_active_edge_alone_is_no_step() {
        let mut model = with_source(1000);
        model.set_selection(span(100, 200));
        assert!(model.commit());
        let depth = model.undo_depth();
        let start = Selection { start: 100, end: 200, active: chop::Edge::Start };
        model.set_selection(Some(start));
        assert!(!model.commit());
        model.set_selection(span(150, 200));
        model.set_selection(span(100, 200));
        assert!(!model.commit());
        assert_eq!(model.undo_depth(), depth);
    }

    /// Another source opened: its selection is not an edit of the old one,
    /// the old source's selection steps are passed over rather than
    /// restored onto the new source, and the lane edits under them still
    /// undo.
    #[test]
    fn selection_steps_stay_with_their_source() {
        let mut model = with_source(1000);
        let id = model.lanes()[0].id;
        model.set_volume_db(id, -6.0);
        assert!(model.commit());
        model.set_selection(span(100, 200));
        assert!(model.commit());
        model.set_selection(span(100, 300));
        assert!(model.commit());

        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 2000].into() };
        let peaks = Peaks::build(&data);
        model.open_source("/songs/Next.wav");
        assert!(model.set_source("/songs/Next.wav", data, peaks));
        model.set_selection(span(500, 900));
        assert!(model.commit(), "a first selection on the new source is a step");

        assert!(model.undo());
        assert_eq!(selected(&model), None, "back to the new source's own start");
        assert!(model.undo(), "the old source's steps are skipped, the lane edit is not");
        assert_eq!(model.lanes()[0].volume_db, 0.0);
        assert_eq!(selected(&model), None, "nothing of the old source restored");
        assert!(!model.undo());
    }

    /// A result for a path the editor is no longer loading is refused.
    #[test]
    fn a_result_for_another_path_does_not_land() {
        let mut model = Model::new();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 10].into() };
        let peaks = Peaks::build(&data);
        assert!(!model.set_source("/nothing-asked.wav", data.clone(), peaks.clone()));
        assert!(!model.source_failed("/nothing-asked.wav", "nope"));

        model.open_source("/a.wav");
        assert!(!model.set_source("/b.wav", data, peaks));
        assert!(!model.source_failed("/b.wav", "nope"));
        assert!(model.chop().loading);
        assert_eq!(model.chop().error, None);
    }

    #[test]
    fn set_selection_clamps_to_the_source_and_needs_one() {
        let mut model = Model::new();
        let some = Selection { start: 5, end: 9, active: chop::Edge::Start };
        assert!(!model.set_selection(Some(some)), "nothing to select without a source");
        assert_eq!(model.chop().selection, None);

        let mut model = with_source(100);
        assert!(model.set_selection(Some(Selection { start: 90, end: 500, ..some })));
        let selection = model.chop().selection.unwrap();
        assert_eq!((selection.start, selection.end), (90, 100));
        assert!(model.set_selection(None));
        assert_eq!(model.chop().selection, None);
    }

    #[test]
    fn set_view_clamps_and_remembers_the_width() {
        let mut model = with_source(10_000);
        model.set_view(chop::View { first: 50_000, frames_per_pixel: 0.25 }, 100.0);
        assert_eq!(model.chop().view, chop::View { first: 9900, frames_per_pixel: 1.0 });
        assert_eq!(model.chop().view_width_px, 100.0);

        model.set_view(chop::View { first: 0, frames_per_pixel: f64::NAN }, f32::NAN);
        assert_eq!(model.chop().view_width_px, 1.0);
        assert_eq!(model.chop().view.frames_per_pixel, 10_000.0, "NaN zoom reads as fit");
    }

    #[test]
    fn set_strip_snap_and_looping() {
        let mut model = Model::new();
        model.set_strip(true, 250.0);
        assert!(model.chop().collapsed);
        assert_eq!(model.chop().height_px, 250.0);
        model.set_strip(false, 3.0);
        assert_eq!(model.chop().height_px, chop::STRIP_MIN_HEIGHT);
        model.set_strip(false, f32::INFINITY);
        assert_eq!(model.chop().height_px, chop::STRIP_MAX_HEIGHT);
        model.set_strip(false, f32::NAN);
        assert_eq!(model.chop().height_px, chop::STRIP_HEIGHT);
        model.set_snap(false);
        model.set_looping(true);
        assert!(!model.chop().snap);
        assert!(model.chop().looping);
    }

    /// No chop-editor writer is an undo step, and none publishes.
    #[test]
    fn chop_editor_writers_are_not_undo_steps() {
        let mut model = with_source(1000);
        let before = model.published().get();
        model.set_selection(Some(Selection { start: 1, end: 50, active: chop::Edge::Start }));
        model.set_view(chop::View { first: 10, frames_per_pixel: 2.0 }, 300.0);
        model.set_snap(false);
        model.set_looping(true);
        model.set_strip(true, 120.0);
        model.close_source();
        assert!(!model.commit(), "nothing a commit would record changed");
        assert_eq!(model.undo_depth(), 0);
        assert!(!model.undo());
        assert!(std::ptr::eq(&*before, &*model.published().get()));
    }

    /// Closing hands the source's audio back to the collector.
    #[test]
    fn close_source_frees_the_audio() {
        let mut model = with_source(1000);
        let with = model.alloc_count();
        model.close_source();
        assert_eq!(model.alloc_count(), with - 1);
    }


    // --- chops onto lanes ------------------------------------------------

    fn sel(start: u64, end: u64) -> Selection {
        Selection { start, end, active: chop::Edge::End }
    }

    #[test]
    fn chop_lanes_are_named_after_the_source_and_counted() {
        let mut model = with_source(48_000);
        let first = model.add_chop_lane(sel(100, 2000)).unwrap();
        let second = model.add_chop_lane(sel(2000, 4000)).unwrap();
        assert_eq!(model.lane(first).unwrap().name, "Eye 1");
        assert_eq!(model.lane(second).unwrap().name, "Eye 2");

        // A replaced lane holds a chop of this source too, so it counts.
        let plain = model.lanes()[0].id;
        assert!(model.replace_with_chop(plain, sel(0, 500)));
        let third = model.add_chop_lane(sel(4000, 5000)).unwrap();
        assert_eq!(model.lane(third).unwrap().name, "Eye 4");
    }

    #[test]
    fn a_chop_lane_has_the_v2_defaults_and_the_add_lane_progression() {
        let mut model = with_source(48_000);
        let a = model.add_chop_lane(sel(0, 1000)).unwrap();
        let b = model.add_chop_lane(sel(1000, 2000)).unwrap();
        let c = model.add_lane().unwrap();

        let (a, b, c) = (model.lane(a).unwrap(), model.lane(b).unwrap(), model.lane(c).unwrap());
        assert_eq!((a.note, b.note, c.note), (52, 53, 54), "notes walk on from D#2");
        assert_eq!(
            (a.colour, b.colour, c.colour),
            (LANE_PALETTE[6], LANE_PALETTE[7], LANE_PALETTE[8]),
            "colours follow the lane count"
        );
        for lane in [a, b] {
            assert_v2_defaults(lane, &lane.name);
            assert_eq!(lane.volume_db, 0.0);
            assert_eq!(lane.pan, 0.0);
            assert_eq!(lane.choke, CHOKE_NONE);
            assert!(lane.error.is_none() && !lane.loading);
        }
        assert!(model.published().get().lane(a.id).is_some(), "the chop is published");
    }

    #[test]
    fn a_chop_plays_the_cut_and_saves_it_as_24_bit() {
        let mut model = with_source(48_000);
        let id = model.add_chop_lane(sel(1234, 9876)).unwrap();
        let source = model.chop().source.as_ref().unwrap().data.clone();
        let expected = chop::cut(&source, 1234, 9876);

        let sample = model.lane(id).unwrap().sample.as_ref().unwrap();
        assert_eq!(*sample.data, expected, "the lane plays the cut frames directly");
        assert_eq!(sample.path, "/songs/Eye.wav");
        assert_eq!(sample.file_name, "Eye.wav");
        assert_eq!(
            sample.origin,
            Some(ChopOrigin {
                source_path: "/songs/Eye.wav".to_owned(),
                start_frame: 1234,
                end_frame: 9876,
            })
        );

        let decoded = crate::sample::decode(&sample.bytes, Some("wav")).unwrap();
        assert_eq!((decoded.rate, decoded.channels), (expected.rate, expected.channels));
        assert_eq!(decoded.frames.len(), expected.frames.len());
        let tolerance = 1.0 / 8_388_608.0 + f32::EPSILON;
        for (i, (got, want)) in decoded.frames.iter().zip(expected.frames.iter()).enumerate() {
            assert!((got - want).abs() <= tolerance, "sample {i}: {got} vs {want}");
        }
    }

    #[test]
    fn a_chop_selection_is_clamped_to_the_source() {
        let mut model = with_source(1000);
        let id = model.add_chop_lane(sel(900, 5000)).unwrap();
        let sample = model.lane(id).unwrap().sample.as_ref().unwrap();
        assert_eq!(sample.data.frame_count(), 100);
        let origin = sample.origin.as_ref().unwrap();
        assert_eq!((origin.start_frame, origin.end_frame), (900, 1000));
    }

    #[test]
    fn adding_and_replacing_a_chop_are_one_undo_step_each() {
        let mut model = with_source(48_000);
        let id = model.add_chop_lane(sel(0, 1000)).unwrap();
        assert!(model.commit());
        assert_eq!(model.undo_depth(), 1);

        let target = model.lanes()[0].id;
        assert!(model.replace_with_chop(target, sel(1000, 2000)));
        assert!(model.commit());
        assert_eq!(model.undo_depth(), 2);

        assert!(model.undo());
        assert!(model.lane(target).unwrap().sample.is_none(), "the replace is undone");
        assert!(model.lane(id).is_some(), "the add is still there");
        assert!(model.undo());
        assert!(model.lane(id).is_none(), "the add is undone");
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize);
        assert!(!model.undo());
    }

    #[test]
    fn a_chop_lane_is_refused_at_max_lanes_without_a_source_or_when_empty() {
        let mut model = Model::new();
        assert_eq!(model.add_chop_lane(sel(0, 10)), None, "no source");
        assert!(!model.replace_with_chop(0, sel(0, 10)), "no source");

        let mut model = with_source(1000);
        assert_eq!(model.add_chop_lane(sel(1000, 2000)), None, "empty once clamped");
        assert!(!model.replace_with_chop(0, sel(500, 500)), "empty");
        assert!(!model.replace_with_chop(9999, sel(0, 10)), "no such lane");

        while model.add_lane().is_some() {}
        assert_eq!(model.lanes().len(), MAX_LANES as usize);
        let allocs = model.alloc_count();
        assert_eq!(model.add_chop_lane(sel(0, 10)), None, "full");
        assert_eq!(model.lanes().len(), MAX_LANES as usize);
        assert_eq!(model.alloc_count(), allocs, "nothing was cut for a refused lane");
        assert!(model.lanes().iter().all(|lane| lane.sample.is_none()));
    }

    #[test]
    fn replace_with_chop_keeps_the_lane_and_its_settings() {
        let mut model = with_source(48_000);
        let id = model.lanes()[3].id;
        model.rename(id, "Kick");
        model.set_note(id, 70);
        model.set_colour(id, 0x12_34_56);
        model.set_volume_db(id, -6.0);
        model.set_pan(id, 0.5);
        model.set_choke(id, 3);
        model.set_mode(id, LaneMode::Gate);
        model.set_attack_ms(id, 12.0);
        model.set_release_ms(id, 80.0);
        model.set_cut_itself(id, false);
        model.set_output(id, 4);
        model.set_time_mode(id, TimeMode::Repitch);
        model.set_fine_pct(id, 1.5);
        model.set_pitch_st(id, -3.0);
        let before = model.lane(id).unwrap().clone();

        assert!(model.replace_with_chop(id, sel(100, 900)));
        let after = model.lane(id).unwrap();
        assert!(after.sample.as_ref().unwrap().origin.is_some());
        assert_eq!(after.sample.as_ref().unwrap().data.frame_count(), 800);
        let without_sample = |lane: &Lane| Lane { sample: None, ..lane.clone() };
        let (after, before) = (without_sample(after), without_sample(&before));
        assert_eq!(format!("{after:?}"), format!("{before:?}"), "everything but the sample");
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize, "no lane added");
    }

    // --- Stretch renders -------------------------------------------------

    /// A mono chop of `frames` frames at 48 kHz.
    fn chop_data(frames: usize, value: f32) -> SampleData {
        SampleData { rate: 48_000, channels: 1, frames: vec![value; frames].into() }
    }

    /// A model whose first lane holds a 100-frame chop and is in `mode`,
    /// committed, so undo history starts here.
    fn render_lane(mode: TimeMode) -> (Model, u32) {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        let path = Path::new("/tmp/loop.wav");
        let sample = SampleRef::new(model.handle(), path, vec![1], chop_data(100, 0.1));
        assert!(model.set_sample(id, sample));
        assert!(model.set_time_mode(id, mode));
        model.commit();
        (model, id)
    }

    fn published_frames(model: &Model, id: u32) -> usize {
        model.published().get().lane(id).unwrap().sample.frame_count()
    }

    fn generation(model: &Model, id: u32) -> u64 {
        model.lane(id).unwrap().render_generation
    }

    #[test]
    fn each_stretch_setting_bumps_the_generation_and_marks_pending() {
        type Edit = fn(&mut Model, u32) -> bool;
        let edits: [(&str, Edit); 5] = [
            ("length", |m, id| m.set_length(id, Length::Beats(NoteLength::Bar))),
            ("fine", |m, id| m.set_fine_pct(id, 2.0)),
            ("pitch", |m, id| m.set_pitch_st(id, -5.0)),
            ("time mode", |m, id| m.set_time_mode(id, TimeMode::Stretch)),
            ("retry", |m, id| m.request_render(id)),
        ];
        for (name, edit) in edits {
            let (mut model, id) = render_lane(TimeMode::Stretch);
            let gn = generation(&model, id);
            assert!(model.set_render(id, gn, chop_data(200, 0.2)));
            assert!(!model.lane(id).unwrap().render_pending, "{name}: settled first");

            assert!(edit(&mut model, id), "{name}");
            let lane = model.lane(id).unwrap();
            assert_eq!(lane.render_generation, gn + 1, "{name}: bumped");
            assert!(lane.render_pending, "{name}: pending");
            assert_eq!(published_frames(&model, id), 200, "{name}: old render plays meanwhile");
        }
    }

    #[test]
    fn an_off_lane_asks_for_no_render() {
        let (mut model, id) = render_lane(TimeMode::Off);
        let gn = generation(&model, id);
        assert!(model.set_length(id, Length::Beats(NoteLength::Bar)));
        assert!(model.set_fine_pct(id, 2.0));
        assert!(model.set_pitch_st(id, -5.0));
        assert!(!model.request_render(id), "nothing to render for an Off lane");
        let lane = model.lane(id).unwrap();
        assert_eq!(lane.render_generation, gn);
        assert!(!lane.render_pending);
        assert!(!model.set_render(id, gn, chop_data(200, 0.2)), "and nothing lands");
        assert_eq!(published_frames(&model, id), 100);
    }

    #[test]
    fn a_stale_generation_is_ignored() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        let old = generation(&model, id);
        assert!(model.set_pitch_st(id, 3.0));
        let new = generation(&model, id);
        assert!(new > old);

        assert!(!model.set_render(id, old, chop_data(150, 0.2)), "stale");
        assert!(model.lane(id).unwrap().render.is_none());
        assert!(model.lane(id).unwrap().render_pending, "still waiting for the newest");
        assert_eq!(published_frames(&model, id), 100, "the chop still plays");
        assert!(!model.set_render_error(id, old, "render failed"), "a stale failure too");
        assert!(model.lane(id).unwrap().render_error.is_none());

        assert!(model.set_render(id, new, chop_data(250, 0.2)));
        assert_eq!(published_frames(&model, id), 250);
        let render = model.lane(id).unwrap().render.clone().unwrap();
        assert_eq!(render.generation, new);
        assert!((render.target_secs - 250.0 / 48_000.0).abs() < 1e-12);
        assert!(!model.lane(id).unwrap().render_pending);

        assert!(!model.set_render(id, old, chop_data(150, 0.2)), "nor after a newer one");
        assert_eq!(published_frames(&model, id), 250);
    }

    #[test]
    fn the_published_sample_is_the_render_in_stretch_and_the_chop_otherwise() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        assert_eq!(published_frames(&model, id), 100, "raw chop before a render lands");
        assert!(model.set_render(id, generation(&model, id), chop_data(300, 0.3)));
        assert_eq!(published_frames(&model, id), 300, "the render in Stretch");

        assert!(model.set_time_mode(id, TimeMode::Repitch));
        assert_eq!(published_frames(&model, id), 100, "the chop in Repitch");
        assert!(model.lane(id).unwrap().render.is_none(), "render dropped");
        assert!(model.set_time_mode(id, TimeMode::Off));
        assert_eq!(published_frames(&model, id), 100, "the chop in Off");

        assert!(model.set_time_mode(id, TimeMode::Stretch));
        assert!(model.lane(id).unwrap().render_pending, "back in Stretch: render again");
        assert_eq!(published_frames(&model, id), 100);
    }

    #[test]
    fn leaving_stretch_makes_an_in_flight_render_stale() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        let gn = generation(&model, id);
        assert!(model.set_time_mode(id, TimeMode::Off));
        assert!(generation(&model, id) > gn);
        assert!(!model.lane(id).unwrap().render_pending);
        assert!(!model.set_render(id, gn, chop_data(300, 0.3)));
        assert_eq!(published_frames(&model, id), 100);
    }

    #[test]
    fn a_new_sample_drops_the_render_and_asks_again() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        assert!(model.set_render(id, generation(&model, id), chop_data(300, 0.3)));
        let gn = generation(&model, id);
        let other =
            SampleRef::new(model.handle(), Path::new("/tmp/b.wav"), vec![2], chop_data(40, 0.4));
        assert!(model.set_sample(id, other));
        let lane = model.lane(id).unwrap();
        assert!(lane.render.is_none());
        assert!(lane.render_pending);
        assert!(lane.render_generation > gn);
        assert_eq!(published_frames(&model, id), 40);
    }

    #[test]
    fn a_landed_render_is_not_undone() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        assert!(model.rename(id, "Loop"));
        model.commit();
        let depth = model.undo_depth();

        let gn = generation(&model, id);
        assert!(model.set_render(id, gn, chop_data(300, 0.3)));
        assert_eq!(model.undo_depth(), depth, "not an undo step");
        assert!(!model.commit(), "nothing to commit: the landing is rebased");

        assert!(model.undo());
        assert_eq!(model.lane(id).unwrap().name, "Lane 1", "the rename is undone");
        let lane = model.lane(id).unwrap();
        assert_eq!(lane.render.as_ref().map(|r| r.generation), Some(gn), "the render is not");
        assert_eq!(lane.render_generation, gn, "and no new render was asked for");
        assert!(!lane.render_pending);
        assert_eq!(published_frames(&model, id), 300);

        assert!(model.redo());
        assert_eq!(published_frames(&model, id), 300);
    }

    #[test]
    fn a_render_landing_mid_gesture_keeps_the_gesture_undoable() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        let before = model.lane(id).unwrap().volume_db;
        assert!(model.set_volume_db(id, -12.0), "a drag, not committed yet");
        assert!(model.set_render(id, generation(&model, id), chop_data(300, 0.3)));
        assert!(model.commit(), "the drag is still the user's to commit");
        assert!(model.undo());
        assert_eq!(model.lane(id).unwrap().volume_db, before);
        assert_eq!(published_frames(&model, id), 300);
    }

    #[test]
    fn undoing_a_pitch_change_asks_for_a_render_with_a_newer_generation() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        assert!(model.set_pitch_st(id, 4.0));
        model.commit();
        let gn = generation(&model, id);
        assert!(model.set_render(id, gn, chop_data(300, 0.3)));

        assert!(model.undo());
        let lane = model.lane(id).unwrap();
        assert_eq!(lane.pitch_st, PITCH_DEFAULT);
        assert!(lane.render_generation > gn, "never backwards");
        assert!(lane.render_pending);
        assert_eq!(published_frames(&model, id), 300, "the old render plays until then");
    }

    #[test]
    fn a_failed_render_keeps_what_plays_and_says_so() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        let gn = generation(&model, id);
        assert!(model.set_render_error(id, gn, "render failed"));
        let lane = model.lane(id).unwrap();
        assert!(!lane.render_pending);
        assert_eq!(lane.render_error.as_deref(), Some("render failed"));
        assert_eq!(published_frames(&model, id), 100);

        assert!(model.request_render(id));
        assert!(model.lane(id).unwrap().render_error.is_none(), "a retry clears it");
    }

    #[test]
    fn a_project_load_asks_for_a_render_of_every_stretch_lane() {
        let (mut model, id) = render_lane(TimeMode::Stretch);
        assert!(model.set_render(id, generation(&model, id), chop_data(300, 0.3)));
        let mut lanes = model.lanes().to_vec();
        lanes[1].time_mode = TimeMode::Stretch;
        model.replace_lanes(lanes, false);
        let first = &model.lanes()[0];
        assert!(first.render.is_none(), "renders are rebuilt, never carried in");
        assert!(first.render_pending);
        assert!(first.render_generation > 0);
        let empty = &model.lanes()[1];
        assert!(empty.render_generation > 0 && !empty.render_pending, "no sample to render");
        assert!(model.lanes()[2..].iter().all(|lane| lane.render_generation == 0));
    }
}
