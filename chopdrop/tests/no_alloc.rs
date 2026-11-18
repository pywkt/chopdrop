//! The real-time rule (no allocation on the audio thread), enforced: a
//! deliberately busy block through [`Engine::run_block`] inside
//! `nice_assert_no_alloc::assert_no_alloc`, which aborts the process if the
//! audio path allocates or frees a single byte.
//!
//! **Why this test lives in `tests/` and not in `engine.rs`.** nice-plug
//! installs `nice_assert_no_alloc::AllocDisabler` as the *global* allocator
//! (its `wrapper/util.rs`, behind `debug_assertions` + our
//! `assert_process_allocs` feature) but keeps its own `assert_no_alloc`
//! wrapper crate-private, so the forbid counter has to be raised by calling
//! the dev-dependency directly — and that counter is a thread-local of the
//! *one* copy of the crate cargo resolved, which is why `Cargo.toml` pins it
//! to nice-plug's own requirement. The guard therefore only bites in a binary
//! that really links nice-plug: a test that touched none of `chopdrop` would
//! raise a counter no allocator reads and pass without checking anything.
//! Everything here goes through the `chopdrop` library, which links
//! nice-plug, and the control case below proves the guard is live rather than
//! asserting it in prose.
//!
//! What "busy" means here: ~200 events in one 512-frame block across eight
//! keys, three lanes stacked on each key so every note-on starts a chord of
//! voices, far more voice starts per block than the 64-voice pool holds (so
//! steals and their fade tails run), four choke groups firing against each
//! other, host chokes, gate releases, all three expressions matched both by
//! `voice_id` and by `(channel, key)`, auditions arriving from the GUI ring,
//! mono and stereo samples, a sample whose rate differs from the host's so
//! the interpolator works at fractional positions, and the keytracking
//! switch on.
//!
//! v2's audio path has its own run: voices on all sixteen stereo outputs at
//! once, with missing and short host buffers falling back to Out 1, and
//! cut-itself lanes retriggered while they sound.
//!
//! So do Repitch lanes: a host tempo stored every block inside the guard,
//! re-timing every sounding voice, with fine, pitch and length drags
//! published between blocks.
//!
//! And Stretch lanes: renders landing between blocks while voices still play
//! the ones they replace, re-timed by the speed correction as the tempo
//! moves, the last handle on a retired render dropped inside the guard.
//!
//! The editor's wake-up call is in here too ([`RepaintNotifier`], rung from
//! `process()` while [`Engine::needs_a_frame`] holds). It is one relaxed
//! store into an `Arc<AtomicBool>` and so allocates nothing, but it is a call
//! the audio thread makes on a type from outside this crate, which is exactly
//! the kind of claim the real-time rule wants checked rather than asserted in
//! prose.

mod support;

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use nice_assert_no_alloc::assert_no_alloc;
use nice_plug_egui::RepaintNotifier;

use chopdrop::engine::{
    AuditionRing, Engine, EngineEvent, NoteTarget, OUTPUTS, Outputs, TimedEvent,
};
use chopdrop::model::{Model, OUTPUT_MAIN, SampleRef};
use chopdrop::sample::SampleData;
use chopdrop::voices::{LaneMode, MAX_VOICES};

/// The host rate every block renders at.
const RATE: f64 = 48_000.0;

/// One block, the size Bitwig's default buffer is.
const BLOCK_FRAMES: usize = 512;

/// How many blocks the guarded run renders. More than one because some
/// allocations only happen on a path a first block never reaches — a voice
/// outliving its block, a fade tail finishing, an outbox wrapping.
const BLOCKS: usize = 8;

/// Lanes in the kit: three on each of [`KEYS`] keys, so a note-on is a stack
/// and 22 note-ons overflow the pool.
const LANES: usize = 24;

/// How many keys the lanes spread over.
const KEYS: u8 = 8;

/// The lowest lane note — 36 is C1 in Bitwig's convention.
const FIRST_KEY: u8 = 36;

/// Events in the block. Chosen so note-ons alone (three in eight, each
/// starting three voices) ask for roughly three times the pool's capacity.
const EVENTS: usize = 192;

/// Whether nice-plug installed the guard's allocator for this build: its
/// `#[global_allocator]` is behind `debug_assertions` and our
/// `assert_process_allocs` feature, and `nice-assert-no-alloc`'s own
/// `disable_release` default no-ops `assert_no_alloc` outside debug. With
/// either off there is nothing to check — a plain `cargo test` has both on.
const GUARD_ACTIVE: bool = cfg!(debug_assertions) && cfg!(feature = "assert_process_allocs");

/// A deterministic xorshift64*, so a failure is reproducible to the frame.
/// No `rand` dependency for eight bits of variety.
fn next(state: &mut u64) -> u64 {
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_F491_4F6C_DD1D)
}

/// `count` frames of non-silent, non-constant audio in `channels` channels,
/// interleaved as [`SampleData::frames`] wants it. A triangle rather than a
/// ramp so neighbouring frames differ in both directions and the cubic
/// interpolator cannot be right by accident.
fn tone(count: usize, channels: u8) -> Box<[f32]> {
    let mut frames = Vec::with_capacity(count * channels as usize);
    for n in 0..count {
        let phase = (n % 32) as f32 / 16.0;
        let value = if phase < 1.0 { phase } else { 2.0 - phase };
        for c in 0..channels {
            // The right channel is the left inverted: a stereo sample whose
            // two sides are told apart by eye in a failure message.
            frames.push(if c == 0 { value } else { -value });
        }
    }
    frames.into()
}

/// The kit the guarded run plays: [`LANES`] lanes, all with a sample (so all
/// published), stacked three to a key, with choke groups, both modes, both
/// channel counts, two sample rates and a range of lengths.
///
/// Built through the model's writer API rather than by hand so the block
/// renders against a snapshot the GUI and the loader really produce.
fn busy_model() -> Model {
    let mut model = Model::new();
    while model.lanes().len() < LANES {
        model.add_lane().expect("24 lanes is well inside MAX_LANES");
    }

    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    for (n, id) in ids.into_iter().enumerate() {
        // Every third lane is stereo; every fourth claims 44.1 kHz, which at
        // a 48 kHz host rate is a fractional step for the interpolator.
        let channels = if n % 3 == 2 { 2 } else { 1 };
        let rate = if n % 4 == 3 { 44_100 } else { RATE as u32 };
        // Lengths from well under a block to several blocks long, so voices
        // both end inside a block and survive the whole run.
        let count = 200 + n * 300;
        let data = SampleData { rate, channels, frames: tone(count, channels) };
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/busy.wav"), vec![0], data);

        assert!(model.set_sample(id, sample), "lane {n} takes a sample");
        assert!(model.set_note(id, FIRST_KEY + n as u8 % KEYS), "lane {n} takes a note");
        assert!(model.set_pan(id, -1.0 + (n % 5) as f32 * 0.5), "lane {n} takes a pan");
        assert!(model.set_volume_db(id, -12.0 + n as f32 * 0.5), "lane {n} takes a volume");
        // 0 is "no group", so a fifth of the kit chokes nothing and the rest
        // fight over four groups — including lanes that share a group with
        // themselves, which self-choke.
        assert!(model.set_choke(id, n as u8 % 5), "lane {n} takes a choke group");
        let mode = if n % 2 == 0 { LaneMode::OneShot } else { LaneMode::Gate };
        assert!(model.set_mode(id, mode), "lane {n} takes a mode");
    }
    // On, so every note-on and every `PolyTuning` goes through the switch's
    // key offset as well.
    model.set_sample_pad_switch(true);
    model
}

/// One block's worth of events, in offset order: note-ons, note-offs, host
/// chokes and all three expressions, half of them carrying a `voice_id` and
/// half matching by `(channel, key)` instead.
fn busy_events() -> Vec<TimedEvent> {
    let mut events = Vec::with_capacity(EVENTS);
    let mut state = 0x1234_5678_9ABC_DEF0u64;

    for n in 0..EVENTS {
        let roll = next(&mut state);
        // Spread over the block in order, several events landing on the same
        // frame — which is what bitstep sends: a note-on and its three
        // expressions at one timing.
        let timing = (n * BLOCK_FRAMES / EVENTS) as u32;
        let note = NoteTarget {
            channel: (roll % 2) as u8,
            key: FIRST_KEY + (roll >> 8) as u8 % KEYS,
            // A sixteen-id space the hosts' own ids are drawn from, so some
            // later events name a voice that exists and some name none.
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

/// Push the GUI's auditions for this block.
///
/// The push half runs on the GUI thread, so it is not what the guard is
/// about — but it is wait-free too, and calling it inside the guard costs
/// nothing and says so.
fn push_auditions(ring: &AuditionRing, lane_ids: &[u32], block: usize) {
    for (n, id) in lane_ids.iter().enumerate() {
        if (block + n).is_multiple_of(5) {
            // Best-effort by contract: a full ring drops the audition, and
            // that `false` is not a failure.
            ring.push(*id);
        }
    }
}

/// The test itself: [`BLOCKS`] busy blocks, start to finish, with nothing
/// allocated or freed on the way.
///
/// Everything the run needs is built before the guard goes up, as `lib.rs`
/// builds it before `process()` is ever called: the model and its samples,
/// the event list, the output buffers, the clone of the audition ring. What
/// happens inside is exactly what `lib.rs` does per block — render, then
/// drain the terminated outbox for the host.
#[test]
fn a_busy_block_allocates_nothing() {
    let model = busy_model();
    let events = busy_events();
    let lane_ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    let mut engine = Engine::new();
    let auditions = Arc::clone(engine.auditions());
    // Cloned before the guard, as `lib.rs` clones it when the editor is
    // built and never on this path.
    let repaint = RepaintNotifier::new();
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];

    // One block outside the guard first, then back to an empty pool. Not to
    // excuse the audio path anything — it allocates on no block, which is
    // what the run below asserts — but so that a one-time lazy init inside
    // *std* (a thread-local, a float formatting table) cannot be reported as
    // ours. The engine itself holds no lazily-built state: `Engine::new`
    // allocates the pool and the ring and nothing after it ever does.
    engine.run_block(
        model.published(),
        &events,
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        0.5,
    );
    engine.clear_terminated();
    engine.reset();

    // Counters, so the assertions that the block was really busy can be made
    // after the guard comes down rather than inside it.
    let mut max_active = 0usize;
    let mut max_tails = 0usize;
    let mut terminated_total = 0usize;
    let mut peak = 0.0f32;
    let mut woken = 0usize;

    assert_no_alloc(|| {
        for block in 0..BLOCKS {
            push_auditions(&auditions, &lane_ids, block);

            engine.run_block(
                model.published(),
                &events,
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                0.5,
            );

            // What `lib.rs` does with the block's output and its outbox.
            for sample in left.iter().chain(right.iter()) {
                peak = peak.max(sample.abs());
            }
            max_active = max_active.max(engine.pool().active_count());
            max_tails = max_tails.max(engine.pool().tail_count());
            terminated_total += engine.pool().terminated().len();
            engine.clear_terminated();

            // And the editor's wake-up call, exactly as `process()` makes
            // it.
            if engine.needs_a_frame() {
                repaint.request_repaint();
                woken += 1;
            }
        }
    });

    // The run has to have been as busy as it claims, or a green guard means
    // nothing: a full pool proves voices were stolen (`start` only steals
    // when no slot is free), a non-empty tail set proves the steal fades
    // rendered, and a non-empty outbox proves voices ended and were reported.
    assert_eq!(max_active, MAX_VOICES, "the pool filled, so note-ons stole slots");
    assert!(max_tails > 0, "stolen voices left fade tails rendering, got {max_tails}");
    assert!(terminated_total > 0, "voices ended and were reported to the host");
    assert!(peak > 0.0, "the blocks rendered audible audio, peak {peak}");
    assert_eq!(woken, BLOCKS, "every busy block asked the editor for a frame");
}

/// Outputs the routed run's host leaves out entirely: lanes routed there
/// must fall back to the main output.
const MISSING_OUTPUTS: [usize; 2] = [5, 11];

/// An output the routed run's host provides with too few frames for the
/// block, which the engine drops for the block — its lane falls back too.
const SHORT_OUTPUT: usize = 7;

/// Note-ons per key per block in the routed run, evenly spaced.
const HITS_PER_BLOCK: usize = 2;

/// The routed run's kit: one lane per output, lane `n` on key
/// `FIRST_KEY + n` and output `n`, so every voice is told apart by its key
/// and every output has a lane of its own.
///
/// Even lanes cut themselves and play samples longer than the whole run, so
/// a voice of theirs can only end by being cut by the next hit — half are
/// one-shots, half gates whose note-off starts a long release that the cut
/// then shortens to `FADE_MS` (`Voice::begin_fade`'s exception). Odd lanes
/// do not cut themselves and play samples a little longer than the hit
/// spacing, so their hits stack. No choke groups and no switch: nothing but
/// cut itself (and samples ending) may fade or end a voice here.
fn routed_model() -> (Model, Vec<bool>) {
    let mut model = Model::new();
    while model.lanes().len() < OUTPUTS {
        model.add_lane().expect("16 lanes is well inside MAX_LANES");
    }

    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    let spacing = BLOCK_FRAMES / HITS_PER_BLOCK;
    let mut cuts = Vec::with_capacity(OUTPUTS);
    for (n, id) in ids.into_iter().enumerate() {
        let cut = n % 2 == 0;
        let channels = if n % 3 == 0 { 2 } else { 1 };
        let count = if cut { BLOCK_FRAMES * (BLOCKS + 2) } else { spacing + 100 + n * 10 };
        let data = SampleData { rate: RATE as u32, channels, frames: tone(count, channels) };
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/routed.wav"), vec![0], data);

        assert!(model.set_sample(id, sample), "lane {n} takes a sample");
        assert!(model.set_note(id, FIRST_KEY + n as u8), "lane {n} takes a note");
        assert!(model.set_output(id, n as u8), "lane {n} takes output {n}");
        assert!(model.set_cut_itself(id, cut), "lane {n} sets cut itself");
        assert!(model.set_choke(id, 0), "lane {n} is in no choke group");
        let mode = if n % 4 == 2 || n % 4 == 3 { LaneMode::Gate } else { LaneMode::OneShot };
        assert!(model.set_mode(id, mode), "lane {n} takes a mode");
        assert!(model.set_attack_ms(id, (n % 4) as f32 * 2.0), "lane {n} takes an attack");
        assert!(model.set_release_ms(id, 50.0 + n as f32 * 10.0), "lane {n} takes a release");
        cuts.push(cut);
    }
    (model, cuts)
}

/// One routed block's events: every key hit [`HITS_PER_BLOCK`] times, the
/// keys staggered by a few frames, each gate lane's note released halfway to
/// its next hit — so a cut-itself gate is cut while in its long release.
fn routed_events() -> Vec<TimedEvent> {
    let spacing = BLOCK_FRAMES / HITS_PER_BLOCK;
    let mut events = Vec::new();
    for hit in 0..HITS_PER_BLOCK {
        for n in 0..OUTPUTS {
            let note = NoteTarget { channel: 0, key: FIRST_KEY + n as u8, voice_id: None };
            let on = hit * spacing + n * 4;
            events.push(TimedEvent::new(on as u32, EngineEvent::NoteOn { note, velocity: 0.8 }));
            events.push(TimedEvent::new((on + spacing / 2) as u32, EngineEvent::NoteOff { note }));
        }
    }
    events.sort_by_key(|event| event.timing);
    events
}

/// The host buffers for the routed run: index `n` is Out `n + 1`, `None`
/// where the host left the output out.
fn routed_buffers() -> Vec<Option<(Vec<f32>, Vec<f32>)>> {
    (0..OUTPUTS)
        .map(|n| {
            let frames = if n == SHORT_OUTPUT { BLOCK_FRAMES / 4 } else { BLOCK_FRAMES };
            let present = !MISSING_OUTPUTS.contains(&n);
            present.then(|| (vec![0.0f32; frames], vec![0.0f32; frames]))
        })
        .collect()
}

/// [`Outputs`] over [`routed_buffers`], built per block as `lib.rs` builds
/// it from the host's main and aux buffers: a stack array of borrows.
fn routed_outputs(buffers: &mut [Option<(Vec<f32>, Vec<f32>)>]) -> Outputs<'_> {
    let mut outputs = Outputs::main(&mut [], &mut []);
    for (pair, buffer) in outputs.pairs.iter_mut().zip(buffers.iter_mut()) {
        *pair = buffer.as_mut().map(|(left, right)| (&mut left[..], &mut right[..]));
    }
    outputs
}

/// The v2 audio path under the guard: voices on all sixteen outputs at once —
/// two of them missing and one too short, so the fallback to Out 1 runs as
/// well — and cut-itself lanes retriggered while they sound, one-shots and
/// gates mid-release alike, with attacks and releases on every lane.
#[test]
fn a_routed_block_with_cut_itself_retriggers_allocates_nothing() {
    let (model, cuts) = routed_model();
    let events = routed_events();
    let mut buffers = routed_buffers();
    let mut engine = Engine::new();

    // Warm-up outside the guard, as in the busy run.
    engine.run_block(model.published(), &events, &mut routed_outputs(&mut buffers), RATE, 0.5);
    engine.clear_terminated();
    engine.reset();

    let mut peaks = [0.0f32; OUTPUTS];
    let mut cut_ends = 0usize;
    let mut max_cut_live = 0usize;
    let mut max_stack_live = 0usize;
    let mut max_active = 0usize;
    let mut max_tails = 0usize;

    assert_no_alloc(|| {
        for _ in 0..BLOCKS {
            engine.run_block(
                model.published(),
                &events,
                &mut routed_outputs(&mut buffers),
                RATE,
                0.5,
            );

            for (peak, buffer) in peaks.iter_mut().zip(buffers.iter()) {
                if let Some((left, right)) = buffer {
                    for sample in left.iter().chain(right.iter()) {
                        *peak = peak.max(sample.abs());
                    }
                }
            }
            // A cut-itself lane's sample outlasts the run, so each of its
            // voices that ended was cut by the lane's next hit.
            for ended in engine.pool().terminated() {
                if cuts[usize::from(ended.note_key - FIRST_KEY)] {
                    cut_ends += 1;
                }
            }
            engine.clear_terminated();
            for (n, &cut) in cuts.iter().enumerate() {
                let key = FIRST_KEY + n as u8;
                let live = engine
                    .pool()
                    .voices()
                    .filter(|voice| voice.note_key == key && !voice.is_fading())
                    .count();
                if cut {
                    max_cut_live = max_cut_live.max(live);
                } else {
                    max_stack_live = max_stack_live.max(live);
                }
            }
            max_active = max_active.max(engine.pool().active_count());
            max_tails = max_tails.max(engine.pool().tail_count());
        }
    });

    // The run did what it claims, or a green guard means nothing.
    for (n, peak) in peaks.iter().enumerate() {
        let fallback = MISSING_OUTPUTS.contains(&n) || n == SHORT_OUTPUT;
        if fallback {
            assert_eq!(*peak, 0.0, "Out {} was missing or short, its lane fell back", n + 1);
        } else {
            assert!(*peak > 0.0, "Out {} carried its lane's voices, peak {peak}", n + 1);
        }
    }
    assert!(peaks[usize::from(OUTPUT_MAIN)] > 0.0, "the main output carried the fallbacks");
    let hits = BLOCKS * HITS_PER_BLOCK * cuts.iter().filter(|&&cut| cut).count();
    assert!(
        cut_ends > hits / 2,
        "cut-itself retriggers ended the voices they cut: {cut_ends} of {hits} hits"
    );
    assert_eq!(max_cut_live, 1, "a cut-itself lane never held two unfaded voices");
    assert!(max_stack_live >= 2, "lanes that do not cut stacked, got {max_stack_live}");
    assert!(max_active < MAX_VOICES, "no steals, so every fade was a cut or a release");
    assert_eq!(max_tails, 0, "and no steal tails rendered");
}

/// Lanes in the Repitch run's kit: one per key from [`FIRST_KEY`].
const TIMED_LANES: usize = 12;

/// The host tempos the Repitch run sweeps through, one per block — the
/// last three hostile (NaN, zero, huge), which the cell clamps on the way
/// in. `None` is a block whose transport carried no tempo: the last value
/// holds.
const TEMPOS: [Option<f64>; BLOCKS] = [
    Some(120.0),
    Some(97.0),
    None,
    Some(61.5),
    Some(174.0),
    Some(f64::NAN),
    Some(0.0),
    Some(1e9),
];

/// The Repitch run's kit: lane `n` on key `FIRST_KEY + n`. Most lanes are
/// Repitch on a note length from 1/16 to two bars with a fine trim, so every
/// tempo in [`TEMPOS`] gives them a different, mostly fractional speed; two
/// are Repitch "as cut", two are Off with a pitch, and one is Stretch on its
/// raw chop. The samples outlast the run (some at 44.1 kHz), so voices sound
/// across every tempo change and are re-timed mid-note, and retriggers stack
/// until the pool fills and steals.
fn timed_model() -> Model {
    use chopdrop::time::{Length, NoteLength, TimeMode};

    let lengths = [
        NoteLength::Sixteenth,
        NoteLength::EighthTriplet,
        NoteLength::Quarter,
        NoteLength::QuarterDotted,
        NoteLength::Half,
        NoteLength::Bar,
        NoteLength::TwoBars,
    ];
    let mut model = Model::new();
    while model.lanes().len() < TIMED_LANES {
        model.add_lane().expect("12 lanes is well inside MAX_LANES");
    }
    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    for (n, id) in ids.into_iter().enumerate() {
        let channels = if n % 3 == 0 { 2 } else { 1 };
        let rate = if n % 4 == 1 { 44_100 } else { RATE as u32 };
        // A quarter to half a second: 1/16 at 120 BPM plays it faster,
        // two bars slower, and every voice outlives the run.
        let count = 12_000 + n * 1_000;
        let data = SampleData { rate, channels, frames: tone(count, channels) };
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/timed.wav"), vec![0], data);

        assert!(model.set_sample(id, sample), "lane {n} takes a sample");
        assert!(model.set_note(id, FIRST_KEY + n as u8), "lane {n} takes a note");
        assert!(model.set_cut_itself(id, false), "lane {n} stacks its retriggers");
        assert!(model.set_choke(id, 0), "lane {n} is in no choke group");
        let (mode, length) = match n {
            0..=6 => (TimeMode::Repitch, Length::Beats(lengths[n])),
            7 | 8 => (TimeMode::Repitch, Length::AsCut),
            9 | 10 => (TimeMode::Off, Length::AsCut),
            _ => (TimeMode::Stretch, Length::Beats(NoteLength::Bar)),
        };
        assert!(model.set_time_mode(id, mode), "lane {n} takes a time mode");
        assert!(model.set_length(id, length), "lane {n} takes a length");
        assert!(model.set_fine_pct(id, -6.0 + n as f32 * 1.5), "lane {n} takes a fine");
        assert!(model.set_pitch_st(id, -7.0 + n as f32 * 1.25), "lane {n} takes a pitch");
    }
    model
}

/// One Repitch-run block's events: every key hit at the top of the block
/// and again a third of the way in, each lane's own few frames apart.
fn timed_events() -> Vec<TimedEvent> {
    let mut events = Vec::new();
    for hit in [0, BLOCK_FRAMES / 3] {
        for n in 0..TIMED_LANES {
            let note = NoteTarget { channel: 0, key: FIRST_KEY + n as u8, voice_id: None };
            let on = (hit + n * 3) as u32;
            events.push(TimedEvent::new(on, EngineEvent::NoteOn { note, velocity: 0.7 }));
        }
    }
    events.sort_by_key(|event| event.timing);
    events
}

/// Repitch lanes and tempo changes under the guard: every block
/// stores a new host tempo into the engine's [`TempoCell`] inside the guard,
/// exactly as `process()` does, so `sync_lane_controls` re-times every
/// sounding Repitch voice (`Voice::set_time` and its `powf`) on the audio
/// thread, and every note-on computes a fresh speed. Between blocks — the
/// GUI's side, outside the guard — a fine trim, a pitch and a length are
/// moved, so the per-block re-read sees published atomics change too.
#[test]
fn repitch_lanes_through_tempo_changes_allocate_nothing() {
    use chopdrop::time::{Length, NoteLength};

    let mut model = timed_model();
    let events = timed_events();
    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    let mut engine = Engine::new().with_tempo(Arc::clone(model.tempo()));
    let tempo = Arc::clone(engine.tempo());
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];

    // Warm-up outside the guard, as in the busy run.
    let mut outputs = Outputs::main(&mut left, &mut right);
    engine.run_block(model.published(), &events, &mut outputs, RATE, 0.5);
    engine.clear_terminated();
    engine.reset();

    let mut peak = 0.0f32;
    let mut speeds = [(f64::INFINITY, f64::NEG_INFINITY); BLOCKS];
    let mut stored = [0.0f64; BLOCKS];
    let mut max_active = 0usize;

    for (block, host_tempo) in TEMPOS.iter().enumerate() {
        assert_no_alloc(|| {
            // What `process()` does with the transport, then the block.
            tempo.update(*host_tempo);
            engine.run_block(
                model.published(),
                &events,
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                0.5,
            );
            stored[block] = tempo.get();
            for sample in left.iter().chain(right.iter()) {
                peak = peak.max(sample.abs());
            }
            for voice in engine.pool().voices() {
                let (low, high) = &mut speeds[block];
                *low = low.min(voice.speed);
                *high = high.max(voice.speed);
            }
            max_active = max_active.max(engine.pool().active_count());
            engine.clear_terminated();
        });

        // The GUI's drags between blocks, on the Repitch lanes.
        let n = block % 7;
        assert!(model.set_fine_pct(ids[n], 8.0 - block as f32 * 2.0));
        assert!(model.set_pitch_st(ids[9], block as f32));
        let length = if block % 2 == 0 { NoteLength::Eighth } else { NoteLength::HalfDotted };
        assert!(model.set_length(ids[(n + 3) % 7], Length::Beats(length)));
        model.collect();
    }

    // The run did what it claims, or a green guard means nothing.
    assert!(peak > 0.0, "the blocks rendered audible audio, peak {peak}");
    assert!(max_active > TIMED_LANES, "voices stacked across blocks, got {max_active}");
    assert_eq!(stored[1], 97.0, "the host tempo reached the cell");
    assert_eq!(stored[2], 97.0, "a block without a tempo kept the last one");
    for (block, &bpm) in stored.iter().enumerate() {
        assert!(bpm.is_finite() && bpm > 0.0, "block {block} read a clamped tempo, got {bpm}");
    }
    for (block, &(low, high)) in speeds.iter().enumerate() {
        assert!(low < 1.0 || high > 1.0, "block {block} played Repitch speeds {low}..{high}");
    }
    assert!(speeds.iter().any(|&(low, _)| low < 1.0), "some voices were slowed down");
    assert!(speeds.iter().any(|&(_, high)| high > 1.0), "and some sped up");
    let (_, slow_high) = speeds[3];
    let (_, fast_high) = speeds[4];
    assert!(fast_high > slow_high, "a faster tempo sped the voices up: {speeds:?}");
}

/// Lanes in the Stretch run's kit: one per key from [`FIRST_KEY`].
const STRETCH_LANES: usize = 6;

/// The host tempos the Stretch run sweeps through, one per block, so the
/// speed correction re-times every sounding render voice mid-note — both
/// faster and slower than the 120 BPM the renders are made around.
const STRETCH_TEMPOS: [f64; 16] = [
    120.0, 90.0, 140.0, 60.0, 175.0, 120.0, 100.0, 133.0, 80.0, 160.0, 110.0, 70.0, 150.0,
    95.0, 125.0, 180.0,
];

/// Land a render for a lane's newest request through the writer API,
/// exactly what the render worker's apply step does: the chop's rate and
/// channel count, `factor` times the lane's note length at 120 BPM — so it
/// is a render made for some other tempo, and the correction has work to
/// do. `false` when the lane is not in Stretch.
fn land_stretch_render(model: &mut Model, id: u32, factor: f64) -> bool {
    use chopdrop::time::{self, Length};

    if !model.request_render(id) {
        return false;
    }
    let lane = model.lane(id).expect("lane");
    let generation = lane.render_generation;
    let Length::Beats(length) = lane.length else { panic!("the kit's lanes are on beats") };
    let chop = &lane.sample.as_ref().expect("a chop").data;
    let secs = time::target_secs(length, 0.0, 120.0) * factor;
    let count = (secs * f64::from(chop.rate)) as usize;
    let data =
        SampleData { rate: chop.rate, channels: chop.channels, frames: tone(count, chop.channels) };
    model.set_render(id, generation, data)
}

/// The Stretch run's kit: lane `n` on key `FIRST_KEY + n`, every lane in
/// Stretch on a note length of a sixteenth or so (short, so voices end inside
/// the run), mono and stereo, two chop rates, each with a first render landed
/// — except the last, which plays its raw chop until its first render lands.
fn stretch_model() -> Model {
    use chopdrop::time::{Length, NoteLength, TimeMode};

    let lengths = [
        NoteLength::SixteenthTriplet,
        NoteLength::Sixteenth,
        NoteLength::SixteenthDotted,
        NoteLength::EighthTriplet,
        NoteLength::Sixteenth,
        NoteLength::SixteenthTriplet,
    ];
    let mut model = Model::new();
    while model.lanes().len() < STRETCH_LANES {
        model.add_lane().expect("6 lanes is well inside MAX_LANES");
    }
    while model.lanes().len() > STRETCH_LANES {
        let id = model.lanes()[STRETCH_LANES].id;
        assert!(model.delete_lane(id));
    }
    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    for (n, id) in ids.into_iter().enumerate() {
        let channels = if n % 2 == 0 { 2 } else { 1 };
        let rate = if n % 3 == 1 { 44_100 } else { RATE as u32 };
        let data = SampleData { rate, channels, frames: tone(4_000 + n * 500, channels) };
        let sample = SampleRef::new(model.handle(), Path::new("/tmp/chop.wav"), vec![0], data);

        assert!(model.set_sample(id, sample), "lane {n} takes a chop");
        assert!(model.set_note(id, FIRST_KEY + n as u8), "lane {n} takes a note");
        assert!(model.set_cut_itself(id, false), "lane {n} stacks its retriggers");
        assert!(model.set_choke(id, 0), "lane {n} is in no choke group");
        assert!(model.set_time_mode(id, TimeMode::Stretch), "lane {n} goes Stretch");
        assert!(model.set_length(id, Length::Beats(lengths[n])), "lane {n} takes a length");
        if n + 1 < STRETCH_LANES {
            let factor = 0.7 + n as f64 * 0.15;
            assert!(land_stretch_render(&mut model, id, factor), "lane {n} renders");
        }
    }
    model
}

/// What the Stretch run saw inside the guard, kept in fixed-size fields so
/// gathering it allocates nothing.
#[derive(Debug, Default)]
struct StretchStats {
    /// The loudest sample the blocks produced.
    peak: f32,
    /// Voice-blocks spent playing a render.
    rendered: usize,
    /// Voice-blocks a Stretch lane spent on its raw chop.
    raw: usize,
    /// Blocks in which one lane had two render voices of different lengths
    /// sounding at once — a render swapped under a sounding voice.
    mixed: usize,
    /// The slowest and fastest correction a render voice played at.
    low: f64,
    high: f64,
    /// Voices reported ended to the host.
    terminated: usize,
}

/// Stretch renders landing between blocks while voices play the renders
/// they replace: every block stores a new host tempo inside the
/// guard, so the speed correction (`Voice::set_time` against each voice's
/// own render) runs on the audio thread for every sounding render voice, and
/// every block retriggers every lane, so voices on the old render and on the
/// new one sound together. Between blocks — the worker's side, outside the
/// guard — a fresh render lands for some lanes (republishing), a lane leaves
/// Stretch (its render dropped from the model while voices still hold it)
/// and comes back (its raw chop until the next render lands), and the
/// collector runs. Renders are short enough that voices end inside the
/// guard, so the audio thread drops the last handle on a retired render —
/// which basedrop must queue, not free.
#[test]
fn stretch_renders_swapped_mid_voice_allocate_nothing() {
    use chopdrop::time::TimeMode;

    let mut model = stretch_model();
    let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    let mut events = Vec::new();
    for hit in [0, BLOCK_FRAMES / 2] {
        for n in 0..STRETCH_LANES {
            let note = NoteTarget { channel: 0, key: FIRST_KEY + n as u8, voice_id: None };
            let on = (hit + n * 5) as u32;
            events.push(TimedEvent::new(on, EngineEvent::NoteOn { note, velocity: 0.8 }));
        }
    }
    events.sort_by_key(|event| event.timing);
    let mut engine = Engine::new().with_tempo(Arc::clone(model.tempo()));
    let tempo = Arc::clone(engine.tempo());
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];

    // Warm-up outside the guard, as in the busy run.
    let mut outputs = Outputs::main(&mut left, &mut right);
    engine.run_block(model.published(), &events, &mut outputs, RATE, 0.5);
    engine.clear_terminated();
    engine.reset();

    let mut stats =
        StretchStats { low: f64::INFINITY, high: f64::NEG_INFINITY, ..Default::default() };
    let mut landed = 0usize;
    for (block, &bpm) in STRETCH_TEMPOS.iter().enumerate() {
        assert_no_alloc(|| {
            tempo.update(Some(bpm));
            engine.run_block(
                model.published(),
                &events,
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                0.5,
            );
            for sample in left.iter().chain(right.iter()) {
                stats.peak = stats.peak.max(sample.abs());
            }
            let mut mixed = false;
            for voice in engine.pool().voices() {
                if !voice.rendered {
                    stats.raw += 1;
                    continue;
                }
                stats.rendered += 1;
                stats.low = stats.low.min(voice.speed);
                stats.high = stats.high.max(voice.speed);
                let frames = voice.sample.frame_count();
                mixed |= engine.pool().voices().any(|other| {
                    other.rendered
                        && other.lane_id == voice.lane_id
                        && other.sample.frame_count() != frames
                });
            }
            stats.mixed += usize::from(mixed);
            stats.terminated += engine.pool().terminated().len();
            engine.clear_terminated();
        });

        // The worker's side, between blocks: renders land, a lane leaves
        // Stretch and comes back, and the collector runs.
        for (n, &id) in ids.iter().enumerate() {
            if (n + block) % 2 == 0 {
                let factor = 0.5 + ((n * 7 + block * 3) % 9) as f64 * 0.125;
                landed += usize::from(land_stretch_render(&mut model, id, factor));
            }
        }
        match block {
            4 => assert!(model.set_time_mode(ids[1], TimeMode::Off), "lane 1 leaves Stretch"),
            8 => assert!(model.set_time_mode(ids[1], TimeMode::Stretch), "and comes back"),
            _ => {}
        }
        model.collect();
    }

    // The run did what it claims, or a green guard means nothing.
    assert!(stats.peak > 0.0, "the blocks rendered audible audio, peak {}", stats.peak);
    assert!(landed >= STRETCH_LANES * 3, "renders landed between blocks: {landed}");
    assert!(stats.rendered > 0, "voices played renders: {stats:?}");
    assert!(stats.raw > 0, "and Stretch lanes without one played their chop: {stats:?}");
    assert!(stats.mixed > 0, "a render swapped under a sounding voice: {stats:?}");
    assert!(stats.low < 1.0 && stats.high > 1.0, "the correction ran both ways: {stats:?}");
    assert!(stats.terminated > 0, "render voices ended inside the guard: {stats:?}");

    // And every retired render is handed back once the engine lets go.
    drop(engine);
    model.replace_lanes(Vec::new(), false);
    model.collect();
    assert_eq!(model.alloc_count(), 1, "only the live empty snapshot is left");
}

/// The browser's preview path under the same guard: a preview starting, being
/// replaced while it plays (the old one fading in the tail slot, the one
/// before that dropped), and being stopped. The previewer decodes on its own
/// thread and publishes from there, so only the audio thread's side is inside
/// the guard, one block at a time, with each new request made between blocks.
#[test]
fn previews_allocate_nothing() {
    use chopdrop::preview::{PreviewChannel, PreviewStatus, Previewer};
    use std::time::{Duration, Instant};

    let model = Model::new();
    let channel = Arc::new(PreviewChannel::new(model.handle()));
    let previewer = Previewer::new(Arc::clone(&channel), model.handle().clone());
    let mut engine = Engine::new().with_preview(Arc::clone(&channel));
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];

    let dir = std::env::temp_dir().join(format!("chopdrop-no-alloc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let files: Vec<_> = (0..3)
        .map(|n| {
            let path = dir.join(format!("preview-{n}.wav"));
            let samples = vec![8000i16; RATE as usize];
            std::fs::write(&path, support::wav_i16(1, RATE as u32, &samples)).expect("scratch wav");
            path
        })
        .collect();

    let wait = |previewer: &Previewer| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(previewer.status(), PreviewStatus::Loading(_)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
    };

    // Warm-up outside the guard, as above.
    engine.run_block(model.published(), &[], &mut Outputs::main(&mut left, &mut right), RATE, 1.0);

    let mut peak = 0.0f32;
    let mut render = |engine: &mut Engine, peak: &mut f32| {
        assert_no_alloc(|| {
            engine.run_block(
                model.published(),
                &[],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
            for sample in left.iter() {
                *peak = peak.max(sample.abs());
            }
        });
    };

    for path in &files {
        previewer.play(path);
        wait(&previewer);
        render(&mut engine, &mut peak);
        render(&mut engine, &mut peak);
    }
    previewer.stop();
    render(&mut engine, &mut peak);
    render(&mut engine, &mut peak);

    assert!(peak > 0.0, "the previews were heard, peak {peak}");
    assert!(!channel.is_playing(), "and the stop silenced them");
}

/// The chop editor's looping preview under the same guard: a short region
/// looping several times a block, its live edges moved between blocks (a pass
/// running on, then a jump when the position falls outside), a `play_from`
/// replacing it (the loop fading in the tail slot) and a stop. `play_region`
/// publishes on this thread, outside the guard, between blocks.
#[test]
fn a_looping_chop_preview_allocates_nothing() {
    use chopdrop::preview::{PreviewChannel, Previewer, Region};

    let model = Model::new();
    let channel = Arc::new(PreviewChannel::new(model.handle()));
    let previewer = Previewer::new(Arc::clone(&channel), model.handle().clone());
    let mut engine = Engine::new().with_preview(Arc::clone(&channel));
    let mut left = vec![0.0f32; BLOCK_FRAMES];
    let mut right = vec![0.0f32; BLOCK_FRAMES];
    let source = basedrop::Shared::new(
        model.handle(),
        SampleData { rate: RATE as u32, channels: 2, frames: tone(RATE as usize, 2) },
    );

    // Warm-up outside the guard, as above.
    engine.run_block(model.published(), &[], &mut Outputs::main(&mut left, &mut right), RATE, 1.0);

    let mut peak = 0.0f32;
    let mut render = |engine: &mut Engine, peak: &mut f32| {
        assert_no_alloc(|| {
            engine.run_block(
                model.published(),
                &[],
                &mut Outputs::main(&mut left, &mut right),
                RATE,
                1.0,
            );
            for sample in left.iter() {
                *peak = peak.max(sample.abs());
            }
        });
    };

    // Shorter than a block, so every block crosses at least one seam.
    let short = (BLOCK_FRAMES / 3) as u64;
    previewer.play_region(source.clone(), Region { start: 1000, end: 1000 + short }, true);
    for _ in 0..4 {
        render(&mut engine, &mut peak);
    }
    assert!(channel.is_playing(), "the loop is still going");
    // Edges moved around the position: the pass runs on to its seam.
    channel.set_live_region(Region { start: 900, end: 1100 + short });
    render(&mut engine, &mut peak);
    render(&mut engine, &mut peak);
    // Moved away from it: the voice jumps at once.
    channel.set_live_region(Region { start: 20_000, end: 20_000 + short });
    render(&mut engine, &mut peak);
    let at = channel.position();
    assert!((20_000..20_000 + short).contains(&at), "jumped into the live region, at {at}");
    // A click elsewhere replaces the loop; then stop.
    previewer.play_from(source.clone(), 30_000);
    render(&mut engine, &mut peak);
    previewer.stop();
    render(&mut engine, &mut peak);
    render(&mut engine, &mut peak);

    assert!(peak > 0.0, "the loop was heard, peak {peak}");
    assert!(!channel.is_playing(), "and the stop silenced it");
}

/// The control case: a single `Vec` push inside the same guard, which must
/// kill the process.
///
/// Documented *and* run, because "the guard is active" is the one assumption
/// the test above cannot check itself — a `nice_assert_no_alloc` that is not
/// the copy nice-plug installed, a build without `assert_process_allocs`, or
/// a test binary that fails to link nice-plug at all would all leave
/// `assert_no_alloc` a no-op and make a busy block pass without proving
/// anything.
///
/// A violation calls `std::alloc::handle_alloc_error`, which aborts rather
/// than unwinding, so the allocating half cannot run in this process and be
/// caught: it runs in a child copy of this very test binary, asked for by
/// name with [`CONTROL_ENV`] set. The parent then asserts the child **died**.
/// If the guard were inert the child would instead print
/// [`INERT_MARKER`] and exit 0, which is why the assertion is on the exit
/// status and not merely on failure to pass.
#[test]
fn the_guard_bites_a_deliberate_allocation() {
    if !GUARD_ACTIVE {
        eprintln!("skipped: this build installs no AllocDisabler, so the guard is a no-op");
        return;
    }
    if std::env::var_os(CONTROL_ENV).is_some() {
        allocate_under_the_guard();
        return;
    }

    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let output = Command::new(exe)
        .args(["--exact", CONTROL_TEST, "--nocapture", "--test-threads=1"])
        .env(CONTROL_ENV, "1")
        .output()
        .expect("the control case re-runs this test binary");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        stdout.contains(ENTER_MARKER),
        "the child never reached the guard, so its exit says nothing \
         (status {:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status
    );
    assert!(
        !output.status.success(),
        "allocating inside assert_no_alloc did not kill the child: the no-alloc guard is \
         INERT and `a_busy_block_allocates_nothing` is checking nothing. Likely a second \
         copy of nice-assert-no-alloc (its forbid counter is a thread-local of one copy — \
         keep the dev-dependency pinned to nice-plug's), or a test binary not linking \
         nice-plug.\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );
}

/// Env var that tells a child copy of this binary to be the allocating half
/// of the control case instead of spawning another one.
const CONTROL_ENV: &str = "CHOPDROP_NO_ALLOC_CONTROL";

/// The control case's own test name, for the child's `--exact`.
const CONTROL_TEST: &str = "the_guard_bites_a_deliberate_allocation";

/// Printed by the child before it allocates, so the parent can tell an abort
/// from the guard apart from a child that never started.
const ENTER_MARKER: &str = "chopdrop control case: allocating under the guard";

/// Printed by the child if it survives — i.e. if the guard is inert.
const INERT_MARKER: &str = "chopdrop control case: survived, the guard is inert";

/// The allocating half, run only in the child process: grow a `Vec` by one
/// element inside the guard — the textbook breach of the real-time rule
/// (fixed-capacity buffers on that path).
///
/// `Vec::new` reserves nothing, so the single `push` must call the allocator.
/// `black_box` keeps a release build from deciding the vector is unused.
fn allocate_under_the_guard() {
    let mut events: Vec<TimedEvent> = Vec::new();
    let event = busy_events().remove(0);

    println!("{ENTER_MARKER}");
    std::io::stdout().flush().expect("the marker reaches the parent before the abort");

    assert_no_alloc(|| {
        events.push(std::hint::black_box(event));
        std::hint::black_box(&events);
    });

    // Unreachable while the guard is live: the push above aborts the process.
    println!("{INERT_MARKER}");
}
