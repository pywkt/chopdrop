//! The right-hand dock: one side panel, [`BROWSER_WIDTH`] wide, with a tab
//! row — **lane** · **browse** · **×** — above whichever tab is showing.
//!
//! The **browse** tab is the sample browser ([`browser_panel`], unchanged
//! inside); the **lane** tab is where the selected lanes' settings go.
//! Switching tabs keeps the browser as it was — folder, selection, a
//! preview still sounding — and only closing the dock lets go of it.
//!
//! Opening and closing are also where the window's width changes: opening
//! asks the host for a window [`BROWSER_WIDTH`] wider and closing gives that
//! width back, so the lane list keeps the room it had. Per window; a window
//! opens with the dock closed.

use std::path::{Path, PathBuf};

use egui::{Align, Layout, Rect, RichText, Ui};

use crate::browser::Settings;
use crate::editor::browser_panel::{BROWSER_WIDTH, BrowserPanel, TargetLane};
use crate::editor::focus::KeyOwner;
use crate::editor::lane_panel;
use crate::editor::{MIN_WIDTH, theme};
use crate::model::Model;
use crate::preview::Previewer;

/// The tab row's labels.
pub const LANE_TAB: &str = "lane settings";
pub const BROWSE_TAB: &str = "browse";
pub const CLOSE_LABEL: &str = "×";
/// The close button's hover.
pub const CLOSE_HOVER: &str = "close the side panel";

/// The browse tab before any lane has been browsed for.
pub const BROWSE_HINT: &str = "select a lane to browse for its sample";

/// The dock's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    /// The lane panel: the selected lanes' settings.
    Lane,
    /// The sample browser.
    Browse,
}

/// What the dock asks of the window this frame, and where it was drawn.
#[derive(Debug, Default)]
pub struct DockOutput {
    /// Load this file into this lane: the browser's load, which the window
    /// queues on the loader as a drop onto that row would be.
    pub load: Option<(u32, PathBuf)>,
    /// Open this file in the chop editor: the browser's "open in chop
    /// editor", which the window hands to the loader as a drop on the strip.
    pub open_in_editor: Option<PathBuf>,
    /// The browse tab's rect, when it was drawn this frame: the region that
    /// takes the keyboard (`editor/focus.rs`).
    pub browser_rect: Option<Rect>,
}

/// The dock's per-window state.
pub struct Dock {
    /// The tab showing, or `None` while the dock is closed.
    tab: Option<Tab>,
    /// The browser, once a lane has been browsed for; kept across tab
    /// switches, dropped when the dock closes.
    browser: Option<BrowserPanel>,
    /// Where the browser's settings live; `None` when there is no home to
    /// keep them in. A field so the tests never touch the user's own.
    settings_path: Option<PathBuf>,
    /// Whether opening asked for a wider window, so closing gives it back.
    grew: bool,
    /// A window closed with the dock open comes back at the wider size the
    /// host remembered, without the dock: its first frame gives the width
    /// back ([`Dock::reset`]).
    give_back_width: bool,
}

impl Default for Dock {
    fn default() -> Dock {
        Dock::with_settings_path(Settings::path())
    }
}

impl Dock {
    /// A closed dock whose browser keeps its settings at `settings_path`.
    pub fn with_settings_path(settings_path: Option<PathBuf>) -> Dock {
        Dock { tab: None, browser: None, settings_path, grew: false, give_back_width: false }
    }

    /// The tab showing, or `None` while the dock is closed.
    pub fn tab(&self) -> Option<Tab> {
        self.tab
    }

    /// Whether the dock is open.
    pub fn is_open(&self) -> bool {
        self.tab.is_some()
    }

    /// The browser, if a lane has been browsed for since the dock opened.
    pub fn browser(&self) -> Option<&BrowserPanel> {
        self.browser.as_ref()
    }

    /// The lane the browser loads into while the browse tab shows: the row
    /// the lane list outlines.
    pub fn browse_target(&self) -> Option<u32> {
        match self.tab {
            Some(Tab::Browse) => self.browser.as_ref().map(BrowserPanel::target),
            _ => None,
        }
    }

    /// Whether the browse tab is showing a browser: the frames a preview's
    /// status line is polled on.
    pub fn browsing(&self) -> bool {
        self.browse_target().is_some()
    }

    /// For a newly opened window: the dock closed with nothing previewing,
    /// and — if the last window had grown for it — the width given back on
    /// the first frame ([`Dock::show`]).
    pub fn reset(&mut self, previewer: &Previewer) {
        self.tab = None;
        self.browser = None;
        self.give_back_width = std::mem::take(&mut self.grew);
        previewer.stop();
    }

    /// Opens the dock on `tab`, or switches to it. Opening asks for the
    /// window to grow by the dock's width.
    pub fn open(&mut self, ctx: &egui::Context, tab: Tab) {
        if self.tab.is_none() {
            resize(ctx, true);
            self.grew = true;
        }
        self.tab = Some(tab);
    }

    /// Closes the dock: the browser's preview stops (the strip's does not), the browser is let go of, and the
    /// width opening took is given back.
    pub fn close(&mut self, ctx: &egui::Context, previewer: &Previewer) {
        if self.tab.take().is_none() {
            return;
        }
        self.browser = None;
        previewer.stop_browser();
        if std::mem::take(&mut self.grew) {
            resize(ctx, false);
        }
    }

    /// A click that selected a lane (its name or row background): the dock
    /// shows the lane tab, opening if it was closed and switching from the
    /// browser if that was showing.
    /// Selecting a lane is asking for its settings — the user looked for
    /// them under a browser left open by a sample click and could not find
    /// them (2026-11-16). The browser itself is kept, as any tab switch
    /// keeps it; only its preview stops, as it does when its tab is left.
    pub fn lane_selected(&mut self, ctx: &egui::Context, previewer: &Previewer) {
        if self.tab == Some(Tab::Browse) {
            previewer.stop_browser();
        }
        self.open(ctx, Tab::Lane);
    }

    /// A click on lane `id`'s sample cell (an empty lane's), or **browse…**
    /// in its menu: the dock opens on the browse tab (or switches to it)
    /// with the browser pointed at that lane. Does nothing for a lane that
    /// no longer exists.
    pub fn browse(&mut self, ctx: &egui::Context, model: &Model, id: u32) {
        if self.point_browser(model, id) {
            self.open(ctx, Tab::Browse);
        }
    }

    /// A click on the sample cell of lane `id` that already has a sample:
    /// the dock shows the lane tab, its settings being what a loaded lane
    /// is clicked for (the user's, 2026-11-16), with the browser pointed at
    /// that lane behind it, so the browse tab is one click from replacing
    /// its sample. Leaving the browse tab stops its preview, as any tab
    /// switch does. Does nothing for a lane that no longer exists.
    pub fn lane_settings_for(
        &mut self,
        ctx: &egui::Context,
        model: &Model,
        id: u32,
        previewer: &Previewer,
    ) {
        if self.point_browser(model, id) {
            self.lane_selected(ctx, previewer);
        }
    }

    /// Points the browser at lane `id`, building it if there is none yet.
    /// `false` for a lane that no longer exists.
    fn point_browser(&mut self, model: &Model, id: u32) -> bool {
        let Some(lane) = model.lanes().iter().find(|lane| lane.id == id) else {
            return false;
        };
        let sample = lane.sample.as_ref().map(|sample| PathBuf::from(&sample.path));
        let target = TargetLane { id, name: &lane.name, sample: sample.as_deref() };
        match self.browser.as_mut() {
            Some(panel) => panel.retarget(target),
            None => {
                let path = self.settings_path.as_deref();
                let settings = path.map(Settings::load_from).unwrap_or_default();
                self.browser =
                    Some(BrowserPanel::open_with(target, settings, path.map(Path::to_path_buf)));
            }
        }
        true
    }

    /// Draws the dock on the right of `ui` while it is open, before the
    /// lane list takes the rest of the window. The lane tab shows the lanes
    /// `selected` names; the browser reads keys only while `owner` is the
    /// browser.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        model: &mut Model,
        selected: &[u32],
        previewer: &Previewer,
        owner: KeyOwner,
    ) -> DockOutput {
        let mut out = DockOutput::default();
        if std::mem::take(&mut self.give_back_width) {
            resize(ui.ctx(), false);
        }

        // A browser whose lane has been deleted has nothing to load into:
        // the dock closes if it was showing it, and lets it go if not.
        let target = self.browser.as_ref().map(|panel| {
            let id = panel.target();
            model.lanes().iter().find(|lane| lane.id == id).map(|lane| {
                let sample = lane.sample.as_ref().map(|sample| PathBuf::from(&sample.path));
                (id, lane.name.clone(), sample)
            })
        });
        if matches!(target, Some(None)) {
            if self.tab == Some(Tab::Browse) {
                self.close(ui.ctx(), previewer);
            } else {
                self.browser = None;
                previewer.stop_browser();
            }
        }
        let Some(tab) = self.tab else {
            return out;
        };

        let frame = egui::Frame::new()
            .fill(theme::PANEL)
            .inner_margin(egui::Margin::same(theme::PANEL_PADDING as i8));
        let mut close = false;
        let mut switch = None;
        let shown = egui::Panel::right("chopdrop_dock")
            .exact_size(BROWSER_WIDTH)
            .resizable(false)
            .frame(frame)
            .show(ui, |ui| {
                switch = tab_row(ui, tab, &mut close);
                // A tab clicked this frame shows this frame, so the click
                // that chose the browser lands on a browser that is drawn.
                match switch.unwrap_or(tab) {
                    Tab::Lane => {
                        // Scrolls when the window is too short for every
                        // setting, so the last ones are never cut off.
                        egui::ScrollArea::vertical().id_salt("chopdrop_lane_panel_scroll").show(
                            ui,
                            |ui| lane_panel::show(ui, model, selected),
                        );
                        None
                    }
                    Tab::Browse => match (self.browser.as_mut(), target.flatten()) {
                        (Some(panel), Some((id, name, sample))) => {
                            let target =
                                TargetLane { id, name: &name, sample: sample.as_deref() };
                            Some(panel.show(ui, &target, previewer, owner))
                        }
                        _ => {
                            ui.add_space(theme::ITEM_SPACING);
                            ui.label(RichText::new(BROWSE_HINT).color(theme::TEXT_DIM));
                            None
                        }
                    },
                }
            });
        if let Some(tab) = switch {
            // The browser's preview ends with its tab; the strip's does not.
            previewer.stop_browser();
            self.tab = Some(tab);
            // The browse tab browses for the selected lane: one the browser
            // is not already pointed at (or a browser not yet built, the
            // dock having opened from a name click) is pointed at it.
            let target = self.browser.as_ref().map(BrowserPanel::target);
            let first = selected.first().copied();
            let pointed = target.is_some_and(|target| selected.contains(&target));
            if let Some(id) = first.filter(|_| tab == Tab::Browse && !pointed) {
                self.point_browser(model, id);
                // This frame drew the tab for the old target.
                ui.ctx().request_repaint();
            }
        }
        if self.tab == Some(Tab::Browse) {
            out.browser_rect = Some(shown.response.rect);
        }
        if let Some(panel_out) = shown.inner {
            out.load = panel_out.load;
            out.open_in_editor = panel_out.open_in_editor;
            close |= panel_out.close;
        }
        if close {
            self.close(ui.ctx(), previewer);
            out.browser_rect = None;
        }
        out
    }
}

/// The tab row: the two tabs on the left, the close button on the right.
/// Returns the tab clicked, if another one was; `close` is set by **×**.
fn tab_row(ui: &mut Ui, tab: Tab, close: &mut bool) -> Option<Tab> {
    let mut switch = None;
    ui.horizontal(|ui| {
        for (label, which) in [(LANE_TAB, Tab::Lane), (BROWSE_TAB, Tab::Browse)] {
            if ui.selectable_label(tab == which, label).clicked() && tab != which {
                switch = Some(which);
            }
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.button(CLOSE_LABEL).on_hover_text(CLOSE_HOVER).clicked() {
                *close = true;
            }
        });
    });
    ui.separator();
    switch
}

/// The width to ask for when the dock opens (`open`) or closes, from the
/// window's current width. Closing never asks for less than the window's
/// minimum.
pub fn resized_width(width: f32, open: bool) -> f32 {
    if open { width + BROWSER_WIDTH } else { (width - BROWSER_WIDTH).max(MIN_WIDTH) }
}

/// Asks the host for the window resized for the dock. A host that refuses
/// leaves the dock sharing the width the window has.
fn resize(ctx: &egui::Context, open: bool) {
    let size = ctx.content_rect().size();
    let width = resized_width(size.x, open);
    ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(width, size.y)));
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use basedrop::{Collector, Shared};
    use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, ViewportCommand, vec2};

    use super::*;
    use crate::editor::activity::Lights;
    use crate::editor::lanes::{self, Rename};
    use crate::editor::menu::Confirm;
    use crate::editor::selection::Selection;
    use crate::editor::{DEFAULT_HEIGHT, DEFAULT_WIDTH};
    use crate::preview::{PreviewChannel, PreviewOwner, Region};
    use crate::sample::SampleData;

    /// A scratch folder with one subfolder and two WAV-named files (the
    /// browser lists by extension; nothing here is decoded).
    fn kit(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chopdrop-dock-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("toms")).expect("scratch dir");
        for file in ["bd.wav", "sd.wav"] {
            std::fs::write(dir.join(file), b"not audio").expect("scratch file");
        }
        dir
    }

    /// A window as the editor draws one — the dock, then the lane list
    /// beside it, then the list's browse request back into the dock
    /// (`crate::editor::ChopdropEditor::ui`) — over a default model, with the
    /// browser's settings kept in a scratch folder.
    struct Window {
        ctx: egui::Context,
        model: Model,
        dock: Dock,
        previewer: Previewer,
        owner: KeyOwner,
        rename: Rename,
        confirm: Confirm,
        selection: Selection,
        lights: Lights,
        /// The window's width, as the host has it.
        width: f32,
        /// Each row's sample cell, last frame.
        samples: Vec<Rect>,
        /// Each row's name cell, last frame.
        names: Vec<Rect>,
        /// Each lane id in display order.
        ids: Vec<u32>,
        /// The dock's tab buttons and close button, found by their text.
        texts: Vec<(String, Rect)>,
        /// The widths the window asked the host for, in order.
        asked: Vec<f32>,
        /// The last file the dock asked to open in the chop editor.
        opened: Option<PathBuf>,
        _collector: Collector,
    }

    impl Window {
        fn new(dir: &Path) -> Window {
            let collector = Collector::new();
            let channel = Arc::new(PreviewChannel::new(&collector.handle()));
            let previewer = Previewer::new(channel, collector.handle());
            let settings_path = dir.join("settings.json");
            let settings = Settings {
                auto_preview: false,
                folder: Some(dir.to_path_buf()),
                recent: Vec::new(),
            };
            settings.save_to(&settings_path).expect("scratch settings");
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            Window {
                ctx,
                model: Model::new(),
                dock: Dock::with_settings_path(Some(settings_path)),
                previewer,
                owner: KeyOwner::Lanes,
                rename: Rename::default(),
                confirm: Confirm::default(),
                selection: Selection::new(),
                lights: Lights::default(),
                width: DEFAULT_WIDTH,
                samples: Vec::new(),
                names: Vec::new(),
                ids: Vec::new(),
                texts: Vec::new(),
                asked: Vec::new(),
                opened: None,
                _collector: collector,
            }
        }

        fn frame(&mut self, events: Vec<Event>) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    vec2(self.width, DEFAULT_HEIGHT),
                )),
                events,
                ..RawInput::default()
            };
            let (dock, model, previewer, owner) =
                (&mut self.dock, &mut self.model, &self.previewer, self.owner);
            let (rename, confirm, lights) = (&mut self.rename, &mut self.confirm, &self.lights);
            let selection = &mut self.selection;
            let mut samples = Vec::new();
            let mut names = Vec::new();
            let mut ids = Vec::new();
            let mut browser_rect = None;
            let mut opened = None;
            let output = self.ctx.run_ui(input, |ui| {
                let out = dock.show(ui, model, selection.ids(), previewer, owner);
                browser_rect = out.browser_rect;
                opened = out.open_in_editor;
                let target = dock.browse_target();
                let list = lanes::lane_list(ui, model, rename, confirm, selection, lights, target);
                samples = list.rows.iter().map(|row| row.sample.rect).collect();
                names = list.rows.iter().map(|row| row.name.rect).collect();
                ids = list.rows.iter().map(|row| row.id).collect();
                if list.selected.is_some() {
                    dock.lane_selected(ui.ctx(), previewer);
                }
                if let Some(id) = list.browse {
                    dock.browse(ui.ctx(), model, id);
                    self.owner = KeyOwner::Browser;
                }
            });
            if browser_rect.is_none() && dock.tab() != Some(Tab::Browse) {
                self.owner = KeyOwner::Lanes;
            }
            self.opened = opened.or(self.opened.take());
            self.samples = samples;
            self.names = names;
            self.ids = ids;
            self.texts = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => {
                        Some((text.galley.text().to_string(), text.visual_bounding_rect()))
                    }
                    _ => None,
                })
                .collect();
            for viewport in output.viewport_output.values() {
                for command in &viewport.commands {
                    if let ViewportCommand::InnerSize(size) = command {
                        self.asked.push(size.x);
                        // A host that grants it.
                        self.width = size.x;
                    }
                }
            }
            output.drop_without_applying_deltas();
        }

        /// A primary click at `at`, over three frames as a hand makes one.
        fn click(&mut self, at: Pos2) {
            let button = |pressed| Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            };
            self.frame(vec![Event::PointerMoved(at), button(true)]);
            self.frame(vec![button(false)]);
            self.frame(Vec::new());
        }

        /// Where the dock drew `text` last frame.
        fn text(&self, text: &str) -> Pos2 {
            self.texts
                .iter()
                .find(|(drawn, _)| drawn == text)
                .map(|(_, rect)| rect.center())
                .unwrap_or_else(|| panic!("{text:?} is not drawn"))
        }

        fn press(&mut self, key: Key) {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::NONE,
            }]);
        }

        fn selected(&self) -> Option<String> {
            let browser = self.dock.browser().expect("a browser").browser();
            browser.selected().map(|entry| entry.name.clone())
        }
    }

    #[test]
    fn the_dock_opens_closed() {
        let dir = kit("closed");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        assert_eq!(window.dock.tab(), None);
        assert!(window.dock.browser().is_none());
        assert!(window.asked.is_empty(), "a closed dock asks for no width");
    }

    #[test]
    fn a_sample_cell_click_opens_the_browse_tab_on_that_lane() {
        let dir = kit("cell");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        let third = window.ids[2];
        window.click(window.samples[2].center());

        assert_eq!(window.dock.tab(), Some(Tab::Browse));
        let browser = window.dock.browser().expect("the browser opened");
        assert_eq!(browser.target(), third);
        assert_eq!(browser.browser().folder(), Some(dir.as_path()));
        assert_eq!(window.dock.browse_target(), Some(third), "its row is outlined");
        assert_eq!(window.asked, [DEFAULT_WIDTH + BROWSER_WIDTH], "the window grew once");
        assert_eq!(window.owner, KeyOwner::Browser, "the browser has the keyboard");

        // Another cell retargets the open browser without growing again.
        let fifth = window.ids[4];
        window.click(window.samples[4].center());
        assert_eq!(window.dock.browser().map(BrowserPanel::target), Some(fifth));
        assert_eq!(window.asked.len(), 1);
    }

    #[test]
    fn the_browsers_open_in_editor_comes_out_of_the_dock() {
        let dir = kit("chop");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[0].center());
        window.press(Key::ArrowDown);
        window.press(Key::ArrowDown);
        assert_eq!(window.selected().as_deref(), Some("bd.wav"));
        assert_eq!(window.opened, None);
        window.click(window.text(crate::editor::browser_panel::CHOP_LABEL));
        assert_eq!(window.opened, Some(dir.join("bd.wav")));
    }

    #[test]
    fn a_name_click_selects_the_lane_and_opens_the_lane_tab() {
        let dir = kit("select");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        let third = window.ids[2];
        window.click(window.names[2].center());

        assert_eq!(window.selection.ids(), &[third]);
        assert_eq!(window.dock.tab(), Some(Tab::Lane));
        assert!(window.dock.browser().is_none(), "no browser was asked for");
        assert_eq!(window.asked, [DEFAULT_WIDTH + BROWSER_WIDTH], "the window grew once");

        // With the dock open on the browse tab, a row click selects and
        // switches to the lane tab, keeping the browser for later.
        window.click(window.samples[4].center());
        assert_eq!(window.dock.tab(), Some(Tab::Browse));
        let fifth = window.ids[5];
        window.click(window.names[5].center());
        assert_eq!(window.selection.ids(), &[fifth]);
        assert_eq!(window.dock.tab(), Some(Tab::Lane));
        assert!(window.dock.browser().is_some(), "the browser is kept");
        assert_eq!(window.asked.len(), 1, "an open dock does not grow the window again");
    }

    #[test]
    fn a_tab_switch_keeps_the_browsers_folder_and_selection() {
        let dir = kit("switch");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[0].center());
        window.press(Key::ArrowDown);
        window.press(Key::Enter); // into "toms"
        window.press(Key::Backspace); // back up, "toms" selected
        window.press(Key::ArrowDown);
        assert_eq!(window.selected().as_deref(), Some("bd.wav"));

        window.click(window.text(LANE_TAB));
        assert_eq!(window.dock.tab(), Some(Tab::Lane));
        assert!(window.texts.iter().any(|(text, _)| text == lane_panel::HINT));
        assert_eq!(window.dock.browse_target(), None, "no row outlined under the lane tab");
        assert_eq!(window.owner, KeyOwner::Lanes, "the keyboard went back to the lanes");
        // Arrows under the lane tab move nothing in the hidden browser.
        window.press(Key::ArrowDown);

        window.click(window.text(BROWSE_TAB));
        assert_eq!(window.dock.tab(), Some(Tab::Browse));
        let browser = window.dock.browser().expect("the browser was kept").browser();
        assert_eq!(browser.folder(), Some(dir.as_path()));
        assert_eq!(window.selected().as_deref(), Some("bd.wav"));
        assert_eq!(window.asked.len(), 1, "a switch resizes nothing");
    }

    /// One engine block, as far as the previewer can tell: the preview is
    /// sounding exactly when a sample is published.
    fn engine_block(previewer: &Previewer) {
        let channel = previewer.channel();
        channel.set_playing(channel.get().sample.is_some());
    }

    /// The chop strip's looping preview, sounding.
    fn strip_preview(window: &Window) {
        let handle = window._collector.handle();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 10].into() };
        let region = Region { start: 0, end: 10 };
        window.previewer.play_region(Shared::new(&handle, data), region, true);
        engine_block(&window.previewer);
        assert!(window.previewer.is_playing());
    }

    /// A browser preview (the scratch files don't decode, so the engine's
    /// flag is set by hand: what is checked is that the dock stops it).
    fn browser_preview(window: &Window, dir: &Path) {
        window.previewer.play(&dir.join("bd.wav"));
        window.previewer.channel().set_playing(true);
        assert_eq!(window.previewer.owner(), Some(PreviewOwner::Browser));
    }

    #[test]
    fn the_strips_preview_survives_a_tab_switch_and_closing_the_dock() {
        let dir = kit("strip-owns");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[0].center());
        strip_preview(&window);

        window.click(window.text(LANE_TAB));
        assert_eq!(window.dock.tab(), Some(Tab::Lane));
        window.click(window.text(BROWSE_TAB));
        window.click(window.text(CLOSE_LABEL));
        assert_eq!(window.dock.tab(), None);
        engine_block(&window.previewer);
        assert!(window.previewer.is_playing(), "the strip's loop still sounds");
        assert_eq!(window.previewer.owner(), Some(PreviewOwner::Strip));
    }

    #[test]
    fn a_tab_switch_or_closing_the_dock_stops_the_browsers_preview() {
        let dir = kit("browser-owns");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[0].center());

        browser_preview(&window, &dir);
        window.click(window.text(LANE_TAB));
        assert_eq!(window.previewer.owner(), None, "the tab switch stopped it");
        engine_block(&window.previewer);
        assert!(!window.previewer.is_playing());

        window.click(window.text(BROWSE_TAB));
        browser_preview(&window, &dir);
        window.click(window.text(CLOSE_LABEL));
        assert_eq!(window.dock.tab(), None);
        assert_eq!(window.previewer.owner(), None, "closing stopped it");
        engine_block(&window.previewer);
        assert!(!window.previewer.is_playing());
    }

    #[test]
    fn close_gives_the_width_back() {
        let dir = kit("close");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[1].center());
        assert_eq!(window.width, DEFAULT_WIDTH + BROWSER_WIDTH);

        window.click(window.text(CLOSE_LABEL));
        assert_eq!(window.dock.tab(), None);
        assert!(window.dock.browser().is_none(), "closing lets the browser go");
        assert_eq!(window.asked, [DEFAULT_WIDTH + BROWSER_WIDTH, DEFAULT_WIDTH]);
        assert_eq!(window.width, DEFAULT_WIDTH);
        assert_eq!(window.owner, KeyOwner::Lanes);
    }

    #[test]
    fn escape_in_the_browser_closes_the_dock() {
        let dir = kit("escape");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[1].center());
        window.press(Key::Escape);
        assert_eq!(window.dock.tab(), None);
        assert_eq!(window.width, DEFAULT_WIDTH);
    }

    #[test]
    fn deleting_the_browsed_lane_closes_the_dock() {
        let dir = kit("delete");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        let id = window.ids[3];
        window.click(window.samples[3].center());
        window.model.delete_lane(id);
        window.frame(Vec::new());
        assert_eq!(window.dock.tab(), None);
        assert_eq!(window.width, DEFAULT_WIDTH);
    }

    #[test]
    fn a_reopened_window_gives_back_the_width_the_last_one_took() {
        let dir = kit("reopen");
        let mut window = Window::new(&dir);
        window.frame(Vec::new());
        window.click(window.samples[0].center());
        window.dock.reset(&window.previewer);
        assert_eq!(window.dock.tab(), None);
        window.frame(Vec::new());
        assert_eq!(window.width, DEFAULT_WIDTH);
        window.frame(Vec::new());
        assert_eq!(window.asked.len(), 2, "given back once");
    }

    #[test]
    fn closing_never_asks_for_less_than_the_minimum_width() {
        assert_eq!(resized_width(DEFAULT_WIDTH, true), DEFAULT_WIDTH + BROWSER_WIDTH);
        assert_eq!(resized_width(DEFAULT_WIDTH + BROWSER_WIDTH, false), DEFAULT_WIDTH);
        assert_eq!(resized_width(MIN_WIDTH, false), MIN_WIDTH);
    }
}
