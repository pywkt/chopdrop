//! The sample browser panel, docked to the right of the lane list.
//!
//! The panel draws [`crate::browser::Browser`] and turns clicks and keys into
//! what the window does with them: previews through the instance's
//! [`Previewer`], and loads handed back to the window ([`PanelOutput::load`]),
//! which queues them on the loader exactly as a drop onto that lane's row
//! would be. A file can be opened in the chop editor instead, from the
//! footer or the row's right-click menu ([`PanelOutput::open_in_editor`]).

use std::path::{Path, PathBuf};

use egui::{
    Align, Color32, Key, Label, Modifiers, Rect, RichText, Sense, StrokeKind, Ui, Vec2, vec2,
};

use crate::browser::{self, Activation, Browser, Kind, Settings};
use crate::editor::focus::KeyOwner;
use crate::editor::theme;
use crate::preview::{PreviewOwner, PreviewStatus, Previewer};

/// The panel's width, in logical points. Opening it asks the host for a
/// window this much wider (`crate::editor::ChopdropEditor`).
pub const BROWSER_WIDTH: f32 = 300.0;

/// One entry's row height in the list.
const ENTRY_HEIGHT: f32 = 20.0;

/// The list's background: darker than the panel, so it reads as a well.
const LIST_FILL: Color32 = theme::WINDOW;
/// Every other row's background in the list.
const LIST_STRIPE: Color32 = Color32::from_gray(29);
/// The gap between the list's border and its rows.
const LIST_MARGIN: f32 = 2.0;

/// A row's icon: its size, how far it sits from the row's left edge, and
/// the gap after it before the name.
const ICON_SIZE: Vec2 = Vec2::new(13.0, 10.0);
const ICON_INSET: f32 = 5.0;
const ICON_GAP: f32 = 6.0;
/// The folder icon's colour: a muted manila.
const FOLDER_ICON: Color32 = Color32::from_rgb(0xC4, 0xA0, 0x55);
/// The waveform icon's bar heights, as fractions of the icon's height.
const WAVE_BARS: [f32; 4] = [0.45, 1.0, 0.7, 0.35];

/// The height of the panel's buttons and fields: tighter than the lane
/// list's [`theme::CONTROL_HEIGHT`], so the entry list keeps most of a
/// window opened at its default height.
const CONTROL_HEIGHT: f32 = 22.0;

/// The filter box's width, beside the folder name.
const FILTER_WIDTH: f32 = 96.0;

/// How far PageUp/PageDown move the selection.
const PAGE: isize = 10;

/// The header's title.
pub const TITLE: &str = "browse";
/// The close button.
pub const CLOSE_LABEL: &str = "×";
/// The up button.
pub const UP_LABEL: &str = "up";
/// The filter box's hint.
pub const FILTER_HINT: &str = "filter";
/// The recent-folders menu.
pub const RECENT_LABEL: &str = "recent";
/// The header's hover over the target lane's name.
pub const TARGET_HOVER: &str =
    "the lane a load goes into: click another lane's sample to change it";
/// The play button, idle.
pub const PLAY_LABEL: &str = "play";
/// The play button while a preview sounds.
pub const STOP_LABEL: &str = "stop";
/// The auto-preview checkbox.
pub const AUTO_PREVIEW_LABEL: &str = "auto-preview";
/// The load button.
pub const LOAD_LABEL: &str = "load";
/// The footer's open-in-chop-editor button: short, so the footer's row
/// still fits the panel's width.
pub const CHOP_LABEL: &str = "chop";
/// Its hover.
pub const CHOP_HOVER: &str = "open in the chop editor, to cut pieces from it";
/// The row's right-click menu item.
pub const OPEN_IN_EDITOR_LABEL: &str = "open in chop editor";
/// The status line while a preview decodes.
pub const PREVIEW_LOADING: &str = "loading preview…";

/// What the panel asks of the window this frame.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PanelOutput {
    /// Close the panel.
    pub close: bool,
    /// Load this file into this lane.
    pub load: Option<(u32, PathBuf)>,
    /// Open this file in the chop editor (`Model::open_source`, through the
    /// loader).
    pub open_in_editor: Option<PathBuf>,
}

/// The target lane as the panel needs to see it each frame: read from the
/// model by the window, so a rename or a new sample shows at once.
pub struct TargetLane<'a> {
    /// The lane's id.
    pub id: u32,
    /// Its name, for the header.
    pub name: &'a str,
    /// Where its sample came from, if it has one.
    pub sample: Option<&'a Path>,
}

/// The panel's state, per opened window.
pub struct BrowserPanel {
    /// The lane a load goes into.
    target: u32,
    browser: Browser,
    settings: Settings,
    /// Where `settings` is saved; `None` when there is no home to save in.
    settings_path: Option<PathBuf>,
    /// The file auto-preview last played, so it plays once per selection
    /// rather than every frame.
    previewed: Option<PathBuf>,
    /// The list's scroll offset and visible height, as of the last frame,
    /// for keeping a keyboard selection in view.
    scroll: (f32, f32),
    /// Scroll so the selection shows, on the next frame.
    reveal: bool,
}

impl BrowserPanel {
    /// The panel opened on lane `target`, in the folder of its sample if it
    /// has one, else where the user last loaded from.
    /// The dock passes the user's settings; tests pass their own, so they
    /// never read or write the real ones.
    pub fn open_with(
        target: TargetLane<'_>,
        settings: Settings,
        settings_path: Option<PathBuf>,
    ) -> BrowserPanel {
        let mut panel = BrowserPanel {
            target: target.id,
            browser: Browser::default(),
            settings,
            settings_path,
            previewed: None,
            scroll: (0.0, 0.0),
            reveal: false,
        };
        let home = browser::home();
        let folder = browser::start_folder(
            target.sample.and_then(Path::parent),
            &panel.settings,
            home.as_deref(),
        );
        panel.browser.open(&folder);
        panel.show_lane_sample(target.sample);
        panel
    }

    /// The lane a load goes into.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// The browser being drawn, for the window's tests.
    pub fn browser(&self) -> &Browser {
        &self.browser
    }

    /// Point the panel at another lane. A lane with a sample shows its
    /// folder with the sample selected; an empty one keeps the folder shown.
    pub fn retarget(&mut self, target: TargetLane<'_>) {
        self.target = target.id;
        if let Some(folder) = target.sample.and_then(Path::parent)
            && folder.is_dir()
            && self.browser.folder() != Some(folder)
        {
            self.browser.open(folder);
        }
        self.show_lane_sample(target.sample);
    }

    /// Selects the lane's own sample when it is in the folder shown, marking
    /// it previewed so that opening the panel is silent.
    fn show_lane_sample(&mut self, sample: Option<&Path>) {
        if let Some(sample) = sample {
            self.browser.select_path(sample);
            if self.browser.selected().is_some_and(|entry| entry.path == sample) {
                self.previewed = Some(sample.to_path_buf());
                self.reveal = true;
            }
        }
    }

    /// Draws the panel into `ui` (the panel's own area) and says what the
    /// window should do. Its keys are read only while `owner` is the browser
    /// (`editor/focus.rs`); clicks always work.
    pub fn show(
        &mut self,
        ui: &mut Ui,
        target: &TargetLane<'_>,
        previewer: &Previewer,
        owner: KeyOwner,
    ) -> PanelOutput {
        let mut out = PanelOutput::default();
        let filter_id = ui.make_persistent_id("chopdrop_browser_filter");

        // Keys first, so they work while the filter box has the focus — and
        // only then: a key typed into a lane's rename box is not ours, nor
        // is one meant for the chop strip.
        let focus = ui.ctx().memory(|memory| memory.focused());
        let foreign_focus = focus.is_some() && focus != Some(filter_id);
        if owner.reads_keys(KeyOwner::Browser, foreign_focus) {
            self.keys(ui, &mut out, previewer);
        }

        let spacing = ui.spacing_mut();
        spacing.item_spacing = vec2(theme::ITEM_SPACING, theme::ITEM_SPACING);
        spacing.interact_size.y = CONTROL_HEIGHT;
        spacing.button_padding = vec2(6.0, 1.0);
        self.header(ui, target, &mut out, previewer);
        let home = browser::home();
        self.places(ui, home.as_deref(), target.sample.and_then(Path::parent), previewer);
        ui.separator();
        self.path_row(ui, filter_id, previewer);

        // The footer is laid out from the bottom up, so the list takes
        // whatever is left between.
        egui::Panel::bottom("chopdrop_browser_footer")
            .frame(egui::Frame::new())
            .show_separator_line(false)
            .show(ui, |ui| self.footer(ui, target, &mut out, previewer));

        // The list sits in its own darker well, like a file manager's.
        egui::Frame::new()
            .fill(LIST_FILL)
            .stroke(egui::Stroke::new(theme::HAIRLINE, theme::GRID_LINE))
            .corner_radius(theme::CELL_CORNER)
            .inner_margin(LIST_MARGIN)
            .show(ui, |ui| self.list(ui, &mut out, previewer));
        out
    }

    /// The keyboard: arrows and pages move, Enter activates, Backspace goes
    /// up when there is no filter to delete from, Escape clears the filter
    /// and then closes.
    fn keys(&mut self, ui: &Ui, out: &mut PanelOutput, previewer: &Previewer) {
        let filter_empty = self.browser.filter().is_empty();
        let (down, up, page_down, page_up, enter, escape, backspace) = ui.input_mut(|input| {
            let mut take = |key| input.consume_key(Modifiers::NONE, key);
            (
                take(Key::ArrowDown),
                take(Key::ArrowUp),
                take(Key::PageDown),
                take(Key::PageUp),
                take(Key::Enter),
                take(Key::Escape),
                filter_empty && take(Key::Backspace),
            )
        });
        let moved = [(down, 1), (up, -1), (page_down, PAGE), (page_up, -PAGE)];
        for (pressed, delta) in moved {
            if pressed {
                self.browser.move_selection(delta);
                self.reveal = true;
                self.selection_changed(previewer);
            }
        }
        if enter {
            self.activate(out, previewer);
        }
        if backspace {
            self.browser.up();
            self.reveal = true;
        }
        if escape {
            if filter_empty {
                out.close = true;
            } else {
                self.browser.set_filter("");
            }
        }
    }

    /// Opens the selected folder or asks for the selected file to be loaded.
    fn activate(&mut self, out: &mut PanelOutput, previewer: &Previewer) {
        match self.browser.activate() {
            Activation::Load(path) => self.load(out, path),
            Activation::Opened => {
                self.reveal = true;
                previewer.stop_browser();
                self.previewed = None;
            }
            Activation::Nothing => {}
        }
    }

    /// Hands `path` to the window for the target lane, and remembers its
    /// folder for next time.
    fn load(&mut self, out: &mut PanelOutput, path: PathBuf) {
        self.settings.remember_load(&path);
        self.save_settings();
        out.load = Some((self.target, path));
    }

    /// Auto-preview: play a newly selected file once; a folder selected
    /// stops whatever the browser was previewing.
    fn selection_changed(&mut self, previewer: &Previewer) {
        let Some(entry) = self.browser.selected() else { return };
        match entry.kind {
            Kind::Audio if self.settings.auto_preview => {
                if self.previewed.as_deref() != Some(entry.path.as_path()) {
                    self.previewed = Some(entry.path.clone());
                    previewer.play(&entry.path);
                }
            }
            Kind::Audio => {}
            Kind::Folder => {
                if self.previewed.take().is_some() {
                    previewer.stop_browser();
                }
            }
        }
    }

    fn save_settings(&self) {
        if let Some(path) = &self.settings_path {
            // A convenience: a settings file that can't be written costs the
            // user their recent folders, nothing more.
            let _ = self.settings.save_to(path);
        }
    }

    /// The title, the lane the browser loads into, the recent folders behind
    /// one menu, and the close button.
    fn header(
        &mut self,
        ui: &mut Ui,
        target: &TargetLane<'_>,
        out: &mut PanelOutput,
        previewer: &Previewer,
    ) {
        let mut go = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(TITLE).color(theme::TEXT_DIM));
            ui.add(Label::new(RichText::new(target.name).color(theme::TEXT)).truncate())
                .on_hover_text(TARGET_HOVER);
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button(CLOSE_LABEL).on_hover_text("close the browser (Esc)").clicked() {
                    out.close = true;
                }
                if !self.settings.recent.is_empty() {
                    let menu = ui.menu_button(RECENT_LABEL, |ui| {
                        for folder in &self.settings.recent {
                            let name = folder_name(folder);
                            let hover = folder.display().to_string();
                            if ui.button(name).on_hover_text(hover).clicked() {
                                go = Some(folder.clone());
                                ui.close();
                            }
                        }
                    });
                    menu.response.on_hover_text("the folders you loaded from last");
                }
            });
        });
        if let Some(folder) = go {
            self.go_to(&folder, previewer);
        }
    }

    /// The quick places, on one line when they fit.
    fn places(
        &mut self,
        ui: &mut Ui,
        home: Option<&Path>,
        lane_folder: Option<&Path>,
        previewer: &Previewer,
    ) {
        let places = home.map(|home| browser::places(home, lane_folder)).unwrap_or_default();
        let mut go = None;
        ui.horizontal_wrapped(|ui| {
            for place in &places {
                let current = self.browser.folder() == Some(place.path.as_path());
                if ui
                    .selectable_label(current, &place.label)
                    .on_hover_text(place.path.display().to_string())
                    .clicked()
                {
                    go = Some(place.path.clone());
                }
            }
        });
        if let Some(folder) = go {
            self.go_to(&folder, previewer);
        }
    }

    /// Shows `folder`, silencing a preview of the folder left.
    fn go_to(&mut self, folder: &Path, previewer: &Previewer) {
        self.browser.open(folder);
        self.previewed = None;
        previewer.stop_browser();
    }

    /// Up, the folder being shown, and the filter box beside it.
    fn path_row(&mut self, ui: &mut Ui, filter_id: egui::Id, previewer: &Previewer) {
        ui.horizontal(|ui| {
            let can_go_up = self.browser.folder().is_some_and(|folder| folder.parent().is_some());
            let up = ui
                .add_enabled(can_go_up, egui::Button::new(UP_LABEL))
                .on_hover_text("up a folder (Backspace)");
            if up.clicked() {
                self.browser.up();
                self.reveal = true;
                self.previewed = None;
                previewer.stop_browser();
            }

            let folder = self.browser.folder().map(Path::to_path_buf).unwrap_or_default();
            let room = (ui.available_width() - FILTER_WIDTH - theme::ITEM_SPACING).max(0.0);
            ui.allocate_ui_with_layout(
                vec2(room, CONTROL_HEIGHT),
                egui::Layout::left_to_right(Align::Center),
                |ui| {
                    // The whole room, so the filter box sits at the right edge.
                    ui.set_min_width(room);
                    let name = RichText::new(folder_name(&folder)).color(theme::TEXT);
                    ui.add(Label::new(name).truncate())
                        .on_hover_text(folder.display().to_string());
                },
            );

            let mut text = self.browser.filter().to_string();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .id(filter_id)
                    .hint_text(FILTER_HINT)
                    .desired_width(FILTER_WIDTH),
            );
            if response.changed() {
                self.browser.set_filter(&text);
                self.reveal = true;
            }
        });
    }

    /// The entries, virtualised: only the rows in view are laid out, so a
    /// folder of thousands costs what a folder of twenty does.
    fn list(&mut self, ui: &mut Ui, out: &mut PanelOutput, previewer: &Previewer) {
        // Rows touch, so the stripes read as one table.
        ui.spacing_mut().item_spacing.y = 0.0;
        let pitch = ENTRY_HEIGHT;
        let count = self.browser.visible().len();

        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if std::mem::take(&mut self.reveal)
            && let Some(index) = self.browser.selected_index()
        {
            let (offset, height) = self.scroll;
            let top = index as f32 * pitch;
            let wanted = if top < offset {
                Some(top)
            } else if height > pitch && top + pitch > offset + height {
                Some(top + pitch - height)
            } else if height <= pitch {
                Some(top)
            } else {
                None
            };
            if let Some(wanted) = wanted {
                area = area.vertical_scroll_offset(wanted.max(0.0));
            }
        }

        let mut clicked = None;
        let mut activated = None;
        let mut menu_opened = None;
        let selected = self.browser.selected_index();
        let output = area.show_rows(ui, ENTRY_HEIGHT, count, |ui, range| {
            let visible = self.browser.visible();
            for index in range {
                let Some(entry) = visible.get(index) else { break };
                let response = entry_row(ui, entry, index, selected == Some(index));
                if response.double_clicked() {
                    activated = Some(index);
                } else if response.clicked() {
                    clicked = Some(index);
                } else if response.secondary_clicked() {
                    menu_opened = Some(index);
                }
                // A file's right-click menu: its one item opens it in the
                // chop editor. A folder has none.
                if entry.kind == Kind::Audio {
                    response.context_menu(|ui| {
                        if ui.button(OPEN_IN_EDITOR_LABEL).clicked() {
                            out.open_in_editor = Some(entry.path.clone());
                            ui.close();
                        }
                    });
                }
            }
        });
        self.scroll = (output.state.clamped_offset().y, output.inner_rect.height());

        if count == 0 {
            let note = if self.browser.filter().is_empty() {
                "no folders or audio files here"
            } else {
                "nothing matches the filter"
            };
            ui.put(output.inner_rect, Label::new(RichText::new(note).color(theme::TEXT_DIM)));
        }

        // A right-click selects the row too (silently: no auto-preview for
        // a menu), so the highlight shows which file the menu is for.
        if let Some(index) = menu_opened {
            self.browser.select(Some(index));
        }
        if let Some(index) = clicked.or(activated) {
            self.browser.select(Some(index));
            self.selection_changed(previewer);
        }
        if activated.is_some() {
            self.activate(out, previewer);
        }
    }

    /// Play/stop, auto-preview, load, open-in-chop-editor, and the status
    /// line under them.
    fn footer(
        &mut self,
        ui: &mut Ui,
        target: &TargetLane<'_>,
        out: &mut PanelOutput,
        previewer: &Previewer,
    ) {
        ui.add_space(theme::ITEM_SPACING);
        let status = previewer.status();
        // Only the browser's own preview turns the button to stop: the chop
        // strip's loop is not the browser's to end.
        let busy = previewer.owner() == Some(PreviewOwner::Browser)
            && (previewer.is_playing() || matches!(status, PreviewStatus::Loading(_)));
        let file = self
            .browser
            .selected()
            .filter(|entry| entry.kind == Kind::Audio)
            .map(|entry| entry.path.clone());

        ui.horizontal(|ui| {
            let play = if busy {
                ui.button(STOP_LABEL).on_hover_text("stop the preview")
            } else {
                ui.add_enabled(file.is_some(), egui::Button::new(PLAY_LABEL))
                    .on_hover_text("preview the highlighted file")
            };
            if play.clicked() {
                if busy {
                    previewer.stop();
                } else if let Some(path) = &file {
                    self.previewed = Some(path.clone());
                    previewer.play(path);
                }
            }

            let mut auto = self.settings.auto_preview;
            if ui
                .checkbox(&mut auto, AUTO_PREVIEW_LABEL)
                .on_hover_text("play a file as soon as it is selected")
                .changed()
            {
                self.settings.auto_preview = auto;
                self.save_settings();
            }

            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                let chop = ui
                    .add_enabled(file.is_some(), egui::Button::new(CHOP_LABEL))
                    .on_hover_text(CHOP_HOVER);
                if chop.clicked() {
                    out.open_in_editor = file.clone();
                }
                let load = ui
                    .add_enabled(file.is_some(), egui::Button::new(LOAD_LABEL))
                    .on_hover_text(format!("load into {} (double-click, Enter)", target.name));
                if load.clicked()
                    && let Some(path) = file.clone()
                {
                    self.load(out, path);
                }
            });
        });

        // Only when there is something to say: the list needs the room more.
        let line = match (&status, self.browser.error()) {
            (PreviewStatus::Failed(_, message), _) => {
                Some((format!("can't preview: {message}"), theme::WARN))
            }
            (_, Some(error)) => Some((error.to_string(), theme::WARN)),
            (PreviewStatus::Loading(_), None) => {
                Some((PREVIEW_LOADING.to_string(), theme::TEXT_DIM))
            }
            _ => None,
        };
        if let Some((line, colour)) = line {
            ui.add(Label::new(RichText::new(line).color(colour)).truncate());
        }
    }
}

/// One entry's row: the whole width, striped every other row, highlighted
/// when selected, with a folder or waveform icon before the name.
fn entry_row(ui: &mut Ui, entry: &browser::Entry, index: usize, selected: bool) -> egui::Response {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, ENTRY_HEIGHT), Sense::click());
    let fill = if selected {
        Some(theme::dim(theme::ACCENT, 0.45))
    } else if response.hovered() {
        Some(theme::CONTROL_HOVER)
    } else if index % 2 == 1 {
        Some(LIST_STRIPE)
    } else {
        None
    };
    if let Some(fill) = fill {
        ui.painter().rect(rect, theme::CELL_CORNER, fill, egui::Stroke::NONE, StrokeKind::Inside);
    }

    let icon = Rect::from_center_size(
        egui::pos2(rect.left() + ICON_INSET + ICON_SIZE.x / 2.0, rect.center().y),
        ICON_SIZE,
    );
    match entry.kind {
        Kind::Folder => folder_icon(ui.painter(), icon),
        Kind::Audio => {
            let colour = if selected { theme::TEXT } else { theme::ACCENT };
            audio_icon(ui.painter(), icon, colour);
        }
    }

    let left = icon.right() + ICON_GAP;
    let inner = Rect::from_min_max(egui::pos2(left, rect.top()), rect.max - vec2(4.0, 0.0));
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(inner).layout(egui::Layout::left_to_right(Align::Center)),
    );
    let name = RichText::new(&entry.name).color(theme::TEXT);
    child.add(Label::new(name).truncate().selectable(false));
    response.on_hover_text(entry.path.display().to_string())
}

/// A folder: a body with a tab on its top left.
fn folder_icon(painter: &egui::Painter, rect: Rect) {
    let tab = Rect::from_min_size(rect.min, vec2(rect.width() * 0.45, 3.0));
    let body = Rect::from_min_max(rect.min + vec2(0.0, 2.0), rect.max);
    painter.rect_filled(tab, egui::CornerRadius::same(1), FOLDER_ICON);
    painter.rect_filled(body, egui::CornerRadius::same(1), FOLDER_ICON);
}

/// An audio file: a few bars of a waveform.
fn audio_icon(painter: &egui::Painter, rect: Rect, colour: egui::Color32) {
    let bar = rect.width() / (2.0 * WAVE_BARS.len() as f32 - 1.0);
    for (i, height) in WAVE_BARS.iter().enumerate() {
        let x = rect.left() + 2.0 * bar * i as f32;
        let h = rect.height() * height;
        let top = rect.center().y - h / 2.0;
        let bar_rect = Rect::from_min_size(egui::pos2(x, top), vec2(bar, h));
        painter.rect_filled(bar_rect, egui::CornerRadius::ZERO, colour);
    }
}

/// A folder's last name, or the whole path for `/`.
fn folder_name(folder: &Path) -> String {
    folder
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| folder.display().to_string())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use basedrop::{Collector, Shared};
    use egui::{Pos2, RawInput};

    use super::*;
    use crate::preview::{PreviewChannel, Region};
    use crate::sample::SampleData;

    /// A scratch folder with one subfolder and two WAV files that decode.
    fn kit(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chopdrop-panel-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("toms")).expect("scratch dir");
        for file in ["bd.wav", "sd.wav"] {
            std::fs::write(dir.join(file), wav()).expect("scratch wav");
        }
        dir
    }

    /// A ten-frame mono 16-bit WAV.
    fn wav() -> Vec<u8> {
        let frames: u32 = 10;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + frames * 2).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&48_000u32.to_le_bytes());
        bytes.extend_from_slice(&96_000u32.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(frames * 2).to_le_bytes());
        bytes.extend(std::iter::repeat_n(0u8, frames as usize * 2));
        bytes
    }

    /// A panel over `dir` for lane 7, with settings kept in `dir` (never the
    /// user's own), and a previewer to play through.
    struct Harness {
        ctx: egui::Context,
        panel: BrowserPanel,
        previewer: Previewer,
        /// Who owns the keyboard: the browser, unless a test says not.
        owner: KeyOwner,
        /// Every text drawn last frame, with where it was drawn.
        texts: Vec<(String, Rect)>,
        _collector: Collector,
    }

    impl Harness {
        fn new(dir: &Path, auto_preview: bool) -> Harness {
            let collector = Collector::new();
            let channel = Arc::new(PreviewChannel::new(&collector.handle()));
            let previewer = Previewer::new(channel, collector.handle());
            let settings =
                Settings { auto_preview, folder: Some(dir.to_path_buf()), recent: Vec::new() };
            let target = TargetLane { id: 7, name: "Lane 7", sample: None };
            let panel =
                BrowserPanel::open_with(target, settings, Some(dir.join("settings.json")));
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            Harness {
                ctx,
                panel,
                previewer,
                owner: KeyOwner::Browser,
                texts: Vec::new(),
                _collector: collector,
            }
        }

        /// One frame with `events`, the panel filling a 280×400 window.
        fn frame(&mut self, events: Vec<egui::Event>) -> PanelOutput {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(BROWSER_WIDTH, 400.0))),
                events,
                ..RawInput::default()
            };
            let mut out = PanelOutput::default();
            let panel = &mut self.panel;
            let previewer = &self.previewer;
            let owner = self.owner;
            let output = self.ctx.run_ui(input, |ui| {
                let target = TargetLane { id: panel.target(), name: "Lane 7", sample: None };
                out = panel.show(ui, &target, previewer, owner);
            });
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
            output.drop_without_applying_deltas();
            out
        }

        /// Where `text` was drawn last frame.
        fn text(&self, text: &str) -> Pos2 {
            self.texts
                .iter()
                .find(|(drawn, _)| drawn == text)
                .map(|(_, rect)| rect.center())
                .unwrap_or_else(|| panic!("{text:?} is not drawn"))
        }

        /// A click with `button` at `at`, over three frames as a hand makes
        /// one; the frames' outputs folded into one.
        fn click_with(&mut self, at: Pos2, button: egui::PointerButton) -> PanelOutput {
            let event = |pressed| egui::Event::PointerButton {
                pos: at,
                button,
                pressed,
                modifiers: Modifiers::NONE,
            };
            let mut folded = PanelOutput::default();
            for events in
                [vec![egui::Event::PointerMoved(at), event(true)], vec![event(false)], Vec::new()]
            {
                let out = self.frame(events);
                folded.close |= out.close;
                folded.load = folded.load.or(out.load);
                folded.open_in_editor = folded.open_in_editor.or(out.open_in_editor);
            }
            folded
        }

        fn click(&mut self, at: Pos2) -> PanelOutput {
            self.click_with(at, egui::PointerButton::Primary)
        }

        fn press(&mut self, key: Key) -> PanelOutput {
            self.frame(vec![egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Modifiers::default(),
            }])
        }

        fn selected(&self) -> Option<String> {
            self.panel.browser().selected().map(|entry| entry.name.clone())
        }
    }

    use std::sync::Arc;

    /// Waits (bounded) for the previewer to settle out of `Loading`.
    fn settled(previewer: &Previewer) -> PreviewStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = previewer.status();
            if !matches!(status, PreviewStatus::Loading(_)) || Instant::now() > deadline {
                return status;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// One engine block, as far as the previewer can tell: the preview is
    /// sounding exactly when a sample is published.
    fn engine_block(previewer: &Previewer) {
        let channel = previewer.channel();
        channel.set_playing(channel.get().sample.is_some());
    }

    /// The chop strip's looping preview, sounding.
    fn strip_preview(harness: &Harness) {
        let handle = harness._collector.handle();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 10].into() };
        let region = Region { start: 0, end: 10 };
        harness.previewer.play_region(Shared::new(&handle, data), region, true);
        engine_block(&harness.previewer);
        assert!(harness.previewer.is_playing());
    }

    /// Whether `text` was drawn last frame.
    fn drawn(harness: &Harness, text: &str) -> bool {
        harness.texts.iter().any(|(drawn, _)| drawn == text)
    }

    #[test]
    fn the_strips_preview_reads_play_and_survives_a_folder_change() {
        let dir = kit("strip-owns");
        let mut harness = Harness::new(&dir, true);
        harness.frame(Vec::new());
        strip_preview(&harness);
        harness.frame(Vec::new());
        assert!(drawn(&harness, PLAY_LABEL), "the footer offers play");
        assert!(!drawn(&harness, STOP_LABEL), "the strip's loop is not the browser's to stop");

        harness.press(Key::ArrowDown); // "toms", auto-preview on: a folder stops nothing
        harness.press(Key::Enter); // into "toms"
        assert_eq!(harness.panel.browser().folder(), Some(dir.join("toms").as_path()));
        harness.press(Key::Backspace); // and back up
        assert_eq!(harness.panel.browser().folder(), Some(dir.as_path()));
        assert_eq!(harness.selected().as_deref(), Some("toms"));
        harness.press(Key::Enter); // into "toms" again, then out by the up button
        harness.click(harness.text(UP_LABEL));
        assert_eq!(harness.panel.browser().folder(), Some(dir.as_path()));
        engine_block(&harness.previewer);
        assert!(harness.previewer.is_playing(), "the strip's loop still sounds");
        assert_eq!(harness.previewer.owner(), Some(PreviewOwner::Strip));
    }

    #[test]
    fn the_browsers_preview_reads_stop_and_a_folder_change_stops_it() {
        let dir = kit("browser-owns");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.previewer.play(&dir.join("bd.wav"));
        assert_eq!(settled(&harness.previewer), PreviewStatus::Playing(dir.join("bd.wav")));
        engine_block(&harness.previewer);
        assert!(harness.previewer.is_playing());
        harness.frame(Vec::new());
        assert!(drawn(&harness, STOP_LABEL), "the footer offers stop");

        harness.press(Key::ArrowDown);
        harness.press(Key::Enter); // into "toms"
        engine_block(&harness.previewer);
        assert!(!harness.previewer.is_playing(), "the folder change stopped it");
        assert_eq!(harness.previewer.owner(), None);
        harness.frame(Vec::new());
        assert!(drawn(&harness, PLAY_LABEL));

        // The up button stops it the same way.
        harness.previewer.play(&dir.join("sd.wav"));
        settled(&harness.previewer);
        engine_block(&harness.previewer);
        assert!(harness.previewer.is_playing());
        harness.frame(Vec::new());
        harness.click(harness.text(UP_LABEL));
        engine_block(&harness.previewer);
        assert!(!harness.previewer.is_playing(), "going up stopped it");
    }

    #[test]
    fn it_opens_on_the_folder_last_loaded_from() {
        let dir = kit("open");
        let harness = Harness::new(&dir, true);
        assert_eq!(harness.panel.browser().folder(), Some(dir.as_path()));
        assert_eq!(harness.panel.target(), 7);
    }

    #[test]
    fn arrows_move_the_selection_and_enter_loads_a_file_into_the_target() {
        let dir = kit("enter");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown);
        assert_eq!(harness.selected().as_deref(), Some("toms"));
        harness.press(Key::ArrowDown);
        assert_eq!(harness.selected().as_deref(), Some("bd.wav"));
        let out = harness.press(Key::Enter);
        assert_eq!(out.load, Some((7, dir.join("bd.wav"))));
        assert!(!out.close, "the panel stays open after a load");

        let saved = Settings::load_from(&dir.join("settings.json"));
        assert_eq!(saved.recent.first(), Some(&dir), "the folder is remembered");
    }

    #[test]
    fn arrows_move_the_selection_only_while_the_browser_owns_the_keyboard() {
        let dir = kit("owner");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown);
        harness.press(Key::ArrowDown);
        assert_eq!(harness.selected().as_deref(), Some("bd.wav"));
        harness.press(Key::ArrowUp);
        assert_eq!(harness.selected().as_deref(), Some("toms"));

        for owner in [KeyOwner::Lanes, KeyOwner::Chop] {
            harness.owner = owner;
            harness.press(Key::ArrowDown);
            assert_eq!(harness.selected().as_deref(), Some("toms"), "{owner:?} owns it");
            harness.press(Key::ArrowUp);
            assert_eq!(harness.selected().as_deref(), Some("toms"), "{owner:?} owns it");
            let out = harness.press(Key::Escape);
            assert!(!out.close, "Escape is not the browser's either under {owner:?}");
        }

        harness.owner = KeyOwner::Browser;
        harness.press(Key::ArrowDown);
        assert_eq!(harness.selected().as_deref(), Some("bd.wav"));
    }

    #[test]
    fn enter_on_a_folder_opens_it_and_backspace_goes_back_up() {
        let dir = kit("folders");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown);
        let out = harness.press(Key::Enter);
        assert_eq!(out.load, None);
        assert_eq!(harness.panel.browser().folder(), Some(dir.join("toms").as_path()));
        harness.press(Key::Backspace);
        assert_eq!(harness.panel.browser().folder(), Some(dir.as_path()));
        assert_eq!(harness.selected().as_deref(), Some("toms"), "the folder left is selected");
    }

    #[test]
    fn escape_closes_the_panel() {
        let dir = kit("escape");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        assert!(harness.press(Key::Escape).close);
    }

    #[test]
    fn auto_preview_plays_a_file_as_it_is_selected() {
        let dir = kit("auto");
        let mut harness = Harness::new(&dir, true);
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown); // the folder: nothing to play
        assert_eq!(harness.previewer.status(), PreviewStatus::Idle);
        harness.press(Key::ArrowDown);
        assert_eq!(settled(&harness.previewer), PreviewStatus::Playing(dir.join("bd.wav")));
    }

    #[test]
    fn without_auto_preview_selecting_plays_nothing() {
        let dir = kit("manual");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown);
        harness.press(Key::ArrowDown);
        assert_eq!(harness.previewer.status(), PreviewStatus::Idle);
    }

    #[test]
    fn retargeting_changes_the_lane_a_load_goes_into() {
        let dir = kit("retarget");
        let mut harness = Harness::new(&dir, false);
        harness.panel.retarget(TargetLane { id: 3, name: "Lane 3", sample: None });
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown);
        harness.press(Key::ArrowDown);
        assert_eq!(harness.press(Key::Enter).load, Some((3, dir.join("bd.wav"))));
    }

    #[test]
    fn a_lane_with_a_sample_opens_on_its_folder_with_it_selected_and_silent() {
        let dir = kit("lane");
        let collector = Collector::new();
        let channel = Arc::new(PreviewChannel::new(&collector.handle()));
        let previewer = Previewer::new(channel, collector.handle());
        let sample = dir.join("sd.wav");
        let target = TargetLane { id: 1, name: "Lane 1", sample: Some(&sample) };
        let panel = BrowserPanel::open_with(target, Settings::default(), None);
        assert_eq!(panel.browser().folder(), Some(dir.as_path()));
        assert_eq!(panel.browser().selected().map(|entry| entry.path.clone()), Some(sample));
        assert_eq!(previewer.status(), PreviewStatus::Idle, "opening plays nothing");
    }

    #[test]
    fn the_chop_button_opens_the_selected_file_in_the_editor() {
        let dir = kit("chop-button");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.press(Key::ArrowDown);
        let on_folder = harness.click(harness.text(CHOP_LABEL));
        assert_eq!(on_folder.open_in_editor, None, "a folder is not opened");

        harness.press(Key::ArrowDown);
        assert_eq!(harness.selected().as_deref(), Some("bd.wav"));
        let out = harness.click(harness.text(CHOP_LABEL));
        assert_eq!(out.open_in_editor, Some(dir.join("bd.wav")));
        assert_eq!(out.load, None, "opening in the editor loads no lane");
        assert!(!out.close);
    }

    #[test]
    fn a_rows_right_click_menu_opens_that_file_in_the_editor() {
        let dir = kit("chop-menu");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        let opened = harness.click_with(harness.text("sd.wav"), egui::PointerButton::Secondary);
        assert_eq!(opened, PanelOutput::default(), "the menu opens; nothing else happens");
        assert_eq!(harness.selected().as_deref(), Some("sd.wav"), "the row is highlighted");

        let out = harness.click(harness.text(OPEN_IN_EDITOR_LABEL));
        assert_eq!(out.open_in_editor, Some(dir.join("sd.wav")));
        assert_eq!(out.load, None);
        assert!(
            !harness.texts.iter().any(|(text, _)| text == OPEN_IN_EDITOR_LABEL),
            "the menu closes after the click"
        );
    }

    #[test]
    fn a_folders_right_click_has_no_menu() {
        let dir = kit("chop-folder");
        let mut harness = Harness::new(&dir, false);
        harness.frame(Vec::new());
        harness.click_with(harness.text("toms"), egui::PointerButton::Secondary);
        assert!(!harness.texts.iter().any(|(text, _)| text == OPEN_IN_EDITOR_LABEL));
    }
}
