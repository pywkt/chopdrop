//! The egui editor: a top bar and a scrolling list of lanes.
//!
//! Ten modules live here. [`top_bar`] is the strip across the top: the
//! three controls that belong to the instance rather than to a lane, and
//! the one warning about its size.
//! [`lanes`] is
//! the window's middle: the model read out as a row per lane, with the
//! geometry and the field formatting that takes, and [`menu`] is the
//! right-click menu over a row. Three modules hold the parts of that read
//! with nothing of egui's layout in them: [`notes`] formats and parses a
//! note number as the name Bitwig would give it, [`activity`] turns the
//! counters the audio thread leaves on the published lanes into how
//! brightly each row's dot is lit this frame, and [`status`] decides which
//! of the four things a lane could say about its sample it is saying —
//! including both of the window's warnings. [`theme`] is
//! the look — bitstep's `editor/theme.rs` trimmed to this plugin, which is
//! why the two read as a pair when they sit side by side in one project.
//! [`keys`] is the window around the editor: the keys the editor does not use
//! are passed on to Bitwig, and Bitwig's own plugin window is unpinned from
//! its main window. [`history`] is what becomes of the two keys it keeps back
//! — Ctrl+Z and Ctrl+Shift+Z, read out of a frame's events and taken as steps
//! through the model's own undo stacks. [`drops`] is what arrives through the
//! window: the files dragged onto it, read out of egui's input as an ordered
//! list of paths. The rest of this file is the window itself: the
//! [`NiceEguiApp`] nice-plug-egui calls back into, the size it opens at, and
//! [`create`], which the plugin's `editor()` hands to the host.
//!
//! # What is drawn yet
//!
//! The top bar ([`top_bar`]) and the lane list ([`lanes`]) — and, under no
//! lanes at all, the big hint. The bar's three controls answer: the master
//! volume drives the host parameter, the switch and `+ lane` write the
//! model. In a row, seven fields answer: the four number fields — the note,
//! under Bitwig's name for it and through [`notes`], and the volume, the pan
//! and the choke group — each of them dragged, scrolled a notch or clicked
//! and typed into; the one-shot/gate toggle, which a click swaps; the name,
//! which a double click turns into a text box (the one thing in the window
//! this struct holds state for); and the grip, which carries its row to
//! another place in the list (display order only); and the whole row
//! answers a secondary click with its menu — rename, colour, duplicate,
//! delete, reload from file, clear sample ([`menu`]). All of them write
//! through the model's writer API.
//!
//! The sample column is the one field that only ever reads: the file's name,
//! the hint, "loading…" while the background worker is at a file, or the
//! message whatever went wrong last left on the lane, chosen by [`status`]
//! and drawn with the hover the long ones need. Its two warnings are the same
//! read twice over — a sample the project is too small to carry is marked in
//! its row, and a project carrying more than it comfortably can is said in
//! the bar. A lane waiting for the loader is also the one thing in this
//! window that changes with nobody touching it, so the loader's worker rings
//! the repaint hook [`create`] installed as each result lands.
//!
//! The row's last two columns are the ones that answer to the audio thread
//! instead. The colour dot is the lane's activity light: this file reads the
//! published snapshot's counters once a frame and hands them to
//! [`activity::Lights`], which keeps the time each of them last moved, and
//! the list draws the fade that comes back out ([`theme::activity_dot`]).
//! While any of them is still fading the window asks for the next frame
//! itself; while anything is *sounding* the audio thread asks for it
//! (`crate::Chopdrop::process` rings the repaint notifier), which is what
//! lights a dot in a window nobody is touching — and between the two an idle
//! editor still costs nothing. The audition button goes the other way: a
//! click on it leaves the list as [`lanes::LaneList::audition`] and is pushed
//! onto the engine's ring here, best-effort, because the ring is small and
//! lock-free and a click is not worth making an audio block wait.
//!
//! The drops are read every frame ([`drops`]), because a hover and a drop
//! each live for the one frame they arrive in, and answered against the rows
//! the same frame drew: a drag is outlined over the row it would land on, and
//! a drop puts its first file on that row and a lane under the list for each
//! file after it, every one of them handed to the background loader. That is
//! the path a manual check in Bitwig proved reaches this window at all
//! (through the patches in `vendor/UPSTREAMING.md`), end to end. A drop with
//! more files than the list has room left for is the one gesture in this
//! window that cannot do all of what it was asked, so it says so: the files
//! it could not place are counted and the bar shows how many for a few
//! seconds ([`drops::DropIntake::message`]).
//!
//! The keys were wired before any of this, because they are about the window
//! and not about what is in it: Space reached Bitwig's transport while the
//! window still held nothing but the hint. Undo and redo were held back
//! from the host from the start (`editor/keys.rs`) and now drive chopdrop's
//! own history instead ([`history`]): they are the one thing in this window
//! that changes a lane without anything being clicked, and the steps they
//! take are over the gestures every module above has already committed as it
//! applied them.
//!
//! Under the bar, full width, sits the chop editor strip ([`chop_strip`],
//! v2): its header, collapse and resize (kept in the model's chop editor
//! state) and its empty, loading and failed states so far.
//!
//! Everything the editor will do to a lane goes through the model's writer
//! API; the editor holds no truth of its own.

pub mod activity;
pub mod browser_panel;
pub mod chop_strip;
pub mod closing;
pub mod dock;
pub mod drops;
pub mod focus;
pub mod history;
pub mod keys;
pub mod lane_panel;
pub mod lanes;
pub mod menu;
pub mod notes;
pub mod selection;
pub mod status;
pub mod theme;
pub mod top_bar;

use std::sync::Arc;

use nice_plug::context::gui::GuiContext;
use nice_plug::editor::ResizeHint;
use nice_plug::editor::dpi::LogicalSize;
use nice_plug_egui::baseview::HandlerError;
use nice_plug_egui::{
    EguiEditor, EguiEditorState, EguiNiceSettings, Frame, NiceEguiApp, RepaintNotifier,
    create_egui_editor,
};

use crate::ChopdropParams;
use crate::engine::AuditionRing;
use crate::preview::{PreviewStatus, Previewer};
use crate::loader;
use crate::model::{Model, lock_model};

/// What the window manager calls the window. Only ever seen when the window
/// is floated off Bitwig's own frame.
pub const WINDOW_TITLE: &str = "chopdrop";

/// The width the window opens at, in logical points: 1240, the width of the
/// window the user had dragged it to and asked to open at (2026-11-16; it
/// was 560, close to bitstep's window, which left the chop strip's waveform
/// too narrow to chop by eye).
///
/// A judgement rather than a sum: a lane row's fixed fields come to well
/// under this, and the rest goes to the elastic sample column
/// (`theme::HEADER_SAMPLE_MIN_WIDTH` is the least it may be squeezed to, not
/// the width it wants) and to the strip's waveform, the same width.
pub const DEFAULT_WIDTH: f32 = 1240.0;

/// The height the window opens at, in logical points: 800, the user's
/// window again (2026-11-16). The top bar, the chop strip at its default
/// height ([`crate::chop::STRIP_HEIGHT`]) and the lane list under them,
/// which shows as many rows as fit and scrolls the rest — a fresh instance
/// has more lanes than a window this size holds, so it is no longer a sum
/// of whole rows.
pub const DEFAULT_HEIGHT: f32 = 800.0;

/// The narrowest the window may be dragged: one readable lane row, the
/// list's padding either side of it, and the strip the scroll bar takes once
/// the list is long enough to need one.
///
/// A sum and not a judgement, unlike [`DEFAULT_WIDTH`], because this is the
/// width at which the elastic sample column reaches
/// [`theme::HEADER_SAMPLE_MIN_WIDTH`] ([`lanes::sample_column_width`]):
/// below it a row would be drawn wider than the window and its last fields
/// clipped away, the list scrolling sideways no more than it wraps.
pub const MIN_WIDTH: f32 =
    lanes::ROW_MIN_WIDTH + 2.0 * theme::PANEL_PADDING + theme::SCROLL_BAR_STRIP;

/// The shortest the window may be dragged: the top bar, the chop strip
/// collapsed to its header ([`chop_strip::STRIP_HEADER`]) and one lane row.
/// Less than that and there is nothing to scroll the list inside.
pub const MIN_HEIGHT: f32 = top_bar::BAR_HEIGHT
    + chop_strip::STRIP_HEADER
    + theme::ROW_PITCH
    + 2.0 * theme::PANEL_PADDING;

/// The editor window's size and open flag, as a new instance has them.
///
/// Built by the plugin struct rather than by [`create`] on purpose: the host
/// may close and reopen the window many times over one instance, and the
/// size the user dragged it to should survive that — nice-plug-egui writes
/// every resize back into this state.
pub fn default_state() -> Arc<EguiEditorState> {
    // Logical points, so the window is the same size in the same place on a
    // HiDPI screen as on this one; `1.0` is the user zoom on top of whatever
    // the host reports, and the host's own scaling is applied over it.
    EguiEditorState::from_size(LogicalSize::new(DEFAULT_WIDTH, DEFAULT_HEIGHT), 1.0)
}

/// The editor the host is handed, over the window state the plugin instance
/// holds.
///
/// Assembling it opens no window — the host does that later through
/// `Editor::spawn()` — so this is safe to call with no display, which is how
/// it is tested here.
///
/// `params` is the instance's own, and with it everything in the window that
/// is not the window: the master volume the top bar drives through the host
/// ([`top_bar`]), and behind [`crate::LanesField`] the one model the loader
/// writes decoded samples into and the audio thread reads snapshots of. The
/// editor holds no truth of its own — the rows are a read of that model, and
/// every gesture over them a write through its writer API.
///
/// `auditions` is the one handle here that does not come through the model:
/// the engine's ring, which a click on a row's play button pushes a lane
/// onto. An `Arc` because the ring outlives the window — the host may close
/// and reopen the editor many times over one instance — and because the
/// engine is the consumer at the other end of it.
///
/// `repaint_notifier` goes two ways. Down into nice-plug-egui, which is what
/// lets the audio thread ask for the frame that lights a dot
/// (`crate::Chopdrop::process`); and into the model as its repaint hook
/// ([`crate::model::Model::set_repaint_hook`]), which is what lets the
/// loader's worker show a sample the moment it lands rather than at the next
/// mouse move.
pub fn create(
    state: Arc<EguiEditorState>,
    repaint_notifier: RepaintNotifier,
    params: Arc<ChopdropParams>,
    auditions: Arc<AuditionRing>,
    previewer: Arc<Previewer>,
) -> Option<HostEditor> {
    let settings = EguiNiceSettings::new()
        // `with_tile`, not `with_title`: nice-plug-egui 0.6.0's own spelling.
        .with_tile(WINDOW_TITLE)
        // The lane list grows without bound and a sample's file name is as
        // long as the file system made it, so the window resizes; what it
        // may not do is shrink below one readable row.
        .with_resize_hint(
            ResizeHint::RESIZABLE.with_min_logical_size(LogicalSize::new(MIN_WIDTH, MIN_HEIGHT)),
        );

    // The one thing the model is told about this editor: how to ask for a
    // frame. Installed here rather than in [`NiceEguiApp::build`] because the
    // notifier belongs to the editor and not to the window — it outlives
    // every open and close of one, and the loader may land a restore's
    // samples before the host has opened a window at all. The notifier's own
    // `request_repaint` is a relaxed atomic store, so a writer may ring it
    // while holding the writer mutex, and ringing it with no window open
    // simply leaves a flag the next window reads.
    let app = app(&repaint_notifier, params, auditions, Arc::clone(&previewer));
    // Wrapped so that the host closing the window stops the preview
    // ([`closing`]): nice-plug-egui 0.6 never tells the app.
    create_egui_editor(state, repaint_notifier, settings, app)
        .map(|editor| closing::StopsOnClose::new(editor, previewer))
}

/// The editor the host is handed: nice-plug-egui's, wrapped so that a
/// closed window stops the preview ([`closing::StopsOnClose`]).
pub type HostEditor = closing::StopsOnClose<EguiEditor<ChopdropEditor>>;

/// The editor's app — everything [`create`] hands nice-plug-egui bar the
/// window settings — with the repaint hooks installed on the model and the
/// previewer.
///
/// Split out so that the very editor a host gets can be drawn with no
/// window around it ([`crate::Chopdrop::headless_editor`],
/// [`ChopdropEditor::draw`]): nice-plug-egui keeps its app to itself.
pub fn app(
    repaint_notifier: &RepaintNotifier,
    params: Arc<ChopdropParams>,
    auditions: Arc<AuditionRing>,
    previewer: Arc<Previewer>,
) -> ChopdropEditor {
    let notifier = repaint_notifier.clone();
    lock_model(params.lanes.model())
        .set_repaint_hook(Arc::new(move || notifier.request_repaint()));
    // And the previewer, for the same reason: a preview that finishes
    // decoding, or fails to, changes the browser's status line.
    let notifier = repaint_notifier.clone();
    previewer.set_repaint_hook(Arc::new(move || notifier.request_repaint()));

    ChopdropEditor::new(params, auditions, previewer)
}

/// What one frame of [`ChopdropEditor::draw`] laid out: enough for a
/// headless test to find the regions and click into them.
#[derive(Debug, Clone)]
pub struct Drawn {
    /// The chop strip, header included.
    pub strip: egui::Rect,
    /// The dock's tab after the frame, `None` while it is closed.
    pub dock: Option<dock::Tab>,
    /// Each lane row's name cell, in list order, by lane id.
    pub names: Vec<(u32, egui::Rect)>,
    /// The lanes selected after the frame.
    pub selected: Vec<u32>,
}

/// The editor's per-window state.
///
/// The instance's own parameters and the engine's audition ring, and beside
/// them the five things that belong to one opened window: the host context a
/// parameter is written through, the host window the unused keys go to, the
/// files being dragged over it, the lane name being typed into it and the
/// activity lights' own clock. Each is taken afresh in
/// [`NiceEguiApp::build`], as everything that arrives later will be (the
/// delete-confirm flag), because nice-plug-egui 0.6 never calls
/// [`NiceEguiApp::editor_closed`].
pub struct ChopdropEditor {
    /// The instance's parameters: the master volume the top bar drives, and
    /// — through [`crate::LanesField`] — the lane list the window draws and
    /// the writer API every gesture over it goes through, shared with the
    /// loader and with the audio thread's published snapshots.
    params: Arc<ChopdropParams>,

    /// The window's host context, the only way a parameter may be written
    /// (the host owns the value, the GUI asks).
    /// Taken afresh in [`NiceEguiApp::build`] for each opened window, which
    /// is the only place nice-plug-egui offers it, and `None` until then.
    gui_ctx: Option<GuiContext>,

    /// The host window this editor sits in, found afresh for each opened
    /// window. `None` when there is no X11 display to reach it through, in
    /// which case every key stays with the editor.
    host_window: Option<keys::HostWindow>,

    /// The files being dragged over the window, and the drop the frame they
    /// land in answers (`editor/drops.rs`). Per window rather than per
    /// instance: a drag cannot outlive the window it is over.
    drops: drops::DropIntake,

    /// The lane name being typed, if one is ([`lanes::Rename`]): one of the
    /// two things the window holds that are not a read of the model, because
    /// half a typed name belongs to no lane yet.
    rename: lanes::Rename,

    /// The master volume's own half-made gesture
    /// ([`top_bar::MasterVolume`]): the automation gesture the rail has open
    /// on the host, and the value being typed into it. The other thing here
    /// that is a hand over the window rather than a read of anything — and
    /// the one piece of window state that is about the *parameter* rather
    /// than about the model.
    master: top_bar::MasterVolume,

    /// The lane whose menu has had its delete item clicked once and is
    /// waiting for the second ([`menu::Confirm`]) — the window's other
    /// half-made gesture, and per window for the same reason the rename box
    /// is: nothing about a lane, everything about the hand over it.
    confirm: menu::Confirm,

    /// The engine's audition ring, which a click on a row's play button
    /// pushes a lane onto. Per instance and not per window — the ring is the
    /// engine's — but the editor is its one producer, which is what makes the
    /// single-producer ring sound.
    auditions: Arc<AuditionRing>,

    /// Each playable lane's activity counter as this window last saw it, and
    /// when it last moved ([`activity::Lights`]).
    ///
    /// Per window, unlike the counters themselves: the counts belong to the
    /// instance and keep climbing whether or not anyone is looking, while
    /// the *times* are this window's own — a window opened an hour after the
    /// last note opens with its lights out rather than flashing the history
    /// it missed.
    lights: activity::Lights,

    /// The instance's previewer, which the browser plays files through. Per
    /// instance, like the audition ring.
    previewer: Arc<Previewer>,

    /// The dock on the right — the lane tab and the sample browser's browse
    /// tab — and the window width it takes while open ([`dock`]). Per window:
    /// a window opens with it closed.
    dock: dock::Dock,

    /// The lanes the lane panel acts on: set by clicks on rows, pruned by the
    /// list every frame. Per window: a window opens with nothing selected.
    selection: selection::Selection,

    /// The region that reads the unforwarded keys: the last one clicked. Per
    /// window.
    key_owner: focus::KeyOwner,

    /// Where the key-reading regions were laid out last frame, which is
    /// what this frame's clicks are judged against.
    key_regions: focus::Regions,
}

/// How often the browser's play button and status line are redrawn while a
/// preview is decoding or sounding: nothing else asks for a frame when it
/// ends.
const PREVIEW_POLL: std::time::Duration = std::time::Duration::from_millis(100);

impl ChopdropEditor {
    /// An editor over an instance's parameters and the engine's audition
    /// ring, with no window around it yet — [`NiceEguiApp::build`] finds
    /// that, and the host context with it, for each window the host opens.
    fn new(
        params: Arc<ChopdropParams>,
        auditions: Arc<AuditionRing>,
        previewer: Arc<Previewer>,
    ) -> Self {
        ChopdropEditor {
            params,
            gui_ctx: None,
            host_window: None,
            drops: drops::DropIntake::default(),
            rename: lanes::Rename::default(),
            master: top_bar::MasterVolume::default(),
            confirm: menu::Confirm::default(),
            auditions,
            lights: activity::Lights::default(),
            previewer,
            dock: dock::Dock::default(),
            selection: selection::Selection::new(),
            key_owner: focus::KeyOwner::Lanes,
            key_regions: focus::Regions::default(),
        }
    }

    /// Hands the window around the editor everything the editor itself has
    /// no use for, once a frame.
    ///
    /// [`keys::HostWindow::update`] runs whether or not there is a key to
    /// pass on: it is what finds the host's window in the first place, and
    /// what takes the window's pin off again each time Bitwig puts it back.
    /// Key repeats are passed on along with the rest, so holding a key down
    /// reaches the host as the stream of presses it really is.
    ///
    /// Both of those jobs need a next frame to be done on, and the only other
    /// thing in this window that asks for one is a lane whose light is fading
    /// — which is no help on a window that has just opened and played
    /// nothing. So while the windows are still moving, this asks
    /// ([`keys::HostWindow::is_settling`]). Once they have settled the editor
    /// goes back to being reactive and costs nothing idle.
    fn pass_unused_keys_to_the_host(&mut self, ctx: &egui::Context) {
        let Some(host) = self.host_window.as_mut() else {
            return;
        };
        host.update();
        if host.is_settling() {
            ctx.request_repaint_after(keys::SETTLING_POLL);
        }
        if !host.has_host() {
            return;
        }
        // A lane's name is typed into this window, and with Bitwig listening
        // every letter of it would also be one of its shortcuts.
        if ctx.text_edit_focused() {
            return;
        }

        ctx.input(|input| {
            for event in &input.events {
                if let egui::Event::Key { key, pressed, modifiers, .. } = event
                    && let Some(keysym) = keys::host_keysym(*key, *modifiers)
                {
                    host.send(keysym, *pressed, *modifiers);
                }
            }
        });
    }

    /// Steps chopdrop's own edit history on the two keys the editor keeps
    /// back from the host ([`history`]).
    ///
    /// Called with the model already locked and before anything is drawn,
    /// so the rows of this very frame are the state stepped to rather than
    /// the one left behind — an undo shows at the moment the key goes down,
    /// as it does in a host.
    ///
    /// The one thing decided here is whether the keyboard is the window's
    /// to read at all: while a lane's name or the master volume's value is
    /// being typed, Ctrl+Z undoes the typing in that box and not the edit
    /// before it (and either box's first frame is counted as typing too —
    /// it has asked for the focus but egui has not given it yet, and a
    /// shortcut in that frame is meant for the box the user has just
    /// opened; [`history::typing`]).
    fn step_through_the_history(&self, ctx: &egui::Context, model: &mut Model) {
        let typing = history::typing(
            ctx.text_edit_focused(),
            self.rename.editing().is_some(),
            self.master.typing(),
        );
        ctx.input(|input| history::apply(&input.events, typing, model));
    }
}

impl NiceEguiApp for ChopdropEditor {
    /// Called once per opened window, before the first frame.
    ///
    /// The `GuiContext` is kept, because it is the only way the top bar's
    /// master volume may be written: a host parameter belongs to the host,
    /// and the GUI asks it to set one. Everything else in the window writes
    /// through the model instead.
    fn build(
        &mut self,
        egui_ctx: egui::Context,
        nice_gui_ctx: GuiContext,
        frame: &mut Frame,
    ) -> Result<(), HandlerError> {
        theme::apply(&egui_ctx);

        // What the renderer clears to before egui paints: the darkest grey of
        // the chrome rather than baseview's black, so a frame that arrives
        // mid-resize shows the window's own background in the strip the
        // panels have not caught up with yet.
        frame.set_clear_color(theme::WINDOW.into());

        // Per opened window, and before the first frame: the host has not
        // put the editor window into its own yet, which is why
        // `pass_unused_keys_to_the_host` keeps asking (`editor/keys.rs`).
        self.host_window = keys::HostWindow::connect(frame);

        // And everything the last window left behind, since nice-plug-egui
        // 0.6 never calls `editor_closed` to do it on the way out — the
        // host context included, which is per window and not per instance.
        self.gui_ctx = Some(nice_gui_ctx);
        self.drops = drops::DropIntake::default();
        self.rename.close();
        // The rail's gesture included: a window that went away mid-drag owes
        // its host an end-set-parameter nobody can send any more, and the
        // new window must not send one for a drag it never saw.
        self.master = top_bar::MasterVolume::default();
        self.confirm.clear();
        // The lights included, and that one matters: egui's clock starts
        // again with the window, so times kept from the last one would be
        // read against a clock that has gone backwards.
        self.lights = activity::Lights::default();
        // And the dock, closed, with nothing left previewing from it; a
        // window the last one had grown for gives the width back on its
        // first frame (this has no window size to work from).
        self.dock.reset(&self.previewer);
        self.selection.clear();
        // And the keyboard, back with the lanes.
        self.key_owner = focus::KeyOwner::Lanes;
        self.key_regions = focus::Regions::default();

        Ok(())
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut Frame) {
        self.draw(ui);
    }
}

impl ChopdropEditor {
    /// One frame of the window, into `ui`: what [`NiceEguiApp::ui`] runs,
    /// and what a headless test runs inside a test `egui::Context` (the
    /// window's `Frame` cannot be built without a window, and the frame
    /// needs nothing from it).
    pub fn draw(&mut self, ui: &mut egui::Ui) -> Drawn {
        self.pass_unused_keys_to_the_host(ui.ctx());

        // Before anything draws: a hover and a drop are both answered in the
        // frame they arrive in, and `dropped_files` lives for no longer.
        self.drops.read_frame(ui.ctx());

        // Who reads the keys this frame: the region under the frame's last
        // primary press, judged against where the regions were last drawn
        // (`editor/focus.rs`).
        let regions = self.key_regions;
        let owner = self.key_owner;
        self.key_owner = ui.input(|input| owner.after_events(&input.events, &regions));

        // The editor's one lock, held for the one frame it draws: writers
        // (the GUI itself, the loader's results, a project load) are
        // serialized by it and the audio thread never takes it.
        let mut model = lock_model(self.params.lanes.model());

        // And the collect the model asks the editor for, so that the
        // snapshots a writer has replaced are freed on a thread allowed to
        // free them — once a frame, before the read, where it costs nothing
        // when there is no garbage.
        model.collect();

        // And the undo or redo the frame's keys asked for, before a row is
        // laid out over lanes the step is about to replace. The collect above
        // has already run, and the one this step's own commit does is the
        // model's.
        self.step_through_the_history(ui.ctx(), &mut model);

        // Before the rows: what the audio thread has counted since the last
        // frame, read off the published snapshot and timed here. The snapshot
        // and not the model's own lanes, because the counters live on the
        // published side where the audio thread can reach them without a
        // lock.
        let now = ui.input(|input| input.time);
        let published = model.published().get();
        self.lights
            .observe(published.lanes().iter().map(|lane| (lane.id, lane.activity())), now);
        // And whose audition is sounding, for that row's stop button.
        let sounding = self.auditions.sounding();
        self.lights.set_auditioning(sounding);
        drop(published);

        // The bar first and the list after it, because egui lays a panel out
        // in the order it is shown and a central panel takes what the panels
        // before it left.
        //
        // What the bar says beside its three controls may be the message the
        // last drop left behind rather than the project's size
        // (`drops::DropIntake::message`) — read here because the bar is
        // drawn before the list a drop is routed against, so a drop's own
        // message is first shown in the frame `land` asks for.
        let setter = self.gui_ctx.as_ref().map(GuiContext::param_setter);
        let message = self.drops.message().zip(self.drops.message_hover());
        top_bar::top_bar(
            ui,
            &self.params.volume,
            setter.as_ref(),
            &mut model,
            message,
            &mut self.master,
        );

        // The chop strip, full width under the bar and above the dock and the
        // list. Its buttons all act inside it (play and loop through the
        // previewer; add lane through the writer API, one undo step). Its
        // keys are read only while it owns the keyboard.
        let strip = chop_strip::show(ui, &mut model, &self.previewer, self.key_owner);

        // The dock, on the right, before the list takes the rest of the
        // window. The browser's target lane is read afresh every frame, so a
        // rename or a newly landed sample shows at once — and a target that
        // has been deleted closes it.
        let mut dock_out =
            self.dock.show(ui, &mut model, self.selection.ids(), &self.previewer, self.key_owner);

        let target = self.dock.browse_target();
        let list = lanes::lane_list(
            ui,
            &mut model,
            &mut self.rename,
            &mut self.confirm,
            &mut self.selection,
            &self.lights,
            target,
        );

        // And then the drag, against the rows this frame just laid out
        // (`editor/drops.rs`): the outline over the row the files would land
        // on while they are still in the air, and the lanes they land in
        // once they are let go. After the list for the two reasons every
        // other gesture over it is applied after it — an outline belongs
        // over the row's own fields rather than under them, and a writer
        // needs the model the rows were read from to itself.
        let rows: Vec<drops::LaneRect> = list.rows.iter().map(lanes::LaneRow::lane_rect).collect();
        self.drops.outline(ui, &rows, list.rect, strip.rect);
        let loader = self.params.lanes.loader();
        self.drops.land(ui, &mut model, loader, &rows, list.rect, strip.rect);
        // And a selection dragged out of the chop strip, the same way: the
        // same routing and outline, a row replacing its sample with the cut
        // and anywhere else but the strip adding it as a lane. One undo step.
        drops::outline_chop(ui, &rows, list.rect, strip.rect);
        drops::land_chop(ui, &mut model, &rows, list.rect, strip.rect);

        // "give each its own output" that ran out of free outputs says so
        // in the bar's status slot, timed like a drop's message.
        if let Some(text) = status::outputs_kept(list.outputs_kept) {
            self.drops.show_message(ui.ctx(), text, status::OUTPUTS_FULL_HOVER);
        }

        // A click on a play button, onto the ring the engine drains at the
        // top of its next block. Best-effort, so the answer is dropped:
        // filling a ring of thirty-two takes clicking faster than the host
        // calls `process()`, and a lost click is a click that can simply be
        // made again.
        //
        // `S` asks the same of the selected lane, from anywhere in the window
        // but a text box. Either way a lane already sounding is stopped
        // instead of started again, so the button is play and stop, and
        // starting again starts from the top.
        let key = audition_key(ui.ctx(), &self.selection);
        if let Some(lane) = list.audition.or(key) {
            toggle_audition(&self.auditions, sounding, &model, lane);
        }
        // Polled while it sounds, so the stop button turns back into a play
        // button when the sample runs out with nothing else to repaint for.
        if self.auditions.sounding().is_some() || list.audition.or(key).is_some() {
            ui.ctx().request_repaint_after(AUDITION_POLL);
        }

        // And "reload from file" out of a lane's menu, onto the background
        // loader: the one gesture over the list that reads a file, so it
        // goes the way a drop's paths go (`editor/drops.rs`) and the lane
        // says "loading…" until the result lands. No commit here — the
        // loader commits the sample it lands, and a lane that is merely
        // waiting is not a step to undo to.
        if let Some((lane, path)) = list.reload {
            self.params.lanes.loader().queue(&mut model, lane, loader::Source::File(path));
        }

        // "open source in editor" out of a chop lane's menu: its source
        // reopened through the loader with the cut as the pending selection,
        // the strip opened to show it.
        if let Some(origin) = &list.open_source {
            open_origin(&mut model, self.params.lanes.loader(), origin);
        }

        // A click on a lane's sample cell, or "browse…" in its menu: open the
        // dock on the browse tab for that lane, or point the browser at it.
        // It selects that lane too (unless it is already among the
        // selected), so the lane tab beside the browser shows the lane the
        // browser is outlining — the user took the outline for a selection
        // and found the lane tab empty (2026-11-16). A sample-cell click on
        // a lane that already has a sample shows its settings instead, the
        // browser pointed at it behind them (the user's, 2026-11-16); the
        // menu's "browse…" always browses.
        if let Some(id) = list.browse {
            if !self.selection.contains(id) {
                self.selection.click(id);
            }
            let loaded = model.lanes().iter().any(|lane| lane.id == id && lane.sample.is_some());
            if list.sample_click && loaded {
                self.dock.lane_settings_for(ui.ctx(), &model, id, &self.previewer);
            } else {
                self.dock.browse(ui.ctx(), &model, id);
                // Asking for the browser is choosing it: its arrows work at
                // once, without a second click inside it.
                self.key_owner = focus::KeyOwner::Browser;
            }
        }

        // A row selected by its name or background shows the lane tab,
        // opening the dock or switching from the browser.
        if list.selected.is_some() {
            self.dock.lane_selected(ui.ctx(), &self.previewer);
        }

        // What the browser asked for: a load goes to the loader exactly as a
        // drop onto that lane's row does, and is one undo step when it lands.
        // And "open in chop editor" opens the file as the strip's source,
        // as a drop on the strip does.
        browser_requests(&mut model, self.params.lanes.loader(), &mut dock_out);
        // Where the regions were drawn, for next frame's clicks; one that has
        // gone (the dock closed, or turned to the lane tab) hands the keyboard
        // back.
        self.key_regions =
            focus::Regions { chop: Some(strip.rect), browser: dock_out.browser_rect };
        // A browse tab opened this frame has no rect yet, and keeps the
        // keyboard it was just given until it is drawn.
        if self.dock.tab() != Some(dock::Tab::Browse) {
            self.key_owner = self.key_owner.still_on_screen(&self.key_regions);
        }

        if self.dock.browsing()
            && (self.previewer.is_playing()
                || matches!(self.previewer.status(), PreviewStatus::Loading(_)))
        {
            ui.ctx().request_repaint_after(PREVIEW_POLL);
        }

        // And the frame the rest of a fade is drawn on, while there is one to
        // draw. Only then: a window with nothing sounding goes back to egui's
        // reactive repaint and costs nothing idle, and the next hit arrives
        // as the audio thread's own wake-up call
        // ([`crate::Chopdrop::process`]).
        if self.lights.fading() {
            ui.ctx().request_repaint();
        }

        Drawn {
            strip: strip.rect,
            dock: self.dock.tab(),
            names: list.rows.iter().map(|row| (row.id, row.name.rect)).collect(),
            selected: self.selection.ids().to_vec(),
        }
    }
}

/// How often the window looks again at whether an audition is still
/// sounding, while one is.
const AUDITION_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The lane `S` asks to play or stop this frame, taken out of egui's
/// input: the selection's anchor if it is selected, else the first selected
/// lane. Nothing while a text box has the keyboard (a lane's name, the
/// browser's filter), nor with nothing selected.
fn audition_key(ctx: &egui::Context, selection: &selection::Selection) -> Option<u32> {
    if ctx.text_edit_focused() {
        return None;
    }
    let pressed =
        ctx.input_mut(|input| input.count_and_consume_key(egui::Modifiers::NONE, egui::Key::S));
    if pressed == 0 {
        return None;
    }
    let anchor = selection.anchor().filter(|anchor| selection.contains(*anchor));
    anchor.or_else(|| selection.ids().first().copied())
}

/// Play `lane`'s audition, or stop it if it is the one `sounding`. A lane
/// with no sample asks for nothing (it would sound nothing, and the ring is
/// small); stopping needs no sample. Best-effort, as the ring is: a lost
/// push is a click that can be made again.
fn toggle_audition(ring: &AuditionRing, sounding: Option<u32>, model: &Model, lane: u32) {
    if sounding == Some(lane) {
        ring.stop();
    } else if model.lanes().iter().any(|row| row.id == lane && row.sample.is_some()) {
        ring.push(lane);
    }
}

/// What the browser asked of the window this frame, acted on: a load goes to
/// the loader exactly as a drop onto that lane's row does (and is one undo
/// step when it lands); "open in chop editor" opens the file as the chop
/// editor's source ([`loader::Loader::open_source`]), as a drop on the strip
/// does. Takes the requests out of `out`, so each is acted on once.
fn browser_requests(model: &mut Model, loader: &loader::Loader, out: &mut dock::DockOutput) {
    if let Some((lane, path)) = out.load.take() {
        loader.queue(model, lane, loader::Source::File(path));
    }
    if let Some(path) = out.open_in_editor.take() {
        loader.open_source(model, &path);
    }
}

/// The lane menu's "open source in editor": reopens the source `origin`
/// was cut from with the cut pending as the selection
/// ([`loader::Loader::open_origin`]) and opens a collapsed strip, so the
/// source is seen landing.
fn open_origin(model: &mut Model, loader: &loader::Loader, origin: &crate::chop::ChopOrigin) {
    loader.open_origin(model, origin);
    let height = model.chop().height_px;
    model.set_strip(false, height);
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::loader::Loader;
    use crate::model::SharedModel;

    /// A fresh instance's parameters: a default model, the loader that
    /// decodes into it, and the master volume over both — what
    /// [`crate::Chopdrop::default`] builds, assembled here so that the editor
    /// and its top bar can be tested over one.
    ///
    /// Shared with [`top_bar`]'s own tests, which need the same thing: the
    /// bar is drawn over an instance's parameters and writes its model.
    pub(crate) fn fresh_params() -> Arc<ChopdropParams> {
        let model: SharedModel = Arc::new(Mutex::new(Model::new()));
        let loader = Arc::new(Loader::new(Arc::clone(&model)));
        Arc::new(ChopdropParams::new(model, loader))
    }

    /// A previewer over `params`' model, as the plugin builds one per
    /// instance.
    pub(crate) fn fresh_previewer(params: &ChopdropParams) -> Arc<Previewer> {
        let handle = lock_model(params.lanes.model()).handle().clone();
        let channel = Arc::new(crate::preview::PreviewChannel::new(&handle));
        Arc::new(Previewer::new(channel, handle))
    }

    /// An editor over parameters of its own, as the plugin builds one per
    /// instance.
    fn editor_over_a_fresh_model() -> (Arc<EguiEditorState>, HostEditor) {
        let state = default_state();
        let ring = Arc::new(AuditionRing::new());
        let params = fresh_params();
        let previewer = fresh_previewer(&params);
        let editor = create(Arc::clone(&state), RepaintNotifier::new(), params, ring, previewer)
            .expect("an editor is always built");
        (state, editor)
    }

    /// A new instance's window opens at the size this module names, in
    /// logical points, and knows it is not on screen yet.
    #[test]
    fn the_window_opens_at_the_default_size() {
        let state = default_state();

        assert!(!state.is_open(), "no window has been spawned");
        assert_eq!(state.user_scale_factor(), 1.0);

        let size = state.logical_size();
        assert_eq!(size.width, DEFAULT_WIDTH);
        assert_eq!(size.height, DEFAULT_HEIGHT);
    }

    /// The default size is the user's window, with room under the bar and
    /// the strip for a few lane rows, and the minimum is smaller than it on
    /// both axes — a minimum at or above the default would have the host
    /// reject the size the window asked to open at.
    // The constants are the subject: the asserts are here so a change to one
    // of them fails the suite with its numbers.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_default_size_holds_the_top_bar_the_strip_and_some_lanes() {
        assert_eq!((DEFAULT_WIDTH, DEFAULT_HEIGHT), (1240.0, 800.0), "the user's window");
        assert_eq!(crate::chop::STRIP_HEIGHT, 400.0, "the strip opens at its default height");
        let list = DEFAULT_HEIGHT - top_bar::BAR_HEIGHT - crate::chop::STRIP_HEIGHT;
        assert!(list >= 4.0 * theme::ROW_PITCH, "room for {list} points of rows");
        assert_eq!(
            MIN_HEIGHT,
            top_bar::BAR_HEIGHT
                + chop_strip::STRIP_HEADER
                + theme::ROW_PITCH
                + 2.0 * theme::PANEL_PADDING
        );
        // The bar is one control tall and the hairline under it is the
        // window's, not the bar's.
        assert_eq!(top_bar::BAR_HEIGHT, theme::TOP_BAR_HEIGHT + theme::HAIRLINE);

        assert!(MIN_WIDTH < DEFAULT_WIDTH, "the window cannot open below its minimum");
        assert!(MIN_HEIGHT < DEFAULT_HEIGHT, "the window cannot open below its minimum");
        assert!(
            MIN_HEIGHT > top_bar::BAR_HEIGHT + chop_strip::STRIP_HEADER,
            "the top bar and the collapsed strip have to fit"
        );
    }

    /// The minimum width is the width at which a row's elastic sample column
    /// reaches its own minimum and no less — so the narrowest window the host
    /// will allow still draws a whole row, and the row never has to be
    /// clipped or scrolled sideways.
    #[test]
    fn the_minimum_width_is_one_whole_lane_row() {
        let padding = 2.0 * theme::PANEL_PADDING + theme::SCROLL_BAR_STRIP;

        assert_eq!(
            lanes::sample_column_width(MIN_WIDTH - padding),
            theme::HEADER_SAMPLE_MIN_WIDTH,
            "the column is at its minimum when the window is"
        );
        assert!(
            lanes::sample_column_width(MIN_WIDTH + 1.0 - padding) > theme::HEADER_SAMPLE_MIN_WIDTH,
            "a point wider and the column grows, so the minimum is not over-wide"
        );
        assert!(
            lanes::sample_column_width(DEFAULT_WIDTH - padding) > theme::HEADER_SAMPLE_MIN_WIDTH,
            "the window opens with room to spare in the sample column"
        );
    }

    /// The host is given an editor, it is resizable, and it may not be
    /// dragged below one readable lane row. `create` builds no window, so
    /// this runs with no display.
    #[test]
    fn the_host_gets_a_resizable_editor_over_the_instances_window_state() {
        use nice_plug::editor::{Editor, SizeConstraints};

        let (state, editor) = editor_over_a_fresh_model();

        // The editor holds the same state the plugin struct does, so a resize
        // outlives the window being closed and reopened.
        assert_eq!(Arc::strong_count(&state), 2);

        let hint = editor.resize_hint();
        assert!(hint.can_resize);
        assert!(hint.can_resize_horizontally && hint.can_resize_vertically);
        assert!(!hint.preserve_aspect_ratio, "a lane list has no aspect ratio to keep");
        match hint.size_constraints {
            SizeConstraints::Logical { min_size, max_size } => {
                let min = min_size.expect("a minimum size is set");
                assert_eq!(min.width, MIN_WIDTH);
                assert_eq!(min.height, MIN_HEIGHT);
                assert_eq!(max_size, None, "the list is as tall as the user wants");
            }
            other => panic!("the minimum is in logical points, not {other:?}"),
        }

        drop(editor);
        assert_eq!(Arc::strong_count(&state), 1);
    }

    /// The editor is given the instance's own parameters and holds them for
    /// as long as the host keeps the editor: the master volume it drives is
    /// the one the host automates, and the model behind them is the one the
    /// loader writes and the audio thread reads — not a copy of its own that
    /// a project load would leave behind.
    #[test]
    fn the_editor_holds_the_instances_params_and_through_them_its_model() {
        let params = fresh_params();
        let model = Arc::clone(params.lanes.model());
        let ring = Arc::new(AuditionRing::new());
        let previewer = fresh_previewer(&params);
        let editor =
            create(default_state(), RepaintNotifier::new(), Arc::clone(&params), ring, previewer)
            .expect("an editor is always built");

        assert_eq!(Arc::strong_count(&params), 2, "the editor shares the one params struct");
        assert!(
            Arc::ptr_eq(params.lanes.model(), &model),
            "the editor's lanes are the instance's lanes"
        );

        drop(editor);
        assert_eq!(Arc::strong_count(&params), 1);
    }

    /// A chop lane's "open source in editor" reaches the window as the
    /// list's output: the source path remembered and loading, the cut
    /// pending as the selection, and a collapsed strip opened.
    #[test]
    fn open_source_from_a_lane_menu_opens_the_origin_with_its_selection() {
        let params = fresh_params();
        let mut model = lock_model(params.lanes.model());
        let height = model.chop().height_px;
        model.set_strip(true, height);
        let origin = crate::chop::ChopOrigin {
            source_path: String::from("/nowhere/chopdrop-open-origin.wav"),
            start_frame: 100,
            end_frame: 900,
        };

        open_origin(&mut model, params.lanes.loader(), &origin);

        let chop = model.chop();
        assert_eq!(chop.source_path.as_deref(), Some(origin.source_path.as_str()));
        assert!(chop.loading || chop.error.is_some(), "the open was started");
        assert_eq!(chop.pending_selection, Some((100, 900)));
        assert!(!chop.collapsed, "the strip is opened to show the source");
    }

    /// The browser's "open in chop editor" reaches the window as the dock's
    /// output, and the window opens it as the chop editor's source: the path
    /// remembered and "loading…" at once, the job on the loader, no lane
    /// touched.
    #[test]
    fn open_in_editor_from_the_browser_opens_the_chop_source() {
        let params = fresh_params();
        let mut model = lock_model(params.lanes.model());
        let lanes_before: Vec<_> = model.lanes().iter().map(|lane| lane.loading).collect();
        let path = std::path::PathBuf::from("/nowhere/chopdrop-open-in-editor.wav");
        let mut out =
            dock::DockOutput { open_in_editor: Some(path.clone()), ..Default::default() };

        browser_requests(&mut model, params.lanes.loader(), &mut out);

        let chop = model.chop();
        assert_eq!(chop.source_path.as_deref(), path.to_str());
        assert!(chop.loading || chop.error.is_some(), "the open was started");
        assert_eq!(out.open_in_editor, None, "the request is acted on once");
        let lanes_after: Vec<_> = model.lanes().iter().map(|lane| lane.loading).collect();
        assert_eq!(lanes_before, lanes_after, "no lane is loading");
    }
}
