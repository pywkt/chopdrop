//! chopdrop — a CLAP sample-player plugin for Bitwig.
//!
//! A list of lanes, each holding one sample and answering to one note, played
//! by bitstep or any other note source. The pure modules (`model`, `state`,
//! `sample`, `voices`, `engine`) import no nice-plug types; this file is the
//! nice-plug boundary: the `Plugin` impl, the host parameters, `process()`
//! and the CLAP export.
//!
//! # The plugin shell
//!
//! All three of its parts are here. The declarations: the audio and MIDI
//! configuration, the master volume parameter, the `ClapPlugin` constants
//! and the CLAP export. The lanes: [`LanesField`], the `#[persist =
//! "lanes"]` half of [`ChopdropParams`], which saves the model as a
//! [`state::Snapshot`] and hands a restored project's samples to the loader
//! on the way back in. And [`Plugin::process`]: the host's `NoteEvent`s
//! mapped onto [`engine::EngineEvent`]s at their own sample offsets,
//! [`engine::Engine::run_block`] rendering the block, ramped to the smoothed
//! master volume, and the voices that ended on the way reported to the host as
//! `VoiceTerminated`.
//!
//! Both of the editor's own paths to the audio thread are joined up now
//! ([`editor`]): a row's play button pushes a lane onto the
//! engine's audition ring ([`engine::AuditionRing`], handed to the window in
//! [`Plugin::editor`]), and a lane's activity marker lights its dot — which
//! is why `process()` rings the editor's repaint notifier whenever a lane is
//! struck or anything is still sounding ([`engine::Engine::needs_a_frame`]).

pub mod browser;
pub mod chop;
pub mod editor;
pub mod engine;
pub mod loader;
pub mod model;
pub mod preview;
pub mod render;
pub mod sample;
pub mod state;
pub mod stretch;
pub mod time;
pub mod voices;

use std::num::NonZeroU32;
use std::sync::{Arc, Mutex};

use nice_plug::midi::{Channel, Key, VoiceID};
use nice_plug::params::persist::PersistentField;
use nice_plug::prelude::*;
use nice_plug_egui::{EguiEditorState, RepaintNotifier};

use crate::editor::ChopdropEditor;
use crate::engine::{AuditionRing, Engine, EngineEvent, NoteTarget, Outputs, TimedEvent};
use crate::loader::{Loader, Source};
use crate::model::{lock_model, Model, PublishedCell, SharedModel};
use crate::preview::{PreviewChannel, Previewer};
use crate::render::RenderWorker;
use crate::state::Snapshot;
use crate::time::TempoCell;
use crate::voices::{TerminatedVoice, CHANNEL_WILDCARD, KEY_WILDCARD};

/// The quietest the master volume goes, in dB. Not silence: the parameter is
/// a gain whose range is linear in decibels, and −60 dB is far below audible
/// in any mix. Muting a lane is the lane's own business, not the master's.
pub const VOLUME_MIN_DB: f32 = -60.0;
/// The loudest the master volume goes, in dB.
pub const VOLUME_MAX_DB: f32 = 12.0;
/// Where the master volume starts: unchanged, so a fresh instance plays the
/// samples at the level they were recorded.
pub const VOLUME_DEFAULT_DB: f32 = 0.0;

/// Milliseconds the master volume takes to reach a new value.
///
/// `Logarithmic`, which is how a gain is heard, and which is also why the
/// range stops at −60 dB rather than at zero: logarithmic smoothing cannot
/// cross a zero value.
const VOLUME_SMOOTHING_MS: f32 = 10.0;

/// The lane model as the host saves it: a persistent field keyed `"lanes"`.
///
/// A newtype rather than an impl on [`SharedModel`] directly, because the
/// orphan rule forbids that — and because this is where the two worlds
/// meet: `model.rs` and `state.rs` import no nice-plug types, so the trait
/// that hands their snapshots to the framework belongs here in the shell.
///
/// It holds both of a restore's writers. The model is the one every other
/// handle in the instance points at, so a load through
/// [`PersistentField::set`] is visible to the audio thread at its next
/// [`PublishedCell::get`] — there is one lane list, not a saved copy of one.
/// The loader is what finishes the job: a project's samples are decoded on
/// its worker thread and written back through the same writer API, which is
/// why restoring a project needs neither an open editor nor a running audio
/// callback.
///
/// The render worker is the third writer: a restored Stretch lane asks for
/// its render through the model, and the worker built here — one per
/// instance, over the model's one [`TempoCell`](crate::time::TempoCell) —
/// rebuilds it, editor or not.
pub struct LanesField {
    /// The lanes, behind the writer mutex every writer shares.
    model: SharedModel,
    /// The instance's decoder, for the samples a restore brings with it.
    loader: Arc<Loader>,
    /// The instance's time-stretch renderer.
    render: Arc<RenderWorker>,
}

impl LanesField {
    /// Wraps the model and the loader the rest of the instance shares, and
    /// starts the instance's render worker over that model and its tempo
    /// cell — never a second cell.
    pub fn new(model: SharedModel, loader: Arc<Loader>) -> LanesField {
        let tempo = Arc::clone(lock_model(&model).tempo());
        let render = Arc::new(RenderWorker::new(Arc::clone(&model), tempo));
        LanesField { model, loader, render }
    }

    /// The model behind the field — what the editor writes through (plan
    /// 6.x), and what the tests look at.
    pub fn model(&self) -> &SharedModel {
        &self.model
    }

    /// The instance's loader, for the editor's drops.
    pub fn loader(&self) -> &Arc<Loader> {
        &self.loader
    }

    /// The instance's render worker, for the editor to wake after a request.
    pub fn render(&self) -> &Arc<RenderWorker> {
        &self.render
    }

    /// Wait until the loader has decoded everything queued and the render
    /// worker has rendered everything that left pending — in that order,
    /// since a decoded Stretch sample is itself a render request — then stop
    /// both, and hand back the model. Blocks without polling; for tests.
    ///
    /// # Panics
    ///
    /// If another handle on the loader or the worker is still alive (an
    /// open editor holds the params, and through them both).
    pub fn finish(self) -> SharedModel {
        let LanesField { model, loader, render } = self;
        let Ok(loader) = Arc::try_unwrap(loader) else {
            panic!("finish needs the only handle on the loader");
        };
        loader.finish();
        let Ok(render) = Arc::try_unwrap(render) else {
            panic!("finish needs the only handle on the render worker");
        };
        render.finish();
        model
    }
}

/// Both directions are main-thread work that allocates freely; neither is
/// reachable from `process()`.
impl PersistentField<'_, Snapshot> for LanesField {
    /// The host handing back a saved project.
    ///
    /// One write through the writer mutex, so it can neither interleave with
    /// a GUI edit nor tear a block the audio thread is already rendering:
    /// the lanes land clamped and published first, and each sample follows
    /// as the loader decodes it. Every load rule — version, lane cap,
    /// clamping, samples that are already beyond saving — is applied by
    /// [`Snapshot::into_restore`] on the way in, so a hostile project file
    /// cannot panic the host.
    ///
    /// The jobs are queued while the lock is still held, on purpose:
    /// [`Loader::queue`] marks its lane "loading…" as part of this same
    /// write, so nothing ever sees a restored lane that is neither loading
    /// nor loaded. A job that cannot be queued at all leaves its lane empty,
    /// which is what [`Loader::queue`] reporting `false` means and all a
    /// plugin that has lost its worker thread can honestly show.
    ///
    /// And the window is woken once the lanes have landed (the model's
    /// repaint hook): a host loading a project into an instance whose editor
    /// is already open is a non-GUI writer like the loader, and lanes without
    /// samples bring no loader result that would ring for them. Rung under
    /// the writer lock, as the loader does; with no editor ever opened there
    /// is no hook to ring.
    fn set(&self, new_value: Snapshot) {
        let mut model = lock_model(&self.model);
        let pending = model.load_snapshot(new_value);
        for (lane, sample) in pending {
            self.loader.queue(&mut model, lane, Source::Restore(sample));
        }
        // Lanes first (so chops play), then the editor's source.
        self.loader.reopen_source(&mut model);
        model.request_repaint();
        drop(model);
        // The load requested a render for every Stretch lane that already
        // has its audio; the rest ask as their samples land.
        self.render.wake();
    }

    /// The host asking for state: read the live lanes into a snapshot —
    /// embedded sample bytes and all — and let the framework serialise it.
    fn map<F, R>(&self, f: F) -> R
    where
        F: Fn(&Snapshot) -> R,
    {
        f(&lock_model(&self.model).snapshot())
    }
}

/// The plugin's host-visible parameters.
///
/// CLAP parameter lists are fixed-size and lanes come and go, so per-lane
/// controls live in the GUI and in the saved state; v1 exposes exactly one
/// host parameter. The "bitstep Sample pad" switch is model state inside the
/// persisted snapshot, not a parameter — do not scaffold it here.
///
#[derive(Params)]
pub struct ChopdropParams {
    /// Master volume as a **linear gain**, not a dB number: it is handed to
    /// `engine::run_block` once per block, and keeping the stored
    /// value in the domain it is used in means no `powf` on the audio thread.
    /// The range is skewed so that the control still moves linearly in
    /// decibels, and the host's text in and out of it is in dB.
    #[id = "volume"]
    pub volume: FloatParam,
    /// The lanes. Not a parameter but a persistent field, which the host
    /// stores as an opaque blob beside the parameter values and hands
    /// straight back on load ([`LanesField`]).
    #[persist = "lanes"]
    pub lanes: LanesField,
}

impl ChopdropParams {
    /// Parameters over the lanes `model` holds, decoded by `loader`.
    ///
    /// There is deliberately no `Default`: the model here has to be the
    /// *same* one the audio thread reads and the editor writes, and a
    /// `Default` would quietly build a params struct around a model nobody
    /// else holds — saving and loading lanes no one ever hears.
    pub fn new(model: SharedModel, loader: Arc<Loader>) -> ChopdropParams {
        ChopdropParams {
            volume: FloatParam::new(
                "Volume",
                util::db_to_gain(VOLUME_DEFAULT_DB),
                FloatRange::gain_range(VOLUME_MIN_DB, VOLUME_MAX_DB),
            )
            .with_smoother(SmoothingStyle::Logarithmic(VOLUME_SMOOTHING_MS))
            .with_unit(" dB")
            .with_value_to_string(formatters::v2s_f32_gain_to_db(2))
            .with_string_to_value(formatters::s2v_f32_gain_to_db()),
            lanes: LanesField::new(model, loader),
        }
    }

    /// The master gain the engine multiplies by over a block of `frames`,
    /// smoothed.
    ///
    /// One value a block, because the engine is pure and knows nothing of
    /// nice-plug's smoothers: the smoother is advanced by the length of the
    /// block and the value it lands on is the block's *target*, which
    /// [`Engine::run_block`] then ramps to across the block a frame at a
    /// time. So the smoothing is sampled per block and the gain still moves
    /// per frame, and a jump between two blocks — which is what automation
    /// writes — never lands as a step at a block boundary.
    ///
    /// An empty block advances nothing — `Smoother::next_step` wants at
    /// least one step — and reads back the value the last block left.
    ///
    /// nice-plug clamps whatever a host drives through a normalized value,
    /// but the smoother interpolates towards the plain `f32` the parameter
    /// stores, so the shell clamps here rather than trusting it.
    /// `max`-then-`min`, not `clamp`: that order turns a NaN from a hostile
    /// host into the quiet end of the range, where `clamp` would hand the NaN
    /// straight back and silently poison every sample it touched.
    pub fn next_master_gain(&self, frames: usize) -> f32 {
        let steps = u32::try_from(frames).unwrap_or(u32::MAX);
        let gain = if steps == 0 {
            self.volume.smoothed.previous_value()
        } else {
            self.volume.smoothed.next_step(steps)
        };

        clamp_gain(gain)
    }
}

/// Folds any `f32` into the master volume's gain range, NaN included.
fn clamp_gain(raw: f32) -> f32 {
    raw.max(util::db_to_gain(VOLUME_MIN_DB)).min(util::db_to_gain(VOLUME_MAX_DB))
}

/// How many of one block's note events [`Chopdrop::process`] keeps.
///
/// nice-plug's own input queue is `Plugin::INPUT_EVENT_CAPACITY` long — 1024
/// by default, which chopdrop does not change — so this is the most a host
/// can deliver in a block without nice-plug having to grow the queue itself.
/// Matching it puts the one place that drops events at the one boundary that
/// cannot allocate; past it events are dropped and counted
/// ([`Chopdrop::dropped_events`]).
const EVENT_CAPACITY: usize = 1024;

/// How many stereo outputs chopdrop declares: Out 1 (the main output) plus
/// the aux outputs — one per value a lane's output may hold, so the engine's
/// count is the one source.
pub const OUTPUTS: usize = engine::OUTPUTS;

/// The aux outputs, Out 2…Out 16: [`OUTPUTS`] less the main one.
pub const AUX_OUTPUTS: usize = OUTPUTS - 1;

/// Every output's channel count.
const STEREO: NonZeroU32 = NonZeroU32::new(2).unwrap();

/// The aux ports' names as the host lists them; `AuxiliaryBuffers::outputs[n]`
/// is Out `n + 2`.
const AUX_OUTPUT_NAMES: [&str; AUX_OUTPUTS] = [
    "Out 2", "Out 3", "Out 4", "Out 5", "Out 6", "Out 7", "Out 8", "Out 9", "Out 10", "Out 11",
    "Out 12", "Out 13", "Out 14", "Out 15", "Out 16",
];

/// The block's [`Outputs`] from the host's main channels and aux buffers:
/// pair 0 is the main buffer's first two channels, pair `n + 1` is
/// `aux[n]`'s. An aux buffer with fewer than two channels, or fewer frames
/// than the main one, is left out (`None`), so its lanes fall back to the
/// main output. Every channel no pair takes — extra channels, short buffers,
/// aux buffers past the declared 15 — is silenced here, since the engine only
/// silences the pairs it is given.
///
/// `None` when the main buffer has fewer than two channels, after
/// silencing what it does have: nothing can be heard without a main pair.
/// A stack array of borrows; nothing allocates.
fn block_outputs<'s>(
    main: &'s mut [&mut [f32]],
    aux: &'s mut [Buffer<'_>],
) -> Option<Outputs<'s>> {
    for extra in main.iter_mut().skip(2) {
        extra.fill(0.0);
    }
    let [left, right, ..] = main else {
        for channel in main.iter_mut() {
            channel.fill(0.0);
        }
        return None;
    };
    let frames = left.len().min(right.len());
    let mut outputs = Outputs::main(left, right);
    for (index, buffer) in aux.iter_mut().enumerate() {
        let channels = buffer.as_slice();
        let taken = index < AUX_OUTPUTS;
        for extra in channels.iter_mut().skip(if taken { 2 } else { 0 }) {
            extra.fill(0.0);
        }
        if !taken {
            continue;
        }
        let long_enough = channels.len() >= 2
            && channels[0].len().min(channels[1].len()) >= frames;
        if !long_enough {
            for channel in channels.iter_mut() {
                channel.fill(0.0);
            }
        } else if let [left, right, ..] = channels {
            outputs.pairs[index + 1] = Some((&mut **left, &mut **right));
        }
    }
    Some(outputs)
}

/// Which voices a host event is about, as the engine asks it.
///
/// A CLAP host may address a note event to "any key" (`key = -1`) or any
/// channel, which nice-plug passes on as a wildcard. [`NoteTarget`] has no
/// wildcard of its own — it is a plain channel and key, because that is what
/// a voice carries — so the mapping needs a value standing for "the host did
/// not say", and `voices.rs` keeps it beside the predicate that reads it:
/// [`KEY_WILDCARD`] / [`CHANNEL_WILDCARD`], 255, outside MIDI's ranges and
/// read as *match any* in an event.
///
/// So "all notes off" and "choke everything" reach every voice they name
/// even from a host that tracks no voices and sends no id, while a wildcard
/// note-on still starts nothing — no lane answers note 255.
fn note_target(voice_id: VoiceID, channel: Channel, key: Key) -> NoteTarget {
    NoteTarget {
        channel: channel.number().unwrap_or(CHANNEL_WILDCARD),
        key: key.number().unwrap_or(KEY_WILDCARD),
        voice_id: voice_id.id(),
    }
}

/// One host note event as the engine's own, at the frame it happens on — or
/// `None` for an event chopdrop has nothing to do with.
///
/// The six kinds `MidiConfig::Basic` delivers that a sample player acts on,
/// field for field. Everything else is ignored rather than guessed at: a
/// pressure or vibrato expression, a MIDI CC, a poly-modulation event for a
/// parameter chopdrop never decorated — none of them mean anything to a lane,
/// so unknown events are ignored.
///
/// Nothing here clamps. `timing` is the engine's to fold into the block and
/// every value a voice takes is clamped where it is used ([`crate::voices`]),
/// NaN included — one boundary rather than two that can drift apart.
fn engine_event(event: PluginNoteEvent<Chopdrop>) -> Option<TimedEvent> {
    let (timing, event) = match event {
        NoteEvent::NoteOn { timing, voice_id, channel, key, velocity } => {
            (timing, EngineEvent::NoteOn { note: note_target(voice_id, channel, key), velocity })
        }
        NoteEvent::NoteOff { timing, voice_id, channel, key, .. } => {
            (timing, EngineEvent::NoteOff { note: note_target(voice_id, channel, key) })
        }
        NoteEvent::Choke { timing, voice_id, channel, key } => {
            (timing, EngineEvent::Choke { note: note_target(voice_id, channel, key) })
        }
        NoteEvent::PolyTuning { timing, voice_id, channel, key, tuning } => (
            timing,
            EngineEvent::PolyTuning { note: note_target(voice_id, channel, key), semis: tuning },
        ),
        NoteEvent::PolyPan { timing, voice_id, channel, key, pan } => {
            (timing, EngineEvent::PolyPan { note: note_target(voice_id, channel, key), pan })
        }
        NoteEvent::PolyVolume { timing, voice_id, channel, key, gain } => {
            (timing, EngineEvent::PolyVolume { note: note_target(voice_id, channel, key), gain })
        }
        _ => return None,
    };

    Some(TimedEvent::new(timing, event))
}

/// A voice the engine has finished with, as the hint event the host wants
/// back.
///
/// `timing` is the block's last frame rather than the frame the voice
/// actually ended on: the pool reports an ending only once the block is
/// rendered, by which time the exact frame is gone, and the end of the block
/// is the one offset that cannot precede the note-on that started the voice
/// in the same block. nice-plug clamps an out-of-range offset into the
/// block, so an empty block's 0 is safe too.
///
/// A channel or key that came in as a wildcard goes back out as one
/// ([`KEY_WILDCARD`]), so the host reads back what it sent.
fn voice_terminated(voice: TerminatedVoice, timing: u32) -> PluginNoteEvent<Chopdrop> {
    NoteEvent::VoiceTerminated {
        timing,
        voice_id: voice.voice_id.map_or(VoiceID::Wildcard, VoiceID::ID),
        channel: if voice.channel == CHANNEL_WILDCARD {
            Channel::Wildcard
        } else {
            Channel::Number(voice.channel)
        },
        key: if voice.note_key == KEY_WILDCARD {
            Key::Wildcard
        } else {
            Key::Number(voice.note_key)
        },
    }
}

pub struct Chopdrop {
    params: Arc<ChopdropParams>,
    /// The published lane list as the audio thread reads it: one
    /// [`PublishedCell::get`] per block and never a lock.
    ///
    /// The cell is the model's own and outlives every snapshot swapped into
    /// it, so this handle is taken once and stays good for the life of the
    /// instance — a project load replaces the lanes *inside* the cell, not
    /// the cell. The model itself is reached through
    /// [`LanesField::model`](crate::LanesField::model), which is the only
    /// handle on it that anything but a writer needs.
    published: Arc<PublishedCell>,
    /// The voices and the per-block driver. Touched from the audio thread
    /// alone, bar one thing it lends out: the audition ring the GUI's play
    /// buttons push onto ([`Engine::auditions`]), whose handle is taken on
    /// the main thread every time the host opens the editor
    /// ([`Plugin::editor`]) and which is lock-free on both sides.
    engine: Engine,
    /// This block's events, in the order the host sent them. Owned by the
    /// shell and reused every block so that `process()` never allocates one
    /// ([`EVENT_CAPACITY`]).
    events: Vec<TimedEvent>,
    /// Host events that did not fit in [`events`](Self::events).
    ///
    /// Counted rather than reacted to, and deliberately not a debug
    /// assertion: a host is allowed to send more events in a block than
    /// nice-plug's own queue holds, so this is a fact about the host and not
    /// a bug in chopdrop. Dropping the tail is the only answer the real-time
    /// rule leaves.
    dropped_events: u32,
    /// `VoiceTerminated`s the host's output queue refused. Dropped and
    /// counted, as bitstep does with its own sends.
    send_failures: u32,
    /// The editor window's size, kept here rather than inside the editor
    /// because the host may close and reopen the window many times over one
    /// instance and the size should survive that.
    editor_state: Arc<EguiEditorState>,
    /// The editor's wake-up handle. Lives alongside `editor_state` for the
    /// same reason, and is cheap to clone (one `Arc<AtomicBool>`).
    ///
    /// Rung from [`process`](Chopdrop::process) while anything is sounding,
    /// which is how a lane's activity light comes on in a window nobody is
    /// touching: egui repaints when its input changes, and a note arriving
    /// from bitstep is not input. One relaxed store, so the audio thread may
    /// ring it.
    repaint_notifier: RepaintNotifier,
    /// The browser's previewer: it decodes on a thread of its own and
    /// publishes into the channel the engine reads. Per instance, so a
    /// preview outlives a closed window only until the window's `build` stops
    /// it.
    previewer: Arc<Previewer>,
}

impl Default for Chopdrop {
    fn default() -> Self {
        // One model, three handles: the params struct persists it, the
        // loader writes decoded samples into it, and the audio thread reads
        // the snapshots it publishes. Cloning the `Arc`s here is what keeps
        // a project load, a dropped file and the audio thread looking at one
        // lane list rather than three copies of one.
        let model: SharedModel = Arc::new(Mutex::new(Model::new()));
        let published = Arc::clone(lock_model(&model).published());
        // The one tempo cell: the model owns it, the engine reads it,
        // `process()` stores into it.
        let tempo = Arc::clone(lock_model(&model).tempo());
        let loader = Arc::new(Loader::new(Arc::clone(&model)));
        // The preview channel is allocated against the model's collector, so
        // replaced previews are freed where every other replaced snapshot is.
        let handle = lock_model(&model).handle().clone();
        let channel = Arc::new(PreviewChannel::new(&handle));
        let previewer = Arc::new(Previewer::new(Arc::clone(&channel), handle));

        Self {
            params: Arc::new(ChopdropParams::new(model, loader)),
            published,
            engine: Engine::new().with_preview(channel).with_tempo(tempo),
            events: Vec::with_capacity(EVENT_CAPACITY),
            dropped_events: 0,
            send_failures: 0,
            editor_state: editor::default_state(),
            repaint_notifier: RepaintNotifier::new(),
            previewer,
        }
    }
}

impl Chopdrop {
    /// Wait for the instance's background writers to run dry — every queued
    /// decode landed, then every pending render rendered
    /// ([`LanesField::finish`]) — and stop them, handing back the model the
    /// instance played. For tests, which drive the real plugin struct the
    /// way a host does and then need to know a restore is over.
    ///
    /// # Panics
    ///
    /// If a handle on the parameters the host was given (`Plugin::params`)
    /// or an editor built over them is still alive.
    pub fn finish(self) -> SharedModel {
        let Ok(params) = Arc::try_unwrap(self.params) else {
            panic!("finish needs the only handle on the parameters");
        };
        params.lanes.finish()
    }
}

/// The handles one instance is wired from, borrowed off it
/// ([`Chopdrop::wiring`]) so that a test can check they are the one set and
/// not copies: one model, its loader and render worker, the previewer and
/// the cells the engine reads.
pub struct Wiring<'a> {
    /// The model behind the persisted lanes field.
    pub model: &'a SharedModel,
    /// The lane list the audio thread reads.
    pub published: &'a Arc<PublishedCell>,
    /// The instance's background decoder.
    pub loader: &'a Arc<Loader>,
    /// The instance's background time-stretch renderer.
    pub render: &'a Arc<RenderWorker>,
    /// The instance's preview decoder.
    pub previewer: &'a Arc<Previewer>,
    /// The tempo cell the engine reads (and `process()` stores into).
    pub engine_tempo: &'a Arc<TempoCell>,
    /// The preview channel the engine plays.
    pub engine_preview: Option<&'a Arc<PreviewChannel>>,
    /// The audition ring the engine drains.
    pub auditions: &'a Arc<AuditionRing>,
}

impl Chopdrop {
    /// The instance's handles, for the wiring tests (`tests/wiring.rs`).
    pub fn wiring(&self) -> Wiring<'_> {
        Wiring {
            model: self.params.lanes.model(),
            published: &self.published,
            loader: self.params.lanes.loader(),
            render: self.params.lanes.render(),
            previewer: &self.previewer,
            engine_tempo: self.engine.tempo(),
            engine_preview: self.engine.preview(),
            auditions: self.engine.auditions(),
        }
    }

    /// The editor [`Plugin::editor`] hands a host, as its bare app with no
    /// window around it: over the same handles, with the same repaint hooks
    /// installed. Drawn a frame at a time by [`ChopdropEditor::draw`] inside
    /// a test `egui::Context` — display-free.
    pub fn headless_editor(&mut self) -> ChopdropEditor {
        editor::app(
            &self.repaint_notifier,
            Arc::clone(&self.params),
            Arc::clone(self.engine.auditions()),
            Arc::clone(&self.previewer),
        )
    }
}

impl Plugin for Chopdrop {
    const NAME: &'static str = "chopdrop";
    const VENDOR: &'static str = "pywkt";
    const URL: &'static str = "https://github.com/pywkt/chopdrop";
    const EMAIL: &'static str = "bitstep@pywkt.com";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    /// An instrument: no audio in, sixteen stereo outputs — the main "Out 1"
    /// plus 15 aux "Out 2"…"Out 16", one per value a lane's output may hold.
    /// Stereo even though a lane may hold a mono sample — lanes are panned,
    /// so the mix is stereo whatever went into it. One layout only: a second,
    /// smaller one would let a host pick it and silently fold away the lanes
    /// routed past its last port.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: Some(STEREO),
        aux_output_ports: &[STEREO; AUX_OUTPUTS],
        names: PortNames {
            layout: Some("16 outputs"),
            main_output: Some("Out 1"),
            aux_outputs: &AUX_OUTPUT_NAMES,
            ..PortNames::const_default()
        },
        ..AudioIOLayout::const_default()
    }];

    /// `Basic` in: note on/off, choke and the standard per-note expressions
    /// (`PolyTuning`/`PolyPan`/`PolyVolume`), which is everything bitstep
    /// sends. Not `MidiCCs`, which bitstep needs only because it is a note
    /// effect that must pass controller data through to the instrument
    /// behind it — chopdrop *is* that instrument and has nothing to forward.
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    /// `None`: chopdrop emits no notes. `VoiceTerminated` still reaches the
    /// host — nice-plug gates that one event on `MIDI_INPUT`, not
    /// `MIDI_OUTPUT`, because it is a hint to the host about a voice this
    /// plugin was given rather than an event for anything downstream
    /// (checked in nice-plug 0.4.2, `wrapper/clap/context.rs`, the
    /// `NoteEvent::VoiceTerminated` arm). Declaring a note output port would
    /// instead make most hosts stop passing note input through to this
    /// track's other devices.
    const MIDI_OUTPUT: MidiConfig = MidiConfig::None;

    type Editor = editor::HostEditor;
    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    /// The egui editor, over the window state this instance holds.
    ///
    /// Assembling it opens no window — the host does that later through
    /// `Editor::spawn()` — so this is safe to call with no display, which is
    /// how it is tested here. The parameters go in whole: the top bar drives
    /// the master volume through the host and the lanes behind
    /// [`LanesField`] through the model's writer API. The background-task
    /// executor goes unused: the GUI reads the model directly and has
    /// nothing to hand off (`BackgroundTask = ()`).
    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Self::Editor> {
        editor::create(
            Arc::clone(&self.editor_state),
            self.repaint_notifier.clone(),
            Arc::clone(&self.params),
            Arc::clone(self.engine.auditions()),
            Arc::clone(&self.previewer),
        )
    }

    /// One block of audio: the host's events in, the lanes they name out.
    ///
    /// Thin on purpose — which lane answers a note, and where inside the
    /// block it answers, is [`Engine::run_block`]'s to decide and is unit
    /// tested without a host. What is left here is the shell's own: the
    /// three steps below, in order.
    ///
    /// 1. Every host event is drained (an event left in nice-plug's queue is
    ///    an event that arrives late in the next block) and the ones a lane
    ///    can answer are translated into [`TimedEvent`]s keeping their own
    ///    sample offsets.
    /// 2. The block is rendered into the host's stereo pairs — the main
    ///    buffer and the aux ones, each voice into its lane's output
    ///    ([`block_outputs`]) — ramping to the smoothed master gain across
    ///    the block ([`ChopdropParams::next_master_gain`]). The buffers are
    ///    *written*, not added to: they belong to the host and arrive
    ///    holding whatever was last in them, so even "produce nothing" has
    ///    to be said or the track plays back the stale contents of someone
    ///    else's buffer.
    /// 3. The voices that ended on the way are reported to the host as
    ///    `VoiceTerminated`, so a host that tracks voices can free what it
    ///    reserved for them. A refusal is dropped and counted, as bitstep
    ///    does: there is nothing else a block can do about it.
    ///
    /// Allocation-free and lock-free, which the `assert_process_allocs`
    /// feature enforces in debug and test builds: the event buffer was
    /// allocated with the instance, the lane snapshot comes from a
    /// `SharedCell` rather than the writer mutex, and the smoother and the
    /// published atomics are relaxed loads.
    fn process(
        &mut self,
        buffer: &mut Buffer,
        aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let frames = buffer.samples();
        // The host's rate, which every voice's step and fade length is
        // computed from. Guarded where it is used ([`crate::voices::step_for`]),
        // so a host claiming zero or a NaN cannot make a voice immortal.
        let host_rate = f64::from(context.transport().sample_rate);
        // The host tempo, into the instance's one cell (clamped there); a
        // block without one keeps the last.
        self.engine.tempo().update(context.transport().tempo);
        let master_volume = self.params.next_master_gain(frames);

        // Step 1.
        self.events.clear();
        let mut dropped: u32 = 0;
        while let Some(event) = context.next_event() {
            let Some(timed) = engine_event(event) else { continue };
            if self.events.len() < EVENT_CAPACITY {
                self.events.push(timed);
            } else {
                dropped = dropped.saturating_add(1);
            }
        }

        // Step 2. Every output is stereo by contract, so a host that hands
        // over a main buffer with fewer channels has stepped outside it: it
        // gets silence rather than a panic or a half-written buffer.
        if let Some(mut outputs) = block_outputs(buffer.as_slice(), aux.outputs) {
            self.engine.run_block(
                &self.published,
                &self.events,
                &mut outputs,
                host_rate,
                master_volume,
            );
        } else {
            nice_debug_assert!(false, "the host gave fewer than two output channels");
        }

        // Step 3.
        let mut refused: u32 = 0;
        let last_frame = frames.saturating_sub(1) as u32;
        for &voice in self.engine.pool().terminated() {
            if context.try_send_event(voice_terminated(voice, last_frame)).is_err() {
                refused = refused.saturating_add(1);
            }
        }
        self.engine.clear_terminated();

        // Both counters are per-instance totals; the assertion is per block,
        // so a host that refuses one event does not then complain every
        // block for the rest of the session. Neither does anything in
        // release.
        self.dropped_events = self.dropped_events.saturating_add(dropped);
        self.send_failures = self.send_failures.saturating_add(refused);
        nice_debug_assert_eq!(refused, 0, "the host refused {} voice-end event(s)", refused);

        // And the editor's wake-up call, while there is anything to see (the
        // window repaints continuously while any light is fading or any voice
        // sounds). The audio thread's half of that pair: an open window is
        // drawn on the block a lane is struck in and every block something is
        // still playing, so a dot lights on the note that lit it rather than
        // on the next time the pointer moves — and the window asks for the
        // fade's own remaining frames itself
        // ([`editor::activity::Lights::fading`]). A lane struck by a sample
        // shorter than this buffer is why the strike is asked about and not
        // only the sound ([`engine::Engine::needs_a_frame`]). One relaxed
        // atomic store, and nothing at all when the editor is closed.
        if self.engine.needs_a_frame() {
            self.repaint_notifier.request_repaint();
        }

        // `KeepAlive`, not `Normal`: nice-plug reports `Normal` to a CLAP
        // host as "continue if not quiet", and a sampler between two hits is
        // quiet — a host that suspends quiet plugins could then stop calling
        // `process()` and miss the next note. A sample tail is not a reverb
        // tail of knowable length either, so `Tail` has no honest value here.
        ProcessStatus::KeepAlive
    }

    /// The host rewinding, or starting playback somewhere else: everything
    /// sounding stops at once ([`Engine::reset`]).
    ///
    /// No fade, unlike every musical stop: there is no block left to fade
    /// into and nothing listening to the one that was. Called from the audio
    /// thread, so it allocates nothing either.
    fn reset(&mut self) {
        self.engine.reset();
    }

    /// The host putting the instance to sleep. Same answer as
    /// [`Plugin::reset`], for the same reason: the next block, if there is
    /// one, starts from silence rather than from a voice half way through a
    /// sample the host has stopped asking about.
    fn deactivate(&mut self) {
        self.engine.reset();
    }
}

impl ClapPlugin for Chopdrop {
    const CLAP_ID: &'static str = "com.pywkt.chopdrop";
    const CLAP_DESCRIPTION: Option<&'static str> = Some(env!("CARGO_PKG_DESCRIPTION"));
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = Some("https://github.com/pywkt/chopdrop/issues");
    /// `Instrument` is the main category Bitwig's instrument browser reads;
    /// `Sampler` and `Stereo` are the optional hints for what kind of one.
    const CLAP_FEATURES: &'static [ClapFeature] =
        &[ClapFeature::Instrument, ClapFeature::Sampler, ClapFeature::Stereo];
}

nice_export_clap!(Chopdrop);

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::VecDeque;
    use std::path::Path;

    use nice_plug::context::process::SendEventError;

    use crate::model::{SampleRef, DEFAULT_LANE_COUNT};
    use crate::sample::SampleData;
    use crate::state::CURRENT_VERSION;
    use crate::voices::LaneMode;

    /// The rate every test runs at — the host's and every fixture
    /// sample's, so the playback step is exactly 1.0 and a block can be
    /// compared with the file frame for frame (`engine.rs`'s tests do the
    /// same).
    const RATE: f32 = 48_000.0;

    /// Parameters over a fresh default model, as a new instance has them.
    ///
    /// Each comes with its own loader — and so its own worker thread,
    /// stopped when the params are dropped — because a [`LanesField`] holds
    /// both: there is no params struct without the lanes behind it.
    fn test_params() -> ChopdropParams {
        let model: SharedModel = Arc::new(Mutex::new(Model::new()));
        let loader = Arc::new(Loader::new(Arc::clone(&model)));
        let params = ChopdropParams::new(model, loader);
        prime_smoothers(&params);
        params
    }

    /// What the host does before the first block and nothing else does: a
    /// smoother built by `with_smoother` starts at 0.0, and nice-plug's
    /// wrapper resets every one of them to its parameter's value in
    /// `activate()` (`wrapper/clap/wrapper.rs`). Without it a test would be
    /// reading a master volume no host ever has.
    fn prime_smoothers(params: &ChopdropParams) {
        params.volume.smoothed.reset(params.volume.value());
    }

    /// A plugin instance as the host hands it its first block: defaults,
    /// with the parameter smoothers primed ([`prime_smoothers`]) — and the
    /// "bitstep Sample pad" switch turned **off**. A new instance starts
    /// with it on (`SAMPLE_PAD_SWITCH_DEFAULT`), and on, these tests' bare
    /// note-ons on C1 would play two octaves down: the switch's business,
    /// pinned in `engine.rs`, not theirs.
    fn activated_plugin() -> Chopdrop {
        let plugin = Chopdrop::default();
        prime_smoothers(&plugin.params);
        lock_model(model_of(&plugin)).set_sample_pad_switch(false);
        plugin
    }

    /// The model behind `plugin`'s persistent field — the one the audio
    /// thread reads through its published cell.
    fn model_of(plugin: &Chopdrop) -> &SharedModel {
        plugin.params.lanes.model()
    }

    /// A mono sample whose frames are 1.0, 2.0, … `count`, so any frame can
    /// be told from any other by eye in a failure message (`voices.rs` and
    /// `engine.rs` use the same fixture).
    fn ramp(count: usize) -> Vec<f32> {
        (1..=count).map(|n| n as f32).collect()
    }

    /// Give `plugin`'s first lane a sample and hand back its id.
    ///
    /// The lane is panned **hard left**, where the constant-power pan law's
    /// gains are exactly 1.0 and 0.0 and the mix therefore multiplies by one
    /// — the one setting in which a rendered block can be compared with the
    /// file itself (`voices.rs`, §"Unity gain" is a pan position). Its note
    /// is the default 36, which is what [`a_block_of_note_events`] plays.
    fn first_lane_with(plugin: &Chopdrop, frames: Vec<f32>, mode: LaneMode) -> u32 {
        let mut model = lock_model(model_of(plugin));
        let id = model.lanes()[0].id;
        let data = SampleData { rate: RATE as u32, channels: 1, frames: frames.into() };
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/kick.wav"), vec![0], data);
        assert!(model.set_sample(id, sample), "the first lane takes a sample");
        assert!(model.set_pan(id, -1.0), "the first lane pans hard left");
        assert!(model.set_mode(id, mode), "the first lane takes a mode");
        id
    }

    /// Decibels either side of a gain conversion, which rounds: the pin is on
    /// the dB figure a user reads, not on the last bit of a `powf`.
    fn assert_db_near(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.01, "{actual} dB, expected about {expected} dB");
    }

    /// An instrument, not a note effect: no audio input, a stereo main output
    /// and 15 stereo aux outputs. The host picks the first layout, so there
    /// being exactly one is part of the contract.
    #[test]
    fn the_shell_is_an_instrument_with_sixteen_stereo_outputs() {
        assert_eq!(Chopdrop::AUDIO_IO_LAYOUTS.len(), 1);
        let layout = Chopdrop::AUDIO_IO_LAYOUTS[0];
        assert_eq!(layout.main_input_channels, None);
        assert_eq!(layout.main_output_channels, NonZeroU32::new(2));
        assert!(layout.aux_input_ports.is_empty());
        assert_eq!(layout.aux_output_ports.len(), 15);
        assert_eq!(AUX_OUTPUTS, 15);
        assert_eq!(OUTPUTS, 16);
        assert!(layout.aux_output_ports.iter().all(|&port| port == STEREO));
    }

    /// The ports' names, in order: "Out 1" is the main output and
    /// `aux.outputs[n]` is "Out n+2" — the numbering the lane panel's output
    /// dropdown shows and Bitwig's chains are labelled with.
    #[test]
    fn the_outputs_are_named_out_1_to_out_16_in_order() {
        let names = Chopdrop::AUDIO_IO_LAYOUTS[0].names;
        assert_eq!(names.main_output, Some("Out 1"));
        assert_eq!(names.main_input, None);
        assert!(names.aux_inputs.is_empty());
        let expected: Vec<String> = (2..=16).map(|n| format!("Out {n}")).collect();
        assert_eq!(names.aux_outputs, expected);
    }

    /// Notes in, nothing out. `Basic` is the level that carries note
    /// on/off/choke and the per-note expressions bitstep sends, and it is
    /// also the level nice-plug requires before it will pass a
    /// `VoiceTerminated` on to the host.
    #[test]
    fn notes_come_in_and_no_notes_go_out() {
        assert_eq!(Chopdrop::MIDI_INPUT, MidiConfig::Basic);
        assert!(Chopdrop::MIDI_INPUT >= MidiConfig::Basic);
        assert_eq!(Chopdrop::MIDI_OUTPUT, MidiConfig::None);
    }

    /// The host-visible parameter list: exactly one entry, at the top level,
    /// under the stable id that host automation lanes and saved projects key
    /// on. Renaming `volume` silently breaks every existing project, so the
    /// id is pinned here rather than only in the attribute.
    #[test]
    fn param_map_exposes_volume_alone() {
        let params = test_params();
        let exposed: Vec<(String, String)> =
            params.param_map().into_iter().map(|(id, _, group)| (id, group)).collect();
        assert_eq!(exposed, vec![("volume".to_string(), String::new())]);
    }

    /// Range and default of the master volume, as `clap-validator`'s
    /// `param-default-values` test on the bundle also checks: a gain that
    /// reads −60 dB at the bottom, +12 dB at the top and 0 dB — unity — where
    /// it starts.
    #[test]
    fn master_volume_runs_from_minus_60_to_plus_12_db_starting_at_unity() {
        let params = test_params();

        assert_eq!(params.volume.name(), "Volume");
        assert_eq!(params.volume.default_plain_value(), 1.0);
        assert_eq!(params.volume.value(), 1.0);
        assert_eq!(params.next_master_gain(64), 1.0);
        assert_eq!(util::gain_to_db(params.volume.default_plain_value()), VOLUME_DEFAULT_DB);

        // Continuous, so the host draws a knob rather than stepping through
        // discrete positions.
        assert_eq!(params.volume.step_count(), None);

        assert_db_near(util::gain_to_db(params.volume.preview_plain(0.0)), VOLUME_MIN_DB);
        assert_db_near(util::gain_to_db(params.volume.preview_plain(1.0)), VOLUME_MAX_DB);
        // The skew is what makes the control linear in decibels: halfway
        // along it sits at the middle of the dB range, not the middle of the
        // gain range (which would be about −6 dB).
        let middle_db = (VOLUME_MIN_DB + VOLUME_MAX_DB) / 2.0;
        assert_db_near(util::gain_to_db(params.volume.preview_plain(0.5)), middle_db);

        // 0 dB is unity exactly, so an untouched master is bit-transparent.
        assert_eq!(params.volume.preview_normalized(1.0), params.volume.default_normalized_value());
    }

    /// The host's text for the parameter is decibels, and typing decibels
    /// back in lands on the same gain.
    #[test]
    fn the_host_shows_and_reads_the_volume_in_decibels() {
        let params = test_params();

        assert_eq!(params.volume.unit(), " dB");
        assert_eq!(params.volume.to_string(), "0.00 dB");
        let default = params.volume.default_normalized_value();
        assert_eq!(params.volume.normalized_value_to_string(default, true), "0.00 dB");

        let plus_six =
            params.volume.string_to_normalized_value("6.0 dB").expect("\"6.0 dB\" is a dB value");
        assert_db_near(util::gain_to_db(params.volume.preview_plain(plus_six)), 6.0);
    }

    /// Host parameter values are untrusted. A NaN must come out as a usable
    /// gain rather than travelling into the mix, and `max`-then-`min` is what
    /// makes that so — `f32::clamp` would return the NaN.
    #[test]
    fn a_hostile_gain_is_clamped_nan_and_all() {
        let min = util::db_to_gain(VOLUME_MIN_DB);
        let max = util::db_to_gain(VOLUME_MAX_DB);

        assert_eq!(clamp_gain(f32::NAN), min);
        assert_eq!(clamp_gain(f32::NEG_INFINITY), min);
        assert_eq!(clamp_gain(f32::INFINITY), max);
        assert_eq!(clamp_gain(-1.0), min);
        assert_eq!(clamp_gain(1_000.0), max);
        assert_eq!(clamp_gain(1.0), 1.0);
    }

    /// Everything a host finds inside `target/bundled/chopdrop.clap` reaches it
    /// through the `clap_entry` symbol that `nice_export_clap!` generates, so
    /// the bundle's contract is checked here rather than by inspecting the
    /// built `.clap` file: entry point present, plugin factory reachable under
    /// the CLAP factory ID, and exactly one plugin described in it.
    ///
    /// Deliberately stops short of `create_plugin()` — that needs a real
    /// `clap_host`, which is what `clap-validator validate` on the bundle is
    /// for.
    #[test]
    fn clap_entry_exposes_one_sampler_instrument() {
        use nice_plug::wrapper::clap::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
        use std::ffi::CStr;

        assert_eq!(clap_entry.clap_version.major, 1);

        let get_factory = clap_entry.get_factory.expect("no `get_factory` in `clap_entry`");
        let factory =
            unsafe { get_factory(CLAP_PLUGIN_FACTORY_ID.as_ptr()) } as *const clap_plugin_factory;
        assert!(!factory.is_null(), "plugin factory not exposed");

        // SAFETY: `factory` is the macro's own `static CLAP_PLUGIN_FACTORY`,
        // and its function pointers take that same pointer back.
        unsafe {
            let count = (*factory).get_plugin_count.expect("no `get_plugin_count`")(factory);
            assert_eq!(count, 1, "the bundle must expose exactly one plugin");

            let descriptor =
                (*factory).get_plugin_descriptor.expect("no `get_plugin_descriptor`")(factory, 0);
            assert!(!descriptor.is_null());
            assert_eq!(CStr::from_ptr((*descriptor).id).to_str().unwrap(), Chopdrop::CLAP_ID);
            assert_eq!(CStr::from_ptr((*descriptor).name).to_str().unwrap(), Chopdrop::NAME);

            // The feature list is an environ-style null-terminated array; an
            // instrument that does not advertise `instrument` is invisible in
            // Bitwig's instrument browser.
            let mut features = Vec::new();
            let mut ptr = (*descriptor).features;
            while !(*ptr).is_null() {
                features.push(CStr::from_ptr(*ptr).to_str().unwrap());
                ptr = ptr.add(1);
            }
            assert_eq!(features, vec!["instrument", "sampler", "stereo"]);
        }
    }

    /// A stand-in for the host, so `process()` can be driven with no Bitwig
    /// and no audio device.
    ///
    /// nice-plug ships no test host and bitstep never needed one (it tests
    /// its engine directly), but `process()` is the only logic in this file,
    /// and "writes silence" is a claim worth holding to. `ProcessContext` is
    /// nine methods and a sampler shell uses two: events queued in `incoming`
    /// come back out of `next_event()` in order, and anything the plugin
    /// sends lands in `sent` — which is where the tests look for its
    /// `VoiceTerminated`s.
    struct TestHost {
        transport: Transport,
        incoming: VecDeque<PluginNoteEvent<Chopdrop>>,
        sent: Vec<PluginNoteEvent<Chopdrop>>,
    }

    impl TestHost {
        /// A stopped host at 48 kHz with `incoming` queued up for the block.
        fn with_events(incoming: Vec<PluginNoteEvent<Chopdrop>>) -> TestHost {
            TestHost {
                transport: Transport::new(48_000.0),
                incoming: incoming.into(),
                sent: Vec::new(),
            }
        }
    }

    impl ProcessContext<Chopdrop> for TestHost {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Clap
        }

        // `BackgroundTask = ()`, so there is nothing to run and nothing to
        // record: a task that carries no information cannot be asserted on.
        fn execute_background(&self, _task: ()) {}
        fn execute_gui(&self, _task: ()) {}

        fn transport(&self) -> &Transport {
            &self.transport
        }

        fn next_event(&mut self) -> Option<PluginNoteEvent<Chopdrop>> {
            self.incoming.pop_front()
        }

        fn try_send_event(
            &mut self,
            event: PluginNoteEvent<Chopdrop>,
        ) -> Result<(), (PluginNoteEvent<Chopdrop>, SendEventError)> {
            self.sent.push(event);
            Ok(())
        }

        // Three the shell has no business touching. `unreachable!` rather
        // than a no-op so that the day one of them starts being called, a
        // test says so instead of the call vanishing: latency would have to
        // be declared at `activate()`, a restart request from a sampler would
        // be a bug, and voice capacity may only be set with
        // `CLAP_POLY_MODULATION_CONFIG`, which chopdrop does not set.
        fn set_latency_samples(&self, _samples: u32) {
            unreachable!("chopdrop reports no latency");
        }

        fn request_restart(&self) {
            unreachable!("chopdrop never asks the host to restart it");
        }

        fn set_current_voice_capacity(&self, _capacity: u32) {
            unreachable!("chopdrop sets no polyphonic modulation config");
        }
    }

    /// A full-velocity note-on on key 36 — the first lane's note — from a
    /// host that tracks voices, as [`a_block_of_note_events`] sends.
    fn note_on(timing: u32) -> PluginNoteEvent<Chopdrop> {
        NoteEvent::NoteOn {
            timing,
            voice_id: VoiceID::ID(7),
            channel: Channel::Number(0),
            key: Key::Number(36),
            velocity: 1.0,
        }
    }

    /// One note-on and its note-off on key 36 — the first lane's note — plus
    /// the expressions bitstep sends alongside, so a block carries every
    /// event kind `MidiConfig::Basic` delivers.
    fn a_block_of_note_events() -> Vec<PluginNoteEvent<Chopdrop>> {
        let voice_id = VoiceID::ID(7);
        let channel = Channel::Number(0);
        let key = Key::Number(36);
        vec![
            NoteEvent::NoteOn { timing: 0, voice_id, channel, key, velocity: 1.0 },
            NoteEvent::PolyTuning { timing: 0, voice_id, channel, key, tuning: 12.0 },
            NoteEvent::PolyPan { timing: 0, voice_id, channel, key, pan: -1.0 },
            NoteEvent::PolyVolume { timing: 0, voice_id, channel, key, gain: 0.5 },
            NoteEvent::NoteOff { timing: 128, voice_id, channel, key, velocity: 0.0 },
        ]
    }

    /// Runs one block through `process()` and hands back what the host sees:
    /// the status, the two output channels, and the host itself.
    ///
    /// `dirty` is written into both channels first. The host's output buffer
    /// arrives holding whatever was last in it, so starting from silence
    /// would let a `process()` that writes nothing at all pass.
    fn render_block(
        plugin: &mut Chopdrop,
        frames: usize,
        dirty: f32,
        events: Vec<PluginNoteEvent<Chopdrop>>,
    ) -> (ProcessStatus, Vec<Vec<f32>>, TestHost) {
        render_block_for(plugin, frames, dirty, TestHost::with_events(events))
    }

    /// [`render_block`] for a host set up by the caller (its transport, say).
    fn render_block_for(
        plugin: &mut Chopdrop,
        frames: usize,
        dirty: f32,
        mut host: TestHost,
    ) -> (ProcessStatus, Vec<Vec<f32>>, TestHost) {
        let mut channels = vec![vec![dirty; frames], vec![dirty; frames]];

        let mut buffer = Buffer::default();
        // SAFETY: `channels` outlives `buffer`, which is dropped at the end
        // of this scope, and both slices are `frames` long as `set_slices`
        // requires.
        unsafe {
            buffer.set_slices(frames, |slices| {
                let (left, right) = channels.split_at_mut(1);
                *slices = vec![&mut left[0], &mut right[0]];
            });
        }

        let mut aux = AuxiliaryBuffers { inputs: &mut [], outputs: &mut [] };
        let status = plugin.process(&mut buffer, &mut aux, &mut host);
        drop(buffer);

        (status, channels, host)
    }

    /// [`render_block`] with `aux` aux output buffers of `aux_channels`
    /// channels each, all as dirty as the main one: the host's main
    /// channels and then every aux buffer's, in port order.
    fn render_block_with_aux(
        plugin: &mut Chopdrop,
        frames: usize,
        dirty: f32,
        events: Vec<PluginNoteEvent<Chopdrop>>,
        aux: usize,
        aux_channels: usize,
    ) -> (Vec<Vec<f32>>, Vec<Vec<Vec<f32>>>) {
        let mut main = vec![vec![dirty; frames], vec![dirty; frames]];
        let mut aux_data = vec![vec![vec![dirty; frames]; aux_channels]; aux];
        let mut host = TestHost::with_events(events);
        {
            let mut buffer = Buffer::default();
            let mut aux_buffers: Vec<Buffer> = Vec::new();
            // SAFETY: `main` and `aux_data` outlive the buffers, which are
            // dropped at the end of this scope, and every slice is `frames`
            // long as `set_slices` requires.
            unsafe {
                buffer.set_slices(frames, |slices| {
                    *slices = main.iter_mut().map(|channel| channel.as_mut_slice()).collect();
                });
                for channels in aux_data.iter_mut() {
                    let mut aux_buffer = Buffer::default();
                    aux_buffer.set_slices(frames, |slices| {
                        *slices =
                            channels.iter_mut().map(|channel| channel.as_mut_slice()).collect();
                    });
                    aux_buffers.push(aux_buffer);
                }
            }
            let mut aux = AuxiliaryBuffers { inputs: &mut [], outputs: &mut aux_buffers };
            let status = plugin.process(&mut buffer, &mut aux, &mut host);
            assert!(matches!(status, ProcessStatus::KeepAlive), "{status:?}");
        }
        (main, aux_data)
    }

    /// The routing through nice-plug's own types: a lane on Out 4 lands in
    /// `aux.outputs[2]` and nowhere else, and every other buffer the host
    /// handed over — main, the other aux ports, an aux port's extra
    /// channel — comes back silent, not stale.
    #[test]
    fn process_writes_a_routed_lane_into_its_own_aux_buffer() {
        let mut plugin = activated_plugin();
        let id = first_lane_with(&plugin, ramp(8), LaneMode::OneShot);
        assert!(lock_model(model_of(&plugin)).set_output(id, 3), "the lane takes Out 4");

        let (main, aux) =
            render_block_with_aux(&mut plugin, 12, 0.5, vec![note_on(2)], AUX_OUTPUTS, 3);

        let sound = vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0];
        let silence = vec![0.0; 12];
        assert_eq!(main, vec![silence.clone(), silence.clone()], "nothing on Out 1");
        for (index, channels) in aux.iter().enumerate() {
            if index == 2 {
                assert_eq!(channels[0], sound, "Out 4 left");
                assert_eq!(channels[1], silence, "Out 4 right: the lane is hard left");
                assert_eq!(channels[2], silence, "Out 4's extra channel");
            } else {
                for channel in channels {
                    assert_eq!(channel, &silence, "Out {} is not silent", index + 2);
                }
            }
        }
    }

    /// A host that hands over no aux buffers — or mono ones — still hears
    /// every lane: a lane routed past what the host provided falls back to
    /// Out 1, and the unusable aux channels are silenced.
    #[test]
    fn without_usable_aux_buffers_everything_lands_on_main() {
        let sound = vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0];
        for (aux_count, aux_channels) in [(0, 2), (AUX_OUTPUTS, 1)] {
            let mut plugin = activated_plugin();
            let id = first_lane_with(&plugin, ramp(8), LaneMode::OneShot);
            assert!(lock_model(model_of(&plugin)).set_output(id, 15), "the lane takes Out 16");

            let (main, aux) = render_block_with_aux(
                &mut plugin,
                12,
                0.5,
                vec![note_on(2)],
                aux_count,
                aux_channels,
            );

            assert_eq!(main[0], sound, "{aux_count} aux buffer(s) of {aux_channels}");
            assert_eq!(main[1], vec![0.0; 12]);
            assert!(aux.iter().flatten().all(|channel| channel.iter().all(|&s| s == 0.0)));
        }
    }

    /// The shell's whole audible contract: whatever the host left in its
    /// output buffer, chopdrop replaces it with silence. A `process()` that
    /// skipped the write would play back the stale contents of someone
    /// else's buffer, which is the loud kind of bug.
    #[test]
    fn process_writes_silence_over_whatever_the_host_left_in_the_buffer() {
        let mut plugin = activated_plugin();
        let (status, channels, _host) = render_block(&mut plugin, 512, 0.5, Vec::new());

        assert!(matches!(status, ProcessStatus::KeepAlive), "{status:?}");
        for (index, channel) in channels.iter().enumerate() {
            assert_eq!(channel.len(), 512);
            assert!(
                channel.iter().all(|&sample| sample == 0.0),
                "channel {index} is not silent: {:?}",
                &channel[..8]
            );
        }
    }

    /// Every note event the host sends is taken off nice-plug's input queue:
    /// an event left behind is an event that arrives late in the next block.
    /// And with no lane holding a sample, nothing goes back either — a
    /// sampler emits no notes, and the `VoiceTerminated`s that are the one
    /// exception need a voice to have sounded first.
    #[test]
    fn process_drains_the_hosts_events_and_sends_none_back() {
        let mut plugin = activated_plugin();
        let events = a_block_of_note_events();
        let queued = events.len();
        let (_status, _channels, host) = render_block(&mut plugin, 512, 0.0, events);

        assert_eq!(queued, 5, "the block should carry every Basic-level event kind");
        assert!(host.incoming.is_empty(), "{} event(s) left queued", host.incoming.len());
        assert!(host.sent.is_empty(), "the shell sent {:?}", host.sent);
    }

    /// Hosts send odd block sizes, including empty ones, and
    /// `clap-validator`'s `process-random-block-sizes` leans on it. Nothing
    /// here may index past the end or divide by the block length.
    #[test]
    fn process_survives_an_empty_block_and_an_odd_one() {
        let mut plugin = activated_plugin();

        let (status, channels, _host) = render_block(&mut plugin, 0, 0.0, Vec::new());
        assert!(matches!(status, ProcessStatus::KeepAlive), "{status:?}");
        assert!(channels.iter().all(|channel| channel.is_empty()));

        // One frame, and a prime that no block loop divides into.
        for frames in [1, 37] {
            let (status, channels, _host) =
                render_block(&mut plugin, frames, 1.0, a_block_of_note_events());
            assert!(matches!(status, ProcessStatus::KeepAlive), "{status:?}");
            for channel in &channels {
                assert_eq!(channel, &vec![0.0; frames], "{frames}-frame block");
            }
        }
    }

    /// `KeepAlive` every block, never `Normal`: a host that suspends quiet
    /// plugins would otherwise be free to stop calling `process()` between
    /// two hits and miss the note that ends the silence.
    #[test]
    fn process_always_keeps_the_plugin_alive() {
        let mut plugin = activated_plugin();
        for _ in 0..4 {
            let (status, _channels, _host) = render_block(&mut plugin, 64, 0.0, Vec::new());
            assert!(matches!(status, ProcessStatus::KeepAlive), "{status:?}");
        }
    }

    /// The host tempo lands in the instance's one cell: 120 before any block,
    /// the host's value after one, kept through a block without one, clamped
    /// when the host sends nonsense — and the model, which hands the cell to
    /// the editor, sees the same cell the engine reads.
    #[test]
    fn process_stores_the_host_tempo_in_the_shared_cell() {
        let mut plugin = activated_plugin();
        let cell = Arc::clone(lock_model(model_of(&plugin)).tempo());
        assert!(Arc::ptr_eq(&cell, plugin.engine.tempo()));
        assert_eq!(cell.get(), time::TEMPO_DEFAULT);

        let block = |plugin: &mut Chopdrop, tempo: Option<f64>| {
            let mut host = TestHost::with_events(Vec::new());
            host.transport.tempo = tempo;
            render_block_for(plugin, 64, 0.0, host);
        };
        block(&mut plugin, Some(90.0));
        assert_eq!(cell.get(), 90.0);
        block(&mut plugin, None);
        assert_eq!(cell.get(), 90.0);
        block(&mut plugin, Some(f64::NAN));
        assert_eq!(cell.get(), time::TEMPO_DEFAULT);
        block(&mut plugin, Some(0.0));
        assert_eq!(cell.get(), time::TEMPO_MIN);
        block(&mut plugin, Some(f64::INFINITY));
        assert_eq!(cell.get(), time::TEMPO_MAX);
    }

    // ---- step 1: host events into engine events -------------------------

    /// The step 1 mapping, field for field and offset for offset: the five
    /// kinds bitstep sends, plus the host's own choke.
    #[test]
    fn host_note_events_become_engine_events_at_their_own_offsets() {
        let voice_id = VoiceID::ID(7);
        let channel = Channel::Number(3);
        let key = Key::Number(36);
        let note = NoteTarget { channel: 3, key: 36, voice_id: Some(7) };
        let mapped = |event| engine_event(event).expect("a mappable event");

        assert_eq!(
            mapped(NoteEvent::NoteOn { timing: 11, voice_id, channel, key, velocity: 0.75 }),
            TimedEvent::new(11, EngineEvent::NoteOn { note, velocity: 0.75 })
        );
        assert_eq!(
            mapped(NoteEvent::NoteOff { timing: 12, voice_id, channel, key, velocity: 0.0 }),
            TimedEvent::new(12, EngineEvent::NoteOff { note })
        );
        assert_eq!(
            mapped(NoteEvent::Choke { timing: 13, voice_id, channel, key }),
            TimedEvent::new(13, EngineEvent::Choke { note })
        );
        assert_eq!(
            mapped(NoteEvent::PolyTuning { timing: 14, voice_id, channel, key, tuning: -5.0 }),
            TimedEvent::new(14, EngineEvent::PolyTuning { note, semis: -5.0 })
        );
        assert_eq!(
            mapped(NoteEvent::PolyPan { timing: 15, voice_id, channel, key, pan: 0.5 }),
            TimedEvent::new(15, EngineEvent::PolyPan { note, pan: 0.5 })
        );
        assert_eq!(
            mapped(NoteEvent::PolyVolume { timing: 16, voice_id, channel, key, gain: 0.25 }),
            TimedEvent::new(16, EngineEvent::PolyVolume { note, gain: 0.25 })
        );
    }

    /// A host that does not track voices sends no id, and then the note *is*
    /// its channel and key. nice-plug spells that absence
    /// `VoiceID::Wildcard`; the engine spells it `None`.
    #[test]
    fn a_note_from_a_host_that_tracks_no_voices_maps_to_no_voice_id() {
        let mapped = engine_event(NoteEvent::NoteOn {
            timing: 0,
            voice_id: VoiceID::Wildcard,
            channel: Channel::Number(0),
            key: Key::Number(60),
            velocity: 1.0,
        })
        .expect("a note-on is mappable");

        let note = NoteTarget { channel: 0, key: 60, voice_id: None };
        assert_eq!(mapped, TimedEvent::new(0, EngineEvent::NoteOn { note, velocity: 1.0 }));
    }

    /// A host may address an event to any key or any channel (CLAP's
    /// `key = -1`). Both become the out-of-range stand-ins
    /// ([`KEY_WILDCARD`], [`CHANNEL_WILDCARD`]), which the engine reads as
    /// match-any — so the event reaches its voices whether or not it carries
    /// an id, and `NoteTarget` needs no wildcard variant of its own.
    #[test]
    fn a_wildcard_key_or_channel_becomes_the_out_of_range_stand_in() {
        let mapped = engine_event(NoteEvent::Choke {
            timing: 4,
            voice_id: VoiceID::ID(9),
            channel: Channel::Wildcard,
            key: Key::Wildcard,
        })
        .expect("a choke is mappable");

        let note =
            NoteTarget { channel: CHANNEL_WILDCARD, key: KEY_WILDCARD, voice_id: Some(9) };
        assert_eq!(mapped, TimedEvent::new(4, EngineEvent::Choke { note }));
    }

    /// Everything else is ignored rather than guessed at: no event a lane
    /// cannot answer may become an engine event by accident, and that
    /// includes chopdrop's own `VoiceTerminated` coming back the other way.
    #[test]
    fn events_a_lane_cannot_answer_are_ignored() {
        let voice_id = VoiceID::ID(1);
        let channel = Channel::Number(0);
        let key = Key::Number(36);
        let ignored: Vec<PluginNoteEvent<Chopdrop>> = vec![
            NoteEvent::PolyPressure { timing: 0, voice_id, channel, key, pressure: 1.0 },
            NoteEvent::PolyVibrato { timing: 0, voice_id, channel, key, vibrato: 1.0 },
            NoteEvent::PolyExpression { timing: 0, voice_id, channel, key, expression: 1.0 },
            NoteEvent::PolyBrightness { timing: 0, voice_id, channel, key, brightness: 1.0 },
            NoteEvent::MidiChannelPressure { timing: 0, channel: 0, pressure: 1.0 },
            NoteEvent::MidiPitchBend { timing: 0, channel: 0, value: 0.5 },
            NoteEvent::MidiCC { timing: 0, channel: 0, cc: 64, value: 1.0 },
            NoteEvent::MidiProgramChange { timing: 0, channel: 0, program: 3 },
            NoteEvent::MonoAutomation { timing: 0, poly_modulation_id: 0, normalized_value: 1.0 },
            NoteEvent::VoiceTerminated { timing: 0, voice_id, channel, key },
        ];

        for event in ignored {
            assert!(engine_event(event).is_none(), "{event:?} should be ignored");
        }
    }

    // ---- steps 2 and 3: the block, and the voices that ended ------------

    /// The point of all of it: a note the lanes answer comes out of the
    /// host's buffers, at the frame it was sent on. One lane, an eight-frame
    /// sample, hard left and at unity — the same exactness `engine.rs` pins,
    /// now through nice-plug's own types.
    #[test]
    fn process_plays_the_lane_a_note_on_names() {
        let mut plugin = activated_plugin();
        first_lane_with(&plugin, ramp(8), LaneMode::OneShot);

        let (status, channels, _host) = render_block(&mut plugin, 12, 0.5, vec![note_on(2)]);

        assert!(matches!(status, ProcessStatus::KeepAlive), "{status:?}");
        assert_eq!(channels[0], vec![0.0, 0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 0.0, 0.0]);
        assert_eq!(channels[1], vec![0.0; 12], "a hard left lane leaves the right side silent");
    }

    /// Step 3: a voice that has ended is reported back, `MIDI_OUTPUT =
    /// None` and all — nice-plug gates `VoiceTerminated` on `MIDI_INPUT`,
    /// because a host that handed out a voice id needs to hear that the id
    /// is free again. Once, and with the identity the note-on arrived under.
    #[test]
    fn process_tells_the_host_which_voices_ended() {
        let mut plugin = activated_plugin();
        first_lane_with(&plugin, ramp(4), LaneMode::OneShot);

        let (_status, _channels, host) = render_block(&mut plugin, 16, 0.0, vec![note_on(0)]);

        assert_eq!(
            host.sent,
            vec![NoteEvent::VoiceTerminated {
                // The block's last frame: the pool reports an ending only
                // once the block is rendered, and this is the one offset
                // that cannot precede the note-on that started it.
                timing: 15,
                voice_id: VoiceID::ID(7),
                channel: Channel::Number(0),
                key: Key::Number(36),
            }]
        );

        // And only once: the outbox is cleared by the block that drained it.
        let (_status, _channels, host) = render_block(&mut plugin, 16, 0.0, Vec::new());
        assert!(host.sent.is_empty(), "the shell sent {:?} a second time", host.sent);
    }

    /// The master volume is the last factor of every voice's gain chain,
    /// one target a block ([`ChopdropParams::next_master_gain`]) that the
    /// engine ramps to across the block. Flat here because this is the
    /// instance's first block, which has no previous gain to ramp from —
    /// the ramp itself is pinned in `engine.rs`.
    #[test]
    fn the_master_volume_scales_what_process_writes() {
        let mut plugin = activated_plugin();
        first_lane_with(&plugin, ramp(4), LaneMode::OneShot);

        // What a host's parameter change does: nice-plug points the smoother
        // at the new gain over `VOLUME_SMOOTHING_MS`, which at this rate is
        // fewer steps than this block is long — so the block is already all
        // the way there.
        plugin.params.volume.smoothed.set_target(RATE, 0.5);

        let (_status, channels, _host) = render_block(&mut plugin, 1024, 0.0, vec![note_on(0)]);
        assert_eq!(&channels[0][..4], &[0.5, 1.0, 1.5, 2.0]);
    }

    /// A block carrying more events than nice-plug's own queue holds is still
    /// drained to the end, and the tail is dropped rather than grown into —
    /// the real-time rule leaves no other answer. The count is the only
    /// report: a host is allowed to send this many.
    #[test]
    fn a_block_with_more_events_than_fit_drops_the_tail_and_counts_it() {
        let mut plugin = activated_plugin();
        first_lane_with(&plugin, ramp(64), LaneMode::OneShot);
        let capacity = plugin.events.capacity();

        let over = 176;
        let events: Vec<PluginNoteEvent<Chopdrop>> =
            (0..EVENT_CAPACITY + over).map(|n| note_on((n % 512) as u32)).collect();
        let (_status, _channels, host) = render_block(&mut plugin, 512, 0.0, events);

        assert!(host.incoming.is_empty(), "{} event(s) left queued", host.incoming.len());
        assert_eq!(plugin.events.len(), EVENT_CAPACITY, "the buffer took more than it holds");
        assert_eq!(plugin.events.capacity(), capacity, "the event buffer grew");
        assert_eq!(plugin.dropped_events, over as u32);
        assert_eq!(plugin.send_failures, 0, "this host refuses nothing");
    }

    /// The host's reset: everything sounding stops at once, with no fade and
    /// nothing reported — there is no block left to fade into and no one
    /// listening to the one that was. `deactivate()` is the same answer.
    #[test]
    fn reset_and_deactivate_stop_every_voice() {
        let mut plugin = activated_plugin();
        first_lane_with(&plugin, ramp(64), LaneMode::Gate);

        let (_status, channels, _host) = render_block(&mut plugin, 8, 0.0, vec![note_on(0)]);
        assert_eq!(channels[0], vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(plugin.engine.pool().active_count(), 1, "a gate voice is still held");

        plugin.reset();
        assert_eq!(plugin.engine.pool().active_count(), 0);
        let (_status, channels, host) = render_block(&mut plugin, 8, 0.0, Vec::new());
        assert_eq!(channels[0], vec![0.0; 8], "a reset voice does not come back");
        assert!(host.sent.is_empty(), "a reset tells the host nothing");

        let (_status, _channels, _host) = render_block(&mut plugin, 8, 0.0, vec![note_on(0)]);
        assert_eq!(plugin.engine.pool().active_count(), 1);
        plugin.deactivate();
        assert_eq!(plugin.engine.pool().active_count(), 0);
    }

    /// The handle `process()` reads is the model's own published cell, not a
    /// copy of it: a writer's publish reaches the audio thread with nothing
    /// re-wired, and only lanes that have a sample are in it.
    #[test]
    fn the_audio_thread_reads_the_model_the_writers_write() {
        let plugin = activated_plugin();
        assert!(plugin.published.get().lanes().is_empty(), "no lane has a sample yet");
        assert_eq!(
            lock_model(model_of(&plugin)).lanes().len(),
            usize::try_from(DEFAULT_LANE_COUNT).unwrap(),
            "a fresh instance has the pinned default lanes"
        );

        let id = first_lane_with(&plugin, ramp(4), LaneMode::OneShot);
        let published = plugin.published.get();
        assert_eq!(published.lanes().len(), 1, "only a lane with a sample is published");
        assert_eq!(published.lanes()[0].id, id);
    }

    // ---- the `#[persist = "lanes"]` field --------------------------------

    /// The host's save and load path through the field itself: what `map`
    /// hands the framework is the live lanes, and what `set` takes back
    /// becomes them — clamped, published, and the state undo returns to.
    ///
    /// Samples are left out on purpose: restoring one is the loader's work
    /// on its own thread, which `tests/state_round_trip.rs` drives end to
    /// end.
    #[test]
    fn the_persistent_field_saves_and_restores_the_lanes() {
        let params = test_params();
        {
            let mut model = lock_model(params.lanes.model());
            let id = model.lanes()[1].id;
            assert!(model.rename(id, "snare"));
            assert!(model.set_note(id, 42));
            model.set_sample_pad_switch(true);
        }

        let saved = PersistentField::map(&params.lanes, Snapshot::clone);
        assert_eq!(saved.version, CURRENT_VERSION);
        assert_eq!(saved.lanes.len(), usize::try_from(DEFAULT_LANE_COUNT).unwrap());
        assert!(saved.sample_pad_switch, "the switch is model state, not a parameter");

        let restored = test_params();
        PersistentField::set(&restored.lanes, saved.clone());

        {
            let model = lock_model(restored.lanes.model());
            assert_eq!(model.lanes()[1].name, "snare");
            assert_eq!(model.lanes()[1].note, 42);
            assert!(model.sample_pad_switch());
            assert!(!model.can_undo(), "opening a project is not an edit");
        }
        assert_eq!(PersistentField::map(&restored.lanes, Snapshot::clone), saved);
    }

    /// The persistent field reopens the chop editor's source
    /// through the loader on load; a file that is gone is the strip's
    /// message, and the lanes load regardless.
    #[test]
    fn the_persistent_field_reopens_the_chop_source() {
        let gone = std::env::temp_dir()
            .join(format!("chopdrop-lib-{}-gone-source.wav", std::process::id()));
        let source = test_params();
        {
            let mut model = lock_model(source.lanes.model());
            let id = model.lanes()[0].id;
            assert!(model.rename(id, "kick"));
            model.open_source(&gone.to_string_lossy());
        }
        let saved = PersistentField::map(&source.lanes, Snapshot::clone);

        let model: SharedModel = Arc::new(Mutex::new(Model::new()));
        let loader = Arc::new(Loader::new(Arc::clone(&model)));
        let field = LanesField::new(Arc::clone(&model), Arc::clone(&loader));
        PersistentField::set(&field, saved);
        drop(field);
        Arc::into_inner(loader).expect("the only loader handle").finish();

        let model = lock_model(&model);
        let name = gone.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(model.chop().error, Some(format!("source not found: {name}")));
        assert_eq!(model.lanes()[0].name, "kick");
    }

    /// A project loaded with the editor open redraws it at once,
    /// even when no lane has a sample and so no loader result will ever ring
    /// — one load, one ring, and nothing to ring with no hook installed.
    #[test]
    fn loading_a_project_rings_the_repaint_hook_once() {
        use std::sync::atomic::{AtomicU32, Ordering};

        let source = test_params();
        {
            let mut model = lock_model(source.lanes.model());
            let id = model.lanes()[0].id;
            assert!(model.rename(id, "kick"));
        }
        let saved = PersistentField::map(&source.lanes, Snapshot::clone);
        assert!(saved.lanes.iter().all(|lane| lane.sample.is_none()), "no samples here");

        // No hook: the load simply lands.
        let headless = test_params();
        PersistentField::set(&headless.lanes, saved.clone());
        assert_eq!(lock_model(headless.lanes.model()).lanes()[0].name, "kick");

        let restored = test_params();
        let rings = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&rings);
        lock_model(restored.lanes.model()).set_repaint_hook(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));

        PersistentField::set(&restored.lanes, saved);

        assert_eq!(rings.load(Ordering::Relaxed), 1, "one load, one ring");
        assert_eq!(lock_model(restored.lanes.model()).lanes()[0].name, "kick");
    }

    /// And through the framework's own door: `Params::serialize_fields` is
    /// what nice-plug writes into a project, keyed by the name in
    /// `#[persist = "lanes"]`, and `deserialize_fields` is what it hands
    /// back. Renaming that key would silently lose every saved project's
    /// lanes, so it is pinned here.
    #[test]
    fn the_host_saves_the_lanes_under_the_persist_key() {
        let params = test_params();
        {
            let mut model = lock_model(params.lanes.model());
            let id = model.lanes()[0].id;
            assert!(model.rename(id, "kick"));
        }

        let fields = params.serialize_fields();
        assert_eq!(fields.keys().collect::<Vec<&String>>(), vec!["lanes"]);
        assert!(fields["lanes"].contains("kick"), "{}", fields["lanes"]);

        let restored = test_params();
        restored.deserialize_fields(&fields);
        assert_eq!(lock_model(restored.lanes.model()).lanes()[0].name, "kick");
    }

    /// The CLAP id is what a host keys a saved project's device on, so it has
    /// to be set, reverse-domain and chopdrop's own.
    #[test]
    fn clap_ids_and_urls_are_chopdrops_own() {
        assert_eq!(Chopdrop::CLAP_ID, "com.pywkt.chopdrop");
        assert_eq!(Chopdrop::CLAP_DESCRIPTION, Some("CLAP sample-player plugin"));
        assert_eq!(Chopdrop::CLAP_MANUAL_URL, Some("https://github.com/pywkt/chopdrop"));
        assert!(Chopdrop::CLAP_SUPPORT_URL.is_some_and(|url| url.starts_with(Chopdrop::URL)));
        assert_eq!(Chopdrop::VERSION, env!("CARGO_PKG_VERSION"));
    }

    /// The host asking for a GUI gets one, over the window state this
    /// instance already holds.
    ///
    /// Display-free: `Plugin::editor()` only assembles the editor, and the
    /// window is not created until the host calls `Editor::spawn()` (what the
    /// window then looks like can only be checked by eye, in a host — which
    /// is why 1.6 is `Verify: manual`). The executor is a pair of no-op
    /// closures because `BackgroundTask = ()` — nothing is ever posted to it.
    #[test]
    fn the_host_gets_an_editor_over_this_instances_window_state() {
        use nice_plug::editor::Editor;

        let mut plugin = activated_plugin();
        let executor = AsyncExecutor::new(Arc::new(|()| {}), Arc::new(|()| {}));
        let editor = plugin.editor(executor).expect("an editor is always available");

        assert!(editor.resize_hint().can_resize, "the window is resizable");

        // The editor holds the instance's own window state, not a fresh one:
        // the size the user dragged the window to survives the host closing
        // and reopening it. It cannot be read back out of `ChopdropEditor`,
        // whose fields are private to `editor`, so the handle count is what
        // says so.
        assert_eq!(Arc::strong_count(&plugin.editor_state), 2);
        drop(editor);
        assert_eq!(Arc::strong_count(&plugin.editor_state), 1);
    }

    /// A fresh instance shares one params struct with the host, and that is
    /// the same one `process()` reads the master gain from.
    #[test]
    fn a_fresh_instance_has_one_params_struct() {
        let plugin = activated_plugin();
        assert_eq!(plugin.params.next_master_gain(64), 1.0);

        // `params()` hands the host a clone of the same `Arc`, not a copy of
        // the values: a host automating the volume moves what `process()`
        // reads.
        let handed_out = plugin.params();
        assert_eq!(Arc::strong_count(&plugin.params), 2);
        drop(handed_out);
        assert_eq!(Arc::strong_count(&plugin.params), 1);
    }
}
