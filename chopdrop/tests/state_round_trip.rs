//! A project through the plugin's real persist path, headless.
//!
//! `state.rs` already tests the snapshot as serde code and `lib.rs` already
//! tests [`LanesField`]'s two trait methods by hand. What neither covers is
//! the path a host actually takes: the `#[persist = "lanes"]` key, the
//! `Params` derive's generated save and load either side of it, and the
//! loader finishing the job on its own thread afterwards. That is what this
//! file drives, with the two functions the derive itself calls —
//! `nice_plug::params::persist::serialize_field` and `deserialize_field` —
//! and with the whole generated `serialize_fields`/`deserialize_fields`
//! pair, exactly as bitstep's persistence tests do.
//!
//! **No editor is ever created.** Nothing here imports `chopdrop::editor` or
//! calls `Plugin::editor`: a project restores because the loader is a writer
//! like the GUI is, and a user who opens a Bitwig project with the window
//! closed must still hear their kit. The samples therefore have to arrive
//! with nobody looking, which is the one claim this file exists to make.
//!
//! **The files are gone before the restore.** Both lanes are loaded from
//! scratch copies of the checked-in fixtures, and those copies are deleted
//! before the project is handed back — so a sample that comes back can only
//! have come back from the base64 the project embedded (on load, the lane
//! plays the embedded copy).
//!
//! **Why `ChopdropParams` and not `Chopdrop`.** [`Loader::finish`] is the
//! only honest way to know that the decoding is over (block, do not poll) and
//! it consumes the loader, which a `Chopdrop` keeps private — so the two
//! tests that wait for samples build the same params struct the plugin holds
//! over a model and a loader of their own ([`Instance`]). The real plugin
//! struct is driven too, through the `Arc<dyn Params>` the host gets, for
//! everything that lands while it waits: the lanes, their settings and the
//! switch.
//!
//! The Stretch test waits through [`chopdrop::LanesField::finish`],
//! which drains the loader and then the render worker the params struct
//! built, so a render rebuilt after the restore has landed when it returns.
//!
//! Two more tests close the loop on version 2 with the real plugin struct and
//! [`Chopdrop::finish`]: a project holding chops (with their origins), the
//! chop editor's saved state and every per-lane time field goes out through
//! `serialize_field` and back in through `deserialize_field`; and the
//! checked-in v1 fixture takes the very same path and still behaves like v1.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use nice_plug::params::persist::{deserialize_field, serialize_field, PersistentField};
use nice_plug::prelude::{Params, Plugin};

use chopdrop::chop::{ChopOrigin, Edge, Selection, View};
use chopdrop::engine::{Engine, EngineEvent, NoteTarget, Outputs, TimedEvent};
use chopdrop::loader::{Loader, Source};
use chopdrop::model::{lock_model, LaneMode, Model, SharedModel, CHOKE_GROUPS, DEFAULT_LANE_COUNT};
use chopdrop::state::{LaneSnapshot, Snapshot, CURRENT_VERSION};
use chopdrop::time::{Length, NoteLength, TimeMode, TEMPO_DEFAULT};
use chopdrop::{Chopdrop, ChopdropParams};

mod support;

/// The fixtures the saved project embeds, two real files in two formats
/// (`chopdrop/tests/fixtures/README.md`). Checked in rather than generated
/// because the point here is that the *bytes* survive the round trip, and
/// these are the smallest real ones the tree has.
const FLAC: &[u8] = include_bytes!("fixtures/tiny_mono.flac");
/// The second lane's file: stereo, so the restored audio is compared in two
/// channels and not in one doubled.
const OGG: &[u8] = include_bytes!("fixtures/tiny_stereo.ogg");

/// The host's sample rate. Neither fixture's own (both are 44 100 Hz), so
/// every voice plays through the interpolator at a fractional step and the
/// comparison is of resampled audio, not of a copied buffer.
const HOST_RATE: f64 = 48_000.0;

/// One block, the size Bitwig's default buffer is.
const BLOCK_FRAMES: usize = 512;

/// Blocks each side renders. Twelve covers the longer fixture whole —
/// 4 410 frames at 44 100 Hz played two semitones up at 48 000 Hz is about
/// 4 280 frames — plus the gate release and its fade tail.
const BLOCKS: usize = 12;

/// The note the first loaded lane answers to, as the host sends it: its own
/// channel and the voice id Bitwig would track it by.
///
/// 60 is C3 in Bitwig's convention, which is also
/// [`chopdrop::engine::PAD_ROOT_KEY`] — the saved project has the "bitstep
/// Sample pad" switch **on**, so a lane at the root plays its file at its own
/// pitch and a dozen blocks are enough for it.
const KICK_NOTE: NoteTarget = NoteTarget { channel: 0, key: 60, voice_id: Some(7) };

/// The second loaded lane's note: two semitones up, so keytracking is doing
/// something to its playback step on both sides of the round trip.
const SNARE_NOTE: NoteTarget = NoteTarget { channel: 1, key: 62, voice_id: Some(8) };

/// A plugin instance's state without a plugin: the model, the loader and the
/// params struct that persists them, built the way [`Chopdrop::default`]
/// builds its own — one model, shared.
struct Instance {
    /// The lanes, behind the writer mutex.
    model: SharedModel,
    /// The worker that decodes what a drop or a restore brings.
    loader: Arc<Loader>,
    /// The `#[persist = "lanes"]` half of the plugin's parameters.
    params: ChopdropParams,
}

impl Instance {
    /// A new instance: default lanes, nothing loaded.
    fn fresh() -> Instance {
        Instance::over(Arc::new(Mutex::new(Model::new())))
    }

    /// An instance over lanes that already exist — a fresh worker and a
    /// fresh params struct around the same model, which is all
    /// [`Instance::settle`] needs to rebuild.
    fn over(model: SharedModel) -> Instance {
        let loader = Arc::new(Loader::new(Arc::clone(&model)));
        let params = ChopdropParams::new(Arc::clone(&model), Arc::clone(&loader));
        // What the host does in `activate()` and nothing else does: a
        // smoother starts at 0.0 until it is reset to its parameter's value,
        // and a block rendered without that is a block at a master volume no
        // host ever has (`lib.rs`'s own tests prime it the same way).
        params.volume.smoothed.reset(params.volume.value());
        Instance { model, loader, params }
    }

    /// Wait for every decode this instance has queued, then hand back the
    /// instance with a new worker.
    ///
    /// No sleep and no poll: [`Loader::finish`] closes the job channel and
    /// joins the worker, so when it returns every result has been written
    /// through the writer mutex and published. It consumes the loader,
    /// though, and the params struct holds the other handle on it — hence
    /// dropping the params and building them again over the same model. The
    /// lanes are the model's, not the params struct's, so nothing is lost.
    fn settle(self) -> Instance {
        let Instance { model, loader, params } = self;
        drop(params);
        let Ok(loader) = Arc::try_unwrap(loader) else {
            panic!("the params struct held the only other handle on the loader");
        };
        loader.finish();
        Instance::over(model)
    }
}

/// Hands every [`ScratchFile`] a name of its own.
///
/// The tests here run in parallel threads of one process and all three build
/// the same project, so a name made from the pid and the fixture alone would
/// be the *same* name in all of them: one test's guard would delete the file
/// another was still decoding, and that test would quietly save a project
/// with a sample missing from it.
static SCRATCH_FILES: AtomicU64 = AtomicU64::new(0);

/// A file in the temp directory holding these bytes, removed when the guard
/// drops (`loader.rs`'s and `stress.rs`'s tests do it the same way).
struct ScratchFile {
    path: PathBuf,
}

impl ScratchFile {
    fn new(name: &str, bytes: &[u8]) -> ScratchFile {
        let unique = SCRATCH_FILES.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("chopdrop-round-trip-{}-{unique}-{name}", std::process::id()));
        std::fs::write(&path, bytes).expect("the temp directory is writable");
        ScratchFile { path }
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Load `path` onto `lane` the way a drop does, through the instance's own
/// loader.
fn queue_file(instance: &Instance, lane: u32, path: &Path) {
    let source = Source::File(path.to_path_buf());
    let queued = instance.loader.queue(&mut lock_model(&instance.model), lane, source);
    assert!(queued, "the loader's worker took the job");
}

/// A project no default could produce, with its samples already decoded:
/// three lanes out of the eight, reordered so the empty one is first, every
/// per-lane control moved off its default, two lanes holding real audio and
/// the "bitstep Sample pad" switch on.
///
/// The two files are deleted before this returns — the scratch guards die
/// with this scope — so everything the caller saves and restores afterwards
/// is working from the embedded copies alone.
fn a_filled_project() -> Instance {
    let instance = Instance::fresh();
    let (kick, snare, empty) = {
        let model = lock_model(&instance.model);
        (model.lanes()[0].id, model.lanes()[1].id, model.lanes()[2].id)
    };

    let flac = ScratchFile::new("kick.flac", FLAC);
    let ogg = ScratchFile::new("snare.ogg", OGG);
    queue_file(&instance, kick, &flac.path);
    queue_file(&instance, snare, &ogg.path);
    // Both decodes are finished and published by the time this returns — so
    // the files have done their job and can go, which is what makes every
    // sample the caller restores afterwards an embedded one.
    let instance = instance.settle();
    drop(flac);
    drop(ogg);

    {
        let mut model = lock_model(&instance.model);
        // Said here rather than left to a later assertion on the saved
        // project: a sample that did not decode is a fact about this
        // fixture or this filesystem, not about the persist path.
        for lane in [kick, snare] {
            let loaded = model.lane(lane).expect("the lane is still there");
            assert!(loaded.sample.is_some(), "lane {lane} has no sample: {:?}", loaded.error);
        }

        let spare: Vec<u32> = model.lanes()[3..].iter().map(|lane| lane.id).collect();
        for id in spare {
            assert!(model.delete_lane(id));
        }

        assert!(model.rename(kick, "Kick"));
        assert!(model.set_note(kick, KICK_NOTE.key));
        assert!(model.set_colour(kick, 0x12_34_56));
        assert!(model.set_volume_db(kick, -6.0));
        assert!(model.set_pan(kick, -1.0));
        assert!(model.set_choke(kick, 3));
        assert!(model.set_mode(kick, LaneMode::OneShot));

        assert!(model.rename(snare, "Snare 909"));
        assert!(model.set_note(snare, SNARE_NOTE.key));
        assert!(model.set_colour(snare, 0x65_43_21));
        assert!(model.set_volume_db(snare, 2.5));
        assert!(model.set_pan(snare, 0.5));
        assert!(model.set_choke(snare, 3));
        assert!(model.set_mode(snare, LaneMode::Gate));

        // A lane with nothing in it, to prove an empty lane keeps its
        // settings across a save and is published to nobody.
        assert!(model.rename(empty, "Empty"));
        assert!(model.set_note(empty, 127));
        assert!(model.set_colour(empty, 0x0F_0F_0F));
        assert!(model.set_volume_db(empty, -24.0));
        assert!(model.set_pan(empty, 1.0));
        assert!(model.set_choke(empty, CHOKE_GROUPS));
        assert!(model.set_mode(empty, LaneMode::Gate));

        // Order is the project's, not creation order.
        assert!(model.reorder(2, 0));
        model.set_sample_pad_switch(true);
    }

    instance
}

/// The block script both sides render: a note-on per loaded lane in the
/// first block, on their own channels and with the host's voice ids, and the
/// gate lane's note-off two blocks later so its release and fade are in the
/// comparison too.
fn block_events(block: usize) -> Vec<TimedEvent> {
    match block {
        0 => vec![
            TimedEvent::new(0, EngineEvent::NoteOn { note: KICK_NOTE, velocity: 0.9 }),
            TimedEvent::new(128, EngineEvent::NoteOn { note: SNARE_NOTE, velocity: 0.75 }),
        ],
        2 => vec![TimedEvent::new(64, EngineEvent::NoteOff { note: SNARE_NOTE })],
        _ => Vec::new(),
    }
}

/// Render [`BLOCKS`] blocks of [`block_events`] against what this instance
/// publishes, through the same `Engine` call `process()` makes.
///
/// A fresh engine each time, because the voices are the audio thread's and
/// an instance that has just been loaded has none.
fn render(instance: &Instance) -> (Vec<f32>, Vec<f32>) {
    let published = Arc::clone(lock_model(&instance.model).published());
    let mut engine = Engine::new();
    let mut left = Vec::with_capacity(BLOCKS * BLOCK_FRAMES);
    let mut right = Vec::with_capacity(BLOCKS * BLOCK_FRAMES);
    let mut block_l = [0.0f32; BLOCK_FRAMES];
    let mut block_r = [0.0f32; BLOCK_FRAMES];

    for block in 0..BLOCKS {
        engine.run_block(
            &published,
            &block_events(block),
            &mut Outputs::main(&mut block_l, &mut block_r),
            HOST_RATE,
            instance.params.next_master_gain(BLOCK_FRAMES),
        );
        left.extend_from_slice(&block_l);
        right.extend_from_slice(&block_r);
    }

    (left, right)
}

/// The peak of a rendered channel, so "this really played audio" is a number
/// and not an eyeball.
fn peak(channel: &[f32]) -> f32 {
    channel.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()))
}

/// The lane settings a snapshot holds with the samples left out — what can
/// be compared against an instance whose decoding may still be running.
fn settings(snapshot: &Snapshot) -> Vec<LaneSnapshot> {
    snapshot.lanes.iter().map(|lane| LaneSnapshot { sample: None, ..lane.clone() }).collect()
}

/// The headline test: save a project with embedded samples, lose the
/// files, load it into a fresh instance through the two functions the
/// `Params` derive calls, and find the same settings, the same order, the
/// same samples in the published snapshot and bit-for-bit the same audio —
/// with no editor anywhere in it.
#[test]
fn a_saved_project_restores_its_lanes_samples_and_audio_headless() {
    let saved = a_filled_project();

    // Save exactly as the derive does: `map` over the persistent field,
    // through `serialize_field`.
    let blob = PersistentField::map(&saved.params.lanes, serialize_field)
        .expect("the project serialises");
    assert!(blob.contains("bytes_b64"), "the samples are embedded, not referenced");
    assert!(
        blob.len() > FLAC.len() + OGG.len(),
        "{} bytes of state for {} bytes of audio",
        blob.len(),
        FLAC.len() + OGG.len()
    );

    let saved_audio = render(&saved);
    assert!(peak(&saved_audio.0) > 0.0 && peak(&saved_audio.1) > 0.0, "the kit sounds");

    // Load exactly as the derive does: parse first, hand the result to
    // `set` only if it parsed.
    let parsed = deserialize_field::<Snapshot>(&blob).expect("a saved project parses");
    assert_eq!(parsed.version, CURRENT_VERSION);
    let restored = Instance::fresh();
    PersistentField::set(&restored.params.lanes, parsed);
    let restored = restored.settle();

    // The lanes: same settings, same order, fresh ids, nothing loading and
    // nothing to report.
    {
        let model = lock_model(&restored.model);
        let names: Vec<&str> = model.lanes().iter().map(|lane| lane.name.as_str()).collect();
        assert_eq!(names, ["Empty", "Kick", "Snare 909"]);
        assert!(model.sample_pad_switch(), "the switch is model state, and it was on");
        assert!(
            model.lanes().iter().all(|lane| lane.id >= DEFAULT_LANE_COUNT),
            "a load hands out this instance's own ids, never the file's"
        );
        assert!(model.lanes().iter().all(|lane| !lane.loading));
        assert!(model.lanes().iter().all(|lane| lane.error.is_none()));
        assert!(model.lanes()[0].sample.is_none(), "the empty lane stayed empty");
        assert!(!model.can_undo(), "opening a project is not an edit");
    }

    // And the whole project, settings and embedded bytes together: what this
    // instance would save is what was loaded into it.
    assert_eq!(
        PersistentField::map(&restored.params.lanes, Snapshot::clone),
        PersistentField::map(&saved.params.lanes, Snapshot::clone)
    );

    // The restored samples reached the published snapshot — the only view of
    // the lanes the engine has.
    {
        let saved_lanes = lock_model(&saved.model).published().get();
        let restored_lanes = lock_model(&restored.model).published().get();
        assert_eq!(saved_lanes.lanes().len(), 2, "only the two loaded lanes are published");
        assert_eq!(restored_lanes.lanes().len(), 2);
        assert!(restored_lanes.sample_pad_switch());

        for (before, after) in saved_lanes.lanes().iter().zip(restored_lanes.lanes()) {
            assert_eq!(before.note, after.note);
            assert_eq!(before.choke, after.choke);
            assert_eq!(before.mode, after.mode);
            assert_eq!(before.volume_db(), after.volume_db());
            assert_eq!(before.pan(), after.pan());
            assert_eq!(before.sample.rate, after.sample.rate);
            assert_eq!(before.sample.channels, after.sample.channels);
            assert_eq!(before.sample.frames, after.sample.frames, "the audio itself");
        }
    }

    // The point of all of it: the project plays the same.
    let restored_audio = render(&restored);
    assert_eq!(restored_audio.0, saved_audio.0, "left channel after reload");
    assert_eq!(restored_audio.1, saved_audio.1, "right channel after reload");
}

/// The same trip through the generated `Params` methods rather than the two
/// functions underneath them: the blob the host stores is keyed by the name
/// in `#[persist = "lanes"]` — renaming it would silently lose every saved
/// project — and re-saving the restored instance writes the identical blob,
/// which is only true once the embedded bytes are back on their lanes.
#[test]
fn the_params_derive_saves_the_whole_project_under_the_lanes_key() {
    let saved = a_filled_project();

    let fields = saved.params.serialize_fields();
    assert_eq!(fields.keys().collect::<Vec<&String>>(), vec!["lanes"]);

    let restored = Instance::fresh();
    restored.params.deserialize_fields(&fields);
    let restored = restored.settle();

    assert_eq!(restored.params.serialize_fields(), fields);
}

/// And the real plugin struct, with no editor made: a `Chopdrop` takes a
/// project through the `Arc<dyn Params>` its host holds and saves the same
/// lanes back.
///
/// The samples are left out of the comparison on purpose. `set` publishes
/// the lanes while the host waits and leaves the decoding to the loader, and
/// a `Chopdrop` keeps that loader private — so there is nothing here to wait
/// on and the samples may or may not have landed yet. Everything that lands
/// synchronously is compared; the asynchronous half is what the test above
/// drives to the end.
#[test]
fn a_real_plugin_instance_takes_a_project_with_no_editor() {
    let saved = {
        let project = a_filled_project();
        PersistentField::map(&project.params.lanes, serialize_field).expect("it serialises")
    };
    let expected = deserialize_field::<Snapshot>(&saved).expect("a saved project parses");

    let plugin = Chopdrop::default();
    let params = plugin.params();
    params.deserialize_fields(&BTreeMap::from([(String::from("lanes"), saved)]));

    let written = params.serialize_fields();
    assert_eq!(written.keys().collect::<Vec<&String>>(), vec!["lanes"]);
    let reread = deserialize_field::<Snapshot>(&written["lanes"]).expect("it parses back");

    assert_eq!(reread.version, CURRENT_VERSION);
    assert!(reread.sample_pad_switch);
    assert_eq!(settings(&reread), settings(&expected));
}

/// Renders are never saved, so a project with a Stretch lane has
/// to come back with its render rebuilt — through the derive's own
/// `deserialize_field`, the persistent field's `set`, the loader decoding the
/// embedded sample and the render worker the params struct built over the
/// model's tempo cell. [`chopdrop::LanesField::finish`] waits for the loader
/// and then the worker, so when it returns the render has landed or never
/// will.
#[test]
fn a_saved_stretch_lane_renders_again_after_a_restore() {
    let saved = {
        let project = a_filled_project();
        {
            let mut model = lock_model(&project.model);
            let kick = kick_of(&model).id;
            assert!(model.set_time_mode(kick, TimeMode::Stretch));
            assert!(model.set_length(kick, Length::Beats(NoteLength::Sixteenth)));
        }
        PersistentField::map(&project.params.lanes, serialize_field).expect("it serialises")
    };
    let snapshot = deserialize_field::<Snapshot>(&saved).expect("a saved project parses");
    let saved_kick = snapshot.lanes.iter().find(|lane| lane.name == "Kick").expect("saved");
    assert_eq!(saved_kick.time_mode, "stretch");
    assert_eq!(saved_kick.length, "1/16");

    let model: SharedModel = Arc::new(Mutex::new(Model::new()));
    let loader = Arc::new(Loader::new(Arc::clone(&model)));
    let params = ChopdropParams::new(Arc::clone(&model), loader);
    params.lanes.set(snapshot);
    let model = params.lanes.finish();

    let model = lock_model(&model);
    let kick = kick_of(&model);
    assert_eq!(kick.time_mode, TimeMode::Stretch);
    let chop = kick.sample.as_ref().expect("the embedded sample came back");
    assert!(!kick.render_pending);
    assert_eq!(kick.render_error, None);
    let render = kick.render.as_ref().expect("the render was rebuilt");
    // A sixteenth: short enough that the tiny fixture stays inside the
    // render cap (16× the chop), so the length is the note's, unclamped.
    let secs = 0.25 * 60.0 / TEMPO_DEFAULT;
    let frames = (secs * f64::from(chop.data.rate)).round() as usize;
    assert!(frames < chop.data.frame_count() * 16, "inside the render cap");
    assert_eq!(render.data.frame_count(), frames, "a sixteenth note at the default tempo");
    assert_eq!(render.data.channels, chop.data.channels);

    // The other lanes stayed Off: nothing to render, nothing rendered.
    let others = model.lanes().iter().filter(|lane| lane.id != kick.id);
    assert!(others.clone().count() == 2 && others.into_iter().all(|lane| lane.render.is_none()));
}

/// The kick lane of [`a_filled_project`], found by name: that project
/// reorders its lanes, so its index is not its creation order.
fn kick_of(model: &Model) -> &chopdrop::model::Lane {
    model.lanes().iter().find(|lane| lane.name == "Kick").expect("the kick lane")
}

/// The chop source's rate: not the host's, so the chops are resampled audio.
const SOURCE_RATE: u32 = 44_100;

/// Half a second of source, long enough for two chops and a view that is
/// zoomed in rather than fitted.
const SOURCE_FRAMES: u64 = SOURCE_RATE as u64 / 2;

/// The chop source as a 16-bit mono WAV: a sine with a slow ramp, so every
/// chop's frames differ from its neighbour's.
fn source_wav() -> Vec<u8> {
    let samples: Vec<i16> = (0..SOURCE_FRAMES)
        .map(|n| {
            let ramp = 0.2 + 0.8 * n as f32 / SOURCE_FRAMES as f32;
            ((n as f32 * 0.07).sin() * ramp * 12_000.0) as i16
        })
        .collect();
    support::wav_i16(1, SOURCE_RATE, &samples)
}

/// The saved state of a real plugin, parsed back the way the derive does.
fn saved_snapshot(fields: &BTreeMap<String, String>) -> Snapshot {
    assert_eq!(fields.keys().collect::<Vec<&String>>(), vec!["lanes"]);
    deserialize_field::<Snapshot>(&fields["lanes"]).expect("the saved project parses")
}

/// The lane called `name`, which a restore hands a fresh id.
fn named<'a>(model: &'a Model, name: &str) -> &'a chopdrop::model::Lane {
    model.lanes().iter().find(|lane| lane.name == name).expect("the lane was restored")
}

/// Hand `blob` to a fresh real plugin under the `lanes` key, exactly as a
/// host restoring a project does, then wait for its loader and render
/// worker to run dry and hand back the restored model.
fn restore_into_a_real_plugin(blob: &str) -> SharedModel {
    let plugin = Chopdrop::default();
    let params = plugin.params();
    params.deserialize_fields(&BTreeMap::from([(String::from("lanes"), blob.to_owned())]));
    drop(params);
    plugin.finish()
}

/// A v2 project — two chops with their origins, one of them
/// Stretched to a sixteenth and the other repitched, every per-lane v2
/// setting moved off its default, and a chop editor with its source open, a
/// selection, a zoomed view, snap off and a resized strip — saved through
/// `serialize_field` and restored into a real `Chopdrop` through
/// `deserialize_field`. The chops come back byte for byte with their
/// origins, the time fields as saved, the Stretch lane's render rebuilt,
/// the editor's source reopened by path with its selection and view, and a
/// re-save writes the identical project.
#[test]
fn a_v2_project_with_chops_and_the_chop_editor_round_trips_through_a_real_plugin() {
    let source = ScratchFile::new("Eye.wav", &source_wav());
    let source_path = source.path.to_string_lossy().into_owned();
    let (first_cut, second_cut) = ((4_410, 8_820), (11_025, 15_435));

    let instance = Instance::fresh();
    {
        let mut model = lock_model(&instance.model);
        assert!(instance.loader.open_source(&mut model, &source.path), "the worker took it");
    }
    let instance = instance.settle();
    let (stretched, repitched) = {
        let mut model = lock_model(&instance.model);
        assert!(model.chop().source.is_some(), "the source opened: {:?}", model.chop().error);

        let cut = |(start, end)| Selection { start, end, active: Edge::End };
        let stretched = model.add_chop_lane(cut(first_cut)).expect("the first chop");
        let repitched = model.add_chop_lane(cut(second_cut)).expect("the second chop");

        assert!(model.set_attack_ms(stretched, 12.5));
        assert!(model.set_release_ms(stretched, 80.0));
        assert!(model.set_cut_itself(stretched, false));
        assert!(model.set_output(stretched, 3));
        assert!(model.set_time_mode(stretched, TimeMode::Stretch));
        assert!(model.set_length(stretched, Length::Beats(NoteLength::Sixteenth)));
        assert!(model.set_fine_pct(stretched, -4.0));
        assert!(model.set_pitch_st(stretched, 2.0));

        assert!(model.set_attack_ms(repitched, 3.0));
        assert!(model.set_release_ms(repitched, 1_000.0));
        assert!(model.set_output(repitched, 15));
        assert!(model.set_time_mode(repitched, TimeMode::Repitch));
        assert!(model.set_length(repitched, Length::Beats(NoteLength::Eighth)));
        assert!(model.set_fine_pct(repitched, 7.5));

        // The editor: a third selection, a zoomed-in view, snap off and a
        // collapsed, resized strip — every field the snapshot keeps.
        assert!(model.set_selection(Some(cut((2_000, 6_000)))));
        let width = model.chop().view_width_px;
        model.set_view(View { first: 1_500, frames_per_pixel: 4.0 }, width);
        model.set_snap(false);
        model.set_strip(true, 260.0);
        (stretched, repitched)
    };

    // What the source instance holds, to compare the restored one against.
    let originals: Vec<(String, Vec<u8>, Option<ChopOrigin>)> = {
        let model = lock_model(&instance.model);
        [stretched, repitched]
            .iter()
            .map(|&id| {
                let lane = model.lane(id).expect("the chop lane");
                let sample = lane.sample.as_ref().expect("its chop");
                (lane.name.clone(), sample.bytes.to_vec(), sample.origin.clone())
            })
            .collect()
    };

    let fields = instance.params.serialize_fields();
    let saved = saved_snapshot(&fields);
    assert_eq!(saved.version, CURRENT_VERSION);
    let editor = saved.chop_editor.clone().expect("the editor is saved");
    assert_eq!(editor.source_path, source_path, "by path");
    assert_eq!(editor.selection, Some((2_000, 6_000)));
    assert_eq!((editor.first, editor.frames_per_pixel), (1_500, 4.0));
    assert!(!editor.snap && editor.collapsed);
    assert_eq!(editor.height_px, 260.0);
    let origin_of = |name: &str| {
        let lane = saved.lanes.iter().find(|lane| lane.name == name).expect("the chop is saved");
        let sample = lane.sample.as_ref().expect("with its sample");
        assert!(sample.bytes_b64.is_some(), "{name}: a chop is embedded, never referenced");
        sample.origin.clone().expect("and its origin")
    };
    let (first_name, second_name) = (originals[0].0.as_str(), originals[1].0.as_str());
    assert!(first_name.ends_with("Eye 1") && second_name.ends_with("Eye 2"), "named by source");
    let first = origin_of(first_name);
    let second = origin_of(second_name);
    assert_eq!((first.start_frame, first.end_frame), first_cut);
    assert_eq!((second.start_frame, second.end_frame), second_cut);
    assert!(!fields["lanes"].contains("render"), "renders are never saved");

    // The source file stays: the editor reopens it by path.
    let restored = restore_into_a_real_plugin(&fields["lanes"]);
    let model = lock_model(&restored);
    let names: Vec<&str> = model.lanes().iter().map(|lane| lane.name.as_str()).collect();
    assert_eq!(names.len(), DEFAULT_LANE_COUNT as usize + 2);
    assert_eq!(&names[names.len() - 2..], [first_name, second_name], "the chops, in order");
    assert!(model.lanes().iter().all(|lane| !lane.loading && lane.error.is_none()));
    assert!(!model.can_undo(), "opening a project is not an edit");

    for (name, bytes, origin) in &originals {
        let lane = named(&model, name);
        let sample = lane.sample.as_ref().expect("the embedded chop came back");
        assert_eq!(&sample.bytes.to_vec(), bytes, "{name}: the chop's WAV, byte for byte");
        assert_eq!(&sample.origin, origin, "{name}: where it was cut from");
        let origin = origin.as_ref().expect("a chop has an origin");
        assert_eq!(origin.source_path, source_path);
        let frames = (origin.end_frame - origin.start_frame) as usize;
        assert_eq!(sample.data.frame_count(), frames, "{name}: exactly the cut");
    }

    let eye1 = named(&model, first_name);
    assert_eq!((eye1.attack_ms, eye1.release_ms, eye1.output), (12.5, 80.0, 3));
    assert!(!eye1.cut_itself);
    assert_eq!(eye1.time_mode, TimeMode::Stretch);
    assert_eq!(eye1.length, Length::Beats(NoteLength::Sixteenth));
    assert_eq!((eye1.fine_pct, eye1.pitch_st), (-4.0, 2.0));
    assert!(!eye1.render_pending && eye1.render_error.is_none());
    let render = eye1.render.as_ref().expect("the Stretch chop's render was rebuilt");
    let secs = 0.25 * 60.0 / TEMPO_DEFAULT * (1.0 - 0.04);
    let expected = (secs * f64::from(SOURCE_RATE)).round() as usize;
    let frames = render.data.frame_count();
    assert!(frames.abs_diff(expected) <= 1, "a sixteenth, 4 % short: {frames} vs {expected}");

    let eye2 = named(&model, second_name);
    assert_eq!((eye2.attack_ms, eye2.release_ms, eye2.output), (3.0, 1_000.0, 15));
    assert!(eye2.cut_itself, "a lane made in v2 cuts itself, and that was saved");
    assert_eq!(eye2.time_mode, TimeMode::Repitch);
    assert_eq!(eye2.length, Length::Beats(NoteLength::Eighth));
    assert_eq!((eye2.fine_pct, eye2.pitch_st), (7.5, 0.0));
    assert!(eye2.render.is_none(), "Repitch plays the chop itself");

    // The editor: its source reopened from the path, the saved selection
    // and view applied to it, snap and the strip as they were.
    let editor = model.chop();
    assert_eq!(editor.source_path.as_deref(), Some(source_path.as_str()));
    assert!(!editor.loading && editor.error.is_none());
    let opened = editor.source.as_ref().expect("the source reopened by path");
    assert_eq!(opened.total(), SOURCE_FRAMES);
    let selection = editor.selection.expect("the saved selection");
    assert_eq!((selection.start, selection.end), (2_000, 6_000));
    assert_eq!(editor.view, View { first: 1_500, frames_per_pixel: 4.0 });
    assert!(!editor.snap && editor.collapsed);
    assert_eq!(editor.height_px, 260.0);

    // And the whole project: a re-save is the same project, lanes, chops,
    // origins, time fields and editor together.
    assert_eq!(model.snapshot(), saved);
    assert_eq!(serialize_field(&model.snapshot()).expect("it serialises"), fields["lanes"]);
}

/// The checked-in v1 project through the same real-plugin path —
/// `deserialize_field` into a `Chopdrop` — loads with every v1 behaviour:
/// its own settings, its embedded kick decoded, no lane cutting itself and
/// every v2 field at its v1 value. A re-save writes a version 2 project that
/// keeps all of that, so the project keeps sounding like v1 from then on.
#[test]
fn the_v1_fixture_restores_through_a_real_plugin_with_v1_behaviour() {
    const V1_PROJECT: &str = include_str!("fixtures/v1_project.json");
    let parsed = deserialize_field::<Snapshot>(V1_PROJECT).expect("the v1 fixture parses");
    assert_eq!(parsed.version, 1, "the fixture is a v1 file");
    assert_eq!(parsed.chop_editor, None, "v1 had no chop editor");

    let restored = restore_into_a_real_plugin(V1_PROJECT);
    let model = lock_model(&restored);
    assert!(model.sample_pad_switch());
    let lanes = model.lanes();
    assert_eq!(lanes.len(), 2);
    let (kick, hat) = (&lanes[0], &lanes[1]);
    assert_eq!((kick.name.as_str(), kick.note, kick.colour), ("Kick", 36, 0xE2_6E_6E));
    assert_eq!((kick.volume_db, kick.pan, kick.choke), (-3.0, -0.25, 1));
    assert_eq!(kick.mode, LaneMode::OneShot);
    let sample = kick.sample.as_ref().expect("the embedded kick was decoded");
    assert_eq!(sample.origin, None, "a v1 sample is no chop");
    assert_eq!((sample.data.rate, sample.data.frame_count()), (44_100, 6));
    assert!(!kick.loading && kick.error.is_none());
    assert_eq!((hat.name.as_str(), hat.note, hat.mode), ("Open hat", 46, LaneMode::Gate));
    assert!(hat.sample.is_none() && hat.error.is_none());

    for lane in lanes {
        assert!(!lane.cut_itself, "{}: v1 lanes never cut themselves", lane.name);
        assert_eq!((lane.attack_ms, lane.release_ms), (0.0, 0.0), "{}: v1 fade", lane.name);
        assert_eq!(lane.output, 0, "{}: Out 1", lane.name);
        assert_eq!(lane.time_mode, TimeMode::Off, "{}", lane.name);
        assert_eq!(lane.length, Length::AsCut, "{}", lane.name);
        assert_eq!((lane.fine_pct, lane.pitch_st), (0.0, 0.0), "{}", lane.name);
        assert!(lane.render.is_none(), "{}: nothing to render", lane.name);
    }
    let editor = model.chop();
    assert!(editor.source_path.is_none() && !editor.loading, "a fresh chop editor");
    assert!(!model.can_undo(), "opening a project is not an edit");

    // A re-save is a v2 project that still loads as this one did.
    let resaved = model.snapshot();
    assert_eq!(resaved.version, CURRENT_VERSION);
    assert_eq!(resaved.chop_editor, None, "an untouched editor saves nothing");
    assert!(resaved.lanes.iter().all(|lane| {
        !lane.cut_itself && lane.release_ms == 0.0 && lane.time_mode == "off"
    }));
    let kick_bytes = |snapshot: &Snapshot| {
        snapshot.lanes[0].sample.as_ref().and_then(|sample| sample.bytes_b64.clone())
    };
    assert_eq!(kick_bytes(&resaved), kick_bytes(&parsed), "the kick's bytes, unchanged");
}
