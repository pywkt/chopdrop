//! The concurrency stress test: every thread that writes to the model
//! writing at once, while the audio thread renders block after block against
//! what they publish.
//!
//! Three threads, which are exactly the three a loaded instance has:
//!
//! - **this one, as the audio thread**: [`Engine::run_block`] in a loop, one
//!   `SharedCell::get` a block, never reaching for the writer mutex;
//! - **a GUI thread**: the gestures a user makes — samples dropped onto
//!   lanes and cleared off them, lanes added, deleted, duplicated,
//!   reordered, renoted, rechoked, remoded, volume and pan dragged, undo,
//!   redo, and a whole project loaded over the top — each through the writer
//!   mutex, each followed by the editor's per-frame collect;
//! - **the loader's worker**, writing its own decode results in through that
//!   same mutex. It is the writer whose timing nothing in the GUI
//!   controls, which is the point of having it here: its `set_sample` lands
//!   between a gesture and the block that renders it.
//!
//! What this looks for — no panic, no use-after-free, and a collector that
//! drains — reads in this file as:
//!
//! - *no panic* — every thread is joined and its panic re-raised here, and
//!   the model is then checked for the invariants no interleaving may break:
//!   lane ids unique, every field inside its clamp, and a published list
//!   that still describes the lanes the model has;
//! - *no use-after-free* — the audio loop keeps the snapshot it got at the
//!   top of a block and reads every lane's atomics and audio again at the
//!   bottom, after a block's worth of writers have had their way with the
//!   list. basedrop is what makes that defined, and the loop runs inside
//!   `nice_assert_no_alloc::assert_no_alloc`, so the snapshot whose last
//!   reference the audio thread drops has to be *queued* for the collector
//!   rather than freed here. (`tests/no_alloc.rs`'s control case is what
//!   proves that guard bites at all; an abort out of this test — reported
//!   as a failed allocation rather than as a failed assertion — means the
//!   audio thread allocated or freed something.)
//! - *the collector drains* — once the writers are done and the engine has
//!   let go of its voices, loading an empty project and collecting must
//!   bring the instance back to one live allocation: the snapshot it is
//!   publishing. Every sample, every file's bytes and every retired lane
//!   list the run made is then accounted for.
//!
//! Deterministic by construction rather than by timing: fixed seeds, fixed
//! counts, no sleep anywhere, and no assertion that depends on who won a
//! race. The two sides wait for each other instead. Each publishes a
//! monotonic counter of its own progress — gestures made, blocks
//! rendered — and the audio thread counts a block as *contended* when the
//! GUI's counter moved while it rendered. The GUI keeps cycling its script
//! until there have been [`MIN_CONTENDED`] of those, and the audio thread
//! keeps rendering until the GUI says it has finished. Both conditions are
//! met by either side making progress, so neither can wait on the other for
//! ever, and "the lanes really were rewritten under a running block" is
//! something the test establishes rather than hopes for.
//!
//! A second run does the same for Stretch renders: the real render worker
//! and a GUI thread landing and requesting renders while the audio thread
//! plays voices on the renders they replace and moves the tempo under them.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use nice_assert_no_alloc::assert_no_alloc;

use chopdrop::engine::{AuditionRing, Engine, EngineEvent, NoteTarget, Outputs, TimedEvent};
use chopdrop::loader::{Loader, Source};
use chopdrop::render::RenderWorker;
use chopdrop::time::TempoCell;
use chopdrop::model::{
    CHOKE_GROUPS, LANE_PAN_MAX, LANE_PAN_MIN, LANE_VOLUME_DB_MAX, LANE_VOLUME_DB_MIN, LaneMode,
    lock_model, MAX_LANES, MAX_NAME_LEN, Model, PublishedCell, PublishedLanes, SampleRef,
    SharedModel,
};
use chopdrop::sample::SampleData;
use chopdrop::state::PendingSample;

mod support;

use support::wav_i16;

/// The host rate every block renders at.
const RATE: f64 = 48_000.0;

/// One block, the size Bitwig's default buffer is.
const BLOCK_FRAMES: usize = 512;

/// The master gain the blocks render at: not 1.0, so the master scaling is
/// in the numbers the run looks at too.
const MASTER_VOLUME: f32 = 0.8;

/// Blocks the audio loop renders before the writers are let go, with the kit
/// [`starting_kit`] built and nothing touching it.
///
/// Why there is a solo stretch at all: it is the only part of the run whose
/// output is a function of the fixtures alone, so the "this really played
/// audio" assertions — voices started, voices ended, samples mixed — can be
/// made without depending on any interleaving. Four blocks rather than one,
/// so a one-shot shorter than a block has ended and been reported by the end
/// of it.
const SOLO_BLOCKS: usize = 4;

/// The fewest blocks the contended loop renders, for the case of a GUI
/// thread that gets through its whole script while this one is descheduled:
/// the writers are then finished before the first block, and without a floor
/// the loop would end having rendered one.
const MIN_BLOCKS: usize = 64;

/// The fewest blocks that must have been rendered *while the GUI thread was
/// writing* before the GUI is allowed to stop — a block during which the
/// GUI's gesture counter moved.
///
/// This is what makes the overlap a fact rather than a likelihood, and it is
/// counted from the two threads' own progress counters rather than from
/// snapshot addresses, so nothing about it depends on the allocator.
const MIN_CONTENDED: usize = 256;

/// The gestures the GUI thread makes at least — it keeps cycling the script
/// past this while [`MIN_CONTENDED`] is unmet. Enough that every kind of
/// gesture happens many times over, and small enough that the suite stays
/// quick (under two minutes for all of it).
const GESTURES: usize = 600;

/// How many kinds of gesture [`gesture`] cycles through.
const GESTURE_KINDS: usize = 10;

/// Events in one block: note-ons, note-offs, host chokes and all three
/// expressions, the mix `tests/no_alloc.rs`'s busy block uses, at a size that
/// leaves the writers to be what the run is slow with.
const EVENTS: usize = 64;

/// The lowest note the lanes and the events share — 36 is C1 in Bitwig's
/// convention.
const FIRST_KEY: u8 = 36;

/// How many keys the events spread over, and the window every lane note a
/// gesture sets stays inside, so the blocks keep hitting lanes however often
/// the list is rewritten.
const KEYS: u8 = 8;

/// The interleaved samples the generated WAV files hold: a short mono ramp,
/// as `loader.rs`'s own tests use.
const RAMP: [i16; 6] = [0, 4096, 8192, 12288, 16384, 20480];

/// The rate those WAVs declare — deliberately not the host's, so every
/// sample the loader lands plays through the interpolator at a fractional
/// step.
const FILE_RATE: u32 = 32_000;

/// The GUI thread's seed. Fixed, so a failure is reproducible to the
/// gesture.
const SEED: u64 = 0x5BE1_57A1_1C3D_9F01;

/// The block event list's seed, likewise.
const EVENT_SEED: u64 = 0x1234_5678_9ABC_DEF0;

/// A deterministic xorshift64*, as `tests/no_alloc.rs` uses: no `rand`
/// dependency for a few bits of variety.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// `count` frames of non-silent, non-constant audio in `channels` channels,
/// interleaved as [`SampleData::frames`] wants it.
fn tone(count: usize, channels: u8) -> Box<[f32]> {
    let mut frames = Vec::with_capacity(count * channels as usize);
    for n in 0..count {
        let phase = (n % 32) as f32 / 16.0;
        let value = if phase < 1.0 { phase } else { 2.0 - phase };
        for c in 0..channels {
            frames.push(if c == 0 { value } else { -value });
        }
    }
    frames.into()
}

/// A file in the temp directory holding these bytes, removed when the guard
/// drops so a failing assertion leaves no litter in `/tmp` (`loader.rs`'s
/// tests do it the same way).
struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    fn new(name: &str, bytes: &[u8]) -> Self {
        let path =
            std::env::temp_dir().join(format!("chopdrop-stress-{}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("temp dir is writable");
        Self { path }
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Raises the flag the audio loop stops on, whatever happens to the thread
/// that holds it.
///
/// On `Drop` and not at the end of [`gui_thread`], because a writer that
/// panics must also stop the render loop: the loop would otherwise spin
/// until the harness timed the suite out, and a hang says far less about
/// what broke than the panic this way re-raises in `join`.
struct DoneOnDrop(Arc<AtomicBool>);

impl Drop for DoneOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// One sample as the GUI hands it over: decoded audio, and the file bytes a
/// save would embed, both allocated against this instance's collector
/// handle.
///
/// Mono and stereo, the host's rate and another, and lengths from well under
/// a block to several blocks long, so the voices a gesture starts both end
/// inside a block and survive the writers that follow.
fn gui_sample(model: &Model, roll: u64) -> SampleRef {
    let channels = if roll.is_multiple_of(3) { 2 } else { 1 };
    let rate = if roll.is_multiple_of(4) { 44_100 } else { RATE as u32 };
    let count = 64 + (roll % 2048) as usize;
    let data = SampleData { rate, channels, frames: tone(count, channels) };
    let bytes = wav_i16(u16::from(channels), rate, &RAMP);
    SampleRef::new(model.handle(), Path::new("/tmp/chopdrop-stress-gui.wav"), bytes, data)
}

/// The kit the solo blocks render: every default lane with a sample, both
/// modes, choke groups fighting each other, notes across the event window.
///
/// Built through the writer API, so the blocks render against a snapshot the
/// GUI and the loader really produce.
fn starting_kit(model: &mut Model) {
    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    for (n, id) in ids.into_iter().enumerate() {
        let sample = gui_sample(model, n as u64);
        assert!(model.set_sample(id, sample), "lane {n} takes a sample");
        assert!(model.set_note(id, FIRST_KEY + n as u8 % KEYS), "lane {n} takes a note");
        assert!(model.set_choke(id, n as u8 % 5), "lane {n} takes a choke group");
        let mode = if n % 2 == 0 { LaneMode::OneShot } else { LaneMode::Gate };
        assert!(model.set_mode(id, mode), "lane {n} takes a mode");
    }
    // The kit is the state the run starts from, not an edit of it.
    model.rebase();
}

/// One block's worth of events, in offset order, mixing every kind the
/// engine answers and matching voices both by `voice_id` and by
/// `(channel, key)`.
fn block_events() -> Vec<TimedEvent> {
    let mut events = Vec::with_capacity(EVENTS);
    let mut state = EVENT_SEED;

    for n in 0..EVENTS {
        let roll = next(&mut state);
        let timing = (n * BLOCK_FRAMES / EVENTS) as u32;
        let note = NoteTarget {
            channel: (roll % 2) as u8,
            key: FIRST_KEY + (roll >> 8) as u8 % KEYS,
            voice_id: if n % 2 == 0 { Some((roll >> 16) as i32 % 16) } else { None },
        };
        let event = match n % 8 {
            0..=2 => EngineEvent::NoteOn { note, velocity: (roll >> 24) as u8 as f32 / 255.0 },
            3 => EngineEvent::NoteOff { note },
            4 => EngineEvent::Choke { note },
            5 => EngineEvent::PolyTuning { note, semis: (roll >> 32) as i8 as f32 / 8.0 },
            6 => EngineEvent::PolyPan { note, pan: (roll >> 40) as i8 as f32 / 127.0 },
            _ => EngineEvent::PolyVolume { note, gain: (roll >> 48) as u8 as f32 / 128.0 },
        };
        events.push(TimedEvent::new(timing, event));
    }
    events
}

/// What the GUI thread did, so the audio thread can assert afterwards that
/// the run was as busy as it claims.
#[derive(Debug, Default)]
struct Counts {
    /// Gestures made: [`GESTURES`] or more.
    gestures: usize,
    /// Samples the GUI itself put on a lane or took off one.
    samples: usize,
    /// Structural changes to the list: lanes added, deleted, duplicated or
    /// reordered.
    structural: usize,
    /// Decode jobs queued with the loader — dropped files and restored
    /// projects both.
    jobs: usize,
    /// Project loads: the whole lane list replaced under the audio thread.
    loads: usize,
    /// Undo and redo steps taken.
    steps: usize,
}

/// One gesture, through the writer mutex the way the editor's frame callback
/// takes it: lock, change, collect, unlock.
///
/// `n` chooses the kind and `roll` the details, so the script is a fixed
/// sequence that still touches different lanes, values and samples each time
/// around. Nothing here asserts that a gesture *took*: a lane the loader has
/// since replaced, or one a project load has renumbered, is exactly the race
/// this test exists for, and a writer that lands on nothing is the model
/// doing its job (lanes are named by id, and a gesture naming an id that is
/// gone does nothing).
fn gesture(
    n: usize,
    roll: u64,
    model: &SharedModel,
    loader: &Loader,
    files: &[ScratchFile],
    auditions: &AuditionRing,
    counts: &mut Counts,
) {
    let mut model = lock_model(model);
    // A project load can leave no lanes at all (a snapshot of an emptied
    // list), and every gesture below names one.
    if model.lanes().is_empty() {
        model.add_lane();
        counts.structural += 1;
    }
    let index = roll as usize % model.lanes().len();
    let id = model.lanes()[index].id;

    match n % GESTURE_KINDS {
        0 | 1 => {
            // A sample arrives on a lane: the republish this test is named
            // for, and the audio thread may be rendering the lane it
            // replaces as it lands.
            let sample = gui_sample(&model, roll);
            if model.set_sample(id, sample) {
                counts.samples += 1;
            }
            model.commit();
        }
        2 => {
            if model.clear_sample(id) {
                counts.samples += 1;
            }
            model.commit();
        }
        3 => {
            // Grow, and keep the new lane inside the event window so it is a
            // lane the blocks actually play.
            if let Some(new) = model.add_lane() {
                let sample = gui_sample(&model, roll >> 8);
                model.set_sample(new, sample);
                model.set_note(new, FIRST_KEY + (roll >> 16) as u8 % KEYS);
                counts.structural += 1;
            }
            model.commit();
        }
        4 => {
            // Shrink — deleting a lane whose voices may be sounding, which
            // is the use-after-free case: a sounding voice holds its audio
            // through the snapshot, not through the model.
            if model.lanes().len() > 2 && model.delete_lane(id) {
                counts.structural += 1;
            }
            model.commit();
        }
        5 => {
            if model.duplicate_lane(id).is_some() {
                counts.structural += 1;
            }
            model.commit();
        }
        6 => {
            let to = (roll >> 24) as usize % model.lanes().len();
            if model.reorder(index, to) {
                counts.structural += 1;
            }
            model.commit();
        }
        7 => {
            // The structural per-lane settings, and then the three values
            // that reach the audio thread through the atomics of the
            // snapshot it is already holding — a knob drag, mid-block.
            model.set_note(id, FIRST_KEY + (roll >> 32) as u8 % KEYS);
            model.set_choke(id, (roll >> 40) as u8 % (CHOKE_GROUPS + 1));
            let mode = if roll.is_multiple_of(2) { LaneMode::OneShot } else { LaneMode::Gate };
            model.set_mode(id, mode);
            model.set_volume_db(id, LANE_VOLUME_DB_MIN + (roll >> 48) as u8 as f32 / 4.0);
            model.set_pan(id, LANE_PAN_MIN + (roll >> 56) as u8 as f32 / 128.0);
            model.set_sample_pad_switch(roll.is_multiple_of(4));
            model.commit();
        }
        8 => {
            // A drop, and a restore: the loader's worker is the writer for
            // both, and lands its result whenever it lands it. One of the
            // files is not audio at all, so some of these finish as a lane
            // message instead of a sample.
            let file = &files[roll as usize % files.len()];
            if roll.is_multiple_of(2) {
                if loader.queue(&mut model, id, Source::File(file.path.clone())) {
                    counts.jobs += 1;
                }
            } else {
                let pending = PendingSample {
                    path: file.path.to_string_lossy().into_owned(),
                    file_name: String::from("restored.wav"),
                    bytes: Some(wav_i16(1, FILE_RATE, &RAMP)),
                    origin: None,
                };
                if loader.queue(&mut model, id, Source::Restore(pending)) {
                    counts.jobs += 1;
                }
            }
        }
        _ => {
            // A project load: the whole lane list swapped out from under the
            // block being rendered, with fresh ids, and its samples queued
            // for the loader to bring back. Not an edit, so the result is
            // rebased rather than committed.
            let snapshot = model.snapshot();
            for (lane, pending) in model.load_snapshot(snapshot) {
                if loader.queue(&mut model, lane, Source::Restore(pending)) {
                    counts.jobs += 1;
                }
            }
            model.rebase();
            counts.loads += 1;
        }
    }

    // Undo and redo, on strides of their own so they fall across every kind
    // of gesture above rather than always after the same one.
    if n.is_multiple_of(7) && model.undo() {
        counts.steps += 1;
    }
    if n.is_multiple_of(11) && model.redo() {
        counts.steps += 1;
    }

    // The editor's per-frame collect, which is what finally frees a sample
    // the audio thread was still playing when a writer replaced it.
    model.collect();
    drop(model);

    // And a play button, pushed from the GUI thread as the editor pushes it.
    // Best-effort by contract: a full ring drops it, and that is not a
    // failure.
    auditions.push(id);
}

/// The GUI thread: the gesture script, then the loader's shutdown.
///
/// It keeps cycling the script while fewer than [`MIN_CONTENDED`] blocks have
/// been rendered against a list it was in the middle of rewriting, so the
/// test cannot pass having overlapped the two sides by accident of
/// scheduling. [`Loader`] is built and finished here because `finish` waits
/// for the worker's queued jobs to land and must not be called with the
/// writer lock held — which means that by the time this returns, every
/// decode the run started has been written into the model.
fn gui_thread(
    model: SharedModel,
    auditions: Arc<AuditionRing>,
    start: Arc<Barrier>,
    progress: Arc<AtomicUsize>,
    contended: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
) -> Counts {
    // Declared first, so it is dropped last: the audio loop must keep
    // rendering until the loader's last result has landed.
    let _done = DoneOnDrop(done);
    let loader = Loader::new(Arc::clone(&model));
    let files = [
        ScratchFile::new("mono.wav", &wav_i16(1, FILE_RATE, &RAMP)),
        ScratchFile::new("stereo.wav", &wav_i16(2, FILE_RATE, &RAMP)),
        ScratchFile::new("unreadable.wav", b"not a wav at all"),
    ];
    let mut counts = Counts::default();
    let mut state = SEED;

    start.wait();
    let mut n = 0;
    while n < GESTURES || contended.load(Ordering::Acquire) < MIN_CONTENDED {
        let roll = next(&mut state);
        gesture(n, roll, &model, &loader, &files, &auditions, &mut counts);
        n += 1;
        progress.store(n, Ordering::Release);
    }
    counts.gestures = n;

    loader.finish();
    counts
}

/// What the audio loop counted, so every assertion about it can be made
/// after the no-alloc guard has come down: a failing assertion allocates,
/// and inside the guard that would abort the process instead of failing the
/// test.
#[derive(Debug, Default)]
struct Stats {
    /// Blocks rendered.
    blocks: usize,
    /// Blocks rendered while the GUI thread's gesture counter was moving.
    contended: usize,
    /// How many times the published list changed under the loop. A lower
    /// bound: it compares the snapshot's address with the last one's, and a
    /// retired snapshot's memory can be handed straight back out for its
    /// replacement. [`Stats::contended`] is the number the handshake uses;
    /// this one is here because an address that changes is direct evidence
    /// that a block held a list a writer had already retired.
    swaps: usize,
    /// The address of the last one.
    last_list: usize,
    /// The loudest sample the blocks produced.
    peak: f32,
    /// The fullest the voice pool got.
    max_active: usize,
    /// Voices reported ended to the host.
    terminated: usize,
    /// The most playable lanes one snapshot held.
    lanes_seen: usize,
    /// The checksum of the use-after-free probe.
    probe: f32,
}

/// What one block does on the audio thread, start to finish: the snapshot,
/// the render, the host's outbox — and then a second read of that same
/// snapshot, which is the use-after-free probe.
///
/// Every line of this is on the real-time path and allocates nothing: the
/// `get` is a refcount bump, the accounting is arithmetic on the stack, and
/// the drop of the handle at the end *queues* a retired snapshot for the
/// collector rather than freeing it.
fn audio_block(
    engine: &mut Engine,
    published: &PublishedCell,
    events: &[TimedEvent],
    out_l: &mut [f32],
    out_r: &mut [f32],
    progress: &AtomicUsize,
    stats: &mut Stats,
) {
    let held = published.get();
    let list = (&*held as *const PublishedLanes) as usize;
    if list != stats.last_list {
        stats.last_list = list;
        stats.swaps += 1;
    }
    let before = progress.load(Ordering::Acquire);

    engine.run_block(published, events, &mut Outputs::main(out_l, out_r), RATE, MASTER_VOLUME);

    for sample in out_l.iter().chain(out_r.iter()) {
        stats.peak = stats.peak.max(sample.abs());
    }
    stats.max_active = stats.max_active.max(engine.pool().active_count());
    stats.terminated += engine.pool().terminated().len();
    engine.clear_terminated();

    // The probe: the list this block started with, read again now that a
    // block's worth of writers has been and gone. Any of these lanes may
    // have been deleted, replaced or renumbered in the meantime, and every
    // one of them must still be here to read — that is what basedrop's
    // deferred collection buys, and what a collect on the wrong thread would
    // take away.
    for lane in held.lanes() {
        stats.probe += lane.volume_db() + lane.pan() + lane.sample.frames[0];
    }
    stats.lanes_seen = stats.lanes_seen.max(held.lanes().len());

    if progress.load(Ordering::Acquire) != before {
        stats.contended += 1;
    }
    stats.blocks += 1;
}

/// The headline test.
///
/// Reads top to bottom as the life of an instance: build it, play it alone
/// for a few blocks, then let the GUI and the loader rewrite it under the
/// render loop until both sides have seen enough of each other, join them,
/// check what is left, and hand the memory back.
#[test]
fn the_gui_and_the_loader_rewrite_the_lanes_while_the_engine_renders() {
    let model: SharedModel = Arc::new(Mutex::new(Model::new()));
    let published = Arc::clone(lock_model(&model).published());
    starting_kit(&mut lock_model(&model));

    let events = block_events();
    let mut engine = Engine::new();
    let auditions = Arc::clone(engine.auditions());
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];

    // One block before any of the counting, as `tests/no_alloc.rs` does it, so
    // that a one-time lazy init inside *std* cannot be reported as the audio
    // path's allocation. The engine itself holds no lazily-built state.
    engine.run_block(
        &published,
        &events,
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        MASTER_VOLUME,
    );
    engine.clear_terminated();
    engine.reset();

    let progress = Arc::new(AtomicUsize::new(0));
    let contended = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(2));
    let writer = {
        let (model, auditions, start) =
            (Arc::clone(&model), Arc::clone(&auditions), Arc::clone(&start));
        let (progress, contended, done) =
            (Arc::clone(&progress), Arc::clone(&contended), Arc::clone(&done));
        std::thread::Builder::new()
            .name(String::from("chopdrop-stress-gui"))
            .spawn(move || gui_thread(model, auditions, start, progress, contended, done))
            .expect("the GUI thread spawns")
    };

    // The solo stretch: the writers are parked on the barrier, so these
    // blocks are a function of `starting_kit` alone.
    let mut solo = Stats::default();
    assert_no_alloc(|| {
        for _ in 0..SOLO_BLOCKS {
            audio_block(
                &mut engine,
                &published,
                &events,
                &mut left,
                &mut right,
                &progress,
                &mut solo,
            );
        }
    });
    assert_eq!(solo.blocks, SOLO_BLOCKS);
    assert!(solo.peak > 0.0, "the kit rendered audible audio, peak {}", solo.peak);
    assert!(solo.max_active > 0, "the events started voices");
    assert!(solo.terminated > 0, "voices ended and were reported to the host");
    assert_eq!(solo.swaps, 1, "nothing was writing, so there was one lane list");
    assert_eq!(solo.contended, 0, "and no gesture had been made yet");

    // And the contended stretch: from here until the GUI says it is done,
    // every block renders against whatever the two writer threads have most
    // recently published.
    start.wait();
    let mut stats = Stats::default();
    assert_no_alloc(|| {
        while !done.load(Ordering::Acquire) || stats.blocks < MIN_BLOCKS {
            audio_block(
                &mut engine,
                &published,
                &events,
                &mut left,
                &mut right,
                &progress,
                &mut stats,
            );
            contended.store(stats.contended, Ordering::Release);
        }
    });

    let counts = writer.join().expect("the GUI thread finished without panicking");

    // The run really was contended, and really was busy. The first of these
    // is guaranteed by the handshake rather than hoped for; the rest follow
    // from the script the GUI thread completed.
    assert!(
        stats.contended >= MIN_CONTENDED,
        "{} of {} blocks rendered while the GUI was writing, wanted {MIN_CONTENDED}",
        stats.contended,
        stats.blocks
    );
    assert!(stats.blocks >= MIN_BLOCKS, "blocks rendered: {}", stats.blocks);
    assert!(stats.swaps > 1, "the published list changed under the loop: {}", stats.swaps);
    assert!(stats.peak > 0.0, "the contended blocks rendered audio too, peak {}", stats.peak);
    assert!(stats.probe.is_finite(), "the probe read live memory, got {}", stats.probe);
    assert!(stats.lanes_seen > 0, "some snapshot held a playable lane");
    assert!(stats.max_active > 0, "voices played while the lanes were being rewritten");

    assert!(counts.gestures >= GESTURES, "gestures made: {}", counts.gestures);
    assert!(counts.samples > 0, "the GUI swapped samples onto lanes: {}", counts.samples);
    assert!(counts.structural > 0, "the GUI rewrote the lane list: {}", counts.structural);
    assert!(counts.jobs > 0, "the loader decoded while the engine played: {}", counts.jobs);
    assert!(counts.loads > 0, "whole projects were loaded under the blocks: {}", counts.loads);
    assert!(counts.steps > 0, "undo and redo ran as well: {}", counts.steps);

    // Nothing in all of that may have broken the model: ids unique, every
    // field inside its clamp, and a published list that still describes the
    // lanes it was built from.
    let invariants = lock_model(&model);
    let lanes = invariants.lanes();
    assert!(lanes.len() <= MAX_LANES as usize, "{} lanes", lanes.len());
    for (index, lane) in lanes.iter().enumerate() {
        assert_eq!(
            lanes.iter().filter(|other| other.id == lane.id).count(),
            1,
            "lane {index} shares its id {} with another",
            lane.id
        );
        assert!(lane.name.chars().count() <= MAX_NAME_LEN, "lane {index} name: {}", lane.name);
        assert!((LANE_VOLUME_DB_MIN..=LANE_VOLUME_DB_MAX).contains(&lane.volume_db));
        assert!((LANE_PAN_MIN..=LANE_PAN_MAX).contains(&lane.pan));
        assert!(lane.choke <= CHOKE_GROUPS, "lane {index} choke: {}", lane.choke);
        assert!(lane.note <= 127, "lane {index} note: {}", lane.note);
    }

    let playable: Vec<_> = lanes.iter().filter(|lane| lane.sample.is_some()).collect();
    let listed = invariants.published().get();
    assert_eq!(
        listed.lanes().len(),
        playable.len(),
        "the audio thread is given the lanes that have samples, in display order"
    );
    for (entry, lane) in listed.lanes().iter().zip(&playable) {
        assert_eq!(entry.id, lane.id);
        assert_eq!(entry.note, lane.note);
        assert_eq!(entry.choke, lane.choke);
        assert_eq!(entry.mode, lane.mode);
        assert_eq!(entry.volume_db(), lane.volume_db);
        assert_eq!(entry.pan(), lane.pan);
    }
    assert_eq!(listed.sample_pad_switch(), invariants.sample_pad_switch());
    drop(listed);
    drop(invariants);

    // And the collector drains. The writers are joined and the loader's
    // worker with them, so once the engine has let go of the audio its
    // voices were playing, the model holds the last handle on everything the
    // run made: loading an empty project retires the lot — lanes, samples
    // and history — and one collect must leave exactly the snapshot the
    // instance is publishing.
    drop(engine);
    let mut model = lock_model(&model);
    model.replace_lanes(Vec::new(), false);
    model.collect();
    assert_eq!(
        model.alloc_count(),
        1,
        "only the live empty snapshot is left — every sample, every file's bytes and every \
         retired lane list of the run has been handed back"
    );
}

/// Lanes in the Stretch run's kit.
const STRETCH_LANES: usize = 6;

/// The Stretch run's writer gestures at least; like [`GESTURES`] it keeps
/// cycling while [`STRETCH_CONTENDED`] is unmet.
const STRETCH_GESTURES: usize = 300;

/// The fewest Stretch-run blocks rendered while the writer was writing.
const STRETCH_CONTENDED: usize = 128;

/// How many kinds of gesture [`stretch_gesture`] cycles through.
const STRETCH_KINDS: usize = 6;

/// The render worker's debounce in the Stretch run: short, so the real
/// worker renders while the blocks run rather than only at the end.
const STRETCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(2);

/// The host tempos the Stretch run's audio thread cycles through, one per
/// block, stored inside the guard as `process()` stores them: the speed
/// correction re-times sounding render voices, and the worker sees the
/// drift and re-renders.
const STRETCH_TEMPOS: [f64; 5] = [120.0, 90.0, 150.0, 75.0, 175.0];

/// The writer seed of the Stretch run.
const STRETCH_SEED: u64 = 0x0D1E_5EED_A11C_E517;

/// Land a render for a Stretch lane's newest request, as the render
/// worker's apply step does — but from the writer thread and at once, so
/// renders replace each other under the blocks far more often than the real
/// worker's debounce allows. `factor` scales the lane's note length at
/// 120 BPM. `false` for a lane that is not in Stretch or has no chop.
fn land_render(model: &mut Model, id: u32, factor: f64) -> bool {
    use chopdrop::time::{self, Length};

    if !model.request_render(id) {
        return false;
    }
    let Some(lane) = model.lane(id) else { return false };
    let Some(chop) = lane.sample.as_ref().map(|sample| &sample.data) else { return false };
    let secs = match lane.length {
        Length::Beats(length) => time::target_secs(length, lane.fine_pct, 120.0) * factor,
        Length::AsCut => chop.frame_count() as f64 / f64::from(chop.rate.max(1)),
    };
    let count = ((secs * f64::from(chop.rate)) as usize).max(1);
    let (generation, rate, channels) = (lane.render_generation, chop.rate, chop.channels);
    let data = SampleData { rate, channels, frames: tone(count, channels) };
    model.set_render(id, generation, data)
}

/// The Stretch run's kit: lane `n` on key `FIRST_KEY + n`, all in Stretch on
/// short note lengths (so voices end and are stolen within the run),
/// stacking their retriggers, mono and stereo chops at two rates, and a
/// first render landed on every lane, so the solo blocks play renders
/// whatever the worker does.
fn stretch_kit(model: &mut Model) {
    use chopdrop::time::{Length, NoteLength, TimeMode};

    while model.lanes().len() < STRETCH_LANES {
        model.add_lane().expect("6 lanes is well inside MAX_LANES");
    }
    while model.lanes().len() > STRETCH_LANES {
        let id = model.lanes()[STRETCH_LANES].id;
        assert!(model.delete_lane(id));
    }
    let lengths = [NoteLength::SixteenthTriplet, NoteLength::Sixteenth, NoteLength::EighthTriplet];
    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    for (n, id) in ids.into_iter().enumerate() {
        let channels = if n % 2 == 0 { 2 } else { 1 };
        let rate = if n % 3 == 1 { 44_100 } else { RATE as u32 };
        let data = SampleData { rate, channels, frames: tone(3_000 + n * 400, channels) };
        let bytes = wav_i16(u16::from(channels), rate, &RAMP);
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/chop.wav"), bytes, data);
        assert!(model.set_sample(id, sample), "lane {n} takes a chop");
        assert!(model.set_note(id, FIRST_KEY + n as u8), "lane {n} takes a note");
        assert!(model.set_choke(id, 0), "lane {n} is in no choke group");
        assert!(model.set_cut_itself(id, false), "lane {n} stacks its retriggers");
        assert!(model.set_time_mode(id, TimeMode::Stretch), "lane {n} goes Stretch");
        assert!(model.set_length(id, Length::Beats(lengths[n % 3])), "lane {n} takes a length");
        assert!(land_render(model, id, 0.6 + n as f64 * 0.2), "lane {n} has a first render");
    }
    model.rebase();
}

/// Every lane hit twice a block, each a few frames apart: the kit's lanes do
/// not cut themselves, so retriggers stack and a note started on an old
/// render sounds alongside one started on its replacement.
fn stretch_events() -> Vec<TimedEvent> {
    let mut events = Vec::new();
    for hit in [0, BLOCK_FRAMES / 2] {
        for n in 0..STRETCH_LANES {
            let note = NoteTarget { channel: 0, key: FIRST_KEY + n as u8, voice_id: None };
            let on = (hit + n * 5) as u32;
            events.push(TimedEvent::new(on, EngineEvent::NoteOn { note, velocity: 0.8 }));
        }
    }
    events.sort_by_key(|event| event.timing);
    events
}

/// What the Stretch run's writer did.
#[derive(Debug, Default)]
struct StretchCounts {
    /// Gestures made.
    gestures: usize,
    /// Renders the writer itself landed.
    landed: usize,
    /// Edits that asked for a render: length, fine, pitch, retry, back into
    /// Stretch.
    requests: usize,
    /// Times a lane left Stretch, dropping a render voices may still hold.
    exits: usize,
    /// Renders the real worker ran over its life.
    worker_renders: usize,
}

/// One Stretch-run gesture under the writer mutex: a render landing, or an
/// edit that requests one, then the editor's collect — and a wake-up for
/// the worker, made with the lock released as the editor makes it.
fn stretch_gesture(
    n: usize,
    roll: u64,
    model: &SharedModel,
    worker: &RenderWorker,
    counts: &mut StretchCounts,
) {
    use chopdrop::time::{Length, NoteLength, TimeMode};

    let mut model = lock_model(model);
    let index = roll as usize % model.lanes().len();
    let id = model.lanes()[index].id;
    let stretch = model.lane(id).is_some_and(|lane| lane.time_mode == TimeMode::Stretch);
    match n % STRETCH_KINDS {
        0 | 1 => {
            let factor = 0.5 + (roll >> 8) as u8 as f64 / 255.0;
            counts.landed += usize::from(land_render(&mut model, id, factor));
        }
        2 => {
            let lengths =
                [NoteLength::SixteenthTriplet, NoteLength::Sixteenth, NoteLength::SixteenthDotted];
            let length = Length::Beats(lengths[(roll >> 16) as usize % lengths.len()]);
            counts.requests += usize::from(model.set_length(id, length) && stretch);
        }
        3 => {
            let fine = (roll >> 24) as u8 as f32 / 12.75 - 10.0;
            counts.requests += usize::from(model.set_fine_pct(id, fine) && stretch);
        }
        4 => {
            let pitch = (roll >> 32) as u8 as f32 / 10.0 - 12.0;
            counts.requests += usize::from(model.set_pitch_st(id, pitch) && stretch);
        }
        _ => {
            // Out of Stretch drops the lane's render while voices still
            // hold it; back in asks for a new one and plays the raw chop
            // until it lands. A retry on a lane that stays.
            if stretch && roll.is_multiple_of(2) {
                counts.exits += usize::from(model.set_time_mode(id, TimeMode::Off));
            } else if !stretch {
                counts.requests += usize::from(model.set_time_mode(id, TimeMode::Stretch));
            } else {
                counts.requests += usize::from(model.request_render(id));
            }
        }
    }
    model.commit();
    model.collect();
    drop(model);
    worker.wake();
}

/// The Stretch run's writer thread: the real render worker, and a GUI that
/// lands renders and edits Stretch lanes under the blocks until enough of
/// them were contended. `finish` waits for the worker's last render, so by
/// the time this returns every request the run made has been answered.
fn stretch_writer(
    model: SharedModel,
    tempo: Arc<TempoCell>,
    start: Arc<Barrier>,
    progress: Arc<AtomicUsize>,
    contended: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
) -> StretchCounts {
    let _done = DoneOnDrop(done);
    let worker = RenderWorker::with_debounce(Arc::clone(&model), tempo, STRETCH_DEBOUNCE);
    let mut counts = StretchCounts::default();
    let mut state = STRETCH_SEED;

    start.wait();
    let mut n = 0;
    while n < STRETCH_GESTURES || contended.load(Ordering::Acquire) < STRETCH_CONTENDED {
        let roll = next(&mut state);
        stretch_gesture(n, roll, &model, &worker, &mut counts);
        n += 1;
        progress.store(n, Ordering::Release);
    }
    counts.gestures = n;
    counts.worker_renders = worker.finish();
    counts
}

/// What the Stretch run's audio loop counted, in fixed-size fields.
#[derive(Debug, Default)]
struct StretchStats {
    /// Blocks rendered.
    blocks: usize,
    /// Blocks rendered while the writer's gesture counter moved.
    contended: usize,
    /// The loudest sample.
    peak: f32,
    /// Voice-blocks spent playing a render.
    rendered: usize,
    /// Blocks in which one lane had render voices of two different lengths
    /// sounding — a render swapped under a sounding voice. Reported, not
    /// asserted: how often it happens is up to the scheduler (the no-alloc
    /// run asserts it deterministically).
    mixed: usize,
    /// Voices reported ended to the host.
    terminated: usize,
    /// The use-after-free probe: every voice's audio read at the bottom of
    /// the block, after the writers have retired what they liked.
    probe: f32,
}

/// One Stretch-run block on the audio thread: the host tempo, the render,
/// the outbox, and the probe — inside the caller's no-alloc guard.
fn stretch_block(
    engine: &mut Engine,
    published: &PublishedCell,
    events: &[TimedEvent],
    out: (&mut [f32], &mut [f32]),
    progress: &AtomicUsize,
    stats: &mut StretchStats,
) {
    let before = progress.load(Ordering::Acquire);
    engine.tempo().update(Some(STRETCH_TEMPOS[stats.blocks % STRETCH_TEMPOS.len()]));
    engine.run_block(published, events, &mut Outputs::main(out.0, out.1), RATE, MASTER_VOLUME);

    for sample in out.0.iter().chain(out.1.iter()) {
        stats.peak = stats.peak.max(sample.abs());
    }
    let mut mixed = false;
    for voice in engine.pool().voices() {
        stats.probe += voice.sample.frames[0];
        if voice.rendered {
            stats.rendered += 1;
            let frames = voice.sample.frame_count();
            mixed |= engine.pool().voices().any(|other| {
                other.rendered
                    && other.lane_id == voice.lane_id
                    && other.sample.frame_count() != frames
            });
        }
    }
    stats.mixed += usize::from(mixed);
    stats.terminated += engine.pool().terminated().len();
    engine.clear_terminated();

    if progress.load(Ordering::Acquire) != before {
        stats.contended += 1;
    }
    stats.blocks += 1;
}

/// Stretch renders swapped under sounding voices while the audio
/// thread renders. The real render worker renders on its own thread and
/// lands through the writer mutex, a GUI thread lands renders of its own
/// and edits the Stretch lanes (each edit a render request; leaving Stretch
/// drops a render voices still hold), and the audio thread stores a new
/// tempo every block, so the speed correction re-times what is sounding and
/// the worker re-renders for the drift — all inside the no-alloc guard,
/// with the same progress handshake as the main stress test.
///
/// Afterwards: every request answered (nothing left pending, every Stretch
/// lane's render answering its newest generation), and every render the run
/// made handed back to the collector.
#[test]
fn renders_swap_under_sounding_voices_while_the_engine_renders() {
    use chopdrop::time::TimeMode;

    let model: SharedModel = Arc::new(Mutex::new(Model::new()));
    let (published, tempo) = {
        let mut model = lock_model(&model);
        stretch_kit(&mut model);
        (Arc::clone(model.published()), Arc::clone(model.tempo()))
    };
    let events = stretch_events();
    let mut engine = Engine::new().with_tempo(Arc::clone(&tempo));
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];

    // The warm-up block, outside the guard and the counting.
    engine.run_block(
        &published,
        &events,
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        MASTER_VOLUME,
    );
    engine.clear_terminated();
    engine.reset();

    let progress = Arc::new(AtomicUsize::new(0));
    let contended = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let start = Arc::new(Barrier::new(2));
    let writer = {
        let (model, tempo, start) = (Arc::clone(&model), Arc::clone(&tempo), Arc::clone(&start));
        let (progress, contended, done) =
            (Arc::clone(&progress), Arc::clone(&contended), Arc::clone(&done));
        std::thread::Builder::new()
            .name(String::from("chopdrop-stress-stretch"))
            .spawn(move || stretch_writer(model, tempo, start, progress, contended, done))
            .expect("the writer thread spawns")
    };

    // Solo: the writer is parked on the barrier (its worker idles — nothing
    // is pending), so these blocks play the kit's first renders.
    let mut solo = StretchStats::default();
    assert_no_alloc(|| {
        for _ in 0..SOLO_BLOCKS {
            let out = (&mut left[..], &mut right[..]);
            stretch_block(&mut engine, &published, &events, out, &progress, &mut solo);
        }
    });
    assert!(solo.peak > 0.0, "the kit rendered audible audio, peak {}", solo.peak);
    assert!(solo.rendered > 0, "and its voices played renders: {solo:?}");
    assert_eq!(solo.contended, 0, "no gesture had been made yet");

    start.wait();
    let mut stats = StretchStats::default();
    assert_no_alloc(|| {
        while !done.load(Ordering::Acquire) || stats.blocks < MIN_BLOCKS {
            let out = (&mut left[..], &mut right[..]);
            stretch_block(&mut engine, &published, &events, out, &progress, &mut stats);
            contended.store(stats.contended, Ordering::Release);
        }
    });

    let counts = writer.join().expect("the writer thread finished without panicking");

    assert!(
        stats.contended >= STRETCH_CONTENDED,
        "{} of {} blocks rendered while the writer was writing, wanted {STRETCH_CONTENDED}",
        stats.contended,
        stats.blocks
    );
    assert!(stats.peak > 0.0, "the contended blocks rendered audio, peak {}", stats.peak);
    assert!(stats.rendered > 0, "voices played renders under the writers: {stats:?}");
    assert!(stats.terminated > 0, "voices ended and were reported: {stats:?}");
    assert!(stats.probe.is_finite(), "the probe read live memory: {stats:?}");
    assert!(counts.gestures >= STRETCH_GESTURES, "{counts:?}");
    assert!(counts.landed > 0, "the writer landed renders under the blocks: {counts:?}");
    assert!(counts.requests > 0, "edits asked for renders: {counts:?}");
    assert!(counts.exits > 0, "lanes left Stretch holding a render: {counts:?}");
    assert!(counts.worker_renders > 0, "the real worker rendered too: {counts:?}");

    // Every request answered: `finish` waited the worker out.
    let lanes_ok = {
        let model = lock_model(&model);
        for lane in model.lanes() {
            assert!(!lane.render_pending, "lane {} still rendering", lane.id);
            if lane.time_mode == TimeMode::Stretch && lane.render_error.is_none() {
                let render = lane.render.as_ref().expect("a Stretch lane has a render");
                assert_eq!(render.generation, lane.render_generation, "lane {}", lane.id);
            }
        }
        model.lanes().len()
    };
    assert_eq!(lanes_ok, STRETCH_LANES);

    // And the collector drains: the engine lets go of its voices' renders,
    // the model of everything else.
    drop(engine);
    let mut model = lock_model(&model);
    model.replace_lanes(Vec::new(), false);
    model.collect();
    assert_eq!(model.alloc_count(), 1, "every render and chop of the run handed back");
}
