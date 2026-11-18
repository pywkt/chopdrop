//! The finished plugin, assembled once and read the way a host reads it.
//!
//! Every piece of the shell already has a test inside `lib.rs`, where the
//! struct's private fields are in reach — and that reach is the gap this
//! file exists to close. Those tests may build a [`chopdrop::ChopdropParams`]
//! over a model of their own, touch `plugin.params` directly and call
//! `editor::create` by hand, so a shell that assembled the wrong handles
//! into the wrong places could still pass all of them.
//!
//! Nothing here has that reach. This is a dependent crate holding exactly
//! what a host holds — a [`Chopdrop`], the `Arc<dyn Params>` it hands out and
//! the `Editor` it builds — and it asserts the whole composition through
//! them: an instrument with one stereo output, notes in and none out, the
//! one host parameter (the master volume, in decibels, starting at unity),
//! the "bitstep Sample pad" switch in the saved state rather than in that
//! parameter list, a default project of the pinned eight lanes, and an
//! editor over *this instance's* parameters.
//!
//! Display-free, like `lib.rs`'s own editor test: `Plugin::editor` only
//! assembles the editor and the window is not created until the host calls
//! `Editor::spawn` — what the window then looks like is for the user to
//! check in Bitwig (the walkthrough in `tests/feature_walkthrough.rs`).
//!
//! One `#[test]` for the declarations and the editor, because the claim is
//! about the composition and not about any one part of it: a `Chopdrop` is
//! instantiated once and everything is asked of that instance. A second asks
//! the same of the instance's background writers: a project with a Stretch
//! lane, handed to a fresh instance, comes back with its render rebuilt by
//! the instance's own loader and render worker over its own tempo cell —
//! nothing in the test builds either. A third checks the v2 composition
//! directly: the instance's loader, previewer, render worker and tempo cell
//! are one set over one model, shared with the engine, and the very editor a
//! host gets draws — headless, in a test `egui::Context` — the chop strip,
//! and the dock with the lane panel in it once a lane is clicked.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::Arc;

use nice_plug::editor::dpi::{LogicalSize, NativeSize};
use nice_plug::params::persist::{deserialize_field, serialize_field};
use nice_plug::prelude::{AsyncExecutor, Editor, MidiConfig, Plugin, ResizeHint};
use nice_plug::util::gain_to_db;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;

use chopdrop::editor::dock::{self, Tab};
use chopdrop::editor::{chop_strip, lane_panel, theme, ChopdropEditor, Drawn};
use chopdrop::editor::{DEFAULT_HEIGHT, DEFAULT_WIDTH, MIN_HEIGHT, MIN_WIDTH};
use chopdrop::model::{
    lock_model, CHOKE_NONE, DEFAULT_FIRST_NOTE, DEFAULT_LANE_COUNT, LANE_PAN_DEFAULT,
    LANE_VOLUME_DB_DEFAULT, SAMPLE_PAD_SWITCH_DEFAULT,
};
use chopdrop::state::{LaneSnapshot, SampleSnapshot, Snapshot, CURRENT_VERSION, LANE_PALETTE};
use chopdrop::chop::STRIP_HEIGHT;
use chopdrop::time::{TimeMode, TEMPO_DEFAULT};
use chopdrop::{Chopdrop, VOLUME_DEFAULT_DB, VOLUME_MAX_DB, VOLUME_MIN_DB};

mod support;

/// How one-shot is spelled in a project file. `state.rs` keeps its own
/// constant private, so the word is pinned here from the outside as well:
/// it is in every saved Bitwig project and a later version may not rename
/// it.
const MODE_ONE_SHOT: &str = "one_shot";

/// The sixteen lanes a fresh instance has, as saving them writes them out.
///
/// Spelled out from the model's own constants rather than from
/// `LaneSnapshot::default()`: the default impl is a field-by-field fallback
/// for a file that left something out, and comparing one default against
/// another would agree however both drifted.
fn pinned_default_lanes() -> Vec<LaneSnapshot> {
    (0..DEFAULT_LANE_COUNT as usize)
        .map(|index| LaneSnapshot {
            name: format!("Lane {}", index + 1),
            note: DEFAULT_FIRST_NOTE + index as u8,
            colour: LANE_PALETTE[index % LANE_PALETTE.len()],
            volume_db: LANE_VOLUME_DB_DEFAULT,
            pan: LANE_PAN_DEFAULT,
            choke: CHOKE_NONE,
            mode: String::from(MODE_ONE_SHOT),
            sample: None,
            // A fresh instance's lanes are v2 lanes: they cut themselves.
            attack_ms: 0.0,
            release_ms: 300.0,
            cut_itself: true,
            output: 0,
            time_mode: String::from("off"),
            length: String::from("as_cut"),
            fine_pct: 0.0,
            pitch_st: 0.0,
        })
        .collect()
}

/// Decibels either side of a gain conversion, which rounds: the pin is on
/// the dB figure a user reads, not on the last bit of a `powf` (`lib.rs`'s
/// tests compare the same way).
fn assert_db_near(actual: f32, expected: f32) {
    assert!((actual - expected).abs() < 0.01, "{actual} dB, expected about {expected} dB");
}

/// The whole of what a host meets, off one instance of the real plugin
/// struct.
#[test]
fn the_real_plugin_instance_is_wired_up_as_the_host_expects() {
    // The declarations first, since they are what a host reads before it
    // instantiates anything. An instrument, not a note effect: no audio
    // input, a stereo main output plus 15 stereo aux outputs, and exactly
    // one layout because the host picks the first.
    assert_eq!(Chopdrop::AUDIO_IO_LAYOUTS.len(), 1, "one layout, the one the host picks");
    let layout = Chopdrop::AUDIO_IO_LAYOUTS[0];
    assert_eq!(layout.main_input_channels, None, "an instrument takes no audio in");
    assert_eq!(layout.main_output_channels, NonZeroU32::new(2), "one stereo output");
    assert!(layout.aux_input_ports.is_empty());
    assert_eq!(layout.aux_output_ports.len(), 15, "Out 2 to Out 16");
    assert!(layout.aux_output_ports.iter().all(|&port| port == NonZeroU32::new(2).unwrap()));
    assert_eq!(layout.names.main_output, Some("Out 1"));
    let aux_names: Vec<String> = (2..=16).map(|n| format!("Out {n}")).collect();
    assert_eq!(layout.names.aux_outputs, aux_names, "each aux port named by its number");

    // Notes in, nothing out. `Basic` carries note on/off, choke and the
    // per-note expressions bitstep sends, and is also the level nice-plug
    // requires before it passes a `VoiceTerminated` back to the host.
    assert_eq!(Chopdrop::MIDI_INPUT, MidiConfig::Basic);
    assert_eq!(Chopdrop::MIDI_OUTPUT, MidiConfig::None, "chopdrop emits no notes");

    let mut plugin = Chopdrop::default();
    let params = plugin.params();

    // The host-visible parameter list: the master volume alone, at the top
    // level, under the id saved projects and automation lanes key on.
    let exposed = params.param_map();
    let ids: Vec<&str> = exposed.iter().map(|(id, _, _)| id.as_str()).collect();
    assert_eq!(ids, ["volume"], "v1 exposes the master volume and nothing else");
    assert!(exposed.iter().all(|(_, _, group)| group.is_empty()), "and in no parameter group");

    // And what the host sees of it, through the type-erased pointer it is
    // given: a continuous gain named in decibels, unity where it starts and
    // −60 dB to +12 dB end to end.
    //
    // SAFETY: the pointer points into `params`, the `Arc<dyn Params>` this
    // test holds for the whole of its body — which is the contract a host
    // wrapper keeps for the life of the instance.
    let volume = exposed[0].1;
    unsafe {
        assert_eq!(volume.name(), "Volume");
        assert_eq!(volume.unit(), " dB");
        assert_eq!(volume.step_count(), None, "a continuous gain, not a stepped control");
        assert_eq!(volume.default_plain_value(), 1.0, "unity gain, not a dB number");
        assert_db_near(gain_to_db(volume.default_plain_value()), VOLUME_DEFAULT_DB);
        assert_eq!(
            volume.normalized_value_to_string(volume.default_normalized_value(), true),
            "0.00 dB",
            "the host reads the gain back in decibels"
        );
        assert_db_near(gain_to_db(volume.preview_plain(0.0)), VOLUME_MIN_DB);
        assert_db_near(gain_to_db(volume.preview_plain(1.0)), VOLUME_MAX_DB);
    }

    // The state beside it: one `#[persist = "lanes"]` blob, holding the
    // whole project. The "bitstep Sample pad" switch lives in there and not
    // in the parameter list above — it is instance state, not automation.
    let fields = params.serialize_fields();
    assert_eq!(fields.keys().collect::<Vec<&String>>(), vec!["lanes"]);
    let blob = &fields["lanes"];

    let saved: serde_json::Value = serde_json::from_str(blob).expect("the saved state is JSON");
    let mut keys: Vec<&str> =
        saved.as_object().expect("a JSON object").keys().map(String::as_str).collect();
    keys.sort_unstable();
    let whole = ["chop_editor", "lanes", "sample_pad_switch", "version"];
    assert_eq!(keys, whole, "the whole of what is saved");
    assert!(saved["chop_editor"].is_null(), "an untouched chop editor saves nothing");
    assert_eq!(
        saved["sample_pad_switch"].as_bool(),
        Some(SAMPLE_PAD_SWITCH_DEFAULT),
        "the switch is saved by name, and starts off"
    );

    // The default project itself, parsed back the way a load parses it: the
    // pinned eight lanes, in order, each on its own note and palette colour.
    let snapshot = deserialize_field::<Snapshot>(blob).expect("a saved project parses");
    assert_eq!(snapshot.version, CURRENT_VERSION);
    assert_eq!(snapshot.sample_pad_switch, SAMPLE_PAD_SWITCH_DEFAULT);
    assert_eq!(snapshot.lanes.len(), DEFAULT_LANE_COUNT as usize, "eight lanes to fill");
    assert_eq!(snapshot.lanes, pinned_default_lanes());

    // Last, the GUI. The host asking for one gets one — and it is over
    // *this* instance's parameters, which is what makes the window's
    // gestures reach the lanes the audio thread plays and the blob above
    // saves. The editor holds no model of its own to compare, so the handle
    // count on the params the host was given is what says so: assembling
    // the editor takes a clone of that very `Arc`, and dropping it gives it
    // back.
    let handles = Arc::strong_count(&params);
    let executor = AsyncExecutor::new(Arc::new(|()| {}), Arc::new(|()| {}));
    let editor = plugin.editor(executor).expect("the host always gets an editor");
    assert_eq!(
        Arc::strong_count(&params),
        handles + 1,
        "the editor holds the instance's own parameters, not a fresh set"
    );

    // The window it will open: the default size, resizable, and no smaller
    // than one readable lane row. Logical points at scale 1.0, since no host
    // has told it a scale factor yet.
    assert_eq!(
        editor.size(),
        NativeSize::new(DEFAULT_WIDTH.round() as u32, DEFAULT_HEIGHT.round() as u32)
    );
    assert_eq!(
        editor.resize_hint(),
        ResizeHint::RESIZABLE.with_min_logical_size(LogicalSize::new(MIN_WIDTH, MIN_HEIGHT))
    );

    drop(editor);
    assert_eq!(Arc::strong_count(&params), handles, "and gives them back when it closes");
}

/// The rate of the Stretch lane's embedded sample.
const STRETCH_RATE: u32 = 44_100;

/// A saved project of one Stretch lane: a quarter-second 16-bit WAV, embedded,
/// stretched to a quarter note — half a second at the default tempo.
fn a_stretch_project() -> String {
    let samples: Vec<i16> = (0..STRETCH_RATE / 4)
        .map(|n| ((n as f32 * 0.05).sin() * 8_000.0) as i16)
        .collect();
    let wav = support::wav_i16(1, STRETCH_RATE, &samples);
    let lane = LaneSnapshot {
        name: String::from("Loop"),
        sample: Some(SampleSnapshot {
            path: String::from("/nowhere/loop.wav"),
            file_name: String::from("loop.wav"),
            bytes_b64: Some(STANDARD.encode(&wav)),
            origin: None,
        }),
        time_mode: String::from("stretch"),
        length: String::from("1/4"),
        ..LaneSnapshot::default()
    };
    let project = Snapshot {
        version: CURRENT_VERSION,
        sample_pad_switch: false,
        lanes: vec![lane],
        chop_editor: None,
    };
    serialize_field(&project).expect("the project serialises")
}

/// The real instance rebuilds a restored Stretch lane's render
/// by itself — the loader decodes the embedded sample, which requests the
/// render, and the instance's render worker renders it at the instance's
/// tempo. [`Chopdrop::finish`] waits for both, in that order, and fails if
/// either was never built.
#[test]
fn a_restored_stretch_lane_gets_its_render_from_the_instance() {
    let plugin = Chopdrop::default();
    let params = plugin.params();
    params.deserialize_fields(&BTreeMap::from([(String::from("lanes"), a_stretch_project())]));
    drop(params);

    let model = plugin.finish();
    let model = lock_model(&model);
    let lane = &model.lanes()[0];
    assert_eq!(lane.time_mode, TimeMode::Stretch);
    assert!(lane.sample.is_some(), "the embedded sample was decoded");
    assert!(!lane.render_pending, "nothing left to render");
    assert_eq!(lane.render_error, None);
    let render = lane.render.as_ref().expect("the restore rebuilt the render");
    assert_eq!(render.generation, lane.render_generation, "the newest request answered");

    // A quarter note at the tempo the instance's one cell holds (no block has
    // run, so the default): half a second of audio at the sample's rate.
    let secs = 60.0 / TEMPO_DEFAULT;
    let frames = (secs * f64::from(STRETCH_RATE)).round() as usize;
    assert_eq!(render.data.frame_count(), frames);
    assert!(render.data.frames.iter().any(|s| s.abs() > 0.01), "a render, not silence");
}

/// One headless frame of `editor` in a window of the default size: what it
/// laid out, and the texts it painted.
fn frame(
    ctx: &egui::Context,
    editor: &mut ChopdropEditor,
    events: Vec<egui::Event>,
) -> (Drawn, Vec<String>) {
    let size = egui::vec2(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        events,
        ..egui::RawInput::default()
    };
    let mut drawn = None;
    let output = ctx.run_ui(input, |ui| drawn = Some(editor.draw(ui)));
    let texts = output
        .shapes
        .iter()
        .filter_map(|clipped| match &clipped.shape {
            egui::Shape::Text(text) => Some(text.galley.text().to_string()),
            _ => None,
        })
        .collect();
    output.drop_without_applying_deltas();
    (drawn.expect("the editor draws a frame"), texts)
}

/// A primary click at `pos`: the move, the press and the release.
fn click_at(pos: egui::Pos2) -> Vec<egui::Event> {
    let button = |pressed| egui::Event::PointerButton {
        pos,
        button: egui::PointerButton::Primary,
        pressed,
        modifiers: egui::Modifiers::NONE,
    };
    vec![egui::Event::PointerMoved(pos), button(true), button(false)]
}

/// The v2 handles of one real instance are one set — the loader,
/// render worker and previewer over its one model, the engine reading that
/// model's tempo cell and playing the previewer's channel — and the editor
/// the instance builds draws the chop strip, the dock and the lane panel.
#[test]
fn the_instance_wires_its_v2_workers_and_the_editor_draws_strip_dock_and_panel() {
    let mut plugin = Chopdrop::default();

    {
        let wiring = plugin.wiring();
        let model = lock_model(wiring.model);
        assert!(
            Arc::ptr_eq(wiring.published, model.published()),
            "the audio thread reads the model's own published cell"
        );
        assert!(
            Arc::ptr_eq(wiring.engine_tempo, model.tempo()),
            "the engine reads the model's one tempo cell, not a cell of its own"
        );
        let preview = wiring.engine_preview.expect("the engine plays previews");
        assert!(
            Arc::ptr_eq(preview, wiring.previewer.channel()),
            "the engine plays the channel the instance's previewer publishes into"
        );
        drop(model);

        // The tempo cell is one cell: what `process()` stores, the model
        // (and so the render worker, built over the model's cell) reads.
        wiring.engine_tempo.set(133.0);
        assert_eq!(lock_model(wiring.model).tempo().get(), 133.0);
        wiring.engine_tempo.set(TEMPO_DEFAULT);

        // The loader and the render worker are the instance's own and are
        // held once each (by the lanes field); `finish` below proves they
        // run dry over this model, as the 17.6 test proves they render.
        assert_eq!(Arc::strong_count(wiring.loader), 1, "one loader, held by the lanes field");
        assert_eq!(Arc::strong_count(wiring.render), 1, "one render worker, likewise");
        assert_eq!(wiring.render.render_count(), 0, "nothing rendered for a fresh instance");
    }

    // The editor the host would get, without its window.
    let mut editor = plugin.headless_editor();
    let ctx = egui::Context::default();

    // The first frame: the chop strip under the top bar, open at its
    // default height and showing its empty hint; the dock closed; the
    // eight lane rows.
    let (drawn, texts) = frame(&ctx, &mut editor, Vec::new());
    let strip_height = drawn.strip.height();
    assert!(
        strip_height >= STRIP_HEIGHT,
        "the strip is drawn open, not collapsed to its header: {strip_height}"
    );
    assert!(texts.iter().any(|t| t == chop_strip::EMPTY_HINT), "the strip shows its hint");
    assert_eq!(drawn.dock, None, "a window opens with the dock closed");
    assert_eq!(drawn.names.len(), DEFAULT_LANE_COUNT as usize, "a row per lane");
    assert!(!texts.iter().any(|t| t == lane_panel::ATTACK_LABEL), "no lane panel yet");

    // A click on the first lane's name selects it and opens the dock on the
    // lane tab; the next frame draws the tab row and the lane panel in it,
    // over that lane.
    let (first, name) = drawn.names[0];
    frame(&ctx, &mut editor, click_at(name.center()));
    let (drawn, texts) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.selected, [first], "the click selected the lane");
    assert_eq!(drawn.dock, Some(Tab::Lane), "and opened the dock on the lane tab");
    for label in [dock::LANE_TAB, dock::BROWSE_TAB] {
        assert!(texts.iter().any(|t| t == label), "the dock's tab row shows {label:?}");
    }
    for label in [
        lane_panel::ATTACK_LABEL,
        lane_panel::RELEASE_LABEL,
        lane_panel::CUT_ITSELF_LABEL,
        lane_panel::OUTPUT_LABEL,
        lane_panel::TIME_LABEL,
    ] {
        assert!(texts.iter().any(|t| t == label), "the lane panel shows {label:?}: {texts:?}");
    }
    assert!(
        texts.iter().any(|t| t == chop_strip::EMPTY_HINT),
        "the strip stays drawn beside the dock"
    );

    // The editor holds the instance's parameters; letting it go hands the
    // instance back whole, and its workers run dry over its model.
    drop(editor);
    let model = plugin.finish();
    assert_eq!(lock_model(&model).lanes().len(), DEFAULT_LANE_COUNT as usize);
}

/// A point on the sample cell of the row whose name cell is `name`: just
/// right of the name, where the sample column starts.
fn beside(name: egui::Rect) -> egui::Pos2 {
    egui::pos2(name.max.x + theme::HEADER_FIELD_GAP + 30.0, name.center().y)
}

/// Where `label` was painted in the last frame `editor` drew: a second
/// frame with no input, read for its text shapes' rects.
fn painted_at(ctx: &egui::Context, editor: &mut ChopdropEditor, label: &str) -> egui::Rect {
    let size = egui::vec2(DEFAULT_WIDTH, DEFAULT_HEIGHT);
    let input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..egui::RawInput::default()
    };
    let output = ctx.run_ui(input, |ui| drop(editor.draw(ui)));
    let rect = output.shapes.iter().find_map(|clipped| match &clipped.shape {
        egui::Shape::Text(text) if text.galley.text() == label => {
            Some(text.galley.rect.translate(text.pos.to_vec2()))
        }
        _ => None,
    });
    output.drop_without_applying_deltas();
    rect.unwrap_or_else(|| panic!("{label:?} was painted"))
}

/// A click on a lane's sample cell opens the browser on that lane *and*
/// selects it, so the lane tab one click away shows that lane's settings
/// rather than the "click a lane name" hint (the user's report,
/// 2026-11-16: the browser's outline looked like a selection, and the lane
/// tab came up empty).
#[test]
fn a_sample_click_selects_its_lane_for_the_lane_tab() {
    let mut plugin = Chopdrop::default();
    let mut editor = plugin.headless_editor();
    let ctx = egui::Context::default();
    let (drawn, _) = frame(&ctx, &mut editor, Vec::new());

    // The sample cell is the line under the name.
    let (second, name) = drawn.names[1];
    let sample = beside(name);
    frame(&ctx, &mut editor, click_at(sample));
    let (drawn, _) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.dock, Some(Tab::Browse), "the sample click opened the browser");
    assert_eq!(drawn.selected, [second], "and selected its lane");

    let tab = painted_at(&ctx, &mut editor, dock::LANE_TAB);
    frame(&ctx, &mut editor, click_at(tab.center()));
    let (drawn, texts) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.dock, Some(Tab::Lane), "the tab click switched to the lane tab");
    assert_eq!(drawn.selected, [second], "with the lane still selected");
    assert!(texts.iter().any(|t| t == lane_panel::ATTACK_LABEL), "its settings show");
    assert!(!texts.iter().any(|t| t == lane_panel::HINT), "not the empty hint");

    // A sample click on a lane already among several selected keeps them.
    let (third, name) = drawn.names[2];
    let held = |modifiers| vec![egui::Event::ModifiersChanged(modifiers)];
    frame(&ctx, &mut editor, held(egui::Modifiers::SHIFT));
    frame(&ctx, &mut editor, click_at(name.center()));
    frame(&ctx, &mut editor, held(egui::Modifiers::NONE));
    let (drawn, _) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.selected, [second, third]);
    frame(&ctx, &mut editor, click_at(beside(name)));
    let (drawn, _) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.dock, Some(Tab::Browse));
    assert_eq!(drawn.selected, [second, third], "the selection it was part of stays");

    drop(editor);
    plugin.finish();
}

/// A click on the sample cell of a lane that already has a sample shows
/// that lane's settings, not the browser (the user's, 2026-11-16) — the
/// browser pointed at it behind them, one tab click away. An empty lane's
/// sample cell still opens the browser.
#[test]
fn a_loaded_lanes_sample_click_shows_its_settings() {
    let mut plugin = Chopdrop::default();
    let params = plugin.params();
    params.deserialize_fields(&BTreeMap::from([(String::from("lanes"), a_stretch_project())]));
    drop(params);
    // The loader decodes the embedded sample in the background.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while lock_model(plugin.wiring().model).lanes()[0].sample.is_none() {
        assert!(std::time::Instant::now() < deadline, "the sample decoded");
        std::thread::yield_now();
    }

    let mut editor = plugin.headless_editor();
    let ctx = egui::Context::default();
    let (drawn, _) = frame(&ctx, &mut editor, Vec::new());
    let (loaded, name) = drawn.names[0];
    frame(&ctx, &mut editor, click_at(beside(name)));
    let (drawn, texts) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.dock, Some(Tab::Lane), "the loaded lane's settings, not the browser");
    assert_eq!(drawn.selected, [loaded]);
    assert!(texts.iter().any(|t| t == lane_panel::ATTACK_LABEL), "its settings show");

    // The browse tab is then the browser for that lane.
    let tab = painted_at(&ctx, &mut editor, dock::BROWSE_TAB);
    frame(&ctx, &mut editor, click_at(tab.center()));
    let (drawn, texts) = frame(&ctx, &mut editor, Vec::new());
    assert_eq!(drawn.dock, Some(Tab::Browse));
    assert!(!texts.iter().any(|t| t == dock::BROWSE_HINT), "a browser, not the hint");

    drop(editor);
    plugin.finish();
}
