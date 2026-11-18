//! The chop editor strip: a panel across the window under the top bar,
//! above the dock and the lane list.
//!
//! So far the skeleton: the header row (collapse toggle, the source's file
//! name, the six buttons), collapse and resize — both kept in the model's
//! [`crate::chop::ChopEditor`], so they survive the window closing — and the
//! body's empty, loading and failed states. With a source open the body is
//! the canvas: the overview row (the whole source, the visible span and the
//! selection), the main view (the waveform at the view's zoom, the
//! selection with its edge handles), the time ruler under it and the
//! selection's readout line under that ([`chop::readout`], at the live
//! host tempo in the model's [`crate::time::TempoCell`]). The
//! wheel over the body is the strip's own: the wheel pans the view (down
//! goes earlier), Ctrl+wheel zooms it around the pointer (down zooms in),
//! sideways scrolling pans with or without Ctrl, and all of it is taken
//! out of egui's input so nothing under the strip scrolls with it. The main
//! view's mouse gestures drag an edge, move the nearer edge to a press inside
//! or outside the selection, start a new selection on a double-click (or with
//! none), or carry it off as an egui drag payload ([`ChopPayload`]) — from
//! its grip, with Ctrl, or by dragging down off the waveform — that the
//! window lands on a row or below the list (`editor/drops.rs`); edges snap
//! unless Shift is held, each gesture that changed the selection is one undo
//! step, and a click with nothing selected reports its frame in
//! [`StripOutput::click`]. On the overview row a click centres the view, a
//! drag on the selection's band moves the whole selection and a drag
//! elsewhere pans the view. While the strip owns the keyboard
//! (`editor/focus.rs`) and no text box has the focus, its keys are read
//! before anything draws: ←/→ nudge the active edge by
//! [`chop::NUDGE_COARSE_MS`] (Shift: [`chop::NUDGE_FINE_MS`]) at the source's
//! rate, ↑/↓ make the start or end the active edge, `Z` toggles snap, `P`
//! plays, `L` toggles loop, `N` moves the selection on by its own length
//! ([`chop::Selection::advance`], refused at the source's end), `Enter` adds
//! the selection as a new lane ([`Model::add_chop_lane`], one undo step) and
//! Escape stops the strip's preview, else clears the selection. Space and
//! every other key Bitwig is sent stay in egui's input. The buttons are drawn
//! and report their clicks in [`StripOutput`]; every one of them acts inside
//! the strip. Previews go through the instance's [`Previewer`]: play sends
//! the selection as a region (looping while loop is on) and its edges follow
//! every change through the live region; a click away from the selection
//! plays from there to the end; either stops a preview that is sounding. The
//! play hairline marks the engine's position while the strip's preview
//! sounds, and a preview of a source the strip no longer has (closed, or
//! another opened) is stopped.
//!
//! The model is the truth for the strip's height: egui's own memory of the
//! panel's size is dropped every frame, so a project load (or another
//! window) that changes the height shows at once, and a drag on the
//! panel's lower edge is written back through [`Model::set_strip`].

use std::path::Path;

use egui::{
    Align, Align2, Button, Color32, FontId, Key, Label, Layout, Margin, Mesh, Modifiers, Painter,
    Pos2, Rect, Response, RichText, Sense, Shape, Stroke, Ui, pos2, vec2,
};

use crate::chop::{
    self, ChopEditor, Edge, PressZone, RULER_MIN_TICK_PX, STRIP_MAX_HEIGHT, STRIP_MIN_HEIGHT,
    Source, View,
};
use crate::editor::drops::ChopPayload;
use crate::editor::focus::KeyOwner;
use crate::editor::theme;
use crate::model::Model;
use crate::preview::{PreviewOwner, Previewer, Region};

/// The panel's id salt.
pub const PANEL_ID: &str = "chopdrop_chop";

/// The strip's height while collapsed: the header row and the hairline
/// under it, nothing else. Also the most of the strip the window's minimum
/// height allows for ([`crate::editor::MIN_HEIGHT`]).
pub const STRIP_HEADER: f32 = 38.0;

/// The header row's own height: the collapsed strip less the hairline the
/// panel's separator takes.
pub const HEADER_ROW: f32 = STRIP_HEADER - theme::HAIRLINE;

/// The header's buttons are this tall: a lane row's control height, so they
/// are as easy to hit (the user's, 2026-11-16: at 19 they were too small to
/// click easily).
pub const HEADER_CONTROL_HEIGHT: f32 = theme::CONTROL_HEIGHT;

/// The space between the header's buttons.
const HEADER_GAP: f32 = 6.0;

/// The least width of a header button, so a short word ("play") is no
/// smaller a target than a long one.
pub const HEADER_BUTTON_MIN_WIDTH: f32 = 64.0;

/// The header buttons' text size.
const HEADER_FONT: f32 = 14.0;

/// The collapse toggle's square, and the triangle drawn in it.
pub const TOGGLE_SIZE: f32 = 24.0;
const TRIANGLE: f32 = 6.0;

/// The space under the body, between the canvas and the hairline.
const BODY_PADDING: f32 = 4.0;

/// The overview row's height: the whole source at a glance.
pub const OVERVIEW_HEIGHT: f32 = 18.0;
/// The time ruler's height, under the main view.
pub const RULER_HEIGHT: f32 = 14.0;
/// The readout line's height, under the ruler.
pub const READOUT_HEIGHT: f32 = 16.0;
/// The space between the overview row and the main view.
pub const OVERVIEW_GAP: f32 = 3.0;

/// The waveform's colour, in the overview and the main view.
const WAVE: Color32 = Color32::from_gray(172);
/// The overview's band for the span the main view shows.
const VISIBLE_BAND: Color32 = Color32::from_gray(52);
/// The selection's fill: translucent over the waveform in the main view,
/// a band behind it in the overview.
const SELECTION_FILL: Color32 = Color32::from_rgba_premultiplied(0x13, 0x23, 0x30, 0x38);
const OVERVIEW_SELECTION: Color32 = Color32::from_rgb(0x2C, 0x4E, 0x6A);
/// The selection's edge lines, the active edge drawn wider, and the tab
/// at the top of each.
const HANDLE_WIDTH: f32 = 1.0;
const ACTIVE_HANDLE_WIDTH: f32 = 2.0;
const HANDLE_TAB: f32 = 6.0;
/// The grip along the bottom of the selection ([`grip_rect`]): its height,
/// the narrowest it is drawn, its fill and its label (shown when it fits).
pub const GRIP_HEIGHT: f32 = 18.0;
const GRIP_MIN_WIDTH: f32 = 12.0;
const GRIP_FILL: Color32 = Color32::from_rgba_premultiplied(0x2C, 0x4E, 0x6A, 0xC0);
const GRIP_FILL_HOVER: Color32 = Color32::from_rgb(0x3A, 0x66, 0x8A);
const GRIP_FONT: f32 = 11.0;
pub const GRIP_LABEL: &str = "drag to a lane";
/// The grip's hover text.
pub const GRIP_HOVER: &str = "drag the selection onto a lane, or below the list for a new one";
/// The ruler's ticks and labels.
const TICK_LENGTH: f32 = 4.0;
const RULER_FONT: f32 = 10.0;
/// The readout line's text size.
const READOUT_FONT: f32 = 11.0;

/// What the strip says with no source open.
pub const EMPTY_HINT: &str = "drop a song or loop here, or open one from the browser";
/// What the strip says while a source decodes.
pub const LOADING_TEXT: &str = "loading…";

/// The header's buttons, left to right.
pub const PLAY_LABEL: &str = "play";
/// The play button's face while the strip's preview sounds.
pub const STOP_LABEL: &str = "stop";
pub const LOOP_LABEL: &str = "loop";
pub const SNAP_LABEL: &str = "snap";
pub const NEXT_LABEL: &str = "next";
pub const ADD_LANE_LABEL: &str = "add lane";
pub const CLOSE_LABEL: &str = "×";

/// The buttons' hover texts.
pub const PLAY_HOVER: &str = "play the selection (A or P)";
pub const STOP_HOVER: &str = "stop the preview (A or P)";
pub const LOOP_HOVER: &str = "loop the preview (L)";
pub const SNAP_HOVER: &str = "snap edges to zero crossings (Z)";
pub const NEXT_HOVER: &str = "move the selection on by its own length (N)";
pub const ADD_LANE_HOVER: &str = "add the selection as a new lane (Enter)";
pub const CLOSE_HOVER: &str = "close the source";

/// The collapse toggle's hover text, expanded and collapsed.
pub const COLLAPSE_HOVER: &str = "hide the chop editor";
pub const EXPAND_HOVER: &str = "show the chop editor";

/// What the strip was asked for this frame, and where it was drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StripOutput {
    /// The whole strip, header and hairline included: the keyboard region
    /// a click inside hands the keys to (`editor/focus.rs`).
    pub rect: Rect,
    /// The waveform canvas, with a source open and the strip expanded.
    pub canvas: Option<Rect>,
    pub play: bool,
    pub looping: bool,
    pub snap: bool,
    pub next: bool,
    pub add_lane: bool,
    /// The lane add lane (the button, `Enter`) made from the selection.
    pub added: Option<u32>,
    pub close: bool,
    /// A click in the main view (press and release without moving, away
    /// from the selection): the frame clicked, for the preview to start
    /// from.
    pub click: Option<u64>,
}

/// What the body shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// No source: the hint.
    Empty,
    /// A source is decoding.
    Loading,
    /// The source could not be opened: the message and the path asked for.
    Failed { message: String, path: String },
    /// A source is open.
    Loaded,
}

/// The body state of `model`'s editor.
pub fn body(model: &Model) -> Body {
    let editor = model.chop();
    if editor.source.is_some() {
        Body::Loaded
    } else if editor.loading {
        Body::Loading
    } else if let Some(message) = &editor.error {
        let path = editor.source_path.clone().unwrap_or_default();
        Body::Failed { message: message.clone(), path }
    } else {
        Body::Empty
    }
}

/// The last component of `path`, or the whole of it.
pub fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map_or_else(|| path.to_owned(), |name| name.to_string_lossy().into_owned())
}

/// The play hairline's colour and width.
const HAIRLINE_COLOUR: Color32 = theme::TEXT;
const PLAY_HAIRLINE_WIDTH: f32 = 1.0;

/// How often to look again for a preview the engine has not yet reported
/// sounding, so its hairline appears without the pointer moving.
const PREVIEW_START_POLL: std::time::Duration = std::time::Duration::from_millis(30);

/// The generation of the preview started from the selection (`P`, the play
/// button), kept in egui's memory: while it is the channel's, every change
/// to the selection's edges goes to the live region.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectionPreview(u64);

/// The generation of the strip's preview the engine was last seen playing,
/// kept in egui's memory, so a preview not yet started is polled for and
/// one that has finished is not.
#[derive(Debug, Clone, Copy, PartialEq)]
struct HeardPreview(u64);

/// What the strip's keys asked for beyond what [`keys`] does itself.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct KeyAsks {
    snap: bool,
    play: bool,
    looping: bool,
    next: bool,
    add_lane: bool,
}

/// Whether the current preview is the strip's (the previewer records who
/// asked).
fn strip_owns(previewer: &Previewer) -> bool {
    previewer.owner() == Some(PreviewOwner::Strip)
}

/// Whether the current preview is the strip's and of the source it has open
/// now: `false` once that source is closed or another is opened.
fn previewing_source(model: &Model, previewer: &Previewer) -> bool {
    if !strip_owns(previewer) {
        return false;
    }
    let state = previewer.channel().get();
    match (&state.sample, &model.chop().source) {
        (Some(sample), Some(source)) => {
            std::ptr::eq::<crate::sample::SampleData>(&**sample, &*source.data)
        }
        _ => false,
    }
}

/// Whether the strip's preview is sounding (as of the engine's last block).
fn source_playing(previewer: &Previewer) -> bool {
    previewer.is_playing() && strip_owns(previewer)
}

/// Draws the strip at the top of what is left of `ui`, before the dock and
/// the lane list, and plays its previews through `previewer`. Its keys are
/// read only while `owner` is the strip.
pub fn show(ui: &mut Ui, model: &mut Model, previewer: &Previewer, owner: KeyOwner) -> StripOutput {
    // Keys first, before a widget under the strip can take them — and only
    // when they are the strip's: a letter typed into a lane's name box is
    // not, nor is an arrow meant for the browser.
    let mut asks = KeyAsks::default();
    if owner.reads_keys(KeyOwner::Chop, ui.ctx().text_edit_focused()) {
        asks = keys(ui, model, previewer);
    }
    // `A` plays or stops the selection from anywhere in the window, not only
    // while the strip has the keyboard (the user's, 2026-11-16: clicking
    // into the strip first was one click too many) — but never while a
    // text box is being typed in.
    if !ui.ctx().text_edit_focused() {
        asks.play |= ui.input_mut(|input| input.count_and_consume_key(Modifiers::NONE, Key::A)) > 0;
    }

    let collapsed = model.chop().collapsed;
    let height = model.chop().height_px;

    // The model, not egui's memory, holds the height: forget the size egui
    // stored last frame so `default_size` (the model's) is the one used.
    // A drag in progress is unaffected — egui sizes it from the pointer.
    let id = ui.scope_id().with(PANEL_ID);
    ui.ctx().data_mut(|data| data.remove::<egui::PanelState>(id));
    let available = ui.available_rect_before_wrap().height();

    let frame = egui::Frame::new()
        .fill(theme::PANEL)
        .inner_margin(Margin::symmetric(theme::PANEL_PADDING as i8, 0));
    let panel = egui::Panel::top(PANEL_ID).frame(frame).show_separator_line(true);
    let panel = if collapsed {
        panel.exact_size(STRIP_HEADER).resizable(false)
    } else {
        panel
            .resizable(true)
            .size_range(STRIP_MIN_HEIGHT..=STRIP_MAX_HEIGHT)
            .default_size(height)
    };

    let mut out = StripOutput {
        rect: Rect::NOTHING,
        canvas: None,
        play: false,
        looping: false,
        snap: false,
        next: false,
        add_lane: false,
        added: None,
        close: false,
        click: None,
    };
    let mut toggle = false;
    let playing = source_playing(previewer);
    let hairline = playing.then(|| previewer.channel().position());
    let shown = panel.show(ui, |ui| {
        toggle = header(ui, model, collapsed, playing, &mut out);
        if !collapsed {
            out.canvas = draw_body(ui, model, hairline, &mut out.click);
        }
    });
    out.rect = shown.response.rect;
    out.snap |= asks.snap;
    out.play |= asks.play;
    out.looping |= asks.looping;
    out.next |= asks.next;
    out.add_lane |= asks.add_lane;

    // The header's settings and actions, applied here — add lane before
    // next, so a frame asking both adds the selection it saw.
    if out.snap {
        let snap = model.chop().snap;
        model.set_snap(!snap);
    }
    if out.add_lane {
        out.added = add_lane(model);
    }
    if out.next {
        next(model);
    }
    if out.close {
        model.close_source();
    }
    hear(ui, model, previewer, &out);

    if toggle {
        model.set_strip(!collapsed, height);
    } else if !collapsed {
        // A drag on the lower edge: the drawn height is not the one asked
        // for (which a window too short for it may cut down, and which is
        // not a drag).
        let drawn = out.rect.height();
        let asked = height.min(available);
        if (drawn - asked).abs() > 0.5 && drawn >= STRIP_MIN_HEIGHT {
            model.set_strip(false, drawn);
        }
    }
    out
}

/// Add lane (the button, `Enter`): the selection, cut from the source, as
/// a new lane at the end of the list ([`Model::add_chop_lane`]), one undo
/// step. Nothing — and `None` — with no source, no selection or a full
/// list.
fn add_lane(model: &mut Model) -> Option<u32> {
    let selection = model.chop().selection.filter(|_| model.chop().source.is_some())?;
    let id = model.add_chop_lane(selection)?;
    model.commit();
    Some(id)
}

/// Next (the button, `N`): the selection moves on by its own length
/// ([`chop::Selection::advance`]), unsnapped, and the view follows it when
/// it would leave the visible span. Refused — nothing changes — with no
/// selection or when the next step would pass the source's end. A preview
/// of the selection follows it through the live region ([`hear`]). One
/// undo step.
fn next(model: &mut Model) {
    let editor = model.chop();
    let total = editor.total();
    let Some(mut selection) = editor.selection.filter(|_| total > 0) else { return };
    if !selection.advance(total) {
        return;
    }
    let (view, width) = (editor.view, editor.view_width_px);
    let shown_to = view.frame_at(width.max(1.0));
    let followed = if selection.start < view.first || selection.end > shown_to {
        let centre = selection.start + selection.len() / 2;
        Some(view.centred_on(centre, total, width))
    } else {
        None
    };
    model.set_selection(Some(selection));
    model.commit();
    if let Some(view) = followed.filter(|followed| *followed != view) {
        model.set_view(view, width);
    }
}

/// What the strip's preview does this frame, after its gestures: play (the
/// button, `P`) plays the selection — looping if loop is on — or stops the
/// strip's preview if it is sounding; loop (the button, `L`) flips the
/// setting and the live loop flag of whatever region is playing; a click in
/// the main view plays from its frame to the end, or stops. A preview started
/// from the selection follows its edges through the live region. A preview of
/// a source that is no longer the strip's (closed, or another opened) is
/// stopped.
fn hear(ui: &Ui, model: &mut Model, previewer: &Previewer, out: &StripOutput) {
    let channel = previewer.channel();
    let following_id = ui.scope_id().with("chopdrop_chop_selection_preview");
    let heard_id = ui.scope_id().with("chopdrop_chop_heard_preview");

    if out.looping && model.chop().source.is_some() {
        let looping = !model.chop().looping;
        model.set_looping(looping);
        channel.set_live_looping(looping);
    }
    let playing = source_playing(previewer);
    let editor = model.chop();
    let asked = out.play || out.click.is_some();
    if let Some(source) = editor.source.as_ref().filter(|_| asked) {
        if playing {
            previewer.stop();
        } else if let Some(frame) = out.click {
            previewer.play_from(source.data.clone(), frame);
        } else if let Some(selection) = editor.selection {
            let region = Region { start: selection.start, end: selection.end };
            previewer.play_region(source.data.clone(), region, editor.looping);
            let following = SelectionPreview(channel.get().generation);
            ui.ctx().data_mut(|data| data.insert_temp(following_id, following));
        } else {
            // Nothing selected: what the view shows onwards.
            previewer.play_from(source.data.clone(), editor.view.first);
        }
    }

    let state = channel.get();
    let editor = model.chop();
    // The selection's edges, live, for the preview started from it.
    let following = ui.ctx().data(|data| data.get_temp::<SelectionPreview>(following_id));
    if let (Some(SelectionPreview(generation)), Some(selection)) = (following, editor.selection) {
        let region = Region { start: selection.start, end: selection.end };
        if generation == state.generation && channel.live_region() != region {
            channel.set_live_region(region);
        }
    }
    // The strip's preview of a source it no longer has: stop it.
    if strip_owns(previewer) && !previewing_source(model, previewer) {
        drop(state);
        previewer.stop();
        return;
    }
    // Repaint while the strip's preview sounds, for the hairline — and
    // poll for one the engine has not started yet.
    if previewing_source(model, previewer) {
        let heard = ui.ctx().data(|data| data.get_temp::<HeardPreview>(heard_id));
        if previewer.is_playing() {
            ui.ctx().data_mut(|data| data.insert_temp(heard_id, HeardPreview(state.generation)));
            ui.ctx().request_repaint();
        } else if heard != Some(HeardPreview(state.generation)) {
            ui.ctx().request_repaint_after(PREVIEW_START_POLL);
        }
    }
}

/// The strip's keys, taken out of egui's input. Acts on the selection itself,
/// and Escape stops the strip's preview before it clears the selection;
/// returns what `Z`, `P`, `L` and `N` and `Enter` asked for (applied with the
/// header's buttons).
fn keys(ui: &Ui, model: &mut Model, previewer: &Previewer) -> KeyAsks {
    let (fine_left, fine_right, left, right, up, down, z, escape, p, l, n, enter) =
        ui.input_mut(|input| {
        // Shift's presses first: a plain match takes a Shifted key too.
        let mut take = |modifiers, key| input.count_and_consume_key(modifiers, key);
        (
            take(Modifiers::SHIFT, Key::ArrowLeft),
            take(Modifiers::SHIFT, Key::ArrowRight),
            take(Modifiers::NONE, Key::ArrowLeft),
            take(Modifiers::NONE, Key::ArrowRight),
            take(Modifiers::NONE, Key::ArrowUp) > 0,
            take(Modifiers::NONE, Key::ArrowDown) > 0,
            take(Modifiers::NONE, Key::Z) > 0,
            take(Modifiers::NONE, Key::Escape) > 0,
            take(Modifiers::NONE, Key::P) > 0,
            take(Modifiers::NONE, Key::L) > 0,
            take(Modifiers::NONE, Key::N) > 0,
            take(Modifiers::NONE, Key::Enter) > 0,
        )
    });

    let editor = model.chop();
    let rate = editor.source.as_ref().map(|source| source.data.rate);
    let (total, selection) = (editor.total(), editor.selection);
    if let (Some(rate), Some(mut selection)) = (rate, selection) {
        let mut changed = false;
        if up || down {
            // Both in one frame: the end wins (their order is not kept).
            selection.active = if down { Edge::End } else { Edge::Start };
            changed = true;
        }
        let fine = chop::nudge_frames(chop::NUDGE_FINE_MS, rate);
        let coarse = chop::nudge_frames(chop::NUDGE_COARSE_MS, rate);
        let delta = (right as i64 - left as i64) * coarse
            + (fine_right as i64 - fine_left as i64) * fine;
        if delta != 0 {
            let edge = selection.active;
            let at = match edge {
                Edge::Start => selection.start,
                Edge::End => selection.end,
            };
            selection.set_edge(edge, at.saturating_add_signed(delta), total);
            changed = true;
        }
        if changed {
            model.set_selection(Some(selection));
            // Each frame's nudges are one undo step (picking an edge none).
            model.commit();
        }
    }
    if escape {
        if source_playing(previewer) {
            previewer.stop();
        } else if selection.is_some() {
            model.set_selection(None);
            model.commit();
        }
    }
    KeyAsks { snap: z, play: p, looping: l, next: n, add_lane: enter }
}

/// The header row, its play button saying stop while `playing`. Returns
/// whether the collapse toggle was clicked.
fn header(
    ui: &mut Ui,
    model: &Model,
    collapsed: bool,
    playing: bool,
    out: &mut StripOutput,
) -> bool {
    let editor = model.chop();
    let has_source = editor.source.is_some();
    let can_add = has_source
        && editor.selection.is_some()
        && model.lanes().len() < crate::model::MAX_LANES as usize;
    let can_close = editor.source_path.is_some();
    let name = editor.source_path.as_deref().map(|path| (file_name(path), path.to_owned()));
    let (looping, snap) = (editor.looping, editor.snap);

    let size = vec2(ui.available_width(), HEADER_ROW);
    let row = ui.allocate_ui_with_layout(size, Layout::left_to_right(Align::Center), |ui| {
        ui.set_min_size(size);
        let spacing = ui.spacing_mut();
        spacing.interact_size.y = HEADER_CONTROL_HEIGHT;
        spacing.button_padding = vec2(theme::CONTROL_PADDING.x, 1.0);
        spacing.item_spacing.x = HEADER_GAP;

        let toggle = collapse_toggle(ui, collapsed);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let button = |ui: &mut Ui, label: &str, hover: &str, lit: bool, enabled: bool| {
                let text = RichText::new(label).size(HEADER_FONT);
                // × is a square; the words get the same least width.
                let width = if label == CLOSE_LABEL { 0.0 } else { HEADER_BUTTON_MIN_WIDTH };
                let size = vec2(width.max(HEADER_CONTROL_HEIGHT), HEADER_CONTROL_HEIGHT);
                let button = Button::new(text).selected(lit).min_size(size);
                ui.add_enabled(enabled, button).on_hover_text(hover).clicked()
            };
            // × closes whatever there is: a source, a load or a failure.
            // Snap is a setting, not an action on the source: always on hand.
            out.close = button(ui, CLOSE_LABEL, CLOSE_HOVER, false, can_close);
            out.add_lane = button(ui, ADD_LANE_LABEL, ADD_LANE_HOVER, false, can_add);
            out.next = button(ui, NEXT_LABEL, NEXT_HOVER, false, has_source);
            out.snap = button(ui, SNAP_LABEL, SNAP_HOVER, snap, true);
            out.looping = button(ui, LOOP_LABEL, LOOP_HOVER, looping, has_source);
            let (play, play_hover) =
                if playing { (STOP_LABEL, STOP_HOVER) } else { (PLAY_LABEL, PLAY_HOVER) };
            out.play = button(ui, play, play_hover, playing, has_source);

            // What is left goes to the name, from the left.
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| match &name {
                Some((name, path)) => {
                    ui.add(Label::new(RichText::new(name).color(theme::TEXT)).truncate())
                        .on_hover_text(path);
                }
                // Collapsed, the body's hint is hidden: the header says it.
                None if collapsed => {
                    ui.add(Label::new(RichText::new(EMPTY_HINT).color(theme::TEXT_DIM)).truncate());
                }
                None => {}
            });
        });
        toggle
    });
    row.inner
}

/// The hand-drawn ▸/▾ toggle (the fonts have no such glyphs). Returns
/// whether it was clicked.
fn collapse_toggle(ui: &mut Ui, collapsed: bool) -> bool {
    let (rect, response) = ui.allocate_exact_size(vec2(TOGGLE_SIZE, TOGGLE_SIZE), Sense::click());
    let colour = if response.hovered() { theme::TEXT } else { theme::TEXT_DIM };
    if ui.is_rect_visible(rect) {
        let points = triangle(rect.center(), collapsed);
        ui.painter().add(Shape::convex_polygon(points, colour, Stroke::NONE));
    }
    response.on_hover_text(if collapsed { EXPAND_HOVER } else { COLLAPSE_HOVER }).clicked()
}

/// The toggle's triangle around `centre`: pointing right while collapsed,
/// down while expanded, clockwise.
pub fn triangle(centre: Pos2, collapsed: bool) -> Vec<Pos2> {
    let (x, y, r) = (centre.x, centre.y, TRIANGLE);
    if collapsed {
        vec![pos2(x - r * 0.6, y - r), pos2(x + r * 0.8, y), pos2(x - r * 0.6, y + r)]
    } else {
        vec![pos2(x - r, y - r * 0.6), pos2(x + r, y - r * 0.6), pos2(x, y + r * 0.8)]
    }
}

/// The body under the header: one of the three messages, or the canvas
/// the waveform is painted into, which is returned.
fn draw_body(
    ui: &mut Ui,
    model: &mut Model,
    hairline: Option<u64>,
    click: &mut Option<u64>,
) -> Option<Rect> {
    // The whole of what is left is taken, padding included: the panel is
    // as tall as its contents, so a body that took less would shrink it.
    let full = ui.available_rect_before_wrap();
    if full.height() <= 0.0 || full.width() <= 0.0 {
        return None;
    }
    let response = ui.allocate_rect(full, Sense::hover());
    let wheel = take_wheel(ui, &response);
    let mut rect = full;
    rect.max.y -= BODY_PADDING;
    if rect.height() <= 0.0 {
        return None;
    }
    let painter = ui.painter_at(rect);
    let font = FontId::proportional(theme::HINT_SIZE);
    let centre = rect.center();
    match body(model) {
        Body::Empty => {
            painter.text(centre, Align2::CENTER_CENTER, EMPTY_HINT, font, theme::TEXT_DIM);
            None
        }
        Body::Loading => {
            painter.text(centre, Align2::CENTER_CENTER, LOADING_TEXT, font, theme::TEXT_DIM);
            None
        }
        Body::Failed { message, path } => {
            let gap = theme::HINT_SIZE * 0.7;
            let (above, below) = (centre - vec2(0.0, gap), centre + vec2(0.0, gap));
            painter.text(above, Align2::CENTER_CENTER, message, font, theme::WARN);
            let small = FontId::proportional(theme::HINT_SIZE * 0.8);
            painter.text(below, Align2::CENTER_CENTER, path, small, theme::TEXT_DIM);
            None
        }
        Body::Loaded => {
            let layout = canvas_layout(rect);
            fit_width(model, layout.main.width());
            if let Some(wheel) = wheel {
                apply_wheel(model, wheel, layout.main);
            }
            overview_gestures(ui, model, layout.overview);
            *click = main_gestures(ui, model, layout.main);
            // The host's tempo as the audio thread last stored it (one relaxed
            // load).
            let tempo = model.tempo().get();
            let editor = model.chop();
            if let Some(source) = &editor.source {
                paint_overview(&painter, layout.overview, source, editor);
                // The ruler's grid lines go under the waveform.
                if layout.main.height() >= 1.0 {
                    painter.rect_filled(layout.main, theme::CELL_CORNER, theme::WINDOW);
                }
                paint_ruler(&painter, layout.ruler, layout.main, source, &editor.view);
                // The grip lit and the hand shown while the pointer is on it.
                let hover = ui.input(|input| input.pointer.hover_pos());
                let grip = grip_rect(layout.main, editor);
                let grip_hovered = grip.zip(hover).is_some_and(|(grip, at)| grip.contains(at));
                if grip_hovered {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
                paint_main(&painter, layout.main, source, editor, hairline, grip_hovered);
                if let Some(selection) = &editor.selection {
                    paint_readout(&painter, layout.readout, source, selection, tempo);
                }
            }
            Some(rect)
        }
    }
}

/// Where the canvas's rows go.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasLayout {
    /// The whole source at a glance, along the top.
    pub overview: Rect,
    /// The waveform at the view's zoom: its width is the view's.
    pub main: Rect,
    /// The time ruler, under the main view.
    pub ruler: Rect,
    /// The selection's readout line, along the bottom.
    pub readout: Rect,
}

/// Splits `canvas` into the overview row, the main view, the ruler and the
/// readout line. A canvas too short for all of them squeezes the main view
/// first, then the ruler, then the readout; the rows never overlap or leave
/// the canvas.
pub fn canvas_layout(canvas: Rect) -> CanvasLayout {
    let (top, bottom) = (canvas.min.y, canvas.max.y);
    let overview_bottom = (top + OVERVIEW_HEIGHT).min(bottom);
    let main_top = (overview_bottom + OVERVIEW_GAP).min(bottom);
    let readout_top = (bottom - READOUT_HEIGHT).max(main_top);
    let ruler_top = (readout_top - RULER_HEIGHT).max(main_top);
    let row = |y0: f32, y1: f32| Rect::from_x_y_ranges(canvas.x_range(), y0..=y1);
    CanvasLayout {
        overview: row(top, overview_bottom),
        main: row(main_top, ruler_top),
        ruler: row(ruler_top, readout_top),
        readout: row(readout_top, bottom),
    }
}

/// What the wheel did over the strip's body this frame.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Wheel {
    /// The pointer's x, in screen points.
    pointer_x: f32,
    /// How far to pan, in points ([`View::pan`]: positive goes later).
    pan: f32,
    /// What to multiply the view's frames per pixel by ([`View::zoom_at`]:
    /// above 1 zooms out).
    zoom: f64,
}

/// The wheel over the body, taken out of egui's input so that nothing
/// under the strip (the lane list) scrolls with it — the row fields' pattern
/// (`lanes::wheel_steps`). Taken every frame the pointer is over the body,
/// because egui hands a notch over in instalments.
///
/// The directions are the user's (2026-11-16). The **vertical** wheel is
/// the reverse of egui's scroll-area habit: **down goes earlier**, up
/// later, and with Ctrl **down zooms in**, up zooms out. **Sideways**
/// scrolling is reversed too: **left goes earlier**, right later, with or
/// without Ctrl (the user tried egui's way first and asked for this).
///
/// With Ctrl held egui folds *both* axes of the wheel into its zoom factor
/// and scrolls nothing, which would leave a sideways scroll (a touchpad, a
/// tilting wheel) zooming too. So for Ctrl the strip reads the raw wheel
/// events instead, by each event's own modifiers as egui does
/// ([`ctrl_wheel`]): the vertical part zooms, the sideways part pans as it
/// does without Ctrl. egui's zoom factor is not read at all — it would
/// replay a Ctrl notch the raw events already applied as egui smooths it
/// over the next frames, and the pinch it also carries never reaches a
/// plugin window on X11.
fn take_wheel(ui: &Ui, body: &Response) -> Option<Wheel> {
    if !body.contains_pointer() {
        return None;
    }
    let pointer = ui.input(|input| input.pointer.hover_pos())?;
    let (line, zoom_speed) = ui.ctx().options(|options| {
        (options.input_options.line_scroll_speed, options.input_options.scroll_zoom_speed)
    });
    let (pan, zoom) = ui.input_mut(|input| {
        // Without Ctrl, egui's smoothed scroll; with it, nothing here.
        let scroll = std::mem::take(&mut input.smooth_scroll_delta);
        let page = input.viewport_rect().height();
        let (sideways, vertical) = ctrl_wheel(&input.events, line, page);
        let zoom = (f64::from(zoom_speed) * f64::from(vertical)).exp();
        (scroll.y + scroll.x + sideways, zoom)
    });
    let moved = pan != 0.0 || zoom != 1.0;
    moved.then_some(Wheel { pointer_x: pointer.x, pan, zoom })
}

/// The Ctrl-held wheel events of one frame, in points: `(sideways,
/// vertical)`. Lines and pages convert the way egui converts them
/// (`line_scroll_speed` points a line, the viewport's height a page), so a
/// notch zooms by as much as egui's own Ctrl+scroll would.
fn ctrl_wheel(events: &[egui::Event], line: f32, page: f32) -> (f32, f32) {
    events.iter().fold((0.0, 0.0), |(x, y), event| match event {
        egui::Event::MouseWheel { unit, delta, modifiers, .. } if modifiers.command => {
            let points = match unit {
                egui::MouseWheelUnit::Point => *delta,
                egui::MouseWheelUnit::Line => *delta * line,
                egui::MouseWheelUnit::Page => *delta * page,
            };
            (x + points.x, y + points.y)
        }
        _ => (x, y),
    })
}

/// The view after `wheel` over the main view at `main`: zoomed around the
/// pointer ([`View::zoom_at`]), then panned ([`View::pan`]).
fn wheeled_view(view: View, wheel: Wheel, main: Rect, total: u64) -> View {
    let mut view = view;
    let width = main.width();
    if wheel.zoom.is_finite() && wheel.zoom > 0.0 && wheel.zoom != 1.0 {
        view.zoom_at(wheel.pointer_x - main.min.x, wheel.zoom, total, width);
    }
    if wheel.pan.is_finite() && wheel.pan != 0.0 {
        view.pan(wheel.pan, total, width);
    }
    view
}

/// Writes the wheel's view back through [`Model::set_view`].
fn apply_wheel(model: &mut Model, wheel: Wheel, main: Rect) {
    if main.width() < 1.0 {
        return;
    }
    let editor = model.chop();
    let view = wheeled_view(editor.view, wheel, main, editor.total());
    if view != editor.view {
        model.set_view(view, main.width());
    }
}

/// What a press in the main view started.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Gesture {
    /// A new selection from `anchor` (the unsnapped frame pressed).
    NewSelection { anchor: u64 },
    /// Dragging one edge.
    Edge(Edge),
    /// The drag onto a lane, carried as a [`ChopPayload`]: pressed on the
    /// grip, pressed inside with Ctrl held, or an edge press from inside
    /// dragged down off the waveform.
    Carry,
}

/// A press in the main view, kept in egui's memory until its release.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Press {
    gesture: Gesture,
    origin: Pos2,
    /// Whether it has become a drag (else its release is a click).
    moved: bool,
    /// The selection before the press moved an edge: what a drag down off
    /// the waveform puts back and carries instead.
    before: Option<chop::Selection>,
    /// Pressed inside the selection away from its edges and its grip, so a
    /// drag down off the waveform turns into the carry.
    carries_down: bool,
    /// The second press of a double-click: its release plays nothing.
    double: bool,
}

/// The last click in the main view (a press released without moving), kept
/// in egui's memory so the next press can tell it is a double-click.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LastClick {
    /// egui's clock at the release.
    time: f64,
    pos: Pos2,
    /// Whether the click made an undo step (it moved an edge), which a
    /// double-click takes back before it starts afresh.
    stepped: bool,
}

/// The frame under screen x `x` in the main view at `main`, inside the
/// source.
fn frame_under(editor: &ChopEditor, main: Rect, x: f32) -> u64 {
    editor.view.frame_at(x - main.min.x).min(editor.total())
}

/// `frame` snapped to the nearest zero crossing when `snap` is on.
fn snapped(editor: &ChopEditor, frame: u64, snap: bool) -> u64 {
    match &editor.source {
        Some(source) if snap => {
            let window = chop::snap_window_frames(source.data.rate);
            chop::snap_zero_crossing(&source.data, frame, window).min(editor.total())
        }
        _ => frame,
    }
}

/// The grip along the bottom of the selection in the main view at `main`: a
/// plain drag from it carries the chop onto a lane. Clear of the edges' grab
/// zones; `None` when the selection is too narrow on screen to hold one, or
/// off it.
pub fn grip_rect(main: Rect, editor: &ChopEditor) -> Option<Rect> {
    let selection = editor.selection?;
    let view = &editor.view;
    let left = (main.min.x + view.px_of(selection.start) + chop::EDGE_GRAB_PX).max(main.min.x);
    let right = (main.min.x + view.px_of(selection.end) - chop::EDGE_GRAB_PX).min(main.max.x);
    let height = GRIP_HEIGHT.min(main.height() / 3.0);
    let fits = right - left >= GRIP_MIN_WIDTH && height >= 4.0;
    fits.then(|| Rect::from_x_y_ranges(left..=right, main.max.y - height..=main.max.y))
}

/// Whether a press at `origin` follows the last click closely enough, in
/// time and place, to be a double-click (egui's own limits).
fn is_double(ui: &Ui, last: Option<LastClick>, origin: Pos2) -> bool {
    let now = ui.input(|input| input.time);
    let (delay, distance) = ui.ctx().options(|options| {
        let input = &options.input_options;
        (input.max_double_click_delay, input.max_click_dist)
    });
    last.is_some_and(|last| now - last.time <= delay && last.pos.distance(origin) <= distance)
}

/// The main view's mouse gestures, told apart by where the press lands
/// ([`chop::press_zone`]):
///
/// - near an edge drags that edge;
/// - on the grip along the selection's bottom ([`grip_rect`]), or inside it
///   with Ctrl held, is the drag onto a lane (an egui drag payload,
///   [`ChopPayload`], landed by the window — a release over the strip does
///   nothing);
/// - anywhere else **with a selection** moves the nearer edge to the press
///   at once and drags it from there — inside it trims, outside it extends
///   (the user's, 2026-11-16: zoomed in, an edge is often off screen, and
///   reaching it lost their place). Pressed inside, dragging down off the
///   waveform puts the edge back and carries the chop onto a lane instead;
/// - with **no** selection, drags out a new one;
/// - a **double-click** anywhere clears the selection (taking back what the
///   first click moved) and drags out a new one from there.
///
/// Edges snap to zero crossings while the drag goes and on release when
/// snap is on and Shift is not held. Each press that changed the selection
/// is one undo step at its release. Returns the frame of a click: a press
/// with no selection released without moving (not a double-click's).
fn main_gestures(ui: &Ui, model: &mut Model, main: Rect) -> Option<u64> {
    if main.width() < 1.0 || main.height() < 1.0 {
        return None;
    }
    let id = ui.scope_id().with("chopdrop_chop_main");
    let click_id = ui.scope_id().with("chopdrop_chop_last_click");
    let response = ui.interact(main, id, Sense::click_and_drag());
    let (pressed, released, down, dragging, pos, shift, ctrl) = ui.input(|input| {
        let pointer = &input.pointer;
        (
            pointer.primary_pressed(),
            pointer.primary_released(),
            pointer.primary_down(),
            pointer.is_decidedly_dragging(),
            pointer.interact_pos(),
            input.modifiers.shift,
            input.modifiers.command,
        )
    });
    let mut press: Option<Press> = ui.ctx().data(|data| data.get_temp(id));
    let last_click: Option<LastClick> = ui.ctx().data(|data| data.get_temp(click_id));

    if pressed {
        press = None;
        let origin = ui.input(|input| input.pointer.press_origin()).or(pos);
        if let Some(origin) = origin.filter(|_| response.is_pointer_button_down_on()) {
            let double = is_double(ui, last_click, origin);
            ui.ctx().data_mut(|data| data.remove::<LastClick>(click_id));
            if double {
                // The first click moved an edge: take that back, then clear.
                if last_click.is_some_and(|last| last.stepped) {
                    model.undo();
                }
                model.set_selection(None);
            }
            let editor = model.chop();
            let px = origin.x - main.min.x;
            let under = frame_under(editor, main, origin.x);
            let zone = chop::press_zone(editor.selection.as_ref(), &editor.view, px);
            let on_grip = grip_rect(main, editor).is_some_and(|grip| grip.contains(origin));
            let gesture = match (zone, editor.selection) {
                _ if double => Gesture::NewSelection { anchor: under },
                (PressZone::Edge(edge), _) => Gesture::Edge(edge),
                (PressZone::Inside, _) if ctrl || on_grip => Gesture::Carry,
                (_, Some(selection)) => Gesture::Edge(selection.nearer_edge(under)),
                (_, None) => Gesture::NewSelection { anchor: under },
            };
            let before = editor.selection;
            if let (Gesture::Edge(edge), Some(selection)) = (gesture, before) {
                model.set_selection(Some(chop::Selection { active: edge, ..selection }));
            }
            // A press away from the edges moves its edge where it landed,
            // before any drag: a click alone is enough to trim or extend.
            let moves_edge =
                matches!(gesture, Gesture::Edge(_)) && !matches!(zone, PressZone::Edge(_));
            if moves_edge {
                drag_to(model, main, gesture, origin.x, !shift);
            }
            let carries_down = moves_edge && zone == PressZone::Inside;
            press = Some(Press { gesture, origin, moved: false, before, carries_down, double });
        }
    }

    // Carried, the press becomes the drag onto a lane once it moves (egui
    // sets the payload on the frame its drag starts); the rows and the
    // space below the list read it (`editor/drops.rs`).
    if let (Some(Press { gesture: Gesture::Carry, .. }), Some(selection)) =
        (press, model.chop().selection)
    {
        response.dnd_set_drag_payload(ChopPayload { start: selection.start, end: selection.end });
    }

    let mut click = None;
    if let Some(mut current) = press {
        let moving = pos.filter(|pos| (down || released) && dragging && *pos != current.origin);
        if let Some(pos) = moving {
            current.moved = true;
            if current.carries_down && pos.y > main.max.y {
                // Down off the waveform from inside: the edge goes back and
                // the selection as it was is carried instead.
                current.gesture = Gesture::Carry;
                current.carries_down = false;
                model.set_selection(current.before);
                if let Some(selection) = current.before {
                    let payload = ChopPayload { start: selection.start, end: selection.end };
                    egui::DragAndDrop::set_payload(ui.ctx(), payload);
                }
            } else {
                drag_to(model, main, current.gesture, pos.x, !shift);
            }
        }
        if down {
            press = Some(current);
        } else {
            // One undo step for whatever the press did to the selection.
            let stepped = model.commit();
            if released && !current.moved {
                if let (Gesture::NewSelection { anchor }, false) = (current.gesture, current.double)
                {
                    click = Some(anchor);
                }
                if !current.double {
                    let time = ui.input(|input| input.time);
                    let last = LastClick { time, pos: current.origin, stepped };
                    ui.ctx().data_mut(|data| data.insert_temp(click_id, last));
                }
            }
            press = None;
        }
    }
    ui.ctx().data_mut(|data| match press {
        Some(press) => {
            data.insert_temp(id, press);
        }
        None => data.remove::<Press>(id),
    });
    click
}

/// What a press on the overview row grabbed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OverviewGrab {
    /// The selection's band: a drag moves this selection (as pressed).
    Selection(chop::Selection),
    /// Anywhere else: a drag pans this view (as pressed).
    View(View),
}

/// A press on the overview row, kept in egui's memory until its release.
#[derive(Debug, Clone, Copy, PartialEq)]
struct OverviewPress {
    grab: OverviewGrab,
    origin: Pos2,
    /// Whether it has become a drag (else its release is a click).
    moved: bool,
}

/// The overview row's mouse gestures: a click (press and release without
/// moving) centres the main view on the frame clicked; a drag that starts on
/// the selection's band ([`chop::on_overview_band`]) moves the whole
/// selection, keeping its length and unsnapped; a drag anywhere else pans the
/// view, the visible band following the pointer. Both drags are measured from
/// the press, at the overview's scale (the whole source across its width).
fn overview_gestures(ui: &Ui, model: &mut Model, overview: Rect) {
    let width = overview.width();
    if width < 1.0 || overview.height() < 1.0 || model.chop().total() == 0 {
        return;
    }
    let id = ui.scope_id().with("chopdrop_chop_overview");
    let response = ui.interact(overview, id, Sense::click_and_drag());
    let (pressed, released, down, dragging, pos) = ui.input(|input| {
        let pointer = &input.pointer;
        (
            pointer.primary_pressed(),
            pointer.primary_released(),
            pointer.primary_down(),
            pointer.is_decidedly_dragging(),
            pointer.interact_pos(),
        )
    });
    let mut press: Option<OverviewPress> = ui.ctx().data(|data| data.get_temp(id));

    if pressed {
        press = None;
        let origin = ui.input(|input| input.pointer.press_origin()).or(pos);
        if let Some(origin) = origin.filter(|_| response.is_pointer_button_down_on()) {
            let editor = model.chop();
            let px = origin.x - overview.min.x;
            let total = editor.total();
            let grab = match editor.selection {
                Some(selection) if chop::on_overview_band(&selection, px, width, total) => {
                    OverviewGrab::Selection(selection)
                }
                _ => OverviewGrab::View(editor.view),
            };
            press = Some(OverviewPress { grab, origin, moved: false });
        }
    }

    if let Some(mut current) = press {
        let editor = model.chop();
        let total = editor.total();
        let moving = pos.filter(|pos| (down || released) && dragging && *pos != current.origin);
        if let Some(pos) = moving {
            current.moved = true;
            let delta = chop::overview_frames(pos.x - current.origin.x, width, total);
            match current.grab {
                OverviewGrab::Selection(mut selection) => {
                    selection.shift(delta, total);
                    if editor.selection != Some(selection) {
                        model.set_selection(Some(selection));
                    }
                }
                OverviewGrab::View(view) => {
                    let view_width = editor.view_width_px;
                    let view = view.scrolled(delta, total, view_width);
                    if view != editor.view {
                        model.set_view(view, view_width);
                    }
                }
            }
        }
        if down {
            press = Some(current);
        } else {
            if let (OverviewGrab::Selection(_), true) = (current.grab, current.moved) {
                // Moving the selection is one undo step.
                model.commit();
            }
            if released && !current.moved {
                let editor = model.chop();
                let frame = chop::overview_frame(current.origin.x - overview.min.x, width, total);
                let view_width = editor.view_width_px;
                let view = editor.view.centred_on(frame, total, view_width);
                if view != editor.view {
                    model.set_view(view, view_width);
                }
            }
            press = None;
        }
    }
    ui.ctx().data_mut(|data| match press {
        Some(press) => {
            data.insert_temp(id, press);
        }
        None => data.remove::<OverviewPress>(id),
    });
}

/// Moves `gesture`'s selection to the pointer at screen x `x`, snapping
/// when the editor's snap is on and `allow_snap` (Shift is not held).
fn drag_to(model: &mut Model, main: Rect, gesture: Gesture, x: f32, allow_snap: bool) {
    let editor = model.chop();
    let snap = editor.snap && allow_snap;
    let total = editor.total();
    let frame = snapped(editor, frame_under(editor, main, x), snap);
    let selection = match gesture {
        Gesture::NewSelection { anchor } => {
            chop::drag_selection(snapped(editor, anchor, snap), frame, total)
        }
        Gesture::Edge(edge) => {
            let Some(mut selection) = editor.selection else { return };
            selection.set_edge(edge, frame, total);
            selection
        }
        Gesture::Carry => return,
    };
    if editor.selection != Some(selection) {
        model.set_selection(Some(selection));
    }
}

/// Keeps the view's width the main view's: a view that showed the whole
/// source still does at the new width; any other keeps its scroll and zoom
/// (clamped).
fn fit_width(model: &mut Model, width: f32) {
    let editor = model.chop();
    if width < 1.0 || (editor.view_width_px - width).abs() <= 0.5 {
        return;
    }
    let total = editor.total();
    let fitted = View::fit(total, editor.view_width_px);
    let view = if editor.view.frames_per_pixel >= fitted.frames_per_pixel {
        View::fit(total, width)
    } else {
        editor.view
    };
    model.set_view(view, width);
}

/// `value` (a sample, -1…1 nominal) as a height in `rect`, NaN at the
/// centre line and anything louder at the edge.
fn sample_y(rect: Rect, value: f32) -> f32 {
    let value = if value.is_nan() { 0.0 } else { value.clamp(-1.0, 1.0) };
    rect.center().y - value * (rect.height() / 2.0 - 1.0).max(0.0)
}

/// One pixel column of waveform from `lo` to `hi`, at least a pixel tall.
fn wave_column(mesh: &mut Mesh, rect: Rect, x: f32, (lo, hi): (f32, f32)) {
    let (top, bottom) = (sample_y(rect, hi), sample_y(rect, lo));
    let bottom = bottom.max(top + 1.0);
    let column = Rect::from_min_max(pos2(x, top), pos2(x + 1.0, bottom));
    mesh.add_colored_rect(column, WAVE);
}

/// How many whole pixel columns `rect` holds.
fn columns(rect: Rect) -> u32 {
    let width = rect.width();
    if width.is_finite() && width >= 1.0 { width.ceil() as u32 } else { 0 }
}

/// The overview row: the whole source from the coarsest peak level, the
/// span the main view shows as a lighter band, the selection as an accent
/// band.
fn paint_overview(painter: &Painter, rect: Rect, source: &Source, editor: &ChopEditor) {
    if rect.height() < 1.0 || columns(rect) == 0 {
        return;
    }
    painter.rect_filled(rect, theme::CELL_CORNER, theme::WINDOW);
    let total = source.total();
    if total == 0 {
        return;
    }
    let width = rect.width();
    let x_of = |frame: u64| rect.min.x + (frame as f64 / total as f64 * width as f64) as f32;
    let band = |from: u64, to: u64, colour: Color32| {
        let (x0, x1) = (x_of(from), x_of(to).max(x_of(from) + 1.0));
        let band = Rect::from_x_y_ranges(x0..=x1.min(rect.max.x), rect.y_range());
        painter.rect_filled(band, 0.0, colour);
    };
    let view = &editor.view;
    let visible_end = view.frame_at(editor.view_width_px).min(total);
    band(view.first.min(total), visible_end, VISIBLE_BAND);
    if let Some(selection) = &editor.selection {
        band(selection.start, selection.end, OVERVIEW_SELECTION);
    }

    let peaks = &source.peaks;
    let Some(level) = peaks.levels().last() else { return };
    let mut mesh = Mesh::default();
    for i in 0..columns(rect) {
        let from = (i as f64 / width as f64 * total as f64) as u64;
        let to = ((i + 1) as f64 / width as f64 * total as f64) as u64;
        if from >= total {
            break;
        }
        let range = peaks.min_max(level, from, to.max(from + 1));
        wave_column(&mut mesh, rect, rect.min.x + i as f32, range);
    }
    painter.add(Shape::mesh(mesh));
}

/// The main view, over its background: the centre line, the waveform at the view's zoom (raw
/// samples once zoomed past the finest peak level), the selection's fill
/// and its edge handles, and the play hairline at `hairline` (the
/// preview's frame) while the strip's preview sounds.
fn paint_main(
    painter: &Painter,
    rect: Rect,
    source: &Source,
    editor: &ChopEditor,
    hairline: Option<u64>,
    grip_hovered: bool,
) {
    if rect.height() < 1.0 || columns(rect) == 0 {
        return;
    }
    let centre = rect.center().y;
    let line = Stroke::new(theme::HAIRLINE, theme::GRID_LINE);
    painter.hline(rect.x_range(), centre, line);

    let view = &editor.view;
    let total = source.total();
    let peaks = &source.peaks;
    let level = peaks.level_for(view.frames_per_pixel);
    let raw = view.frames_per_pixel < level.frames_per_bucket() as f64;
    let mut mesh = Mesh::default();
    for i in 0..columns(rect) {
        let from = view.frame_at(i as f32);
        if from >= total {
            break;
        }
        let to = view.frame_at((i + 1) as f32).max(from + 1);
        // Raw: one frame past the column joins it to the next.
        let range = if raw {
            chop::raw_min_max(&source.data, from, to + 1)
        } else {
            peaks.min_max(level, from, to)
        };
        wave_column(&mut mesh, rect, rect.min.x + i as f32, range);
    }
    painter.add(Shape::mesh(mesh));
    paint_selection(painter, rect, editor, grip_hovered);

    if let Some(x) = hairline.and_then(|frame| hairline_x(rect, view, frame)) {
        painter.vline(x, rect.y_range(), Stroke::new(PLAY_HAIRLINE_WIDTH, HAIRLINE_COLOUR));
    }
}

/// Where the play hairline for `frame` goes in the main view at `rect`, or
/// `None` when the view does not show that frame.
fn hairline_x(rect: Rect, view: &View, frame: u64) -> Option<f32> {
    let x = rect.min.x + view.px_of(frame);
    rect.x_range().contains(x).then_some(x)
}

/// The selection's fill, its grip ([`grip_rect`], lit while `grip_hovered`)
/// and its edge handles over the main view at `rect`.
fn paint_selection(painter: &Painter, rect: Rect, editor: &ChopEditor, grip_hovered: bool) {
    let view = &editor.view;
    let Some(selection) = &editor.selection else { return };
    let (start, end) = (view.px_of(selection.start), view.px_of(selection.end));
    let (x0, x1) = (rect.min.x + start, rect.min.x + end);
    let (left, right) = (x0.max(rect.min.x), x1.min(rect.max.x));
    if right > left {
        let fill = Rect::from_x_y_ranges(left..=right, rect.y_range());
        painter.rect_filled(fill, 0.0, SELECTION_FILL);
    }
    if let Some(grip) = grip_rect(rect, editor) {
        let fill = if grip_hovered { GRIP_FILL_HOVER } else { GRIP_FILL };
        painter.rect_filled(grip, 0.0, fill);
        let font = FontId::proportional(GRIP_FONT);
        let galley = painter.layout_no_wrap(GRIP_LABEL.to_owned(), font, theme::TEXT);
        if galley.size().x + 8.0 <= grip.width() {
            let at = grip.center() - galley.size() / 2.0;
            painter.galley(at, galley, theme::TEXT);
        } else {
            // Too narrow for the words: three dots say "grab here".
            for k in [-1.0, 0.0, 1.0] {
                let centre = grip.center() + vec2(k * 4.0, 0.0);
                painter.circle_filled(centre, 1.2, theme::TEXT);
            }
        }
    }
    for (x, edge) in [(x0, Edge::Start), (x1, Edge::End)] {
        if !(rect.min.x - ACTIVE_HANDLE_WIDTH..=rect.max.x + ACTIVE_HANDLE_WIDTH).contains(&x) {
            continue;
        }
        let width = if selection.active == edge { ACTIVE_HANDLE_WIDTH } else { HANDLE_WIDTH };
        painter.vline(x, rect.y_range(), Stroke::new(width, theme::ACCENT));
        // The tab hangs inward, off the selection's side of the line.
        let tab_x = if edge == Edge::Start { x..=x + HANDLE_TAB } else { x - HANDLE_TAB..=x };
        let tab = Rect::from_x_y_ranges(tab_x, rect.min.y..=rect.min.y + HANDLE_TAB);
        painter.rect_filled(tab, 0.0, theme::ACCENT);
    }
}

/// The ruler under `main`: a tick and a `m:ss.mmm` label every
/// [`chop::ruler_step`] seconds, and a faint line through the main view at
/// each.
fn paint_ruler(painter: &Painter, rect: Rect, main: Rect, source: &Source, view: &View) {
    let rate = source.data.rate.max(1) as f64;
    let fpp = view.frames_per_pixel;
    if rect.height() < 1.0 || columns(rect) == 0 || !(fpp.is_finite() && fpp > 0.0) {
        return;
    }
    let step = chop::ruler_step(fpp / rate);
    let first_sec = view.first as f64 / rate;
    let width = rect.width() as f64;
    // Never more ticks than fit, whatever the numbers say.
    let most = (width / RULER_MIN_TICK_PX).ceil() as u64 + 2;
    let font = FontId::proportional(RULER_FONT);
    let tick = Stroke::new(theme::HAIRLINE, theme::TEXT_DIM);
    let grid = Stroke::new(theme::HAIRLINE, theme::GRID_LINE);
    let mut k = (first_sec / step).ceil();
    for _ in 0..most {
        let secs = k * step;
        let px = (secs * rate - view.first as f64) / fpp;
        if px > width {
            break;
        }
        let x = rect.min.x + px as f32;
        painter.vline(x, rect.min.y..=rect.min.y + TICK_LENGTH, tick);
        if main.height() >= 1.0 {
            painter.vline(x, main.y_range(), grid);
        }
        let at = pos2(x + 2.0, rect.min.y + 1.0);
        painter.text(at, Align2::LEFT_TOP, chop::format_time(secs), font.clone(), theme::TEXT_DIM);
        k += 1.0;
    }
}

/// The readout line: the selection's times, length and nearest note
/// length ([`chop::readout`]), at the host's `tempo`.
fn paint_readout(
    painter: &Painter,
    rect: Rect,
    source: &Source,
    selection: &chop::Selection,
    tempo: f64,
) {
    if rect.height() < 1.0 {
        return;
    }
    let text = chop::readout(selection, source.data.rate, tempo);
    let at = pos2(rect.min.x + 2.0, rect.center().y);
    let font = FontId::proportional(READOUT_FONT);
    painter.text(at, Align2::LEFT_CENTER, text, font, theme::TEXT);
}

#[cfg(test)]
mod tests {
    use egui::{Event, Modifiers, PointerButton, RawInput, Vec2};

    use super::*;
    use crate::chop::{Peaks, STRIP_HEIGHT};
    use crate::editor::drops;
    use crate::preview::PreviewChannel;
    use crate::sample::SampleData;
    use crate::time::TEMPO_DEFAULT;
    use std::sync::Arc;

    const WIDTH: f32 = 560.0;
    const HEIGHT: f32 = 500.0;

    /// A window holding only the strip, over a model of its own.
    struct Window {
        ctx: egui::Context,
        model: Model,
        /// The previewer the strip plays through (no engine: tests stand in
        /// for it on the channel).
        previewer: Previewer,
        out: Option<StripOutput>,
        texts: Vec<(String, Rect)>,
        /// Every shape the last frame drew.
        shapes: Vec<Shape>,
        /// Draw a long scrolling list under the strip, as the lane list is.
        list: bool,
        /// That list's scroll offset after the last frame.
        list_offset: f32,
        /// The keyboard modifiers held during each frame.
        modifiers: Modifiers,
        /// Who owns the keyboard during each frame.
        owner: KeyOwner,
        /// Draw a focused text box above the strip.
        text_box: bool,
        /// The input's events left once the strip had read its keys.
        leftover: Vec<Event>,
        /// Draw the model's lanes as rows under the strip and land chops
        /// dragged onto them, as the window does (`editor/drops.rs`).
        lanes: bool,
        /// Where those rows and their list were last drawn.
        rows: Vec<drops::LaneRect>,
        list_rect: Rect,
        /// The lane the last frame's chop landed on.
        landed: Option<u32>,
    }

    impl Window {
        fn new() -> Window {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let model = Model::new();
            let handle = model.handle().clone();
            let previewer = Previewer::new(Arc::new(PreviewChannel::new(&handle)), handle);
            Window {
                ctx,
                model,
                previewer,
                out: None,
                texts: Vec::new(),
                shapes: Vec::new(),
                list: false,
                list_offset: 0.0,
                modifiers: Modifiers::NONE,
                owner: KeyOwner::Chop,
                text_box: false,
                leftover: Vec::new(),
                lanes: false,
                rows: Vec::new(),
                list_rect: Rect::NOTHING,
                landed: None,
            }
        }

        fn frame(&mut self, events: Vec<Event>) -> StripOutput {
            self.frame_sized(HEIGHT, events)
        }

        fn frame_sized(&mut self, height: f32, events: Vec<Event>) -> StripOutput {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(WIDTH, height))),
                events: [vec![Event::ModifiersChanged(self.modifiers)], events].concat(),
                ..RawInput::default()
            };
            let model = &mut self.model;
            let previewer = &self.previewer;
            let mut out = None;
            let (list, mut list_offset) = (self.list, 0.0);
            let (owner, text_box) = (self.owner, self.text_box);
            let mut leftover = Vec::new();
            let lanes = self.lanes;
            let (mut rows, mut list_rect, mut landed) = (Vec::new(), Rect::NOTHING, None);
            let output = self.ctx.run_ui(input, |ui| {
                if text_box {
                    // A name box with the focus, as a lane's rename is.
                    let id = egui::Id::unique("test_text_box");
                    let mut text = String::from("name");
                    ui.add(egui::TextEdit::singleline(&mut text).id(id));
                    ui.memory_mut(|memory| memory.request_focus(id));
                }
                out = Some(show(ui, model, previewer, owner));
                leftover = ui.input(|input| input.events.clone());
                if lanes {
                    let strip = out.expect("drawn").rect;
                    list_rect = ui.available_rect_before_wrap();
                    rows = model
                        .lanes()
                        .iter()
                        .enumerate()
                        .map(|(index, lane)| {
                            let top = list_rect.min.y + index as f32 * LANE_ROW;
                            let size = vec2(list_rect.width(), LANE_ROW - 2.0);
                            let rect = Rect::from_min_size(pos2(list_rect.min.x, top), size);
                            drops::LaneRect { id: lane.id, rect }
                        })
                        .collect();
                    drops::outline_chop(ui, &rows, list_rect, strip);
                    landed = drops::land_chop(ui, model, &rows, list_rect, strip);
                }
                if list {
                    let area = egui::ScrollArea::vertical().show(ui, |ui| {
                        for row in 0..200 {
                            ui.label(format!("row {row}"));
                        }
                    });
                    list_offset = area.state.clamped_offset().y;
                }
            });
            self.list_offset = list_offset;
            self.leftover = leftover;
            (self.rows, self.list_rect, self.landed) = (rows, list_rect, landed);
            self.texts = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    Shape::Text(text) => {
                        Some((text.galley.text().to_string(), text.visual_bounding_rect()))
                    }
                    _ => None,
                })
                .collect();
            self.shapes = output.shapes.iter().map(|clipped| clipped.shape.clone()).collect();
            output.drop_without_applying_deltas();
            self.out = out;
            out.expect("the strip is drawn")
        }

        fn drawn(&self, text: &str) -> bool {
            self.texts.iter().any(|(drawn, _)| drawn == text)
        }

        fn button(at: Pos2, pressed: bool) -> Event {
            Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            }
        }

        fn click(&mut self, at: Pos2) {
            self.frame(vec![Event::PointerMoved(at)]);
            self.frame(vec![Window::button(at, true)]);
            self.frame(vec![Window::button(at, false)]);
            self.frame(Vec::new());
        }

        /// The centre of the text `text` was last drawn at.
        fn text_at(&self, text: &str) -> Pos2 {
            let found = self.texts.iter().find(|(drawn, _)| drawn == text);
            found.unwrap_or_else(|| panic!("{text} is drawn")).1.center()
        }

        /// Where the collapse toggle is: the header's left end.
        fn toggle(&self) -> Pos2 {
            let rect = self.out.expect("drawn").rect;
            let x = rect.min.x + theme::PANEL_PADDING + TOGGLE_SIZE / 2.0;
            pos2(x, rect.min.y + HEADER_ROW / 2.0)
        }

        fn load(&mut self, path: &str) {
            self.model.open_source(path);
            let data = SampleData {
                rate: 48_000,
                channels: 1,
                frames: (0..4800).map(|n| (n as f32 * 0.05).sin()).collect(),
            };
            let peaks = Peaks::build(&data);
            assert!(self.model.set_source(path, data, peaks));
        }
    }

    #[test]
    fn labels_are_pinned() {
        assert_eq!(PANEL_ID, "chopdrop_chop");
        assert_eq!(STRIP_HEADER, 38.0);
        assert_eq!(HEADER_CONTROL_HEIGHT, theme::CONTROL_HEIGHT, "as easy to hit as a row's");
        assert_eq!(STRIP_HEIGHT, 400.0);
        assert_eq!(STRIP_MIN_HEIGHT, 96.0);
        assert_eq!(EMPTY_HINT, "drop a song or loop here, or open one from the browser");
        assert_eq!(LOADING_TEXT, "loading…");
        assert_eq!(
            [PLAY_LABEL, LOOP_LABEL, SNAP_LABEL, NEXT_LABEL, ADD_LANE_LABEL, CLOSE_LABEL],
            ["play", "loop", "snap", "next", "add lane", "×"]
        );
        assert_eq!(STOP_LABEL, "stop");
        assert_eq!(PLAY_HOVER, "play the selection (A or P)");
        assert_eq!(STOP_HOVER, "stop the preview (A or P)");
        assert_eq!(GRIP_LABEL, "drag to a lane");
        assert_eq!(COLLAPSE_HOVER, "hide the chop editor");
        assert_eq!(EXPAND_HOVER, "show the chop editor");
    }

    #[test]
    fn empty_strip_shows_the_hint_at_the_default_height() {
        let mut window = Window::new();
        assert_eq!(body(&window.model), Body::Empty);
        window.frame(Vec::new());
        let out = window.frame(Vec::new());
        assert_eq!(out.rect.height(), STRIP_HEIGHT);
        assert_eq!(out.rect.min, Pos2::ZERO);
        assert!(window.drawn(EMPTY_HINT));
        assert!(out.canvas.is_none());
        for label in [PLAY_LABEL, LOOP_LABEL, SNAP_LABEL, NEXT_LABEL, ADD_LANE_LABEL, CLOSE_LABEL]
        {
            assert!(window.drawn(label), "{label} is drawn");
        }
        // The height the strip already had is not written back.
        assert_eq!(window.model.chop().height_px, STRIP_HEIGHT);
    }

    #[test]
    fn loading_strip_says_so_and_names_the_file() {
        let mut window = Window::new();
        window.model.open_source("/music/Eye.wav");
        assert_eq!(body(&window.model), Body::Loading);
        window.frame(Vec::new());
        window.frame(Vec::new());
        assert!(window.drawn(LOADING_TEXT));
        assert!(window.drawn("Eye.wav"));
        assert!(!window.drawn(EMPTY_HINT));
    }

    #[test]
    fn failed_strip_shows_the_message_and_the_path() {
        let mut window = Window::new();
        window.model.open_source("/music/Gone.wav");
        assert!(window.model.source_failed("/music/Gone.wav", "source not found: Gone.wav"));
        assert_eq!(
            body(&window.model),
            Body::Failed {
                message: "source not found: Gone.wav".into(),
                path: "/music/Gone.wav".into()
            }
        );
        window.frame(Vec::new());
        window.frame(Vec::new());
        assert!(window.drawn("source not found: Gone.wav"));
        assert!(window.drawn("/music/Gone.wav"));
        assert!(!window.drawn(LOADING_TEXT));
    }

    #[test]
    fn loaded_strip_has_a_canvas_under_the_header() {
        let mut window = Window::new();
        window.load("/music/Eye.wav");
        assert_eq!(body(&window.model), Body::Loaded);
        window.frame(Vec::new());
        let out = window.frame(Vec::new());
        assert!(window.drawn("Eye.wav"));
        let canvas = out.canvas.expect("a canvas with a source open");
        assert!(canvas.min.y >= out.rect.min.y + HEADER_ROW);
        assert!(canvas.max.y <= out.rect.max.y);
        assert!(canvas.width() > 0.0 && canvas.height() > 0.0);
    }

    #[test]
    fn every_state_renders_collapsed_and_in_a_tiny_window() {
        for state in 0..4 {
            let mut window = Window::new();
            match state {
                1 => window.model.open_source("/a.wav"),
                2 => {
                    window.model.open_source("/a.wav");
                    window.model.source_failed("/a.wav", "nope");
                }
                3 => window.load("/a.wav"),
                _ => {}
            }
            for height in [HEIGHT, 60.0, 10.0, 0.0] {
                for collapsed in [false, true] {
                    window.model.set_strip(collapsed, STRIP_HEIGHT);
                    window.frame_sized(height, Vec::new());
                    let out = window.frame_sized(height, Vec::new());
                    if collapsed {
                        assert!(out.canvas.is_none());
                    }
                }
            }
            // A window too short for the strip does not shrink the saved height.
            assert_eq!(window.model.chop().height_px, STRIP_HEIGHT);
        }
    }

    #[test]
    fn snap_button_toggles_snap_with_or_without_a_source() {
        for loaded in [false, true] {
            let mut window = Window::new();
            if loaded {
                window.load("/music/Eye.wav");
            }
            assert!(window.model.chop().snap, "snap is on by default");
            window.frame(Vec::new());
            window.frame(Vec::new());
            let at = window.text_at(SNAP_LABEL);
            window.click(at);
            assert!(!window.model.chop().snap);
            window.click(at);
            assert!(window.model.chop().snap);
            assert_eq!(window.model.chop().source.is_some(), loaded, "the source is untouched");
        }
    }

    #[test]
    fn close_button_empties_the_strip() {
        let mut window = Window::new();
        window.load("/music/Eye.wav");
        window.model.set_selection(Some(chop::Selection {
            start: 100,
            end: 900,
            active: chop::Edge::Start,
        }));
        window.model.set_snap(false);
        window.frame(Vec::new());
        window.frame(Vec::new());
        let at = window.text_at(CLOSE_LABEL);
        window.click(at);
        let editor = window.model.chop();
        assert!(editor.source.is_none());
        assert!(editor.source_path.is_none());
        assert!(editor.selection.is_none());
        assert!(editor.error.is_none());
        assert!(!editor.snap, "snap is a setting, not part of the source");
        assert_eq!(body(&window.model), Body::Empty);
        assert!(window.drawn(EMPTY_HINT));
        assert!(!window.drawn("Eye.wav"));

        // With nothing open, × does nothing.
        window.click(at);
        assert_eq!(body(&window.model), Body::Empty);
    }

    #[test]
    fn close_button_clears_a_failure_and_a_load() {
        for failed in [false, true] {
            let mut window = Window::new();
            window.model.open_source("/music/Gone.wav");
            if failed {
                assert!(window.model.source_failed("/music/Gone.wav", "source not found"));
            }
            window.frame(Vec::new());
            window.frame(Vec::new());
            let at = window.text_at(CLOSE_LABEL);
            window.click(at);
            let editor = window.model.chop();
            assert!(editor.error.is_none() && !editor.loading);
            assert!(editor.source_path.is_none());
            assert_eq!(body(&window.model), Body::Empty);
            assert!(window.drawn(EMPTY_HINT));
        }
    }

    #[test]
    fn collapse_toggle_persists_in_the_model() {
        let mut window = Window::new();
        window.model.set_strip(false, 240.0);
        window.frame(Vec::new());
        assert_eq!(window.frame(Vec::new()).rect.height(), 240.0);

        let at = window.toggle();
        window.click(at);
        assert!(window.model.chop().collapsed);
        let out = window.frame(Vec::new());
        assert_eq!(out.rect.height(), STRIP_HEADER);
        assert!(window.drawn(EMPTY_HINT), "collapsed, the header carries the hint");
        assert_eq!(window.model.chop().height_px, 240.0, "the expanded height is kept");

        let at = window.toggle();
        window.click(at);
        assert!(!window.model.chop().collapsed);
        assert_eq!(window.frame(Vec::new()).rect.height(), 240.0);
    }

    #[test]
    fn the_models_height_drives_the_strip() {
        let mut window = Window::new();
        window.frame(Vec::new());
        window.model.set_strip(false, 300.0);
        assert_eq!(window.frame(Vec::new()).rect.height(), 300.0);
        window.model.set_strip(false, 120.0);
        assert_eq!(window.frame(Vec::new()).rect.height(), 120.0);
    }

    #[test]
    fn dragging_the_lower_edge_resizes_and_persists() {
        let mut window = Window::new();
        window.frame(Vec::new());
        window.frame(Vec::new());
        let edge = pos2(WIDTH / 2.0, STRIP_HEIGHT);
        let to = pos2(WIDTH / 2.0, STRIP_HEIGHT + 60.0);
        window.frame(vec![Event::PointerMoved(edge)]);
        window.frame(vec![Window::button(edge, true)]);
        for step in 1..=6 {
            let y = STRIP_HEIGHT + 10.0 * step as f32;
            window.frame(vec![Event::PointerMoved(pos2(WIDTH / 2.0, y))]);
        }
        window.frame(vec![Window::button(to, false)]);
        window.frame(Vec::new());
        let height = window.model.chop().height_px;
        assert!((height - (STRIP_HEIGHT + 60.0)).abs() <= 1.0, "dragged to {height}");
        assert!((window.frame(Vec::new()).rect.height() - height).abs() <= 0.5);

        // Never below the minimum.
        let edge = pos2(WIDTH / 2.0, height);
        window.frame(vec![Event::PointerMoved(edge)]);
        window.frame(vec![Window::button(edge, true)]);
        for y in [height - 20.0, 60.0, 10.0] {
            window.frame(vec![Event::PointerMoved(pos2(WIDTH / 2.0, y))]);
        }
        window.frame(vec![Window::button(pos2(WIDTH / 2.0, 10.0), false)]);
        window.frame(Vec::new());
        assert_eq!(window.model.chop().height_px, STRIP_MIN_HEIGHT);
    }

    /// A 3 s stereo source: a decaying 220 Hz tone, loud enough to clip.
    fn long_source(window: &mut Window, path: &str) -> u64 {
        window.model.open_source(path);
        let frames = 3 * 48_000;
        let data = SampleData {
            rate: 48_000,
            channels: 2,
            frames: (0..frames * 2)
                .map(|n| {
                    let t = (n / 2) as f32 / 48_000.0;
                    1.4 * (-t).exp() * (t * 220.0 * std::f32::consts::TAU).sin()
                })
                .collect(),
        };
        let peaks = Peaks::build(&data);
        assert!(window.model.set_source(path, data, peaks));
        frames as u64
    }

    #[test]
    fn canvas_rows_stack_inside_the_canvas() {
        let canvas = Rect::from_min_size(pos2(6.0, 24.0), vec2(500.0, 150.0));
        let layout = canvas_layout(canvas);
        assert_eq!(layout.overview.height(), OVERVIEW_HEIGHT);
        assert_eq!(layout.overview.min, canvas.min);
        assert_eq!(layout.main.min.y, canvas.min.y + OVERVIEW_HEIGHT + OVERVIEW_GAP);
        assert_eq!(layout.ruler.height(), RULER_HEIGHT);
        assert_eq!(layout.readout.height(), READOUT_HEIGHT);
        assert_eq!(layout.readout.max, canvas.max);
        assert_eq!(layout.ruler.max.y, layout.readout.min.y);
        assert_eq!(layout.main.max.y, layout.ruler.min.y);
        assert_eq!(layout.main.width(), canvas.width());

        // Too short: the main view goes first, nothing leaves the canvas.
        for height in [30.0, 10.0, 0.0] {
            let canvas = Rect::from_min_size(pos2(0.0, 0.0), vec2(100.0, height));
            let layout = canvas_layout(canvas);
            for row in [layout.overview, layout.main, layout.ruler, layout.readout] {
                assert!(row.height() >= 0.0 && canvas.contains_rect(row), "{height}: {row:?}");
            }
            assert!(layout.overview.max.y <= layout.main.min.y);
            assert!(layout.main.max.y <= layout.ruler.min.y);
            assert!(layout.ruler.max.y <= layout.readout.min.y);
        }
    }

    /// The strip and the lane list drawn together over one model, as the
    /// window draws them, with the host at 90 BPM: the readout is worked out
    /// at the live tempo, not the default, and a synced lane's row carries
    /// its note length as the marker.
    #[test]
    fn a_render_at_90_bpm_shows_the_live_tempo_and_a_synced_lanes_marker() {
        use crate::editor::activity::Lights;
        use crate::editor::lanes::{self, Rename};
        use crate::editor::menu::Confirm;
        use crate::editor::selection::Selection;
        use crate::time::{Length, NoteLength, TimeMode};

        let mut window = Window::new();
        long_source(&mut window, "/music/Tone.wav");
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        let synced = window.model.add_lane().expect("room for a lane");
        // To the top, where the window shows it.
        let last = window.model.lanes().len() - 1;
        assert!(window.model.reorder(last, 0));
        assert!(window.model.set_time_mode(synced, TimeMode::Repitch));
        assert!(window.model.set_length(synced, Length::Beats(NoteLength::Bar)));
        window.model.tempo().update(Some(90.0));

        let (mut rename, mut confirm) = (Rename::default(), Confirm::default());
        let (mut lanes_selection, lights) = (Selection::new(), Lights::default());
        let mut texts = Vec::new();
        for _ in 0..2 {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    vec2(crate::editor::DEFAULT_WIDTH, crate::editor::DEFAULT_HEIGHT),
                )),
                ..RawInput::default()
            };
            let model = &mut window.model;
            let previewer = &window.previewer;
            let output = window.ctx.run_ui(input, |ui| {
                show(ui, model, previewer, KeyOwner::Lanes);
                let (rename, confirm) = (&mut rename, &mut confirm);
                lanes::lane_list(ui, model, rename, confirm, &mut lanes_selection, &lights, None);
            });
            texts = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
        }

        let line = chop::readout(&selection, 48_000, 90.0);
        assert!(line.ends_with("at 90 BPM"), "{line}");
        assert!(texts.contains(&line), "the readout is at the live tempo: {texts:?}");
        assert!(!texts.iter().any(|text| text.ends_with("at 120 BPM")), "not the default");
        assert!(texts.iter().any(|text| text == "1 bar"), "the synced row's marker: {texts:?}");
    }

    #[test]
    fn loaded_source_paints_at_every_zoom_without_panicking() {
        let mut window = Window::new();
        let total = long_source(&mut window, "/music/Tone.wav");
        window.frame(Vec::new());
        window.frame(Vec::new());

        // The fitted view follows the main view's width and shows it all.
        let canvas = window.out.expect("drawn").canvas.expect("a canvas");
        let width = canvas_layout(canvas).main.width();
        let editor = window.model.chop();
        assert_eq!(editor.view_width_px, width);
        assert_eq!(editor.view, View::fit(total, width));
        assert!(window.drawn("0:00.000"), "the ruler labels the start");
        assert!(window.drawn("0:01.000"), "and each second at fit");

        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        window.frame(Vec::new());
        let line = chop::readout(&selection, 48_000, TEMPO_DEFAULT);
        assert_eq!(
            line,
            "0:00.625 \u{2013} 0:01.458 \u{b7} 0.833 s \u{b7} \u{2248} 1/4d, 10 % long \
             at 120 BPM"
        );
        assert!(window.drawn(&line), "the readout line is drawn under the ruler");
        // Fit, peak levels coarse to fine, past the finest, 1 frame/px;
        // scrolled to the start, the middle and the end.
        for fpp in [1e9, 2000.0, 600.0, 70.0, 20.0, 1.0] {
            for first in [0, 40_000, total] {
                window.model.set_view(View { first, frames_per_pixel: fpp }, width);
                for height in [HEIGHT, 120.0, 40.0] {
                    window.model.set_strip(false, STRIP_MIN_HEIGHT.max(height - 40.0));
                    window.frame_sized(height, Vec::new());
                    window.frame_sized(height, Vec::new());
                }
            }
        }
        // All the way in, the ruler ticks every couple of milliseconds.
        window.model.set_strip(false, STRIP_HEIGHT);
        window.model.set_view(View { first: 48_000, frames_per_pixel: 1.0 }, width);
        window.frame(Vec::new());
        assert!(window.drawn("0:01.002"), "ticks 2 ms apart at 1 frame/px");
        // Drawing keeps a zoomed view's zoom (only a fitted one refits).
        assert_eq!(window.model.chop().view.frames_per_pixel, 1.0);
    }

    /// A wheel event of `delta` points, small enough that egui hands it
    /// over in one frame rather than smoothing it out over several.
    fn wheel_points(delta: Vec2, modifiers: Modifiers) -> Event {
        Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta,
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers,
        }
    }

    /// A long source, a fitted strip with the list under it, and where the
    /// main view is.
    fn wheel_window() -> (Window, u64, Rect) {
        let mut window = Window::new();
        window.list = true;
        let total = long_source(&mut window, "/music/Tone.wav");
        window.frame(Vec::new());
        window.frame(Vec::new());
        let main = canvas_layout(window.out.expect("drawn").canvas.expect("a canvas")).main;
        (window, total, main)
    }

    #[test]
    fn ctrl_scroll_zooms_around_the_pointer() {
        let (mut window, total, main) = wheel_window();
        let ctrl = Modifiers::CTRL | Modifiers::COMMAND;
        let at = pos2(main.min.x + 200.0, main.center().y);
        window.frame(vec![Event::PointerMoved(at)]);

        // Wheel down zooms in (twice), up zooms out (once): the reverse of
        // egui's own Ctrl+scroll, by the same amount a notch.
        for delta in [-5.0, -5.0, 5.0] {
            let before = window.model.chop().view;
            window.frame(vec![wheel_points(vec2(0.0, delta), ctrl)]);
            let mut expected = before;
            // egui's default scroll_zoom_speed, 1/200 a point, as the f32 it is.
            let factor = (f64::from(1.0f32 / 200.0) * f64::from(delta)).exp();
            expected.zoom_at(200.0, factor, total, main.width());
            let view = window.model.chop().view;
            assert_eq!(view, expected);
            if delta < 0.0 {
                assert!(view.frames_per_pixel < before.frames_per_pixel, "zoomed in");
            } else {
                assert!(view.frames_per_pixel > before.frames_per_pixel, "zoomed out");
            }
            let drift = view.frame_at(200.0).abs_diff(before.frame_at(200.0));
            assert!(drift as f64 <= before.frames_per_pixel, "the pointer's frame stays");
        }
        assert_eq!(window.list_offset, 0.0, "the lane list does not scroll");

        // Zoomed all the way in it stops at 1 frame/px.
        for _ in 0..200 {
            window.frame(vec![wheel_points(vec2(0.0, -7.0), ctrl)]);
        }
        assert_eq!(window.model.chop().view.frames_per_pixel, 1.0);
        assert_eq!(window.list_offset, 0.0);
    }

    /// With Ctrl held, sideways scrolling still pans, the same way round as
    /// without Ctrl, and does not zoom — egui alone would fold it into the
    /// zoom.
    #[test]
    fn ctrl_sideways_scroll_pans_instead_of_zooming() {
        let (mut window, total, main) = wheel_window();
        let width = main.width();
        window.model.set_view(View { first: 40_000, frames_per_pixel: 20.0 }, width);
        let ctrl = Modifiers::CTRL | Modifiers::COMMAND;
        window.frame(vec![Event::PointerMoved(main.center())]);

        for delta in [vec2(-4.0, 0.0), vec2(6.0, 0.0)] {
            let before = window.model.chop().view;
            window.frame(vec![wheel_points(delta, ctrl)]);
            let mut expected = before;
            expected.pan(delta.x, total, width);
            let view = window.model.chop().view;
            assert_eq!(view, expected);
            assert_eq!(view.frames_per_pixel, before.frames_per_pixel, "no zoom");
            assert_ne!(view, before);
        }
        // Left goes earlier, as it does without Ctrl.
        let before = window.model.chop().view;
        window.frame(vec![wheel_points(vec2(-4.0, 0.0), ctrl)]);
        assert!(window.model.chop().view.first < before.first);
        assert_eq!(window.list_offset, 0.0, "the lane list does not scroll");
    }

    #[test]
    fn plain_scroll_pans_and_is_taken_from_the_list() {
        let (mut window, total, main) = wheel_window();
        let width = main.width();
        window.model.set_view(View { first: 40_000, frames_per_pixel: 20.0 }, width);
        let at = pos2(main.center().x, main.center().y);
        window.frame(vec![Event::PointerMoved(at)]);

        // The wheel: down goes earlier, up later (the user's way round).
        // Sideways the same way round: left earlier, right later.
        for delta in [vec2(0.0, -5.0), vec2(0.0, 6.0), vec2(-4.0, 0.0), vec2(3.0, 0.0)] {
            let before = window.model.chop().view;
            window.frame(vec![wheel_points(delta, Modifiers::NONE)]);
            let mut expected = before;
            expected.pan(delta.y + delta.x, total, width);
            assert_eq!(window.model.chop().view, expected);
            assert_ne!(expected, before);
            let left = window.ctx.input(|input| input.smooth_scroll_delta());
            assert_eq!(left, Vec2::ZERO, "the scroll is taken out of egui's input");
            assert_eq!(window.list_offset, 0.0, "the lane list does not scroll");
        }

        // A whole notch (up, so later), smoothed over many frames, still all
        // goes to the view.
        let before = window.model.chop().view;
        window.frame(vec![Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: Modifiers::NONE,
        }]);
        for _ in 0..200 {
            if !window.ctx.input(|input| input.is_scrolling()) {
                break;
            }
            window.frame(Vec::new());
        }
        let moved = window.model.chop().view.first - before.first;
        assert!((780..=820).contains(&moved), "a notch is ~40 px: {moved} frames");
        assert_eq!(window.list_offset, 0.0);

        // Panning stops at the end of the source.
        for _ in 0..2000 {
            window.frame(vec![wheel_points(vec2(0.0, 7.0), Modifiers::NONE)]);
        }
        let view = window.model.chop().view;
        assert_eq!(view, view.clamped(total, width));
        assert!(view.frame_at(width) >= total - 20, "at the end");

        // The control: over the list the strip leaves the wheel in egui's
        // input for the list to scroll by, and the view does not move.
        let below = pos2(WIDTH / 2.0, HEIGHT - 20.0);
        window.frame(vec![Event::PointerMoved(below)]);
        window.frame(vec![wheel_points(vec2(0.0, -5.0), Modifiers::NONE)]);
        let left = window.ctx.input(|input| input.smooth_scroll_delta());
        assert_eq!(left, vec2(0.0, -5.0), "the wheel over the list is the list's");
        assert_eq!(window.model.chop().view, view, "and the view does not move");
    }

    #[test]
    fn hostile_samples_paint() {
        let mut window = Window::new();
        window.model.open_source("/x.wav");
        let frames = vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, 5.0, -5.0];
        let data = SampleData { rate: 1, channels: 1, frames: frames.into_boxed_slice() };
        let peaks = Peaks::build(&data);
        assert!(window.model.set_source("/x.wav", data, peaks));
        window.frame(Vec::new());
        window.frame(Vec::new());
        assert!(window.out.expect("drawn").canvas.is_some());
    }

    /// A long source with a fitted strip, and where the main view is.
    fn gesture_window() -> (Window, u64, Rect) {
        let mut window = Window::new();
        let total = long_source(&mut window, "/music/Tone.wav");
        window.frame(Vec::new());
        window.frame(Vec::new());
        let main = canvas_layout(window.out.expect("drawn").canvas.expect("a canvas")).main;
        (window, total, main)
    }

    impl Window {
        fn pointer_button(&self, at: Pos2, pressed: bool) -> Event {
            Event::PointerButton {
                pos: at,
                button: PointerButton::Primary,
                pressed,
                modifiers: self.modifiers,
            }
        }

        /// Press at `from`, drag to `to` in steps, release there. Returns
        /// the release frame's output.
        fn drag(&mut self, from: Pos2, to: Pos2) -> StripOutput {
            self.frame(vec![Event::PointerMoved(from)]);
            self.frame(vec![self.pointer_button(from, true)]);
            for step in 1..=5 {
                let at = from + (to - from) * (step as f32 / 5.0);
                self.frame(vec![Event::PointerMoved(at)]);
            }
            let out = self.frame(vec![self.pointer_button(to, false)]);
            self.frame(Vec::new());
            out
        }

        /// Press and release at `from`, press again at once (a double-click),
        /// drag to `to` in steps and release there. Returns the last
        /// release frame's output.
        fn double_drag(&mut self, from: Pos2, to: Pos2) -> StripOutput {
            self.frame(vec![Event::PointerMoved(from)]);
            self.frame(vec![self.pointer_button(from, true)]);
            self.frame(vec![self.pointer_button(from, false)]);
            self.drag(from, to)
        }

        /// Enough idle frames (egui counts a sixtieth of a second each) for
        /// the next press not to be a double-click.
        fn wait_out_a_double_click(&mut self) {
            for _ in 0..30 {
                self.frame(Vec::new());
            }
        }

        /// The x of `frame` in the main view at `main`.
        fn x_of(&self, main: Rect, frame: u64) -> f32 {
            main.min.x + self.model.chop().view.px_of(frame)
        }

        /// The frame under screen x `x`, unsnapped and snapped.
        fn frames_at(&self, main: Rect, x: f32) -> (u64, u64) {
            let editor = self.model.chop();
            let raw = editor.view.frame_at(x - main.min.x);
            let data = &editor.source.as_ref().expect("a source").data;
            (raw, chop::snap_zero_crossing(data, raw, chop::snap_window_frames(data.rate)))
        }
    }

    #[test]
    fn dragging_outside_makes_a_new_snapped_selection_either_way() {
        let (mut window, _, main) = gesture_window();
        assert!(window.model.chop().snap);
        let y = main.center().y;
        let (a, b) = (main.min.x + 101.3, main.min.x + 263.7);
        let (raw_a, snap_a) = window.frames_at(main, a);
        let (raw_b, snap_b) = window.frames_at(main, b);
        assert_ne!((raw_a, raw_b), (snap_a, snap_b), "the snap has something to do");

        // Left to right: the end is the active edge.
        let out = window.drag(pos2(a, y), pos2(b, y));
        assert_eq!(out.click, None, "a drag is not a click");
        let expected = chop::Selection { start: snap_a, end: snap_b, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(expected));

        // Right to left, double-clicked outside the one there: a new one,
        // the start active.
        let (c, d) = (main.min.x + 400.2, main.min.x + 300.6);
        let (_, snap_c) = window.frames_at(main, c);
        let (_, snap_d) = window.frames_at(main, d);
        let out = window.double_drag(pos2(c, y), pos2(d, y));
        assert_eq!(out.click, None);
        let expected = chop::Selection { start: snap_d, end: snap_c, active: Edge::Start };
        assert_eq!(window.model.chop().selection, Some(expected));
    }

    /// With a selection, a press outside it moves the nearer edge out to
    /// the press and drags it on (the user's, 2026-11-16): it extends, it
    /// does not start again — and plays nothing.
    #[test]
    fn pressing_outside_extends_the_nearer_edge() {
        let (mut window, _, main) = gesture_window();
        let y = main.center().y;
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        window.modifiers = Modifiers::SHIFT;

        // Left of the selection: the start comes out to the click.
        let at = window.x_of(main, 20_000);
        let (raw, _) = window.frames_at(main, at);
        let out = window.drag(pos2(at, y), pos2(at, y));
        assert_eq!(out.click, None, "not a click to play");
        let expected = chop::Selection { start: raw, end: 70_000, active: Edge::Start };
        assert_eq!(window.model.chop().selection, Some(expected));

        // Right of it: the end goes out, and on with the drag.
        let at = window.x_of(main, 80_000);
        let to = at + 30.0;
        let (raw_to, _) = window.frames_at(main, to);
        window.drag(pos2(at, y), pos2(to, y));
        let expected = chop::Selection { end: raw_to, active: Edge::End, ..expected };
        assert_eq!(window.model.chop().selection, Some(expected));
    }

    /// A double-click clears the selection and starts a new one from where
    /// it landed — taking back the edge its first click moved, so one undo
    /// returns to the selection from before the double-click.
    #[test]
    fn a_double_click_starts_a_new_selection() {
        let (mut window, _, main) = gesture_window();
        let y = main.center().y;
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        assert!(window.model.commit());
        let depth = window.model.undo_depth();

        // Without moving: nothing selected, and no preview started.
        let at = pos2(window.x_of(main, 90_000), y);
        window.click(at);
        window.click(at);
        assert_eq!(window.model.chop().selection, None);
        assert_eq!(request(&window).0, None, "a double-click plays nothing");
        assert_eq!(window.model.undo_depth(), depth + 1, "one step, not two");
        assert!(window.model.undo());
        let back = window.model.chop().selection.expect("the selection from before");
        assert_eq!((back.start, back.end), (30_000, 70_000));

        // Inside the selection too, and dragged: a new selection.
        window.wait_out_a_double_click();
        window.modifiers = Modifiers::SHIFT;
        let (from, to) = (window.x_of(main, 50_000), window.x_of(main, 60_000));
        let (raw_from, _) = window.frames_at(main, from);
        let (raw_to, _) = window.frames_at(main, to);
        window.double_drag(pos2(from, y), pos2(to, y));
        let expected = chop::Selection { start: raw_from, end: raw_to, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(expected));
    }

    /// Every press that changes the selection is one undo step at its
    /// release, in the model's own history.
    #[test]
    fn each_selection_gesture_is_one_undo_step() {
        let (mut window, _, main) = gesture_window();
        let y = main.center().y;
        let (a, b) = (main.min.x + 101.3, main.min.x + 263.7);
        window.drag(pos2(a, y), pos2(b, y));
        let first = window.model.chop().selection;
        assert_eq!(window.model.undo_depth(), 1, "a whole drag is one step");
        window.drag(pos2(b + 50.0, y), pos2(b + 90.0, y));
        assert_eq!(window.model.undo_depth(), 2);
        assert!(window.model.undo());
        assert_eq!(window.model.chop().selection, first);
        assert!(window.model.undo());
        assert_eq!(window.model.chop().selection, None);
    }

    #[test]
    fn shift_drags_without_the_snap() {
        let (mut window, _, main) = gesture_window();
        let y = main.center().y;
        let (a, b) = (main.min.x + 101.3, main.min.x + 263.7);
        let (raw_a, _) = window.frames_at(main, a);
        let (raw_b, _) = window.frames_at(main, b);
        window.modifiers = Modifiers::SHIFT;
        window.drag(pos2(a, y), pos2(b, y));
        let expected = chop::Selection { start: raw_a, end: raw_b, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(expected));

        // Snap off in the model: no snap without Shift either.
        window.modifiers = Modifiers::NONE;
        window.model.set_snap(false);
        let (c, d) = (main.min.x + 300.6, main.min.x + 400.2);
        let (raw_c, _) = window.frames_at(main, c);
        let (raw_d, _) = window.frames_at(main, d);
        window.double_drag(pos2(c, y), pos2(d, y));
        let expected = chop::Selection { start: raw_c, end: raw_d, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(expected));
    }

    #[test]
    fn pressing_near_an_edge_drags_that_edge() {
        let (mut window, total, main) = gesture_window();
        let y = main.center().y;
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));

        // The start, grabbed 4 px off, dragged left: snapped, active.
        let grab = window.x_of(main, 30_000) + 4.0;
        let to = grab - 37.3;
        let (raw, snap) = window.frames_at(main, to);
        assert_ne!(raw, snap);
        window.drag(pos2(grab, y), pos2(to, y));
        let expected = chop::Selection { start: snap, end: 70_000, active: Edge::Start };
        assert_eq!(window.model.chop().selection, Some(expected));

        // The end, grabbed 5 px inside, dragged right with Shift: unsnapped.
        window.modifiers = Modifiers::SHIFT;
        let grab = window.x_of(main, 70_000) - 5.0;
        let to = grab + 52.9;
        let (raw, _) = window.frames_at(main, to);
        window.drag(pos2(grab, y), pos2(to, y));
        let expected = chop::Selection { start: snap, end: raw, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(expected));

        // An edge dragged past the other stops short of it.
        let grab = window.x_of(main, raw);
        window.drag(pos2(grab, y), pos2(main.min.x + 1.0, y));
        let selection = window.model.chop().selection.expect("kept");
        assert_eq!(selection.start, snap);
        assert_eq!(selection.end, snap + chop::MIN_SELECTION_FRAMES);
        assert!(selection.end <= total);

        // A press on an edge without moving makes it active and moves nothing.
        window.modifiers = Modifiers::NONE;
        let at = pos2(window.x_of(main, snap), y);
        window.click(at);
        let after = window.model.chop().selection.expect("kept");
        assert_eq!((after.start, after.end), (selection.start, selection.end));
        assert_eq!(after.active, Edge::Start);
    }

    /// A press inside the selection moves the edge on its side of the
    /// middle to where it landed, keeping the other, and a drag carries
    /// that edge on — so an edge off screen while zoomed in can be trimmed
    /// from what is on screen (the user's, 2026-11-16).
    #[test]
    fn pressing_inside_moves_the_nearer_edge_there() {
        let (mut window, _, main) = gesture_window();
        let y = main.center().y;
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        // Unsnapped, so the frames are the pointer's.
        window.modifiers = Modifiers::SHIFT;

        // A click left of the middle moves the start, and is not a click to
        // play.
        let at = window.x_of(main, 40_000);
        let (raw, _) = window.frames_at(main, at);
        let out = window.drag(pos2(at, y), pos2(at, y));
        assert_eq!(out.click, None, "a click inside is not a click to play");
        let expected = chop::Selection { start: raw, end: 70_000, active: Edge::Start };
        assert_eq!(window.model.chop().selection, Some(expected));

        // A press right of the middle moves the end, and drags it on.
        let at = window.x_of(main, 60_000);
        let to = at - 40.0;
        let (raw, _) = window.frames_at(main, to);
        window.drag(pos2(at, y), pos2(to, y));
        let expected = chop::Selection { start: expected.start, end: raw, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(expected));

        // With snap on and Shift up, the press snaps the edge it moves.
        window.modifiers = Modifiers::NONE;
        let at = window.x_of(main, expected.start + 2_000);
        let (_, snapped) = window.frames_at(main, at);
        window.click(pos2(at, y));
        assert_eq!(window.model.chop().selection.expect("kept").start, snapped);

        // With Ctrl held a press inside is the drag onto a lane, and moves
        // nothing.
        let before = window.model.chop().selection;
        let middle = before.map(|selection| selection.start + selection.len() / 2);
        window.modifiers = Modifiers::CTRL | Modifiers::COMMAND;
        let inside = window.x_of(main, middle.expect("a selection"));
        let out = window.drag(pos2(inside, y), pos2(inside + 80.0, y));
        assert_eq!(out.click, None);
        assert_eq!(window.model.chop().selection, before);
    }

    /// A lane row's height in the stand-in list [`Window::lanes`] draws.
    const LANE_ROW: f32 = 24.0;

    /// A gesture window with the lane list under the strip and a selection
    /// of `[30 000, 70 000)`; answers it and the main view.
    fn chop_drag_window() -> (Window, chop::Selection, Rect) {
        let (mut window, _, _) = gesture_window();
        window.lanes = true;
        // Four lanes under a shorter strip, so the window has room below
        // the rows.
        while window.model.lanes().len() > 4 {
            let last = window.model.lanes().last().map(|lane| lane.id).expect("a lane");
            assert!(window.model.delete_lane(last));
        }
        window.model.set_strip(false, 200.0);
        window.frame(Vec::new());
        window.frame(Vec::new());
        let main = canvas_layout(window.out.expect("drawn").canvas.expect("a canvas")).main;
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        // Made, as a drag would, in a gesture of its own.
        assert!(window.model.commit());
        window.frame(Vec::new());
        (window, selection, main)
    }

    impl Window {
        /// Press inside the selection at `from`, carry it to `to`, let go
        /// there: answers what landed, and checks the payload was in the air
        /// (with the outline drawn) on the way unless `to` is in the strip.
        /// Ctrl is held for it, as the carry needs.
        fn drag_chop(&mut self, from: Pos2, to: Pos2) -> Option<u32> {
            let held = self.modifiers;
            self.modifiers = Modifiers::CTRL | Modifiers::COMMAND;
            let landed = self.carry_chop(from, to);
            self.modifiers = held;
            landed
        }

        fn carry_chop(&mut self, from: Pos2, to: Pos2) -> Option<u32> {
            self.frame(vec![Event::PointerMoved(from)]);
            self.frame(vec![self.pointer_button(from, true)]);
            for step in 1..=5 {
                let at = from + (to - from) * (step as f32 / 5.0);
                self.frame(vec![Event::PointerMoved(at)]);
            }
            let carried = drops::chop_dragged(&self.ctx);
            let selection = self.model.chop().selection.expect("a selection");
            assert_eq!(
                carried,
                Some(drops::ChopPayload { start: selection.start, end: selection.end }),
                "the selection is in the air"
            );
            let outlined = self.shapes.iter().any(|shape| {
                matches!(shape, Shape::Rect(rect) if rect.stroke == theme::drop_target_stroke())
            });
            assert_eq!(outlined, !self.out.expect("drawn").rect.contains(to), "outline");
            self.frame(vec![self.pointer_button(to, false)]);
            let landed = self.landed;
            self.frame(Vec::new());
            assert_eq!(drops::chop_dragged(&self.ctx), None, "the payload is gone");
            landed
        }
    }

    /// The grip along the selection's bottom carries the chop with a plain
    /// drag — no Ctrl — and moves no edge on the way.
    #[test]
    fn a_plain_drag_from_the_grip_carries_the_chop() {
        let (mut window, selection, main) = chop_drag_window();
        let grip = grip_rect(main, window.model.chop()).expect("the selection has a grip");
        assert!(window.texts.iter().any(|(text, _)| text == GRIP_LABEL), "the grip says so");
        let row = window.rows[2];
        assert_eq!(window.carry_chop(grip.center(), row.rect.center()), Some(row.id));
        let sample = window.model.lane(row.id).and_then(|lane| lane.sample.clone());
        let origin = sample.and_then(|sample| sample.origin.clone()).expect("a chop");
        assert_eq!((origin.start_frame, origin.end_frame), (selection.start, selection.end));
        assert_eq!(window.model.chop().selection, Some(selection), "no edge moved");
    }

    /// A press inside moves the nearer edge — but dragged down off the
    /// waveform it puts the edge back and carries the selection as it was.
    #[test]
    fn a_press_inside_dragged_down_off_the_waveform_carries_the_chop() {
        let (mut window, selection, main) = chop_drag_window();
        let depth = window.model.undo_depth();
        let row = window.rows[1];
        let from = pos2(window.x_of(main, 40_000), main.center().y);
        assert_eq!(window.carry_chop(from, row.rect.center()), Some(row.id));
        let sample = window.model.lane(row.id).and_then(|lane| lane.sample.clone());
        let origin = sample.and_then(|sample| sample.origin.clone()).expect("a chop");
        assert_eq!((origin.start_frame, origin.end_frame), (selection.start, selection.end));
        let after = window.model.chop().selection.expect("kept");
        assert_eq!((after.start, after.end), (selection.start, selection.end), "edge put back");
        assert_eq!(window.model.undo_depth(), depth + 1, "one step: the lane's");

        // Dragged sideways, it stays an edge drag.
        let to = pos2(from.x + 40.0, main.center().y);
        window.drag(from, to);
        assert!(drops::chop_dragged(&window.ctx).is_none());
        assert_ne!(window.model.chop().selection.map(|s| s.start), Some(selection.start));
    }

    /// The play button says stop while the strip's preview sounds.
    #[test]
    fn the_play_button_says_stop_while_playing() {
        let mut window = key_window();
        assert!(window.texts.iter().any(|(text, _)| text == PLAY_LABEL));
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        window.previewer.channel().set_playing(true);
        window.frame(Vec::new());
        assert!(window.texts.iter().any(|(text, _)| text == STOP_LABEL), "stop while playing");
        assert!(!window.texts.iter().any(|(text, _)| text == PLAY_LABEL));
    }

    #[test]
    fn a_selection_dropped_on_a_row_replaces_its_sample() {
        let (mut window, selection, main) = chop_drag_window();
        let lanes = window.model.lanes().len();
        let row = window.rows[1];
        let before = window.model.lane(row.id).expect("a lane").clone();
        let depth = window.model.undo_depth();
        let from = pos2(window.x_of(main, 50_000), main.center().y);
        assert_eq!(window.drag_chop(from, row.rect.center()), Some(row.id));

        assert_eq!(window.model.lanes().len(), lanes, "no lane added");
        let lane = window.model.lane(row.id).expect("still there");
        let sample = lane.sample.as_ref().expect("a sample");
        let origin = sample.origin.as_ref().expect("a chop");
        assert_eq!((origin.start_frame, origin.end_frame), (selection.start, selection.end));
        assert_eq!(origin.source_path, "/music/Tone.wav");
        assert_eq!(sample.data.frames.len() as u64, 2 * (selection.end - selection.start));
        assert_eq!((&lane.name, lane.note), (&before.name, before.note), "kept");
        assert_eq!(window.model.undo_depth(), depth + 1, "one undo step");
        assert_eq!(window.model.chop().selection, Some(selection), "selection untouched");
    }

    #[test]
    fn a_selection_dropped_below_the_list_adds_a_lane() {
        let (mut window, selection, main) = chop_drag_window();
        let lanes = window.model.lanes().len();
        let below = pos2(window.list_rect.center().x, window.list_rect.max.y - 4.0);
        assert!(below.y > window.rows.last().expect("rows").rect.max.y, "below the rows");
        let from = pos2(window.x_of(main, 40_000), main.center().y);
        let id = window.drag_chop(from, below).expect("a lane added");

        assert_eq!(window.model.lanes().len(), lanes + 1);
        assert_eq!(window.model.lanes().last().map(|lane| lane.id), Some(id));
        let lane = window.model.lane(id).expect("the new lane");
        assert_eq!(lane.name, "Tone 1");
        let origin = lane.sample.as_ref().and_then(|sample| sample.origin.as_ref());
        let frames = origin.map(|origin| (origin.start_frame, origin.end_frame));
        assert_eq!(frames, Some((selection.start, selection.end)));
        assert!(window.model.undo(), "one undo");
        assert_eq!(window.model.lanes().len(), lanes, "takes it away");
    }

    #[test]
    fn a_selection_let_go_over_the_strip_does_nothing() {
        let (mut window, selection, main) = chop_drag_window();
        let lanes = window.model.lanes().len();
        let depth = window.model.undo_depth();
        let from = pos2(window.x_of(main, 50_000), main.center().y);
        assert_eq!(window.drag_chop(from, from + vec2(120.0, 0.0)), None);
        assert_eq!(window.model.lanes().len(), lanes);
        assert_eq!(window.model.undo_depth(), depth);
        assert_eq!(window.model.chop().selection, Some(selection));
        assert!(window.model.lanes().iter().all(|lane| lane.sample.is_none()));
    }

    #[test]
    fn a_click_outside_reports_its_frame_and_selects_nothing() {
        let (mut window, _, main) = gesture_window();
        let at = pos2(main.min.x + 222.4, main.center().y);
        let (raw, _) = window.frames_at(main, at.x);
        window.frame(vec![Event::PointerMoved(at)]);
        assert_eq!(window.frame(vec![window.pointer_button(at, true)]).click, None);
        let out = window.frame(vec![window.pointer_button(at, false)]);
        assert_eq!(out.click, Some(raw));
        assert_eq!(window.frame(Vec::new()).click, None, "once");
        assert_eq!(window.model.chop().selection, None);

        // Outside the main view nothing is selected or clicked (the
        // overview's drag pans a fitted view nowhere).
        let layout = canvas_layout(window.out.expect("drawn").canvas.expect("a canvas"));
        for at in [layout.overview.center(), layout.ruler.center()] {
            let out = window.drag(at, at + vec2(120.0, 0.0));
            assert_eq!(out.click, None);
            assert_eq!(window.model.chop().selection, None);
        }
    }

    /// A gesture window zoomed in to 20 frames a pixel, scrolled to
    /// `first`, and where the overview row is.
    fn overview_window(first: u64) -> (Window, u64, Rect, Rect) {
        let (mut window, total, main) = gesture_window();
        window.model.set_view(View { first, frames_per_pixel: 20.0 }, main.width());
        window.frame(Vec::new());
        assert_eq!(window.model.chop().view, View { first, frames_per_pixel: 20.0 });
        let canvas = window.out.expect("drawn").canvas.expect("a canvas");
        let overview = canvas_layout(canvas).overview;
        (window, total, main, overview)
    }

    #[test]
    fn an_overview_click_centres_the_view_there() {
        let (mut window, total, main, overview) = overview_window(0);
        let at = pos2(overview.min.x + overview.width() * 0.6, overview.center().y);
        let frame = chop::overview_frame(at.x - overview.min.x, overview.width(), total);
        window.click(at);
        let view = window.model.chop().view;
        assert_eq!(view.frames_per_pixel, 20.0, "the zoom stays");
        let middle = view.frame_at(main.width() / 2.0);
        assert!(middle.abs_diff(frame) <= 20, "{middle} is the middle, clicked {frame}");
        assert_eq!(window.model.chop().selection, None);

        // On the selection's band too: a click centres, the selection stays.
        let selection = chop::Selection { start: 30_000, end: 40_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        let band = overview.min.x + 35_000.0 / total as f32 * overview.width();
        window.click(pos2(band, overview.center().y));
        let view = window.model.chop().view;
        let frame = chop::overview_frame(band - overview.min.x, overview.width(), total);
        assert!(view.frame_at(main.width() / 2.0).abs_diff(frame) <= 20);
        assert_eq!(window.model.chop().selection, Some(selection));

        // Near the end it stops at the source's end.
        window.click(pos2(overview.max.x - 1.0, overview.center().y));
        let view = window.model.chop().view;
        let at_end = View { first: u64::MAX, frames_per_pixel: 20.0 }.clamped(total, main.width());
        assert_eq!(view, at_end);
    }

    #[test]
    fn dragging_the_overview_band_moves_the_selection() {
        let (mut window, total, _, overview) = overview_window(10_000);
        let selection = chop::Selection { start: 30_000, end: 40_000, active: Edge::Start };
        assert!(window.model.set_selection(Some(selection)));
        let y = overview.center().y;
        let from = overview.min.x + 35_000.0 / total as f32 * overview.width();
        let out = window.drag(pos2(from, y), pos2(from + 60.0, y));
        assert_eq!(out.click, None);
        let delta = chop::overview_frames(60.0, overview.width(), total) as u64;
        assert!(delta > 0);
        let moved = chop::Selection { start: 30_000 + delta, end: 40_000 + delta, ..selection };
        assert_eq!(window.model.chop().selection, Some(moved), "same length, unsnapped");
        assert_eq!(window.model.chop().view.first, 10_000, "the view stays put");

        // Dragged far left it stops at the source's start, length kept.
        let from = overview.min.x + (35_000 + delta) as f32 / total as f32 * overview.width();
        window.drag(pos2(from, y), pos2(overview.min.x - 50.0, y));
        let at_start = chop::Selection { start: 0, end: 10_000, ..selection };
        assert_eq!(window.model.chop().selection, Some(at_start));
    }

    #[test]
    fn dragging_the_overview_elsewhere_pans() {
        let (mut window, total, _, overview) = overview_window(10_000);
        let selection = chop::Selection { start: 100_000, end: 110_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        let y = overview.center().y;
        let from = overview.min.x + 40.0;
        let out = window.drag(pos2(from, y), pos2(from + 30.0, y));
        assert_eq!(out.click, None);
        let delta = chop::overview_frames(30.0, overview.width(), total) as u64;
        let view = window.model.chop().view;
        assert_eq!(view, View { first: 10_000 + delta, frames_per_pixel: 20.0 }, "not centred");
        assert_eq!(window.model.chop().selection, Some(selection), "the selection stays");

        // Back left past the start: it stops there.
        window.drag(pos2(from, y), pos2(from - 300.0, y));
        assert_eq!(window.model.chop().view.first, 0);

        // And a fitted view has nowhere to pan.
        let main = canvas_layout(window.out.expect("drawn").canvas.expect("a canvas")).main;
        let fit = View::fit(total, main.width());
        window.model.set_view(fit, main.width());
        window.drag(pos2(from, y), pos2(from + 30.0, y));
        assert_eq!(window.model.chop().view, fit);
    }

    fn key(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// A 48 kHz source (10 ms = 480 frames) with a selection whose end is
    /// active, the strip owning the keyboard.
    fn key_window() -> Window {
        let mut window = Window::new();
        window.load("/songs/keys.wav");
        let selection = chop::Selection { start: 1_000, end: 3_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        window.frame(Vec::new());
        window
    }

    fn selection(window: &Window) -> chop::Selection {
        window.model.chop().selection.expect("a selection")
    }

    #[test]
    fn arrows_nudge_the_active_edge_and_shift_is_fine() {
        let mut window = key_window();
        window.frame(vec![key(Key::ArrowRight, Modifiers::NONE)]);
        assert_eq!((selection(&window).start, selection(&window).end), (1_000, 3_480));
        window.frame(vec![key(Key::ArrowLeft, Modifiers::SHIFT)]);
        assert_eq!(selection(&window).end, 3_432);
        window.frame(vec![key(Key::ArrowLeft, Modifiers::NONE)]);
        assert_eq!(selection(&window).end, 2_952);
        let keys = window.leftover.iter().filter(|event| matches!(event, Event::Key { .. }));
        assert_eq!(keys.count(), 0, "the arrows are the strip's");

        // Up makes the start active; the arrows move it instead.
        window.frame(vec![key(Key::ArrowUp, Modifiers::NONE)]);
        assert_eq!(selection(&window).active, Edge::Start);
        assert_eq!((selection(&window).start, selection(&window).end), (1_000, 2_952));
        window.frame(vec![key(Key::ArrowLeft, Modifiers::NONE)]);
        assert_eq!(selection(&window).start, 520);
        window.frame(vec![key(Key::ArrowRight, Modifiers::SHIFT)]);
        assert_eq!(selection(&window).start, 568);
        // Two presses in a frame (a key repeat) move twice.
        let right = key(Key::ArrowRight, Modifiers::NONE);
        window.frame(vec![right.clone(), right]);
        assert_eq!(selection(&window).start, 1_528);

        // Down makes the end active again.
        window.frame(vec![key(Key::ArrowDown, Modifiers::NONE)]);
        assert_eq!(selection(&window).active, Edge::End);
        window.frame(vec![key(Key::ArrowRight, Modifiers::SHIFT)]);
        assert_eq!((selection(&window).start, selection(&window).end), (1_528, 3_000));
    }

    #[test]
    fn nudges_stop_at_the_ends_and_at_the_other_edge() {
        let mut window = key_window();
        let total = window.model.chop().total();
        for _ in 0..20 {
            window.frame(vec![key(Key::ArrowRight, Modifiers::NONE)]);
        }
        assert_eq!(selection(&window).end, total);
        for _ in 0..20 {
            window.frame(vec![key(Key::ArrowLeft, Modifiers::NONE)]);
        }
        let shortest = selection(&window);
        assert_eq!(shortest.end - shortest.start, chop::MIN_SELECTION_FRAMES);
        assert_eq!(shortest.start, 1_000, "the start stays");
        window.frame(vec![key(Key::ArrowUp, Modifiers::NONE)]);
        for _ in 0..20 {
            window.frame(vec![key(Key::ArrowLeft, Modifiers::NONE)]);
        }
        assert_eq!(selection(&window).start, 0);
    }

    #[test]
    fn z_toggles_snap_and_escape_clears_the_selection() {
        let mut window = key_window();
        assert!(window.model.chop().snap);
        let out = window.frame(vec![key(Key::Z, Modifiers::NONE)]);
        assert!(out.snap);
        assert!(!window.model.chop().snap);
        window.frame(vec![key(Key::Z, Modifiers::NONE)]);
        assert!(window.model.chop().snap);

        window.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert_eq!(window.model.chop().selection, None);
        // With nothing to clear Escape does nothing, and the arrows neither.
        window.frame(vec![key(Key::Escape, Modifiers::NONE), key(Key::ArrowLeft, Modifiers::NONE)]);
        assert_eq!(window.model.chop().selection, None);
        assert!(window.model.chop().source.is_some(), "the source stays open");

        // Z works without a source, as the button does.
        let mut empty = Window::new();
        empty.frame(vec![key(Key::Z, Modifiers::NONE)]);
        assert!(!empty.model.chop().snap);
    }

    #[test]
    fn space_and_ctrl_keys_are_left_for_bitwig() {
        let mut window = key_window();
        let events = vec![
            key(Key::Space, Modifiers::NONE),
            key(Key::Z, Modifiers::CTRL),
            key(Key::ArrowLeft, Modifiers::CTRL),
            key(Key::F1, Modifiers::NONE),
        ];
        window.frame(events.clone());
        assert!(window.model.chop().snap, "Ctrl+Z is not Z");
        assert_eq!(selection(&window).end, 3_000);
        let left: Vec<&Event> =
            window.leftover.iter().filter(|event| matches!(event, Event::Key { .. })).collect();
        assert_eq!(left, events.iter().collect::<Vec<_>>(), "none is taken");
    }

    #[test]
    fn keys_do_nothing_unless_the_strip_owns_the_keyboard() {
        for owner in [KeyOwner::Lanes, KeyOwner::Browser] {
            let mut window = key_window();
            window.owner = owner;
            let events = vec![
                key(Key::ArrowRight, Modifiers::NONE),
                key(Key::ArrowUp, Modifiers::NONE),
                key(Key::Z, Modifiers::NONE),
                key(Key::Escape, Modifiers::NONE),
            ];
            window.frame(events.clone());
            let untouched = chop::Selection { start: 1_000, end: 3_000, active: Edge::End };
            assert_eq!(window.model.chop().selection, Some(untouched), "{owner:?}");
            assert!(window.model.chop().snap);
            let left = window.leftover.iter().filter(|event| matches!(event, Event::Key { .. }));
            assert_eq!(left.count(), events.len(), "{owner:?} leaves them in the input");
        }
    }

    #[test]
    fn keys_do_nothing_while_a_text_box_has_the_focus() {
        let mut window = key_window();
        window.text_box = true;
        window.frame(Vec::new());
        window.frame(Vec::new());
        assert!(window.ctx.text_edit_focused(), "the box has the focus");
        window.frame(vec![
            key(Key::ArrowRight, Modifiers::NONE),
            key(Key::ArrowUp, Modifiers::NONE),
            key(Key::Z, Modifiers::NONE),
            key(Key::Escape, Modifiers::NONE),
        ]);
        let untouched = chop::Selection { start: 1_000, end: 3_000, active: Edge::End };
        assert_eq!(window.model.chop().selection, Some(untouched));
        assert!(window.model.chop().snap);

        // The box let go, the keys are the strip's again.
        window.text_box = false;
        window.ctx.memory_mut(|memory| memory.surrender_focus(egui::Id::unique("test_text_box")));
        window.frame(Vec::new());
        window.frame(vec![key(Key::ArrowRight, Modifiers::NONE)]);
        assert_eq!(selection(&window).end, 3_480);
    }

    #[test]
    fn n_moves_the_selection_on_by_its_length_and_refuses_at_the_end() {
        let mut window = key_window();
        let total = window.model.chop().total();
        assert_eq!(total, 4_800);
        let first = chop::Selection { start: 0, end: 1_000, active: Edge::End };
        assert!(window.model.set_selection(Some(first)));
        let out = window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert!(out.next);
        assert_eq!((selection(&window).start, selection(&window).end), (1_000, 2_000));
        assert_eq!(selection(&window).active, Edge::End, "the active edge stays");
        let keys = window.leftover.iter().filter(|event| matches!(event, Event::Key { .. }));
        assert_eq!(keys.count(), 0, "N is the strip's");
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!((selection(&window).start, selection(&window).end), (3_000, 4_000));

        // 4000 + 1000 would pass the end: refused, nothing moves.
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!((selection(&window).start, selection(&window).end), (3_000, 4_000));

        // Landing exactly on the end is allowed.
        let tail = chop::Selection { start: 2_400, end: 3_600, active: Edge::Start };
        assert!(window.model.set_selection(Some(tail)));
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!((selection(&window).start, selection(&window).end), (3_600, 4_800));
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!((selection(&window).start, selection(&window).end), (3_600, 4_800));

        // Nothing selected, nothing to move; no source, nothing either.
        assert!(window.model.set_selection(None));
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!(window.model.chop().selection, None);
        let mut empty = Window::new();
        empty.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!(empty.model.chop().selection, None);
    }

    #[test]
    fn n_does_nothing_unless_the_strip_owns_the_keyboard() {
        for owner in [KeyOwner::Lanes, KeyOwner::Browser] {
            let mut window = key_window();
            window.owner = owner;
            let first = chop::Selection { start: 0, end: 1_000, active: Edge::End };
            assert!(window.model.set_selection(Some(first)));
            window.frame(vec![key(Key::N, Modifiers::NONE)]);
            assert_eq!(window.model.chop().selection, Some(first), "{owner:?}");
            let left = window.leftover.iter().filter(|event| matches!(event, Event::Key { .. }));
            assert_eq!(left.count(), 1, "{owner:?} leaves N in the input");
        }
    }

    #[test]
    fn enter_with_a_selection_adds_a_lane_and_without_one_does_nothing() {
        let mut window = key_window();
        let before = window.model.lanes().len();
        let depth = window.model.undo_depth();
        let out = window.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert!(out.add_lane);
        let id = out.added.expect("a lane was added");
        let lanes = window.model.lanes();
        assert_eq!(lanes.len(), before + 1);
        let lane = lanes.last().expect("the new lane");
        assert_eq!(lane.id, id);
        assert_eq!(lane.name, "keys 1");
        let sample = lane.sample.as_ref().expect("the chop is the lane's sample");
        assert_eq!(sample.data.frames.len(), 2_000, "the selection's frames, mono");
        let origin = sample.origin.as_ref().expect("the chop knows its source");
        assert_eq!((origin.start_frame, origin.end_frame), (1_000, 3_000));
        assert_eq!(window.model.undo_depth(), depth + 1, "one undo step");
        let keys = window.leftover.iter().filter(|event| matches!(event, Event::Key { .. }));
        assert_eq!(keys.count(), 0, "Enter is the strip's");
        assert!(window.model.chop().selection.is_some(), "the selection stays");

        // Again: the second chop of the same source.
        window.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(window.model.lanes().last().expect("a lane").name, "keys 2");

        // Nothing selected: nothing added, no step.
        assert!(window.model.set_selection(None));
        let (count, depth) = (window.model.lanes().len(), window.model.undo_depth());
        let out = window.frame(vec![key(Key::Enter, Modifiers::NONE)]);
        assert_eq!(out.added, None);
        assert_eq!(window.model.lanes().len(), count);
        assert_eq!(window.model.undo_depth(), depth);

        // No source: nothing either.
        let mut empty = Window::new();
        let count = empty.model.lanes().len();
        assert_eq!(empty.frame(vec![key(Key::Enter, Modifiers::NONE)]).added, None);
        assert_eq!(empty.model.lanes().len(), count);
    }

    #[test]
    fn enter_does_nothing_unless_the_strip_owns_the_keyboard() {
        for owner in [KeyOwner::Lanes, KeyOwner::Browser] {
            let mut window = key_window();
            window.owner = owner;
            let count = window.model.lanes().len();
            let out = window.frame(vec![key(Key::Enter, Modifiers::NONE)]);
            assert_eq!(out.added, None, "{owner:?}");
            assert_eq!(window.model.lanes().len(), count, "{owner:?}");
            let left = window.leftover.iter().filter(|event| matches!(event, Event::Key { .. }));
            assert_eq!(left.count(), 1, "{owner:?} leaves Enter in the input");
        }
    }

    #[test]
    fn add_lane_button_adds_the_selection_and_is_off_without_one() {
        let mut window = key_window();
        let count = window.model.lanes().len();
        let at = window.text_at(ADD_LANE_LABEL);
        window.click(at);
        assert_eq!(window.model.lanes().len(), count + 1);
        let lane = window.model.lanes().last().expect("the new lane");
        assert_eq!(lane.name, "keys 1");
        assert!(window.model.undo());
        assert_eq!(window.model.lanes().len(), count, "undone in one step");

        // Without a selection the button is disabled: a click adds nothing.
        assert!(window.model.set_selection(None));
        window.frame(Vec::new());
        window.click(at);
        assert_eq!(window.model.lanes().len(), count);
    }

    #[test]
    fn next_button_advances_and_the_view_follows() {
        let mut window = key_window();
        let first = chop::Selection { start: 0, end: 1_000, active: Edge::End };
        assert!(window.model.set_selection(Some(first)));
        // Zoomed in on the selection: the next step lies off to the right.
        let width = window.model.chop().view_width_px;
        window.model.set_view(View::around(0, 1_000, 4_800, width), width);
        window.frame(Vec::new());
        let before = window.model.chop().view;
        assert!(before.frame_at(width) < 2_000, "the next step is not shown yet");

        let at = window.text_at(NEXT_LABEL);
        window.click(at);
        assert_eq!((selection(&window).start, selection(&window).end), (1_000, 2_000));
        let view = window.model.chop().view;
        assert_eq!(view.frames_per_pixel, before.frames_per_pixel, "the zoom stays");
        assert!(view.first <= 1_000 && view.frame_at(width) >= 2_000, "{view:?}");

        // A step that stays in view leaves the view alone.
        let fit = View::fit(4_800, width);
        window.model.set_view(fit, width);
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        assert_eq!((selection(&window).start, selection(&window).end), (2_000, 3_000));
        assert_eq!(window.model.chop().view, fit);
    }

    #[test]
    fn next_moves_a_playing_selection_preview_with_it() {
        let mut window = key_window();
        let first = chop::Selection { start: 0, end: 1_000, active: Edge::End };
        assert!(window.model.set_selection(Some(first)));
        window.model.set_looping(true);
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        window.previewer.channel().set_playing(true);
        window.frame(vec![key(Key::N, Modifiers::NONE)]);
        let live = window.previewer.channel().live_region();
        assert_eq!(live, Region { start: 1_000, end: 2_000 });
        assert!(window.previewer.channel().get().region.is_some(), "still playing");
    }

    /// What the channel asks the engine for: the region, the start flag
    /// and the generation.
    fn request(window: &Window) -> (Option<Region>, bool, u64) {
        let state = window.previewer.channel().get();
        (state.region, state.looping, state.generation)
    }

    /// Whether the channel holds the open source's audio.
    fn previews_the_source(window: &Window) -> bool {
        previewing_source(&window.model, &window.previewer)
    }

    /// `A` plays and stops the selection whoever has the keyboard (the
    /// user's, 2026-11-16), but not into a text box.
    #[test]
    fn a_plays_the_selection_from_anywhere_but_a_text_box() {
        for owner in [KeyOwner::Chop, KeyOwner::Lanes, KeyOwner::Browser] {
            let mut window = key_window();
            window.owner = owner;
            window.frame(vec![key(Key::A, Modifiers::NONE)]);
            let (region, _, first) = request(&window);
            assert_eq!(region, Some(Region { start: 1_000, end: 3_000 }), "{owner:?}");
            let taken = window.leftover.iter().all(|event| !matches!(event, Event::Key { .. }));
            assert!(taken, "{owner:?}: A is taken out of the input");

            window.previewer.channel().set_playing(true);
            window.frame(vec![key(Key::A, Modifiers::NONE)]);
            let (region, _, stopped) = request(&window);
            assert_eq!(region, None, "{owner:?}: A again stops it");
            assert!(stopped > first);
        }

        let mut window = key_window();
        window.text_box = true;
        window.frame(vec![key(Key::A, Modifiers::NONE)]);
        assert_eq!(request(&window).0, None, "typed into the box, not played");
    }

    #[test]
    fn p_plays_the_selection_and_stops_it() {
        let mut window = key_window();
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        let (region, looping, first) = request(&window);
        assert_eq!(region, Some(Region { start: 1_000, end: 3_000 }));
        assert!(!looping);
        assert!(previews_the_source(&window));
        assert_eq!(window.previewer.channel().live_region(), region.expect("a region"));

        // Sounding (as the engine reports it), P stops it.
        window.previewer.channel().set_playing(true);
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        let (region, _, stopped) = request(&window);
        assert_eq!(region, None);
        assert!(stopped > first);

        // The play button does the same; with loop on the region loops.
        window.previewer.channel().set_playing(false);
        window.model.set_looping(true);
        window.frame(Vec::new());
        let at = window.text_at(PLAY_LABEL);
        window.click(at);
        let (region, looping, _) = request(&window);
        assert_eq!(region, Some(Region { start: 1_000, end: 3_000 }));
        assert!(looping);
        assert!(window.previewer.channel().live_looping());
    }

    #[test]
    fn p_with_nothing_selected_plays_from_the_view() {
        let mut window = key_window();
        window.model.set_selection(None);
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        let total = window.model.chop().total();
        let first = window.model.chop().view.first;
        assert_eq!(request(&window).0, Some(Region { start: first, end: total }));

        // And with no source at all, P and play do nothing.
        let mut empty = Window::new();
        empty.frame(vec![key(Key::P, Modifiers::NONE)]);
        assert_eq!(request(&empty), (None, false, 0));
    }

    #[test]
    fn l_flips_the_live_loop_flag_and_the_setting() {
        let mut window = key_window();
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        let generation = request(&window).2;
        assert!(!window.previewer.channel().live_looping());

        window.frame(vec![key(Key::L, Modifiers::NONE)]);
        assert!(window.previewer.channel().live_looping());
        assert!(window.model.chop().looping);
        assert_eq!(request(&window).2, generation, "no republish");

        let at = window.text_at(LOOP_LABEL);
        window.click(at);
        assert!(!window.previewer.channel().live_looping());
        assert!(!window.model.chop().looping);
        assert_eq!(request(&window).2, generation, "no republish");
    }

    #[test]
    fn an_edge_drag_moves_the_live_region() {
        let (mut window, _, main) = gesture_window();
        window.model.set_snap(false);
        let selection = chop::Selection { start: 30_000, end: 70_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        let generation = request(&window).2;
        let channel = Arc::clone(window.previewer.channel());
        assert_eq!(channel.live_region(), Region { start: 30_000, end: 70_000 });

        let y = main.center().y;
        let grab = window.x_of(main, 70_000);
        window.drag(pos2(grab, y), pos2(grab + 40.0, y));
        let moved = window.model.chop().selection.expect("kept");
        assert!(moved.end > 70_000);
        assert_eq!(channel.live_region(), Region { start: 30_000, end: moved.end });
        // A nudge too.
        window.frame(vec![key(Key::ArrowLeft, Modifiers::NONE)]);
        let nudged = window.model.chop().selection.expect("kept");
        assert_eq!(channel.live_region(), Region { start: 30_000, end: nudged.end });
        assert_eq!(request(&window).2, generation, "edges never republish");

        // A click-started preview does not follow the selection.
        window.model.set_selection(None);
        let at = pos2(main.min.x + 50.0, y);
        let (raw, _) = window.frames_at(main, at.x);
        window.click(at);
        let total = window.model.chop().total();
        assert_eq!(request(&window).0, Some(Region { start: raw, end: total }));
        let selection = chop::Selection { start: 1_000, end: 2_000, active: Edge::End };
        assert!(window.model.set_selection(Some(selection)));
        window.frame(Vec::new());
        assert_eq!(channel.live_region(), Region { start: raw, end: total });
    }

    #[test]
    fn a_click_plays_from_its_frame_or_stops() {
        let (mut window, total, main) = gesture_window();
        let at = pos2(main.min.x + 222.4, main.center().y);
        let (raw, _) = window.frames_at(main, at.x);
        window.click(at);
        assert_eq!(request(&window).0, Some(Region { start: raw, end: total }));
        assert!(!request(&window).1);

        window.previewer.channel().set_playing(true);
        window.wait_out_a_double_click();
        window.click(at);
        assert_eq!(request(&window).0, None, "a click while playing stops");
    }

    #[test]
    fn escape_stops_the_preview_before_clearing_the_selection() {
        let mut window = key_window();
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        window.previewer.channel().set_playing(true);
        window.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert_eq!(request(&window).0, None);
        assert!(window.model.chop().selection.is_some(), "kept for the first Escape");
        window.previewer.channel().set_playing(false);
        window.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert_eq!(window.model.chop().selection, None);
    }

    #[test]
    fn closing_the_source_stops_its_preview() {
        let mut window = key_window();
        window.frame(vec![key(Key::P, Modifiers::NONE), key(Key::L, Modifiers::NONE)]);
        assert!(previews_the_source(&window));
        let at = window.text_at(CLOSE_LABEL);
        window.click(at);
        assert!(window.model.chop().source.is_none());
        assert_eq!(request(&window).0, None, "stopped");
        assert!(window.previewer.channel().get().sample.is_none());
        assert!(!window.previewer.channel().live_looping());

        // Opening another source stops it too.
        window.load("/songs/one.wav");
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        assert!(request(&window).0.is_some());
        window.load("/songs/two.wav");
        window.frame(Vec::new());
        assert_eq!(request(&window).0, None);
    }

    #[test]
    fn the_browsers_previews_are_left_alone() {
        let mut window = key_window();
        // A browser preview: a sample with no region, not the source.
        let handle = window.model.handle().clone();
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 10].into() };
        let state = crate::preview::PreviewState {
            generation: 99,
            sample: Some(basedrop::Shared::new(&handle, data)),
            region: None,
            looping: false,
        };
        window.previewer.channel().publish(&handle, state);
        window.previewer.channel().set_playing(true);
        window.frame(Vec::new());
        assert_eq!(request(&window).2, 99);
        window.frame(vec![key(Key::Escape, Modifiers::NONE)]);
        assert_eq!(request(&window).2, 99, "Escape is not the browser's stop");
        assert_eq!(window.model.chop().selection, None);
        window.model.close_source();
        window.frame(Vec::new());
        assert_eq!(request(&window).2, 99);
    }

    #[test]
    fn the_hairline_shows_only_while_the_strips_preview_sounds() {
        let (mut window, _, main) = gesture_window();
        let view = window.model.chop().view;
        let x = hairline_x(main, &view, 48_000).expect("on screen");
        assert_eq!(x, main.min.x + view.px_of(48_000));
        assert_eq!(hairline_x(main, &view, u64::MAX), None, "off the right edge");

        assert!(!source_playing(&window.previewer));
        window.frame(vec![key(Key::P, Modifiers::NONE)]);
        // Published but not yet sounding: no hairline, but a poll for it.
        assert!(!source_playing(&window.previewer));
        let channel = Arc::clone(window.previewer.channel());
        channel.set_playing(true);
        channel.set_position(48_000);
        assert!(source_playing(&window.previewer));
        window.frame(Vec::new());
        let stroke = |shape: &Shape| match shape {
            Shape::LineSegment { points, stroke } => {
                points[0].x == x && points[1].x == x && stroke.color == HAIRLINE_COLOUR
            }
            _ => false,
        };
        assert!(window.shapes.iter().any(stroke), "drawn at the position");
        channel.set_playing(false);
        window.frame(Vec::new());
        assert!(!window.shapes.iter().any(stroke), "gone once it stops");
    }

    #[test]
    fn the_hairline_follows_the_owner_not_the_audio() {
        let (mut window, _, main) = gesture_window();
        let view = window.model.chop().view;
        let x = hairline_x(main, &view, 48_000).expect("on screen");
        let hairline = |shapes: &[Shape]| {
            shapes.iter().any(|shape| match shape {
                Shape::LineSegment { points, stroke } => {
                    points[0].x == x && points[1].x == x && stroke.color == HAIRLINE_COLOUR
                }
                _ => false,
            })
        };
        let channel = Arc::clone(window.previewer.channel());
        channel.set_position(48_000);
        channel.set_playing(true);

        // The browser previewing a file while the source is open: no hairline,
        // even with the engine reporting a preview sounding at that frame.
        let dir = std::env::temp_dir().join(format!("chopdrop-strip-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let path = dir.join("browser.wav");
        std::fs::write(&path, b"not audio").expect("scratch file");
        window.previewer.play(&path);
        assert_eq!(window.previewer.owner(), Some(PreviewOwner::Browser));
        window.frame(Vec::new());
        assert!(!hairline(&window.shapes), "not for the browser's preview");
        assert_eq!(window.previewer.owner(), Some(PreviewOwner::Browser), "left alone");

        // The strip's own preview of that source: the hairline shows.
        let source = window.model.chop().source.as_ref().expect("open").data.clone();
        window.previewer.play_region(source, Region { start: 0, end: 96_000 }, false);
        channel.set_position(48_000);
        assert_eq!(window.previewer.owner(), Some(PreviewOwner::Strip));
        window.frame(Vec::new());
        assert!(hairline(&window.shapes), "shown for the strip's preview");
        std::fs::remove_dir_all(&dir).ok();
    }
}
