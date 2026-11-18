//! The lane list: one row per lane, in display order.
//!
//! The window's whole middle. Ten fields across a row — drag grip, colour
//! dot, audition, note, name, sample, volume, pan, choke group and
//! one-shot/gate — in that order, at the theme's widths, so that a column
//! lines up down the list. The audition button sits up front, after the
//! dot, and is wider than a toggle (the user's, 2026-11-16).
//!
//! # Painted fields and answering ones
//!
//! The row was drawn first as a read of the model, and every gesture over it
//! came after that, one at a time: the note field, the name and the grip,
//! the volume, pan, choke and mode controls, the drops, the audition button
//! and the activity light, then the right-click menu. So a field with a
//! gesture answers — it is one of egui's widgets, styled by
//! [`theme::visuals`], or it is sensed for its gesture — and a field without
//! one is *painted*: a control's box with text placed in it, which cannot be
//! clicked, dragged or focused and so cannot write to the model. The two are
//! drawn through the same [`field`] allocation and come out at the same
//! size, which is what keeps a column lined up down the list. What is still
//! painted is the sample column, which says what a lane holds rather than
//! offering a value to change; the fields that stay hand-drawn are the ones
//! egui has nothing for: the grip, the colour dot, and the audition button's
//! triangle.
//!
//! # Four fields of one kind
//!
//! The note, the volume, the pan and the choke group are four numbers in
//! four units, and a row offers the same three gestures over every one of
//! them: a drag to move it, a notch of the wheel to step it, a click to type
//! it. So they are one widget ([`value_field`]) handed the unit's range and
//! the two functions that spell it, and what comes out of a frame is which
//! setting was touched and the value it now reads ([`LaneValue`]) — which is
//! also what keeps one drag speed and one notch across all four
//! ([`FIELD_DRAG_SPEED`], [`FIELD_SCROLL_POINTS`]) instead of four a user
//! would have to learn field by field. The one-shot/gate toggle is not one
//! of them: two modes are a click, not a number.
//!
//! Every gesture a frame finds is handed to the model's writer API after the
//! drawing and not during it ([`lane_list`]), because a row reads the model
//! as it is drawn and a writer needs it to itself; and each one ends in
//! [`Model::commit`] at the *end* of the gesture, which is what makes a drag
//! through an octave — or a row carried the length of the list — one undo
//! step rather than thirty.
//!
//! # The two fields the model is not the answer to
//!
//! A row's last two columns are about the audio thread rather than about the
//! lane list, and they are the two halves of one loop. The colour dot is the
//! lane's activity light: what it shows is a read of the published snapshot's
//! counters, kept and timed by [`crate::editor::activity::Lights`] and
//! arriving here as one number per row — nought for a lane at rest, one for a
//! lane struck this very frame. The audition button is the other direction: a
//! click on it changes nothing a lane holds, so it writes nothing and is
//! instead handed back out of [`LaneList`] for the window to put on the
//! engine's ring, which is the one gesture over this list that the list
//! cannot answer itself.
//!
//! # Ids of the lane's own
//!
//! egui names a widget by where it was drawn, counting them as they go by.
//! That is wrong for this list for one frame in every reorder: a row dragged
//! up the list changes places with the row above it *while the pointer is
//! still down*, and a grip named after its place would hand the drag it is
//! holding to whichever row took that place. So every row is drawn inside a
//! scope of its lane's own id ([`row_scope`]) and every widget in it is
//! named after the lane instead — the grip keeps its drag, the open rename
//! box keeps its focus, and a half-typed note keeps its text.
//!
//! What the sample column says is a read of one lane too, but of four
//! things at once — a file name, a hint, a job in flight or a message left
//! behind — so the choice between them is made outside this module
//! ([`crate::editor::status::sample_status`]) and [`sample_cell`] draws
//! whichever it is handed, in the colour and with the hover that come with
//! it.
//!
//! # The widths
//!
//! Nine of the ten fields are fixed ([`ROW_FIXED_WIDTH`]) and the sample
//! column takes what is left ([`sample_column_width`]), down to
//! [`theme::HEADER_SAMPLE_MIN_WIDTH`] — at which point the window itself
//! stops shrinking ([`crate::editor::MIN_WIDTH`] is built out of this
//! module's arithmetic, so the two cannot drift). The list scrolls
//! vertically and never sideways: a row that cannot fit is a window that is
//! too narrow, which the host is told not to allow.

use std::ops::RangeInclusive;
use std::path::PathBuf;

use egui::{
    Align, DragValue, Label, Modifiers, Pos2, Rect, Response, RichText, Sense, Shape, StrokeKind,
    TextEdit, Ui, Vec2,
};

use crate::chop::ChopOrigin;
use crate::editor::activity::Lights;
use crate::editor::menu::{self, Action, Confirm};
use crate::editor::selection::Selection;
use crate::editor::{drops, notes, status, theme};
use crate::model::{
    CHOKE_GROUPS, CHOKE_NONE, LANE_PAN_MAX, LANE_PAN_MIN, LANE_VOLUME_DB_MAX, LANE_VOLUME_DB_MIN,
    Lane, LaneMode, MAX_NAME_LEN, Model, OUTPUT_MAIN, clamp_pan, clamp_volume_db,
};
use crate::time::{Length, TimeMode};

// A row spells the empty choke group "0", and this is the zero it means: the
// model's own, so the number this module prints and the number the engine
// reads as "no group" cannot come apart.
const _: () = assert!(CHOKE_NONE == 0);

/// The one text in the editor that belongs to no widget: what a list with no
/// lanes in it says, centred in the window, at [`theme::HINT_SIZE`].
///
/// Only ever seen by a user who has deleted all eight lanes a new instance
/// comes with — and it is the one state of the window that says what to do
/// about it.
pub const EMPTY_HINT: &str = "Drag samples here from Bitwig's browser";

/// The one-shot/gate toggle's two faces: a whole sample, or a note's length.
///
/// A letter each, because the toggle is one square
/// ([`theme::HEADER_TOGGLE_WIDTH`], bitstep's Mute/Solo width) and because
/// the state is what has to be readable at a glance down a column of ten
/// lanes. [`mode_toggle`] hangs the click and the hover text on them.
pub const ONE_SHOT_LABEL: &str = "1";
/// The gate face of the toggle — see [`ONE_SHOT_LABEL`].
pub const GATE_LABEL: &str = "G";

/// What the choke-group field says when the pointer rests on it.
///
/// The one number in a row that cannot say what it is: a bare `3` between a
/// volume and a pan reads as a third setting of the same kind, where what it
/// means is which other lanes this one cuts short.
pub const CHOKE_HOVER: &str =
    "choke group: starting this lane silences the others in the same group (0: no group)";

/// What the toggle says while it is wearing [`ONE_SHOT_LABEL`] — and that it
/// is a switch at all, which one letter cannot.
pub const ONE_SHOT_HOVER: &str =
    "one-shot: plays the whole sample, however briefly the note is held — click for gate";
/// What the toggle says while it is wearing [`GATE_LABEL`]. See
/// [`ONE_SHOT_HOVER`].
pub const GATE_HOVER: &str = "gate: the sound ends when the note does — click for one-shot";

/// What the audition button says when the pointer rests on it, and what it
/// says instead on a lane with nothing to play.
///
/// A triangle is the one glyph in the window that needs no label, but it
/// does need to say *which* note it plays: an audition is the lane's own
/// note at full velocity ([`crate::engine::AUDITION_VELOCITY`]) and not a
/// preview of the file at its recorded pitch, so a tuned lane sounds from
/// here exactly as it will from bitstep.
pub const AUDITION_HOVER: &str = "play this lane at its own note (S plays the selected lane)";
/// What the audition button says while its lane's audition is sounding,
/// when it is drawn as a stop button.
pub const AUDITION_STOP_HOVER: &str = "stop this lane (S stops the selected lane)";
/// The sample cell's hover, after anything the cell itself has to say.
pub const BROWSE_HOVER: &str = "click to browse for a sample";

/// What the audition button says on a lane with no sample in it — the one
/// state in which a click on it does nothing at all.
pub const AUDITION_EMPTY_HOVER: &str = "nothing to play: drop a sample on this lane first";

/// How many fields a row draws, and so how many gaps there are between them.
const ROW_FIELDS: f32 = 10.0;

/// How far a row's number field moves for one point of drag: a quarter of
/// one of the field's own units, so four points of pointer are one semitone,
/// one decibel, one percent of pan or one choke group — and an octave is a
/// drag of about half an inch.
///
/// egui's own default for an integer drag value, said out loud because every
/// field names its own range and would otherwise take the speed from the
/// number behind it. One speed for all four of them (see the module header):
/// they sit in a row and are dragged by the same hand.
const FIELD_DRAG_SPEED: f64 = 0.25;

/// How many points of wheel a row's number field counts as one of its own
/// units: forty, which is what egui makes of one notch of a mouse wheel
/// (`InputOptions::line_scroll_speed`), so a notch is a semitone, a decibel,
/// a percent of pan or a choke group.
const FIELD_SCROLL_POINTS: f32 = 40.0;

/// A number field's unspent wheel, kept in egui's per-widget memory between
/// frames.
///
/// egui smooths a notch of the wheel out over several frames, so the forty
/// points of one arrive a handful at a time and no single frame of them is a
/// whole unit; the leftovers wait here, and a field that is scrolled slowly
/// still steps once for every notch. A type of its own rather than a bare
/// `f32` because [`DragValue`] keeps its own precise drag under the same id,
/// and egui's memory is keyed by id *and* type.
#[derive(Clone, Copy, Default)]
struct ScrollResidual(f32);

/// One of the four settings a row's number fields write, carrying the value
/// its field now reads in the unit the *lane* keeps it in.
///
/// Which setting and not just a number, because the four fields are one
/// widget (see the module header) and [`apply_value`] is where the four
/// writers it stands for are.
enum LaneValue {
    /// The note that plays the lane, 0..=[`notes::HIGHEST_NOTE`].
    Note(u8),
    /// The lane's volume in decibels, always a whole one from this window
    /// ([`whole_db`]).
    VolumeDb(f32),
    /// The lane's pan, [`LANE_PAN_MIN`]..=[`LANE_PAN_MAX`] — the field shows
    /// and drags it as a percentage ([`pan_percent`]).
    Pan(f32),
    /// The lane's choke group, [`CHOKE_NONE`]..=[`CHOKE_GROUPS`].
    Choke(u8),
}

/// What one frame of a gesture over a number field asks of the model.
///
/// Taken out of the row's drawing and applied after it ([`lane_list`]) —
/// the row reads the model while it is being drawn, and a writer needs it
/// to itself.
struct ValueGesture {
    /// The lane whose field it was.
    id: u32,
    /// The setting the field writes and the value it now reads, when this
    /// frame moved it. `None` for a frame that only ends a gesture: a drag
    /// released without having moved any further, or an edit left by Escape.
    value: Option<LaneValue>,
    /// Whether the gesture is over, and so whether this is the frame the
    /// undo step closes on: a drag released, a typed value taken or left, or
    /// a notch of the wheel, which is a whole gesture in itself.
    ended: bool,
}

/// What one frame of gestures over the list asks of the model.
///
/// Four because a frame can carry more than one: a note field clicked while
/// a rename box is open ends the rename and starts the note's own edit in
/// the same frame, and each has to reach the writer API.
#[derive(Default)]
struct Gestures {
    /// A number field dragged, scrolled or typed into — one a frame
    /// ([`collect_value`]).
    value: Option<ValueGesture>,

    /// A rename box taken: the lane, and the name typed into it. A name of
    /// nothing at all never gets this far ([`name_field`]).
    rename: Option<(u32, String)>,

    /// The one-shot/gate toggle clicked: the lane, and the mode the click
    /// asks for.
    mode: Option<(u32, LaneMode)>,

    /// A row carried up or down the list by its grip.
    reorder: Option<ReorderGesture>,

    /// The lane whose audition button was clicked — the one gesture here
    /// that is not a write ([`apply`] never sees it): it leaves the list
    /// through [`LaneList::audition`] for the window to put on the engine's
    /// ring (see the module header).
    audition: Option<u32>,

    /// The lane whose sample cell was clicked, or whose menu asked to
    /// browse: not a write either, it leaves through [`LaneList::browse`]
    /// for the window to open the browser on.
    browse: Option<u32>,
    /// Whether that came from the sample cell rather than the menu
    /// ([`LaneList::sample_click`]).
    sample_click: bool,

    /// What a lane's right-click menu was asked for, if one was open and
    /// asked for anything (`editor/menu.rs`): the lane, and the action.
    ///
    /// The two of the six actions that are not writes never reach here —
    /// "rename" opens the row's box as the frame is drawn
    /// ([`lane_row`]) and "reload from file" leaves through
    /// [`LaneList::reload`] because the loader is the window's, not the
    /// list's.
    menu: Option<(u32, Action)>,

    /// The lane whose menu is open this frame, so that a delete waiting
    /// for its second click is dropped when its menu goes
    /// ([`menu::Confirm::follow`]).
    menu_open: Option<u32>,

    /// A click on a row's name or empty background: the lane, and the
    /// modifiers held, which say whether it selects, toggles or ranges. Not a
    /// write: it lands on the window's [`Selection`] ([`apply_select`]).
    select: Option<(u32, Modifiers)>,

    /// A lane whose sample the menu asked to be read from its file again:
    /// the lane, and the path it was loaded from.
    ///
    /// Handed out of the list through [`LaneList::reload`] for the reason
    /// [`audition`](Self::audition) is: the job goes to the background
    /// loader, which belongs to the instance and not to this module.
    reload: Option<(u32, PathBuf)>,

    /// Where the lane whose menu asked to "open source in editor" was cut
    /// from: handed out through [`LaneList::open_source`] for the window to
    /// open with the loader, as [`reload`](Self::reload) is.
    open_source: Option<ChopOrigin>,
}

/// What one frame of a reorder drag asks of the model.
struct ReorderGesture {
    /// Where the row being dragged sits in display order as this frame finds
    /// it, which is not where the drag began: the list is reordered under
    /// the pointer as it goes.
    from: usize,

    /// Where the pointer has carried the row to.
    to: usize,

    /// Whether the grip has been let go, and so whether this is the frame
    /// the undo step closes on.
    ended: bool,
}

/// A lane's name being typed (a double click on the name renames it).
///
/// The one thing in the lane list that is state rather than a read of the
/// model: half a name is nobody's truth, so it cannot live on a lane until
/// the box is taken. Owned by [`crate::editor::ChopdropEditor`] and built
/// afresh for each opened window, like everything else belonging to a window
/// rather than to the instance (nice-plug-egui 0.6 never calls
/// `editor_closed`).
#[derive(Default)]
pub struct Rename {
    /// The lane whose box is open, and `None` when none is. One at a time,
    /// because one box has the keyboard focus.
    lane: Option<u32>,

    /// What has been typed, starting as the lane's own name.
    buffer: String,

    /// Whether the box still has to ask for the keyboard focus — set when it
    /// opens and spent on the first frame it is drawn. A box that asked
    /// every frame could never be left: it would take the focus back in the
    /// same frame Escape or a click elsewhere gave it up.
    focus: bool,
}

impl Rename {
    /// Opens the box on `lane` with `name` in it, selected, so that what is
    /// typed next replaces the name rather than joining it.
    ///
    /// Public because the double click below is not the only way in: the
    /// right-click menu's "rename" opens the same box.
    pub fn open(&mut self, lane: u32, name: &str) {
        self.lane = Some(lane);
        self.buffer.clear();
        self.buffer.push_str(name);
        self.focus = true;
    }

    /// Which lane's box is open, if any.
    pub fn editing(&self) -> Option<u32> {
        self.lane
    }

    /// Closes the box, keeping nothing of what was in it.
    pub fn close(&mut self) {
        self.lane = None;
        self.buffer.clear();
        self.focus = false;
    }
}

/// Where one row's fields landed this frame, for the window around the list
/// and for the tests.
///
/// Handed back for the reason [`crate::editor::top_bar::TopBar`] is: what
/// the list drew can be asked where it went, which is how a gesture is
/// tested over a real layout and how a drop finds the row under the pointer
/// ([`LaneRow::lane_rect`]). One field per gesture, so this grows as the
/// gestures do.
pub struct LaneRow {
    /// The lane the row is of.
    pub id: u32,

    /// The whole row, as the list laid it out: which row a pointer is over,
    /// for the reorder drag here and for a drop's landing target
    /// (`editor/drops.rs`).
    pub rect: Rect,

    /// The drag grip the row is reordered by.
    pub grip: Response,

    /// The note field.
    pub note: Response,

    /// The name cell, or the box it becomes while the lane is being renamed.
    pub name: Response,

    /// The sample cell: a click on it opens the browser on this lane.
    pub sample: Response,

    /// The volume field, in whole decibels.
    pub volume: Response,

    /// The pan field, as a side and a percentage.
    pub pan: Response,

    /// The choke-group field, `0` for a lane in no group.
    pub choke: Response,

    /// The one-shot/gate toggle.
    pub mode: Response,

    /// The audition button.
    pub audition: Response,

    /// Where the row's right-click menu drew its items, while it is open
    /// (`editor/menu.rs`); `None` for every row whose menu is closed, which
    /// is all of them but at most one.
    pub menu: Option<menu::Items>,
}

impl LaneRow {
    /// The row's lane and its geometry, which is all a drop needs of it
    /// ([`drops::route`]): the responses a row also carries are this
    /// frame's, and where a drop lands is a question about rectangles.
    pub fn lane_rect(&self) -> drops::LaneRect {
        drops::LaneRect { id: self.id, rect: self.rect }
    }
}

/// What one frame of the lane list drew ([`lane_list`]).
///
/// The rows, and the rectangle they were drawn in — which the window needs as
/// much as the rows themselves, because a drop lands either on a row or in
/// the space under the last of them, and only the list knows where that space
/// is.
pub struct LaneList {
    /// Where each row's fields landed, in display order.
    pub rows: Vec<LaneRow>,

    /// The list itself: the rows, the gaps between them and the space below
    /// them, inside the panel's padding. The list and not the window, so a
    /// drag over the top bar is over no row at all.
    pub rect: Rect,

    /// The lane whose audition button was clicked this frame, for the window
    /// to push onto the engine's ring.
    ///
    /// Handed back rather than acted on, because the ring belongs to the
    /// audio thread and this module is the lane list (see the module
    /// header). `None` on all but the one frame a click lands in, and on a
    /// click on a lane with nothing to play.
    pub audition: Option<u32>,

    /// The lane whose menu asked for its sample to be read from its file
    /// again, and the path to read it from (`editor/menu.rs`).
    ///
    /// Handed back rather than acted on for the reason
    /// [`audition`](Self::audition) is: the job goes to the background
    /// loader, which belongs to the instance.
    pub reload: Option<(u32, PathBuf)>,

    /// Where the lane whose menu asked to "open source in editor" was cut
    /// from: the window opens that source through the loader with the cut as
    /// its pending selection.
    pub open_source: Option<ChopOrigin>,

    /// The lane whose sample cell was clicked, or whose menu asked to
    /// browse: the window opens (or retargets) the browser on it.
    pub browse: Option<u32>,
    /// Whether [`browse`](Self::browse) came from a click on the sample cell
    /// rather than from the menu's **browse…**: the window shows the lane's
    /// settings instead when the lane already has a sample, where the menu
    /// always opens the browser.
    pub sample_click: bool,

    /// The lane whose name or row background was clicked this frame, after
    /// the click has been applied to the selection: the window opens the
    /// dock on the lane tab if it is closed.
    pub selected: Option<u32>,

    /// How many lanes "give each its own output" left on their old output
    /// this frame for want of a free one
    /// ([`crate::model::Model::assign_own_outputs`]): the window says so in
    /// the top bar's status slot ([`crate::editor::status::outputs_kept`]).
    /// `0` on every other frame.
    pub outputs_kept: usize,
}

/// A row's answering fields on their way out of the row's own id scope and
/// into the [`LaneRow`] around them ([`lane_row`]).
///
/// A struct rather than a tuple of eight responses, which nothing could read
/// at the call site.
struct Fields {
    grip: Response,
    note: Response,
    name: Response,
    sample: Response,
    volume: Response,
    pan: Response,
    choke: Response,
    mode: Response,
    audition: Response,
}

/// Every field of a row but the sample column, and the gaps between all ten
/// of them.
pub const ROW_FIXED_WIDTH: f32 = theme::HEADER_GRIP_WIDTH
    + theme::HEADER_DOT_WIDTH
    + theme::HEADER_AUDITION_WIDTH
    + theme::HEADER_KEY_WIDTH
    + theme::HEADER_NAME_WIDTH
    + theme::HEADER_VOLUME_WIDTH
    + theme::HEADER_PAN_WIDTH
    + theme::HEADER_CHOKE_WIDTH
    + theme::HEADER_TOGGLE_WIDTH
    + (ROW_FIELDS - 1.0) * theme::HEADER_FIELD_GAP;

/// The narrowest a readable row can be drawn: the fixed fields, plus the
/// least the sample column may be squeezed to.
///
/// What [`crate::editor::MIN_WIDTH`] is made of, with the window's padding
/// and the scroll bar's strip added to it.
pub const ROW_MIN_WIDTH: f32 = ROW_FIXED_WIDTH + theme::HEADER_SAMPLE_MIN_WIDTH;

/// How wide the sample column is drawn in a list `available` points wide:
/// whatever the other nine fields and their gaps leave, and never less than
/// [`theme::HEADER_SAMPLE_MIN_WIDTH`].
///
/// The one elastic field in the row, because a file name is the longest text
/// in the window and the one most worth reading. Pure arithmetic, so the
/// window's minimum size can be built out of it and checked without a
/// display.
///
/// `max` last, so that a width which is not a number — or one egui hands back
/// mid-resize before the window has a size at all — comes out as the minimum
/// rather than as a NaN that would take the whole row's layout with it (the
/// NaN-safe clamp order).
pub fn sample_column_width(available: f32) -> f32 {
    (available - ROW_FIXED_WIDTH).max(theme::HEADER_SAMPLE_MIN_WIDTH)
}

/// A lane volume as its field shows it: whole decibels, signed.
///
/// Whole, because that is what [`theme::HEADER_VOLUME_WIDTH`] is wide enough
/// for ("-60 dB") and what the field's drag steps in ([`whole_db`]); a value
/// with a fraction — from a project saved by some later version, or from
/// host automation — is rounded to the nearest rather than truncated or
/// refused.
pub fn format_db(volume_db: f32) -> String {
    // Through the model's own clamp rather than a second copy of the rule:
    // the field is a window onto a lane, so what it prints for a value that
    // got past a setter — out of a project file, or a NaN, which comparison
    // cannot clamp and which comes back as the lane default — is the volume
    // the lane would actually play at.
    let whole = clamp_volume_db(volume_db).round();
    // `-0.4` rounds to `-0.0`, which would otherwise print as "-0 dB" and
    // read as a cut of nothing at all.
    format!("{} dB", if whole == 0.0 { 0.0 } else { whole })
}

/// A lane volume in the whole decibels its field works in.
///
/// Through the model's own clamp, so that the number the field starts a drag
/// from is one the lane can actually hold — and so that a NaN out of a
/// project file comes out as 0 dB rather than as the `as i32` of a lost
/// number.
fn whole_db(volume_db: f32) -> i32 {
    clamp_volume_db(volume_db).round() as i32
}

/// A decibel value typed into the volume field: a number, with or without
/// the unit the field prints after it ("-12", "-12 dB", "6db").
///
/// A fraction is rounded rather than refused, because the field is whole
/// decibels; a number past either end is clamped by the field's own range
/// rather than refused, because "-80" plainly means as quiet as the lane
/// goes. Only a text with no number in it at all is a parse error, and then
/// [`DragValue`] puts the lane's own volume back.
fn parse_db(text: &str) -> Option<i32> {
    // The unit and nothing else: a trailing letter cannot be part of a
    // decimal number, and taking the letters off is what lets the field be
    // typed at in the spelling it prints.
    let number = text.trim().trim_end_matches(|character: char| character.is_ascii_alphabetic());
    let value: f32 = number.trim().parse().ok()?;
    // Clamped at the boundary, like everything crossing into the model: an
    // infinity would otherwise reach the cast.
    value.is_finite().then(|| value.round() as i32)
}

/// A lane pan as a whole percentage off centre: −100 hard left, 0 centred,
/// 100 hard right — the unit its field drags, scrolls, prints and parses in.
///
/// Percentages rather than the model's −1..=1, because a number with a
/// decimal point in it is harder to read at a glance than a side and a
/// distance, and because a percent is a step small enough to be the wheel's
/// notch.
pub fn pan_percent(pan: f32) -> i32 {
    // The model's own clamp, for the reason [`whole_db`] gives — and a value
    // that is not a number comes back from it as the centre, which is the
    // pan a lane is created with.
    (clamp_pan(pan) * 100.0).round() as i32
}

/// The pan a percentage means, as the model keeps it.
fn pan_from_percent(percent: i32) -> f32 {
    percent as f32 / 100.0
}

/// A pan percentage as its field shows it: `C` in the middle, `L100` hard
/// left, `R50` half way right. The field holds a percentage already (see
/// [`pan_percent`]), so the window never formats a `-1.0..=1.0` pan. The
/// letter is what makes the field legible at [`theme::HEADER_PAN_WIDTH`] without a
/// label beside it.
pub fn format_pan_percent(percent: i32) -> String {
    match percent {
        0 => "C".to_string(),
        // `unsigned_abs` and not a negation, which would overflow on the one
        // percentage that has no positive twin: this is public, and the
        // field's range is not what bounds a caller.
        left if left < 0 => format!("L{}", left.unsigned_abs()),
        right => format!("R{right}"),
    }
}

/// A pan typed into the field: `C` for the centre, `L50` or `R100` for a
/// side and a distance, or a bare signed percentage (negative is left).
///
/// Case-insensitive and sloppy about the per-cent sign, the way the note
/// field accepts flats it never prints; past the ends it is clamped rather
/// than refused, for the reason [`parse_db`] gives.
fn parse_pan(text: &str) -> Option<i32> {
    let text = text.trim();
    // The side letters are one byte each, so the slice is always on a
    // character boundary; anything else is read as a bare percentage.
    let (side, number) = match text.chars().next()?.to_ascii_lowercase() {
        'c' if text.len() == 1 => return Some(0),
        'l' => (-1, &text[1..]),
        'r' => (1, &text[1..]),
        _ => (1, text),
    };

    let percent: f32 = number.trim().trim_end_matches('%').trim().parse().ok()?;
    percent.is_finite().then(|| side * percent.round() as i32)
}

/// The dim marker after a row's sample name: the note length a synced lane
/// plays for ("1 bar", "1/8t"), the output a lane off the main pair renders
/// into ("out 5", 1-based), both separated by a space, or "" for a lane with
/// neither. Plain words: egui's bundled fonts have no arrow, and "→ 5" drew
/// as a blank box (2026-11-16).
pub fn row_marker(lane: &Lane) -> String {
    let mut marker = String::new();
    if let Length::Beats(note) = lane.length
        && lane.time_mode != TimeMode::Off
    {
        marker.push_str(note.label());
    }
    if lane.output != OUTPUT_MAIN {
        if !marker.is_empty() {
            marker.push(' ');
        }
        marker.push_str(&format!("out {}", u32::from(lane.output) + 1));
    }
    marker
}

/// A lane's choke group as its field shows it: the group's own number, and
/// `0` for a lane in no group at all.
///
/// The number and not a word, because it is the same field being dragged and
/// being read, and a control whose resting face and whose dragging face
/// disagreed would read as two different settings. What the number means is
/// in the field's hover text ([`CHOKE_HOVER`]).
pub fn format_choke(choke: u8) -> String {
    choke.to_string()
}

/// The face the one-shot/gate toggle wears in each mode.
pub fn mode_label(mode: LaneMode) -> &'static str {
    match mode {
        LaneMode::OneShot => ONE_SHOT_LABEL,
        LaneMode::Gate => GATE_LABEL,
    }
}

/// What the toggle says about the mode it is wearing.
pub fn mode_hover(mode: LaneMode) -> &'static str {
    match mode {
        LaneMode::OneShot => ONE_SHOT_HOVER,
        LaneMode::Gate => GATE_HOVER,
    }
}

/// The mode a click on the toggle asks for: the other one of the two.
pub fn other_mode(mode: LaneMode) -> LaneMode {
    match mode {
        LaneMode::OneShot => LaneMode::Gate,
        LaneMode::Gate => LaneMode::OneShot,
    }
}

/// The lane list, as the editor's frame callback draws it: the rows, or the
/// big hint when there are no rows to draw, and then whatever gesture the
/// frame found over them.
///
/// Takes the model and not just its lanes, because a field that answers
/// writes through it — but the model behind the editor's lock and nothing of
/// nice-plug, so the whole list is still drawn into a bare [`egui::Context`]
/// in a test with no display and no plugin host. `rename` is the window's
/// open rename box, if any ([`Rename`]), and `lights` the activity this frame
/// has already read off the published snapshot ([`Lights::observe`]) — both
/// of them the window's own state rather than the model's. `selection` is the
/// window's selected lanes: pruned here every frame to the lanes that still
/// exist, painted under its rows, and changed by a click on a row's name or
/// background.
pub fn lane_list(
    ui: &mut Ui,
    model: &mut Model,
    rename: &mut Rename,
    confirm: &mut Confirm,
    selection: &mut Selection,
    lights: &Lights,
    target: Option<u32>,
) -> LaneList {
    // The lanes as the rows are about to show them: what the selection is
    // pruned to, and the display order a Shift+click ranges over.
    let order: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    selection.prune(&order);

    let frame = egui::Frame::new()
        .fill(theme::PANEL)
        // egui's own central panel would use a margin of its own (8), and
        // the window's default height is a sum over this one
        // (`crate::editor::DEFAULT_HEIGHT`).
        .inner_margin(egui::Margin::same(theme::PANEL_PADDING as i8));

    let mut gestures = Gestures::default();
    let mut drawn = egui::CentralPanel::default()
        .frame(frame)
        .show(ui, |ui| {
            // Taken before anything is drawn in it, and from the room the
            // panel has rather than from what ends up in it: this is the
            // space a drop is routed against, and it is the same whether
            // the list holds forty rows or none.
            let rect = ui.max_rect();
            let rows = if model.lanes().is_empty() {
                empty_hint(ui);
                Vec::new()
            } else {
                let marks = ListMarks { lights, target, selection };
                rows(ui, model.lanes(), rename, confirm, &mut gestures, marks)
            };
            LaneList {
                rows,
                rect,
                audition: None,
                reload: None,
                open_source: None,
                browse: None,
                sample_click: false,
                selected: None,
                outputs_kept: 0,
            }
        })
        .inner;

    // The two gestures that go nowhere near the model, taken out before the
    // writers run and carried out of the list instead (see the module
    // header).
    drawn.audition = gestures.audition;
    drawn.reload = gestures.reload.take();
    drawn.open_source = gestures.open_source.take();
    drawn.browse = gestures.browse;
    drawn.sample_click = gestures.browse.is_some() && gestures.sample_click;

    // A row clicked by its name or background: the window's own state, so
    // it lands on the selection here and never reaches the writer API.
    if let Some((id, modifiers)) = gestures.select.take() {
        apply_select(selection, id, modifiers, &order);
        drawn.selected = Some(id);
    }

    // And the half-made gesture the window keeps between frames: a delete
    // waiting for its second click lives exactly as long as the menu it was
    // clicked in (`editor/menu.rs`).
    confirm.follow(gestures.menu_open);

    // After the draw and not inside it: the rows are a read of the lanes and
    // the writer needs them to itself. Still before the frame ends, though,
    // and not kept for the next one, so a drag moves the note it is over and
    // a row follows the pointer that is carrying it.
    drawn.outputs_kept = apply(model, gestures, selection);

    drawn
}

/// Puts a click on lane `id`'s name or row background onto the selection:
/// Shift ranges from the anchor over the display `order`, Ctrl toggles, and a
/// plain click selects just that lane. Shift wins when both are held.
fn apply_select(selection: &mut Selection, id: u32, modifiers: Modifiers, order: &[u32]) {
    if modifiers.shift {
        selection.shift_click(id, order);
    } else if modifiers.ctrl || modifiers.command {
        selection.ctrl_click(id);
    } else {
        selection.click(id);
    }
}

/// Hands this frame's gestures to the writer API, roughly in the order a row
/// reads left to right — which is as far as the order goes, since one of the
/// four stands for four of a row's columns ([`LaneValue`]) and no two of
/// them write the same setting.
///
/// Four of the five: [`Gestures::audition`] changes nothing a lane holds and
/// has left through [`LaneList`] before this runs.
///
/// Answers how many lanes a menu's "give each its own output" could not
/// place ([`apply_menu`]), `0` for every other frame.
fn apply(model: &mut Model, gestures: Gestures, selection: &Selection) -> usize {
    if let Some(value) = gestures.value {
        apply_value(model, value);
    }
    if let Some((id, name)) = gestures.rename {
        apply_rename(model, id, &name);
    }
    if let Some((id, mode)) = gestures.mode {
        apply_mode(model, id, mode);
    }
    if let Some(reorder) = gestures.reorder {
        apply_reorder(model, reorder);
    }
    match gestures.menu {
        Some((id, action)) => apply_menu(model, id, action, selection),
        None => 0,
    }
}

/// Hands a lane's menu action to the writer API, as the one undo step a
/// menu click is.
///
/// Five of the eight arrive here. The other three are not writes: "rename"
/// opens the row's own box as the row is drawn ([`lane_row`]), "browse…"
/// leaves through [`LaneList::browse`] and "reload from file" is the
/// loader's, handed out through [`LaneList::reload`].
///
/// Every one of these is an already-tested writer call and this adds no
/// policy of its own — a duplicate past [`crate::model::MAX_LANES`] (whose
/// item is drawn disabled there anyway, [`menu::duplicate_enabled`]) and a
/// clear on a lane with no sample both simply do nothing, and the commit
/// after them takes no step when nothing changed ([`Model::commit`]).
///
/// "give each its own output" acts on the whole `selection` when lane `id`
/// is in it, and on lane `id` alone when it is not;
/// it is the one action that answers something — how many lanes found no
/// free output ([`Model::assign_own_outputs`]) — and every other answers
/// `0`.
fn apply_menu(model: &mut Model, id: u32, action: Action, selection: &Selection) -> usize {
    let mut unplaced = 0;
    match action {
        Action::Colour(colour) => {
            model.set_colour(id, colour);
        }
        Action::Duplicate => {
            model.duplicate_lane(id);
        }
        Action::Delete => {
            model.delete_lane(id);
        }
        Action::ClearSample => {
            model.clear_sample(id);
        }
        Action::OwnOutputs => {
            unplaced = if selection.contains(id) {
                model.assign_own_outputs(selection.ids())
            } else {
                model.assign_own_outputs(&[id])
            };
        }
        // Render state is outside the history: a fresh render is asked for
        // and nothing is committed (the render worker picks it up).
        Action::ReRender => {
            model.request_render(id);
            return 0;
        }
        // None of these is a write — they are answered where the row is
        // drawn, and this match is total so that a new action cannot be
        // added without a decision being made about it here.
        Action::Rename | Action::Reload | Action::Browse | Action::OpenSource => return 0,
    }
    model.commit();
    unplaced
}

/// Hands one frame of a number-field gesture to the writer API, through
/// whichever setter the field stands for.
///
/// The commit is the end of the gesture and not the end of the frame, which
/// is what makes a drag through an octave one undo step rather than thirty (a
/// drag is one entry, committed at release). A gesture that ended having
/// changed nothing — Escape out of the text box, a release that moved no
/// further — commits nothing either: [`Model::commit`] takes no step when the
/// state is already the one the last step left.
///
/// Every value has already been through the field's own range, and every
/// setter clamps again regardless: the writer API is the boundary, not this
/// window, and the clamps are covered by the model's own tests.
fn apply_value(model: &mut Model, gesture: ValueGesture) {
    let id = gesture.id;
    if let Some(value) = gesture.value {
        // A lane the list drew is a lane the model has, so each of these
        // answers `true`; the answer is dropped because a frame can do
        // nothing about one that did not.
        match value {
            LaneValue::Note(note) => model.set_note(id, note),
            LaneValue::VolumeDb(volume_db) => model.set_volume_db(id, volume_db),
            LaneValue::Pan(pan) => model.set_pan(id, pan),
            LaneValue::Choke(choke) => model.set_choke(id, choke),
        };
    }
    if gesture.ended {
        model.commit();
    }
}

/// Hands a click on the one-shot/gate toggle to the writer API, as the one
/// undo step a click is.
fn apply_mode(model: &mut Model, id: u32, mode: LaneMode) {
    model.set_mode(id, mode);
    model.commit();
}

/// Hands a taken rename box to the writer API, as the one undo step the
/// whole edit is.
///
/// [`Model::rename`] trims the name to [`MAX_NAME_LEN`] on a character
/// boundary, which is the same limit the box itself allows
/// ([`name_field`]), so there is nothing here to clamp: a name a user can
/// type is a name a lane can keep. A rename to the name the lane already had
/// adds no step, because [`Model::commit`] takes none when nothing changed.
fn apply_rename(model: &mut Model, id: u32, name: &str) {
    model.rename(id, name);
    model.commit();
}

/// Hands one frame of a reorder drag to the writer API.
///
/// The move itself happens on every frame the pointer passes another row's
/// middle, so the list the user is looking at is the order they are making;
/// the commit waits for the release, which is what makes a row carried from
/// the top of the list to the bottom one undo step and not seven. Display
/// order only — a lane keeps its note, its colour and its sample wherever it
/// is put.
fn apply_reorder(model: &mut Model, reorder: ReorderGesture) {
    if reorder.from != reorder.to {
        model.reorder(reorder.from, reorder.to);
    }
    if reorder.ended {
        model.commit();
    }
}

/// Draws [`EMPTY_HINT`] centred across the window, a third of the way down.
///
/// A third rather than the middle: the hint is about the space under it,
/// which is where the lanes a drop creates will appear.
fn empty_hint(ui: &mut Ui) {
    ui.vertical_centered(|ui| {
        ui.add_space(ui.available_height() / 3.0);
        ui.label(RichText::new(EMPTY_HINT).size(theme::HINT_SIZE).color(theme::TEXT_DIM));
    });
}

/// The scrolling list itself, one [`lane_row`] per lane in display order.
///
/// The sample column's width is settled once here rather than per row, so
/// that every row's columns line up even as the window is dragged wider:
/// inside the scroll area, [`Ui::available_width`] is already the width
/// without the scroll bar's strip, so the column gives up its six points on
/// the frame a ninth lane makes the list scroll and not before.
fn rows(
    ui: &mut Ui,
    lanes: &[Lane],
    rename: &mut Rename,
    confirm: &mut Confirm,
    gestures: &mut Gestures,
    marks: ListMarks,
) -> Vec<LaneRow> {
    let ListMarks { lights, target, selection } = marks;
    egui::ScrollArea::vertical()
        // The list is as wide and as tall as the window whatever is in it: a
        // list that shrank to its contents would leave the panel's grey
        // showing beside the rows, and the space below the last row is a
        // drop target of its own (`editor/drops.rs`).
        .auto_shrink([false, false])
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing = Vec2::new(theme::HEADER_FIELD_GAP, theme::ROW_GAP);
            let sample_width = sample_column_width(ui.available_width());
            let room = menu::duplicate_enabled(lanes.len());
            let drawn: Vec<LaneRow> = lanes
                .iter()
                .map(|lane| {
                    lane_row(
                        ui,
                        lane,
                        sample_width,
                        rename,
                        confirm,
                        gestures,
                        RowMarks {
                            fade: lights.fade(lane.id),
                            target: target == Some(lane.id),
                            room,
                            selected: selection.contains(lane.id),
                            auditioning: lights.auditioning() == Some(lane.id),
                        },
                    )
                })
                .collect();

            // A reorder is the one gesture that is not a field's own: it is
            // read off the whole list at once, because where a row is going
            // is which *other* row the pointer is over.
            gestures.reorder = reorder_gesture(&drawn, ui.ctx().pointer_interact_pos());

            drawn
        })
        .inner
}

/// What the window marks the list's rows with this frame: the activity
/// lights, the browser's target lane and the selection.
#[derive(Clone, Copy)]
struct ListMarks<'a> {
    lights: &'a Lights,
    target: Option<u32>,
    selection: &'a Selection,
}

/// How a row is marked this frame, besides what its lane holds.
#[derive(Clone, Copy)]
struct RowMarks {
    /// How brightly the lane's activity light is lit ([`Lights::fade`]).
    fade: f32,
    /// Whether the browser loads into this lane.
    target: bool,
    /// Whether the list has room for one more lane, which is whether the
    /// menu's "duplicate" is live ([`menu::duplicate_enabled`]).
    room: bool,
    /// Whether the lane is selected, and so drawn over
    /// [`theme::SELECTED_ROW`].
    selected: bool,
    /// Whether the lane's audition is sounding, and so its play button is
    /// a stop button.
    auditioning: bool,
}

/// One lane, left to right.
///
/// `gestures` collects what the row's fields found, and the first row to
/// find a note-field gesture keeps it: the pointer is over one row and the
/// keyboard focus is in one field, so a frame has at most one to collect
/// anyway.
///
/// The whole row is drawn inside a scope of the lane's own id
/// ([`row_scope`]), which is what names every widget in it after the lane
/// rather than after its place in the list (see the module header).
///
/// `marks` says how brightly the lane's activity light is lit this frame
/// (nought for a lane at rest) and whether the browser loads into it.
fn lane_row(
    ui: &mut Ui,
    lane: &Lane,
    sample_width: f32,
    rename: &mut Rename,
    confirm: &mut Confirm,
    gestures: &mut Gestures,
    marks: RowMarks,
) -> LaneRow {
    let RowMarks { fade, target, room, selected, auditioning } = marks;
    // The selected fill goes *under* the fields, but the row's rect is only
    // known once they are laid out: keep its place in the paint order now
    // and fill it in below.
    let under = ui.painter().add(Shape::Noop);
    // Sensed for a click, so the row's empty background — the gaps between
    // fields, the colour dot — selects the lane. egui senses a container
    // before its children, so every field drawn in it keeps its own clicks.
    let builder = egui::UiBuilder::new().scope_id(row_scope(lane.id)).sense(Sense::click());
    let mut renaming = false;
    let scope = ui.scope_builder(builder, |ui| {
        ui.horizontal(|ui| {
            let grip = grip(ui);
            colour_dot(ui, lane, fade);
            let (audition, asked) = audition_button(ui, lane, auditioning);
            if asked.is_some() {
                gestures.audition = asked;
            }
            let (note, gesture) = note_field(ui, lane);
            collect_value(gestures, gesture);
            renaming = rename.editing() == Some(lane.id);
            let name = name_field(ui, lane, rename, gestures);
            let sample = sample_cell(ui, lane, sample_width, target);
            if sample.clicked() {
                gestures.browse = Some(lane.id);
                gestures.sample_click = true;
            }
            let (volume, gesture) = volume_field(ui, lane);
            collect_value(gestures, gesture);
            let (pan, gesture) = pan_field(ui, lane);
            collect_value(gestures, gesture);
            let (choke, gesture) = choke_field(ui, lane);
            collect_value(gestures, gesture);
            let (mode, clicked) = mode_toggle(ui, lane);
            if clicked.is_some() {
                gestures.mode = clicked;
            }

            Fields { grip, note, name, sample, volume, pan, choke, mode, audition }
        })
        .inner
    });

    let rect = scope.response.rect;
    let Fields { grip, note, name, sample, volume, pan, choke, mode, audition } = scope.inner;

    // A click on the name cell (not into an open rename box) or on the row
    // itself selects; the modifiers held say how. Every other field keeps its
    // click to itself.
    if (!renaming && name.clicked()) || scope.response.clicked() {
        gestures.select = Some((lane.id, ui.input(|input| input.modifiers)));
    }
    if selected {
        ui.painter().set(
            under,
            Shape::rect_filled(rect, theme::CONTROL_CORNER, theme::SELECTED_ROW),
        );
    }

    // The menu hangs off the row's own scope rather than off a field of it,
    // so a secondary click anywhere across the row opens it
    // (`editor/menu.rs`) — and after the fields, because the popup is drawn
    // over the row it belongs to.
    let menu = menu::lane_menu(&scope.response, lane, room, confirm);
    if menu.open {
        gestures.menu_open = Some(lane.id);
    }
    if let Some(action) = menu.action {
        collect_menu(gestures, rename, lane, action);
    }

    // The wash that says which row the hand is holding, over the row's own
    // fields and so painted after them ([`theme::ROW_DRAG`]).
    if grip.dragged() {
        ui.painter().rect_filled(rect, theme::CONTROL_CORNER, theme::ROW_DRAG);
    }

    LaneRow {
        id: lane.id,
        rect,
        grip,
        note,
        name,
        sample,
        volume,
        pan,
        choke,
        mode,
        audition,
        menu: menu.items,
    }
}

/// Puts one menu action where it is answered: the row's rename box, the
/// window's loader, or the writer API after the frame is drawn
/// ([`apply_menu`]).
///
/// The rename is opened here, mid-frame, because the box is the window's own
/// state and not the model's — the same box, with the same behaviour, a
/// double click on the name opens (`editor/menu.rs`). It appears on the next
/// frame, which is the first frame it could have the keyboard focus anyway.
///
/// The reload carries the lane's path out with it: the row is holding the
/// sample that knows it, and looking it up again after the frame would be a
/// second read of a lane that may have been deleted by another gesture in
/// between. A lane with no sample offers no reload at all (the menu's item
/// is disabled), so the `None` here is unreachable from the GUI and simply
/// asks for nothing.
fn collect_menu(gestures: &mut Gestures, rename: &mut Rename, lane: &Lane, action: Action) {
    match action {
        Action::Rename => rename.open(lane.id, &lane.name),
        Action::Browse => {
            gestures.browse = Some(lane.id);
            gestures.sample_click = false;
        }
        Action::Reload => {
            if let Some(sample) = &lane.sample {
                gestures.reload = Some((lane.id, PathBuf::from(&sample.path)));
            }
        }
        Action::OpenSource => {
            gestures.open_source = lane.sample.as_ref().and_then(|sample| sample.origin.clone());
        }
        write => gestures.menu = Some((lane.id, write)),
    }
}

/// Keeps the first number-field gesture a frame finds.
///
/// The first and not the last: the pointer is over one field and the
/// keyboard focus is in one, so a frame has at most one to collect anyway,
/// and a list that took a second would be answering a gesture nobody made.
fn collect_value(gestures: &mut Gestures, found: Option<ValueGesture>) {
    if gestures.value.is_none() {
        gestures.value = found;
    }
}

/// The id every widget in a lane's row is named under.
///
/// [`egui::UiBuilder::scope_id`] and not [`Ui::push_id`], which is the usual
/// way to name a subtree: a scope salted with the parent's id takes the
/// parent's *widget counter* into its own ids too, so the fields of the
/// third row would be named after the row being the third. This is the one
/// egui offers for a subtree that moves — "child widgets can be moved in the
/// ui tree without losing state" — which is a row of this list exactly (see
/// the module header). Globally unique, as that asks: there is one lane list
/// in a window and a lane id is unique within it.
///
/// [`egui::Id::unique`] and not `Id::new`, which 0.37 deprecated in its
/// favour: this is a root id of its own and not a salt mixed into a parent's.
fn row_scope(lane: u32) -> egui::Id {
    egui::Id::unique(("chopdrop_lane_row", lane))
}

/// The reorder drag going on over the list, if there is one: whose grip is
/// being held, and which row the pointer has carried it to.
///
/// egui remembers which widget a drag belongs to, so the row being dragged
/// is simply the one whose grip still answers — and because every widget in
/// a row is named after its lane, that stays the same row as the list is
/// reordered under the pointer (see the module header).
///
/// A pointer egui cannot place leaves the row where it is rather than at the
/// top of the list, which is what an index out of a position that is not a
/// number would come to.
fn reorder_gesture(rows: &[LaneRow], pointer: Option<Pos2>) -> Option<ReorderGesture> {
    let from = rows.iter().position(|row| row.grip.dragged() || row.grip.drag_stopped())?;
    let to = pointer
        .filter(|pointer| pointer.y.is_finite())
        .map_or(from, |pointer| carried_to(rows, pointer.y));

    Some(ReorderGesture { from, to, ended: rows[from].grip.drag_stopped() })
}

/// Which place in the list a pointer at `y` has carried a row to: how many
/// rows it has passed the middle of, held inside the list.
///
/// The middles and not the edges, because the move is made as the drag goes:
/// a row dropped into another row's place takes exactly the place it was
/// dragged past the middle of, and the row it displaced moves into the one
/// the dragged row came from — so the gesture settles rather than flickering
/// between two orders while the pointer sits still.
fn carried_to(rows: &[LaneRow], y: f32) -> usize {
    let passed = rows.iter().filter(|row| row.rect.center().y < y).count();
    passed.min(rows.len().saturating_sub(1))
}

/// One of a row's four number fields: egui's own [`DragValue`] at the row's
/// geometry, reading and writing in the unit the field shows (see the module
/// header).
///
/// The drag and the typing both, which is what the widget is: a drag moves
/// the value, a click turns the field into a text box with the value in it
/// selected, and a text `parse` refuses leaves the lane alone and the field
/// goes back to showing it. The wheel is ours ([`wheel_steps`]) — egui puts
/// no scroll gesture on a drag value, and what the wheel would otherwise
/// reach is the list under the field.
///
/// `value` is what the lane has now and `range` what the field allows, both
/// in the field's own unit; `format` and `parse` are how that unit is
/// spelled. Hands back the field, the value this frame moved it to (`None`
/// for a frame that moved nothing) and whether the gesture ended.
fn value_field(
    ui: &mut Ui,
    width: f32,
    value: i32,
    range: RangeInclusive<i32>,
    format: impl Fn(i32) -> String,
    parse: impl Fn(&str) -> Option<i32>,
) -> (Response, Option<i32>, bool) {
    let mut edited = value;

    let response = ui.add(
        DragValue::new(&mut edited)
            // The field's own range and not the number's behind it, which is
            // what a drag value takes by default (an `i32`'s is no bound at
            // all). It clamps the drag *and* a typed value.
            .range(range.clone())
            .speed(FIELD_DRAG_SPEED)
            // A row is a line of fields and the list is a column of rows: a
            // field that grew to fit what had been typed into it would pull
            // every field right of it out of line with the row above.
            .min_size(Vec2::new(width, theme::ROW_HEIGHT))
            .clip_text(true)
            // The typed value lands when Enter is pressed or the box is
            // left, not as it is typed: half of one is not a value (`C`
            // parses to no note at all, `L` to no pan), and Escape has to be
            // able to abandon an edit the model has not already taken.
            .update_while_editing(false)
            .custom_formatter(|number, _| format(dragged_value(number, &range)))
            .custom_parser(|text| parse(text).map(f64::from)),
    );

    let wheel = wheel_steps(ui, &response);
    let moved = if wheel == 0 {
        edited
    } else {
        // The wheel's own arithmetic: the range bounds what the drag and the
        // typing do, and nothing else.
        (value + wheel).clamp(*range.start(), *range.end())
    };

    // A release or a lost focus that changed nothing still ends a gesture,
    // because that is the frame the undo step closes on.
    let ended = wheel != 0 || response.drag_stopped() || response.lost_focus();

    (response, (moved != value).then_some(moved), ended)
}

/// What one frame of a number field found, as the list carries it to the
/// writer API — and nothing at all for a frame that neither moved the value
/// nor ended a gesture, which is nearly every frame.
fn gesture(id: u32, value: Option<LaneValue>, ended: bool) -> Option<ValueGesture> {
    (value.is_some() || ended).then_some(ValueGesture { id, value, ended })
}

/// The note field: the lane's note under Bitwig's name for it.
///
/// The one of the four whose unit is spelled with letters rather than
/// digits, and so the one whose typed value can be refused outright
/// ([`notes::parse_note_name`]) instead of merely clamped.
fn note_field(ui: &mut Ui, lane: &Lane) -> (Response, Option<ValueGesture>) {
    let (response, moved, ended) = value_field(
        ui,
        theme::HEADER_KEY_WIDTH,
        i32::from(lane.note),
        // MIDI's range, which [`dragged_value`] has already held the
        // formatter's number inside — so the casts below cannot truncate.
        0..=i32::from(notes::HIGHEST_NOTE),
        |note| notes::note_name(note as u8),
        |name| notes::parse_note_name(name).map(i32::from),
    );

    (response, gesture(lane.id, moved.map(|note| LaneValue::Note(note as u8)), ended))
}

/// The volume field: the lane's volume in whole decibels.
///
/// Whole is the unit and not a rounding of the display: a lane whose volume
/// has a fraction in it — out of a project written by some other version —
/// shows the nearest whole decibel and a drag moves on from that, rather
/// than carrying a fraction nothing in this window can see.
fn volume_field(ui: &mut Ui, lane: &Lane) -> (Response, Option<ValueGesture>) {
    let (response, moved, ended) = value_field(
        ui,
        theme::HEADER_VOLUME_WIDTH,
        whole_db(lane.volume_db),
        whole_db(LANE_VOLUME_DB_MIN)..=whole_db(LANE_VOLUME_DB_MAX),
        |volume_db| format_db(volume_db as f32),
        parse_db,
    );

    (response, gesture(lane.id, moved.map(|db| LaneValue::VolumeDb(db as f32)), ended))
}

/// The pan field: where the lane sits across the stereo field, as a side and
/// a percentage.
fn pan_field(ui: &mut Ui, lane: &Lane) -> (Response, Option<ValueGesture>) {
    let (response, moved, ended) = value_field(
        ui,
        theme::HEADER_PAN_WIDTH,
        pan_percent(lane.pan),
        pan_percent(LANE_PAN_MIN)..=pan_percent(LANE_PAN_MAX),
        format_pan_percent,
        parse_pan,
    );

    let moved = moved.map(|percent| LaneValue::Pan(pan_from_percent(percent)));
    (response, gesture(lane.id, moved, ended))
}

/// The choke-group field: which group of lanes this one cuts short and is
/// cut short by, `0` for none.
///
/// The one number field with a hover text, because it is the one whose
/// digits cannot say what they are ([`CHOKE_HOVER`]).
fn choke_field(ui: &mut Ui, lane: &Lane) -> (Response, Option<ValueGesture>) {
    let (response, moved, ended) = value_field(
        ui,
        theme::HEADER_CHOKE_WIDTH,
        i32::from(lane.choke),
        i32::from(CHOKE_NONE)..=i32::from(CHOKE_GROUPS),
        |choke| format_choke(choke as u8),
        // A plain group number, which is all the field ever prints; a
        // fraction or a word is refused and the field goes back to the
        // lane's own group.
        |text| text.trim().parse().ok(),
    );

    let response = response.on_hover_text(CHOKE_HOVER);
    (response, gesture(lane.id, moved.map(|choke| LaneValue::Choke(choke as u8)), ended))
}

/// The one-shot/gate toggle: the mode's letter in a square, clicked to swap.
///
/// Lit in the theme's selection colour for gate and plain for one-shot,
/// which is the other half of telling the two apart down a column: eight
/// default lanes are eight unlit squares, and the lane that ends with its
/// note is the one that stands out of them. A click is the whole gesture, so
/// there is nothing here to end — [`apply_mode`] commits it at once.
fn mode_toggle(ui: &mut Ui, lane: &Lane) -> (Response, Option<(u32, LaneMode)>) {
    let response = ui
        .add(
            egui::Button::new(text(mode_label(lane.mode)))
                .selected(lane.mode == LaneMode::Gate)
                // One square ([`theme::HEADER_TOGGLE_WIDTH`]).
                .min_size(Vec2::new(theme::HEADER_TOGGLE_WIDTH, theme::ROW_HEIGHT)),
        )
        .on_hover_text(mode_hover(lane.mode));

    let clicked = response.clicked().then(|| (lane.id, other_mode(lane.mode)));
    (response, clicked)
}

/// How many of its own units the wheel has turned a number field this frame,
/// and nought when the pointer is somewhere else.
///
/// The scroll is taken out of egui's input as well as read, which is the
/// point of doing this by hand: the fields sit in a scroll area, and a wheel
/// turned over one of them has to move the value rather than the list under
/// it. Taken every frame the pointer is over the field, because egui hands a
/// notch over in instalments ([`ScrollResidual`]) and the list would
/// otherwise take the rest of it.
pub(crate) fn wheel_steps(ui: &Ui, response: &Response) -> i32 {
    if !response.contains_pointer() {
        return 0;
    }

    // Both axes, although only the vertical one is a step: a sideways wheel
    // over a lane's volume is not a gesture this window has, and leaving it
    // in the input would scroll the list.
    let turned = ui.input_mut(|input| std::mem::take(&mut input.smooth_scroll_delta).y);

    ui.data_mut(|data| {
        let residual = data.get_temp::<ScrollResidual>(response.id).unwrap_or_default();
        let points = residual.0 + turned;
        // Towards zero, so a notch up and a notch down are the same distance
        // and what is left over never carries a step of its own.
        let steps = (points / FIELD_SCROLL_POINTS).trunc();
        data.insert_temp(response.id, ScrollResidual(points - steps * FIELD_SCROLL_POINTS));
        steps as i32
    })
}

/// The number a field is showing, out of the `f64` a [`DragValue`] works in.
///
/// Rounded and held in `range` rather than cast straight across: the widget
/// drags in fractions of a unit before it rounds, and `as i32` of a number
/// outside the range saturates. A value that is not a number comes out as
/// nought — the lowest note, unity for a volume, the centre of a pan, no
/// choke group — which is what the cast makes of a NaN and a value every
/// field's range holds, rather than as a panic in a host's window.
fn dragged_value(value: f64, range: &RangeInclusive<i32>) -> i32 {
    value.round().clamp(f64::from(*range.start()), f64::from(*range.end())) as i32
}

/// The sample column: what [`status::sample_status`] has this lane say about
/// its sample — the file's name, the hint, "loading…", or the message of
/// whatever went wrong last.
///
/// The one field of a row whose text, colour and hover all come from the same
/// read, which is what keeps the four states from drifting apart: this draws
/// what it is handed and decides nothing. Two of them have more to say than
/// the column is wide, and both say it on the hover — the whole of a message,
/// or the note a [`status::NOTE_MARK`] stands for.
///
/// A click on it opens the browser on this lane, so it lights under the
/// pointer like any other control, and the lane the browser is loading into
/// (`target`) wears the drop outline.
fn sample_cell(ui: &mut Ui, lane: &Lane, width: f32, target: bool) -> Response {
    let says = status::sample_status(lane);
    let (rect, response) = control_box(ui, width, Sense::click(), true);
    let inner = rect.shrink2(theme::CONTROL_PADDING);
    if target {
        ui.painter().rect_stroke(
            rect,
            theme::CONTROL_CORNER,
            theme::drop_target_stroke(),
            StrokeKind::Inside,
        );
    }

    // The mark's room comes off the right end before the text is laid out, so
    // a long file name is truncated short of the mark rather than drawn
    // under it ([`theme::SAMPLE_MARK_WIDTH`]).
    let split = inner.max.x - theme::SAMPLE_MARK_WIDTH;
    let mut text_rect = if says.marked() { inner.with_max_x(split) } else { inner };

    // The row marker (`row_marker`) sits at the right end of the name's room
    // in `TEXT_DIM`, and comes off it the same way: a long name truncates
    // short of it. It never takes more than half the cell.
    let marker = row_marker(lane);
    if !marker.is_empty() {
        let galley = ui.painter().layout_no_wrap(
            marker.clone(),
            egui::TextStyle::Body.resolve(ui.style()),
            theme::TEXT_DIM,
        );
        let width = (galley.size().x + theme::CONTROL_PADDING.x).min(text_rect.width() * 0.5);
        let marker_rect = text_rect.with_min_x(text_rect.max.x - width);
        text_rect = text_rect.with_max_x(marker_rect.min.x);
        ui.place(
            marker_rect,
            Label::new(RichText::new(marker).color(theme::TEXT_DIM))
                .truncate()
                .halign(Align::Max)
                .selectable(false),
        );
    }

    ui.place(
        text_rect,
        Label::new(RichText::new(says.text()).color(says.colour()))
            .truncate()
            .halign(Align::Min)
            .selectable(false),
    );

    if says.marked() {
        ui.place(
            inner.with_min_x(split),
            Label::new(RichText::new(status::NOTE_MARK).color(theme::WARN))
                .halign(Align::Max)
                .selectable(false),
        );
    }

    response.on_hover_text(sample_hover(lane))
}

/// What the pointer resting on a lane's sample cell says: where a chop lane's
/// sample was cut from ([`crate::model::SampleRef::origin_text`]) while it is
/// the sample shown, the status's own hover, and the browse hint last.
pub fn sample_hover(lane: &Lane) -> String {
    let says = status::sample_status(lane);
    let origin = match says {
        status::SampleStatus::Loaded { .. } => {
            lane.sample.as_ref().and_then(|sample| sample.origin_text())
        }
        _ => None,
    };
    let mut lines: Vec<String> = Vec::new();
    lines.extend(origin);
    lines.extend(says.hover().map(str::to_owned));
    if matches!(says, status::SampleStatus::RenderFailed(_)) {
        lines.push(status::RENDER_FAILED_HOVER.to_owned());
    }
    lines.push(BROWSE_HOVER.to_owned());
    lines.join("\n")
}

/// The lane's name: a cell that becomes a text box on a double click.
///
/// Two faces at one geometry — the name painted in a control's box, and, for
/// the one lane whose box is open, egui's own text box at the same width
/// with the name in it selected. The box is taken by Enter or by losing the
/// focus to whatever the user did next, and abandoned by Escape, which is
/// the note field's bargain too and egui's own convention for a field.
fn name_field(ui: &mut Ui, lane: &Lane, rename: &mut Rename, gestures: &mut Gestures) -> Response {
    if rename.editing() != Some(lane.id) {
        let name = text(&lane.name);
        let response = control_cell(ui, theme::HEADER_NAME_WIDTH, Align::Min, name, Sense::click());
        if response.double_clicked() {
            rename.open(lane.id, &lane.name);
        }
        return response;
    }

    let response = ui.add(
        TextEdit::singleline(&mut rename.buffer)
            // The row's one geometry, as the note field keeps it: a box that
            // sized itself to the name in it would push every field right of
            // it out of its column.
            .min_size(Vec2::new(theme::HEADER_NAME_WIDTH, theme::ROW_HEIGHT))
            .desired_width(theme::HEADER_NAME_WIDTH)
            .margin(theme::CONTROL_PADDING)
            .clip_text(true)
            // What a lane would keep of a longer name anyway
            // ([`apply_rename`]), said here so that the box cannot show a
            // name the lane will not have.
            .char_limit(MAX_NAME_LEN),
    );

    if std::mem::take(&mut rename.focus) {
        response.request_focus();
        select_all(ui, &response, &rename.buffer);
    }

    if response.lost_focus() {
        // Escape leaves a text box in egui by clearing the focus, so it
        // arrives as the same frame a taken box does and is told apart by
        // the key itself — egui's own `DragValue` reads it the same way.
        let escaped = ui.input(|input| input.key_pressed(egui::Key::Escape));
        let typed = std::mem::take(&mut rename.buffer);
        // A name of nothing at all is refused rather than written: the name
        // is a lane's one handle in this window, and a row with nothing in
        // that column could not be found to be renamed back.
        if !escaped && !typed.trim().is_empty() {
            gestures.rename = Some((lane.id, typed));
        }
        rename.close();
    }

    response
}

/// Selects everything in a text box, so that the first thing typed into it
/// replaces what was there.
///
/// Stored into egui's memory under the box's own id before the box has the
/// focus, which is how [`DragValue`] does it for the note field: the widget
/// reads its cursor out of that memory on the frame it gains the focus, and
/// a range already waiting there is the range it starts with.
fn select_all(ui: &Ui, response: &Response, text: &str) {
    let mut state = TextEdit::load_state(ui.ctx(), response.id).unwrap_or_default();
    state.cursor.set_char_range(Some(egui::text::CCursorRange::select_all_str(text)));
    state.store(ui.ctx(), response.id);
}

/// A row's drag grip: two columns of three dots, bitstep's own, which the
/// row is picked up by and carried to another place in the list.
///
/// The one field of a row sensed for a *drag* rather than a click, and the
/// reason the pointer says so on its way past: a column of dots that did
/// nothing when pulled would be a column of dots.
fn grip(ui: &mut Ui) -> Response {
    let (rect, response) = field(ui, theme::HEADER_GRIP_WIDTH, Sense::drag());

    // Three rows of dots over the middle half of the row's height, so the
    // grip reads as a texture to grab rather than as part of the fields
    // beside it.
    let spacing = Vec2::new(theme::HEADER_GRIP_WIDTH / 3.0, theme::ROW_HEIGHT / 6.0);
    let centre = rect.center();
    for row in -1..=1 {
        for column in [-0.5, 0.5] {
            let dot = centre + Vec2::new(column * spacing.x, row as f32 * spacing.y);
            ui.painter().circle_filled(dot, theme::GRIP_DOT_RADIUS, theme::GRIP);
        }
    }

    if response.dragged() {
        // The pointer is no longer over the grip it is holding — it is over
        // the row the drag is aiming at — so this is set on the context
        // rather than hung on the hover.
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
    }

    response.on_hover_cursor(egui::CursorIcon::Grab)
}

/// The lane's colour dot, which doubles as its activity light.
///
/// `fade` is the light: the dot is the lane's own colour at nought and that
/// colour pushed towards white at one ([`theme::activity_dot`]), which is
/// how a hit reads as the same amount of "now" here as a playhead does in
/// bitstep. The curve between the two, and the clock it is measured on, are
/// [`crate::editor::activity`]'s; this draws whatever number it is handed.
fn colour_dot(ui: &mut Ui, lane: &Lane, fade: f32) -> Response {
    let (rect, response) = field(ui, theme::HEADER_DOT_WIDTH, Sense::hover());
    let colour = theme::activity_dot(theme::rgb(lane.colour), fade);
    ui.painter().circle_filled(rect.center(), theme::LANE_DOT_RADIUS, colour);
    response
}

/// The audition button: a control's box with a play triangle in it, and the
/// lane it asks to hear when it is clicked.
///
/// The triangle is drawn and not typed, for the reason
/// [`theme::PLAY_GLYPH`] gives, and it is dimmed on a lane with no sample —
/// the one state in which the button can do nothing, and the only hint of
/// that a square this size has room for beyond its hover text
/// ([`AUDITION_EMPTY_HOVER`]).
///
/// A click on an empty lane asks for nothing at all rather than asking for
/// silence: the engine would find no published lane for it and start no
/// voice, and the ring it would have taken a place in is small and
/// best-effort — a click that cannot sound should not be what drops one that
/// can.
///
/// While the lane's audition sounds (`auditioning`) the triangle is a
/// square — a stop button — and the click the window gets back stops it
/// rather than starting it again (`editor/mod.rs`). The square is a
/// polygon too, so a row draws one polygon whichever it shows.
fn audition_button(ui: &mut Ui, lane: &Lane, auditioning: bool) -> (Response, Option<u32>) {
    let loaded = lane.sample.is_some();
    let (rect, response) = control_box(ui, theme::HEADER_AUDITION_WIDTH, Sense::click(), true);
    let hover = match (loaded, auditioning) {
        (false, _) => AUDITION_EMPTY_HOVER,
        (true, false) => AUDITION_HOVER,
        (true, true) => AUDITION_STOP_HOVER,
    };
    let response = response.on_hover_text(hover);

    let centre = rect.center();
    let half = theme::PLAY_GLYPH / 2.0;
    let points = if auditioning && loaded {
        // A square as tall as the triangle, a little narrower than it is
        // tall so it sits in the same space.
        let side = half.y * 0.85;
        vec![
            centre + Vec2::new(-side, -side),
            centre + Vec2::new(side, -side),
            centre + Vec2::new(side, side),
            centre + Vec2::new(-side, side),
        ]
    } else {
        vec![
            centre + Vec2::new(-half.x, -half.y),
            centre + Vec2::new(-half.x, half.y),
            centre + Vec2::new(half.x, 0.0),
        ]
    };
    ui.painter().add(Shape::convex_polygon(
        points,
        if loaded { theme::TEXT } else { theme::TEXT_DIM },
        egui::Stroke::NONE,
    ));

    let asked = (loaded && response.clicked()).then_some(lane.id);
    (response, asked)
}

/// One of a row's text fields: a control's box, with `label` placed inside
/// it and truncated to fit.
///
/// Truncated and never wrapped: a row is one [`theme::ROW_HEIGHT`] tall
/// whatever is in it, and a long file name that pushed its row taller would
/// break the list's pitch (and with it the window's default height).
///
/// `sense` is the cell's own, not the label's: a painted field asks for
/// [`Sense::hover`] and one with a gesture asks for that (see the module
/// header).
fn control_cell(ui: &mut Ui, width: f32, align: Align, label: RichText, sense: Sense) -> Response {
    let (rect, response) = control_box(ui, width, sense, false);
    ui.place(
        rect.shrink2(theme::CONTROL_PADDING),
        Label::new(label).truncate().halign(align).selectable(false),
    );
    response
}

/// The box every field of a row sits in: the fill, corner and hairline
/// egui's own widgets take from [`theme::visuals`], painted by hand because
/// in this pass there is no widget to take them (see the module header).
///
/// `reactive` asks for the visuals the *response* calls for instead of the
/// flat `inactive` ones, so a field that answers the pointer lightens under
/// it and darkens under a press exactly as the stock button beside it does.
/// Only the audition button asks ([`audition_button`]): it is the one field
/// here that acts on a single click, and a button that does not move under
/// the pointer reads as a dead one.
///
/// Off for everything else, including the fields that *are* sensed. The name
/// cell senses clicks to catch a double one and the number fields sense
/// drags, but they are painted cells whose grey is the row's own — `interact`
/// would light them on mere hover and make a row ripple as the pointer
/// crossed it. A sense is not by itself a reason to look like a button, so
/// this is a caller's decision and not read off `sense`.
fn control_box(ui: &mut Ui, width: f32, sense: Sense, reactive: bool) -> (Rect, Response) {
    let (rect, response) = field(ui, width, sense);
    let visuals = if reactive {
        *ui.style().interact(&response)
    } else {
        ui.style().visuals.widgets.inactive
    };
    ui.painter().rect(
        rect,
        theme::CONTROL_CORNER,
        visuals.bg_fill,
        visuals.bg_stroke,
        StrokeKind::Inside,
    );
    (rect, response)
}

/// One field's worth of a row: `width` points of it, the full row height,
/// and no paint of its own.
///
/// Every field of a row is allocated through here, which is what keeps the
/// row exactly [`theme::ROW_HEIGHT`] tall and its columns at the widths
/// [`ROW_FIXED_WIDTH`] adds up — and what gives every field of it an id of
/// the lane's own, since the row is drawn inside a scope of the lane id (see
/// the module header).
///
/// `sense` is the gesture the field answers: [`Sense::hover`] for a painted
/// one, and the field's own gesture for one that has a gesture.
fn field(ui: &mut Ui, width: f32, sense: Sense) -> (Rect, Response) {
    ui.allocate_exact_size(Vec2::new(width, theme::ROW_HEIGHT), sense)
}

/// A field's text in the editor's primary colour.
///
/// Spelled out on every field rather than left to the style, because egui
/// paints a *non-interactive* widget's text in [`theme::TEXT_DIM`] — the
/// dimmer of the theme's two tones, which is right for a label beside a
/// control and wrong for the value inside one.
fn text(value: impl Into<String>) -> RichText {
    RichText::new(value).color(theme::TEXT)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use egui::{Pos2, Rect, pos2, vec2};

    use super::*;
    use crate::model::{
        DEFAULT_LANE_COUNT, LANE_PAN_MAX, LANE_PAN_MIN, LANE_VOLUME_DB_MAX, LANE_VOLUME_DB_MIN,
        Model, SampleRef,
    };
    use crate::sample::{DecodeError, MAX_SAMPLE_BYTES, SampleData};

    /// Runs one egui pass over `add_contents` on a themed context and hands
    /// back every text egui painted: the string, the colour it was painted
    /// in, and how many vertices its glyphs came to.
    ///
    /// The colour is the laid-out text's own where it has one and the
    /// shape's fallback where it has not, which is the order egui itself
    /// resolves them in: a `RichText::color` lands in the layout job's
    /// section, and the fallback is only what the style would have used.
    ///
    /// Display-free — egui lays out and tessellates without a window, which
    /// is what makes the editor's drawing testable at all without a display.
    fn painted_text(
        mut add_contents: impl FnMut(&mut Ui),
    ) -> Vec<(String, egui::Color32, usize)> {
        let ctx = egui::Context::default();
        theme::apply(&ctx);

        // Twice: egui sizes a centred label from the previous pass's
        // measurements, and a scroll area needs a pass to learn how tall its
        // contents are, so the first pass lays the window out and the second
        // is the one worth looking at.
        ctx.run_ui(egui::RawInput::default(), |ui| add_contents(ui))
            .drop_without_applying_deltas();
        let output = ctx.run_ui(egui::RawInput::default(), |ui| add_contents(ui));

        output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some((
                    text.galley.text().to_string(),
                    text
                        .galley
                        .job
                        .sections
                        .first()
                        .map(|section| section.format.color)
                        .filter(|colour| *colour != egui::Color32::PLACEHOLDER)
                        .unwrap_or(text.fallback_color),
                    text.galley.num_vertices,
                )),
                _ => None,
            })
            .collect()
    }

    /// The texts of one pass, in the order they were painted.
    fn painted_strings(add_contents: impl FnMut(&mut Ui)) -> Vec<String> {
        painted_text(add_contents).into_iter().map(|(text, _, _)| text).collect()
    }

    /// The lanes of a fresh instance, with a sample in the first of them —
    /// a model rather than a hand-built `Vec`, because a lane's sample can
    /// only be made with the collector handle a model owns.
    fn model_with_a_loaded_first_lane() -> Model {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 16].into() };
        let path = Path::new("/home/dev/kit/BD 606.wav");
        let sample = SampleRef::new(model.handle(), path, vec![0], data);
        assert!(model.set_sample(id, sample), "the first lane takes a sample");
        // And the commit the loader makes when a file the user chose lands:
        // without it the sample is an uncommitted edit, and the next
        // gesture's undo would take it off the lane along with its own
        // change.
        model.commit();
        model
    }

    /// How tall the harness's window is: every lane a new instance has, at
    /// the row pitch, inside the list's padding — so a new instance's list
    /// does not scroll and no scroll bar takes its strip out of the rows. A
    /// test that wants a list taller than the window adds lanes past it.
    const LIST_HEIGHT: f32 =
        crate::model::DEFAULT_LANE_COUNT as f32 * theme::ROW_PITCH + 2.0 * theme::PANEL_PADDING;

    /// The list under test, with gestures played over it: a themed context
    /// as wide as a new instance's window, an instance's lanes, and where
    /// each row's note field landed in the last frame.
    ///
    /// The same shape as the top bar's own harness and display-free for the
    /// same reason — egui lays out, interacts and tessellates without a
    /// window, so a drag or a notch of the wheel can be played over the real
    /// layout without a display.
    struct List {
        ctx: egui::Context,
        model: Model,
        /// The window's open rename box, as the editor holds one per opened
        /// window ([`crate::editor::ChopdropEditor`]).
        rename: Rename,
        /// The window's delete confirm, as the editor holds one per opened
        /// window ([`crate::editor::ChopdropEditor`]).
        confirm: Confirm,
        /// Where each row's note field landed in the last frame, in display
        /// order.
        notes: Vec<Rect>,
        /// Where each row's grip landed in the last frame, in display order.
        grips: Vec<Rect>,
        /// Where each row's name field landed in the last frame, in display
        /// order.
        names: Vec<Rect>,
        /// Where each row's volume field landed in the last frame, in
        /// display order.
        volumes: Vec<Rect>,
        /// Where each row's pan field landed in the last frame, in display
        /// order.
        pans: Vec<Rect>,
        /// Where each row's choke field landed in the last frame, in display
        /// order.
        chokes: Vec<Rect>,
        /// Where each row's one-shot/gate toggle landed in the last frame,
        /// in display order.
        modes: Vec<Rect>,
        /// The window's drag state, as the editor holds one per opened
        /// window ([`crate::editor::ChopdropEditor`]).
        drops: drops::DropIntake,
        /// Each row's lane and rectangle in the last frame, as a drop is
        /// routed against them ([`drops::route`]).
        rows: Vec<drops::LaneRect>,
        /// The list's own rectangle in the last frame.
        list: Rect,
        /// Every rectangle outlined in the drop accent in the last frame —
        /// the landing-target highlight, and nothing else in the window
        /// paints in it.
        outlined: Vec<Rect>,
        /// Every text painted in the last frame, in the order it was
        /// painted.
        painted: Vec<String>,
        /// Where each row's audition button landed in the last frame, in
        /// display order.
        auditions: Vec<Rect>,
        /// The window's activity lights, as the editor holds a set per
        /// opened window ([`crate::editor::ChopdropEditor`]): read off the
        /// published snapshot at the top of every frame, as the editor does
        /// it.
        lights: Lights,
        /// Every lane dot painted in the last frame, in display order: the
        /// colour it was painted in, which is the lane's own brightened by
        /// however much of its light is still lit
        /// ([`theme::activity_dot`]).
        dots: Vec<egui::Color32>,
        /// The lane the last frame asked to hear, if any
        /// ([`LaneList::audition`]).
        asked: Option<u32>,
        /// The lane and path the last frame asked the loader to read again,
        /// if any ([`LaneList::reload`]).
        reload: Option<(u32, PathBuf)>,
        /// The origin the last frame asked to open in the chop editor, if
        /// any ([`LaneList::open_source`]).
        opened: Option<ChopOrigin>,
        /// Where the one open right-click menu drew its items in the last
        /// frame, if a menu was open ([`menu::Items`]).
        menu: Option<menu::Items>,
        /// Every rectangle stroked at the colour swatch's ring width in the
        /// last frame (`editor/menu.rs`'s palette), in paint order — the
        /// ring that says which colour a lane is wearing. Nothing else in a
        /// window with no drag over it is stroked that wide.
        rings: Vec<Rect>,
        /// Every box painted in the last frame as `(rect, fill)`: what
        /// [`control_box`] left behind, so a test can read the grey under
        /// one named field ([`List::fill_at`]).
        fills: Vec<(Rect, egui::Color32)>,
        /// Every play triangle painted in the last frame, in display order:
        /// the colour it was painted in, which is the one thing a row says
        /// about whether its audition button can do anything
        /// ([`audition_button`]).
        triangles: Vec<egui::Color32>,
        /// Where each row's sample cell landed in the last frame, in display
        /// order.
        samples: Vec<Rect>,
        /// The lane the last frame asked to browse for, if any
        /// ([`LaneList::browse`]).
        browse: Option<u32>,
        /// The lane the browser loads into, as the window passes it in:
        /// `None` unless a test sets it.
        target: Option<u32>,
        /// The window's selected lanes, as the editor holds them per window.
        selection: Selection,
        /// The lane the last frame's row click selected, if any
        /// ([`LaneList::selected`]).
        selected: Option<u32>,
        /// How many lanes the last frame's "give each its own output" left
        /// where they were ([`LaneList::outputs_kept`]).
        kept: usize,
    }

    impl List {
        fn new() -> List {
            List::over(Model::new())
        }

        /// The same harness over a model built by hand — a lane with a
        /// sample in it, say, which is the only kind of lane that is
        /// published and so the only kind that can be struck
        /// ([`List::strike`]).
        fn over(model: Model) -> List {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            List {
                ctx,
                model,
                rename: Rename::default(),
                confirm: Confirm::default(),
                notes: Vec::new(),
                grips: Vec::new(),
                names: Vec::new(),
                volumes: Vec::new(),
                pans: Vec::new(),
                chokes: Vec::new(),
                modes: Vec::new(),
                drops: drops::DropIntake::default(),
                rows: Vec::new(),
                list: Rect::ZERO,
                outlined: Vec::new(),
                painted: Vec::new(),
                auditions: Vec::new(),
                lights: Lights::default(),
                dots: Vec::new(),
                asked: None,
                reload: None,
                opened: None,
                menu: None,
                rings: Vec::new(),
                triangles: Vec::new(),
                fills: Vec::new(),
                samples: Vec::new(),
                browse: None,
                target: None,
                selection: Selection::new(),
                selected: None,
                kept: 0,
            }
        }

        /// One frame of the window, with `events` as its input.
        fn frame(&mut self, events: Vec<egui::Event>) {
            self.frame_of(egui::RawInput { events, ..self.window() });
        }

        /// The empty input of a window this size, for a frame that is about
        /// something other than events ([`List::hover`]).
        fn window(&self) -> egui::RawInput {
            egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    Pos2::ZERO,
                    vec2(crate::editor::DEFAULT_WIDTH, LIST_HEIGHT),
                )),
                ..Default::default()
            }
        }

        /// One frame of the window over `input`, drawn the way the editor
        /// draws one: the drag state read before anything else, the lights
        /// read off the published snapshot, the list, and then the
        /// landing-target outline over it
        /// (`crate::editor::ChopdropEditor::ui`).
        fn frame_of(&mut self, input: egui::RawInput) {
            let model = &mut self.model;
            let rename = &mut self.rename;
            let confirm = &mut self.confirm;
            let selection = &mut self.selection;
            let intake = &mut self.drops;
            let lights = &mut self.lights;
            let target = self.target;
            let mut drawn =
                LaneList {
                    rows: Vec::new(),
                    rect: Rect::ZERO,
                    audition: None,
                    reload: None,
                    open_source: None,
                    browse: None,
                    sample_click: false,
                    selected: None,
                    outputs_kept: 0,
                };
            let mut rects = Vec::new();
            let output = self.ctx.run_ui(input, |ui| {
                intake.read_frame(ui.ctx());
                let now = ui.input(|input| input.time);
                let published = model.published().get();
                lights.observe(
                    published.lanes().iter().map(|lane| (lane.id, lane.activity())),
                    now,
                );
                drop(published);
                drawn = lane_list(ui, model, rename, confirm, selection, lights, target);
                rects = drawn.rows.iter().map(LaneRow::lane_rect).collect();
                intake.outline(ui, &rects, drawn.rect, Rect::NOTHING);
                // As the editor does with it (`crate::editor::ChopdropEditor`).
                if let Some(text) = crate::editor::status::outputs_kept(drawn.outputs_kept) {
                    let hover = crate::editor::status::OUTPUTS_FULL_HOVER;
                    intake.show_message(ui.ctx(), text, hover);
                }
            });

            let rows = &drawn.rows;
            self.notes = rows.iter().map(|row| row.note.rect).collect();
            self.grips = rows.iter().map(|row| row.grip.rect).collect();
            self.names = rows.iter().map(|row| row.name.rect).collect();
            self.volumes = rows.iter().map(|row| row.volume.rect).collect();
            self.pans = rows.iter().map(|row| row.pan.rect).collect();
            self.chokes = rows.iter().map(|row| row.choke.rect).collect();
            self.modes = rows.iter().map(|row| row.mode.rect).collect();
            self.auditions = rows.iter().map(|row| row.audition.rect).collect();
            self.samples = rows.iter().map(|row| row.sample.rect).collect();
            self.browse = drawn.browse;
            self.selected = drawn.selected;
            self.kept = drawn.outputs_kept;
            self.rows = rects;
            self.list = drawn.rect;
            self.asked = drawn.audition;
            self.reload = drawn.reload.take();
            self.opened = drawn.open_source.take();
            // At most one menu is open, so at most one row carries items.
            self.menu = drawn.rows.iter_mut().find_map(|row| row.menu.take());
            // The lane dots, by the radius nothing else in a row is drawn
            // at ([`theme::LANE_DOT_RADIUS`] against the grip's own): one
            // per row, in the order the rows were drawn.
            self.dots = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Circle(dot) if dot.radius == theme::LANE_DOT_RADIUS => {
                        Some(dot.fill)
                    }
                    _ => None,
                })
                .collect();
            // The play triangles: the only polygons a row draws
            // ([`Shape::convex_polygon`] — a field's box is a `Rect` and
            // the grip's dots are `Circle`s).
            self.triangles = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Path(path) => Some(path.fill),
                    _ => None,
                })
                .collect();
            self.fills = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Rect(box_) => Some((box_.rect, box_.fill)),
                    _ => None,
                })
                .collect();
            self.painted = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            self.rings = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Rect(rect) if rect.stroke.width == theme::SWATCH_RING_WIDTH => {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .collect();
            self.outlined = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Rect(rect) if rect.stroke.color == theme::ACCENT => {
                        Some(rect.rect)
                    }
                    _ => None,
                })
                .collect();
            // `TexturesDelta` panics when dropped with the first frame's
            // font upload still in it.
            output.drop_without_applying_deltas();
        }

        /// A frame with no input, as a window that is merely open draws.
        fn idle(&mut self) {
            self.frame(Vec::new());
        }

        /// A frame with the pointer resting at `at` and nothing else
        /// happening — what a field's hover visuals answer.
        fn point_at(&mut self, at: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(at)]);
        }

        /// The grey painted under the field that filled `rect` this frame
        /// ([`control_box`]).
        fn fill_at(&self, rect: Rect) -> egui::Color32 {
            let found: Vec<egui::Color32> =
                self.fills.iter().filter(|(at, _)| *at == rect).map(|(_, fill)| *fill).collect();
            assert_eq!(found.len(), 1, "one box at {rect:?}, found {}", found.len());
            found[0]
        }

        /// A frame with `paths` dragged over the window at `at`, as the
        /// patched stack reports a drag going on: the files hovering, and
        /// the pointer position the drag event carried
        /// (`vendor/UPSTREAMING.md` §Patch 2).
        fn hover(&mut self, at: Pos2, paths: &[&str]) {
            self.frame_of(egui::RawInput {
                events: vec![egui::Event::PointerMoved(at)],
                hovered_files: paths
                    .iter()
                    .map(|path| egui::HoveredFile {
                        path: Some(std::path::PathBuf::from(path)),
                        mime: String::new(),
                    })
                    .collect(),
                ..self.window()
            });
        }

        /// A press and a release over `at`, one frame each, as a window
        /// receives a click.
        fn click(&mut self, at: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(at), pointer(at, true)]);
            self.frame(vec![pointer(at, false)]);
        }

        /// A click over `at` with `modifiers` held, announced and let go as
        /// egui-baseview does it: `ModifiersChanged` events, which are what
        /// egui reads `InputState::modifiers` from.
        fn click_with(&mut self, at: Pos2, modifiers: egui::Modifiers) {
            let held = egui::Event::ModifiersChanged(modifiers);
            self.frame(vec![held, egui::Event::PointerMoved(at), pointer(at, true)]);
            self.frame(vec![pointer(at, false)]);
            self.frame(vec![egui::Event::ModifiersChanged(egui::Modifiers::NONE)]);
        }

        /// Two clicks over `at` in four frames — a sixteenth of a second,
        /// well inside egui's `max_double_click_delay`.
        fn double_click(&mut self, at: Pos2) {
            self.click(at);
            self.click(at);
        }

        /// A secondary click over `at`, and then a frame of nothing: the
        /// right-click that opens a row's menu, and the frame the menu is
        /// laid out in.
        ///
        /// The extra frame is egui's own doing rather than this harness
        /// being careful: a popup's first frame is a *sizing* pass, so the
        /// items are not where they will be until the frame after it
        /// ([`egui::Popup::show`]).
        fn right_click(&mut self, at: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(at), secondary(at, true)]);
            self.frame(vec![secondary(at, false)]);
            // egui fades a popup in over `Style::animation_time`, and a
            // half-faded menu is painted in half-transparent colours — so
            // the frames here are what make the ring on a swatch the
            // theme's own accent rather than a fraction of it. Ten at
            // egui's own sixty a second, which is twice the fade.
            for _ in 0..10 {
                self.idle();
            }
        }

        /// Escape, and the frame after it: the menu is still drawn in the
        /// frame the key arrives in — that is the frame it reads the key
        /// in — so whether it closed is a question about the next one.
        fn escape(&mut self) {
            self.frame(vec![key(egui::Key::Escape, true)]);
            self.idle();
        }

        /// The open menu's items, as the last frame laid them out.
        fn menu(&self) -> &menu::Items {
            self.menu.as_ref().expect("no menu is open")
        }

        /// A click on one of an open menu's items, and the frame after it.
        ///
        /// The extra frame is what makes the menu's own state readable: the
        /// frame a click lands in is a frame the menu was still open for
        /// (it drew the item that was clicked), so whether the click closed
        /// it is a question about the frame after.
        fn click_menu(&mut self, at: Pos2) {
            self.click(at);
            self.idle();
        }

        /// Whether a menu is open at all.
        fn menu_open(&self) -> bool {
            self.menu.is_some()
        }

        /// What the open menu covers: its items and the margin round them,
        /// which is what a click has to miss to be a click outside it.
        fn menu_rect(&self) -> Rect {
            let items = self.menu();
            let mut rect = items.rename;
            for item in [items.duplicate, items.delete, items.reload, items.clear] {
                rect = rect.union(item);
            }
            for swatch in &items.swatches {
                rect = rect.union(*swatch);
            }
            rect
        }

        /// A press at `from`, the pointer carried to `to` in two steps, and
        /// a release there.
        ///
        /// Two steps because egui decides between a click and a drag by how
        /// far the pointer has gone: the first one spends the few points it
        /// takes to settle that, and the second is the drag proper.
        fn drag(&mut self, from: Pos2, to: Pos2) {
            let halfway = from + (to - from) / 2.0;
            self.frame(vec![egui::Event::PointerMoved(from), pointer(from, true)]);
            self.frame(vec![egui::Event::PointerMoved(halfway)]);
            self.frame(vec![egui::Event::PointerMoved(to)]);
            self.frame(vec![pointer(to, false)]);
        }

        /// `lines` notches of the wheel over `at`, and the frames egui
        /// spreads them over.
        ///
        /// A notch is not a frame: egui smooths a wheel event into its input
        /// a few points at a time, so the loop runs until the input says the
        /// scroll is over and the field has had all forty points of each
        /// notch ([`FIELD_SCROLL_POINTS`]).
        fn scroll(&mut self, at: Pos2, lines: f32) {
            self.frame(vec![egui::Event::PointerMoved(at), wheel(lines)]);
            for _ in 0..200 {
                if !self.ctx.input(|input| input.is_scrolling()) {
                    return;
                }
                self.idle();
            }
            panic!("the wheel is still being smoothed two hundred frames on");
        }

        /// The note of the lane `row` shows, as the model has it.
        fn note(&self, row: usize) -> u8 {
            self.model.lanes()[row].note
        }

        /// The name of the lane `row` shows, as the model has it.
        fn name(&self, row: usize) -> &str {
            &self.model.lanes()[row].name
        }

        /// The volume of the lane `row` shows, in whole decibels.
        fn volume(&self, row: usize) -> i32 {
            whole_db(self.model.lanes()[row].volume_db)
        }

        /// The pan of the lane `row` shows, as a percentage off centre.
        fn pan(&self, row: usize) -> i32 {
            pan_percent(self.model.lanes()[row].pan)
        }

        /// The choke group of the lane `row` shows.
        fn choke(&self, row: usize) -> u8 {
            self.model.lanes()[row].choke
        }

        /// The mode of the lane `row` shows.
        fn mode(&self, row: usize) -> LaneMode {
            self.model.lanes()[row].mode
        }

        /// The engine starting a voice in the lane `row` shows: one count
        /// into the published snapshot, which is all the audio thread
        /// leaves behind ([`crate::model::PublishedLane::mark_activity`],
        /// the one call `Engine::start_lane` makes).
        ///
        /// Only a lane with a sample is published, so this panics on an
        /// empty one — a lane the engine could not have played.
        fn strike(&self, row: usize) {
            let id = self.model.lanes()[row].id;
            self.model
                .published()
                .get()
                .lane(id)
                .expect("only a published lane can be struck")
                .mark_activity();
        }

        /// The lanes in display order, by id — what a reorder moves and a
        /// rename leaves alone.
        fn order(&self) -> Vec<u32> {
            self.model.lanes().iter().map(|lane| lane.id).collect()
        }
    }

    fn pointer(at: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn secondary(at: Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Secondary,
            pressed,
            modifiers: egui::Modifiers::default(),
        }
    }

    fn key(key: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// `lines` notches of a mouse wheel, as egui-baseview hands X11's own on
    /// (`vendor/egui-baseview/src/window.rs`): lines and not points, which
    /// is what makes one of them [`FIELD_SCROLL_POINTS`] wide.
    fn wheel(lines: f32) -> egui::Event {
        egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, lines),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// A model a user has deleted every lane of — the one state of the
    /// window that draws [`EMPTY_HINT`], and only reachable through the
    /// writer API since a new instance comes with eight.
    fn model_with_no_lanes() -> Model {
        let mut model = Model::new();
        for id in model.lanes().iter().map(|lane| lane.id).collect::<Vec<u32>>() {
            assert!(model.delete_lane(id), "a lane the model just listed");
        }
        assert!(model.lanes().is_empty());
        model
    }

    /// A fresh instance's window: eight rows, each with its note name, its
    /// name and the hint for a lane with no sample in it.
    #[test]
    fn every_default_lane_draws_its_note_its_name_and_the_drop_hint() {
        let mut model = Model::new();
        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        for lane in model.lanes() {
            let note = notes::note_name(lane.note);
            let drawn = painted
                .iter()
                .find(|(text, _, _)| *text == note)
                .unwrap_or_else(|| panic!("lane {} drew no note name; got {painted:?}", lane.id));
            assert_eq!(drawn.1, theme::TEXT, "a note name is primary text");
            assert!(drawn.2 > 0, "the note name laid out to no glyphs");

            assert!(
                painted.iter().any(|(text, _, _)| *text == lane.name),
                "lane {} drew no name",
                lane.id
            );
        }

        let hints = painted.iter().filter(|(text, _, _)| text == status::DROP_HINT).count();
        assert_eq!(hints, DEFAULT_LANE_COUNT as usize, "one hint per empty lane");
        let hint = painted.iter().find(|(text, _, _)| text == status::DROP_HINT).expect("a hint");
        assert_eq!(hint.1, theme::TEXT_DIM, "the hint is an invitation, not a value");

        assert!(
            !painted.iter().any(|(text, _, _)| text == EMPTY_HINT),
            "the big hint is only for a list with no lanes at all"
        );
    }

    /// A loaded lane names its sample by its file name, in primary text, and
    /// drops the hint — never its whole path, which would not fit.
    #[test]
    fn a_loaded_lane_shows_its_file_name_and_not_its_path() {
        let mut model = model_with_a_loaded_first_lane();
        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        let drawn = painted
            .iter()
            .find(|(text, _, _)| text == "BD 606.wav")
            .unwrap_or_else(|| panic!("the file name was not drawn; got {painted:?}"));
        assert_eq!(drawn.1, theme::TEXT, "a loaded sample is a value, not a hint");

        assert!(
            !painted.iter().any(|(text, _, _)| text.contains('/')),
            "a path reached the row; got {painted:?}"
        );
        let hints = painted.iter().filter(|(text, _, _)| text == status::DROP_HINT).count();
        assert_eq!(hints, DEFAULT_LANE_COUNT as usize - 1, "the loaded lane needs no hint");
    }

    /// A lane waiting for the background loader says so where its name
    /// would go, and says nothing else there.
    #[test]
    fn a_lane_waiting_for_the_loader_says_so_instead_of_naming_a_file() {
        let mut model = model_with_a_loaded_first_lane();
        let id = model.lanes()[0].id;
        assert!(model.set_loading(id, true), "as the loader marks a queued job");

        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        let drawn = painted
            .iter()
            .find(|(text, _, _)| text == status::LOADING)
            .unwrap_or_else(|| panic!("no lane said it was loading; got {painted:?}"));
        assert_eq!(drawn.1, theme::TEXT_DIM, "a state in hand, not a value and not a failure");
        assert!(drawn.2 > 0, "the text laid out to no glyphs");
        assert!(
            !painted.iter().any(|(text, _, _)| text == "BD 606.wav"),
            "the sample it is replacing was drawn too; got {painted:?}"
        );
    }

    /// A load that failed draws its message in the theme's warning colour,
    /// in place of the file name.
    #[test]
    fn a_failed_load_draws_its_message_in_the_warning_colour() {
        let mut model = model_with_a_loaded_first_lane();
        let id = model.lanes()[0].id;
        let message = DecodeError::BitwigClip.message();
        assert!(model.set_error(id, &message), "as the loader records a rejection");

        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        let drawn = painted
            .iter()
            .find(|(text, _, _)| **text == *message)
            .unwrap_or_else(|| panic!("the message was not drawn; got {painted:?}"));
        assert_eq!(drawn.1, theme::WARN, "a message is the one text in a row that warns");
        assert!(drawn.2 > 0, "the message laid out to no glyphs");
        assert!(
            !painted.iter().any(|(text, _, _)| text == "BD 606.wav"),
            "the message is drawn in place of the name, not beside it; got {painted:?}"
        );
    }

    /// A sample the project is too small to carry keeps its name and is
    /// marked beside it: the lane plays, so this is a note and not a failure.
    #[test]
    fn a_sample_the_project_cannot_carry_is_marked_beside_its_name() {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 16].into() };
        let path = Path::new("/home/dev/kit/long take.wav");
        // One byte over the cap, zeroed and never read past its length: what
        // decides this is how big the file is.
        let bytes = vec![0; MAX_SAMPLE_BYTES + 1];
        assert!(model.set_sample(id, SampleRef::new(model.handle(), path, bytes, data)));

        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        let name = painted
            .iter()
            .find(|(text, _, _)| text == "long take.wav")
            .unwrap_or_else(|| panic!("the file name was not drawn; got {painted:?}"));
        assert_eq!(name.1, theme::TEXT, "the lane is working — the name is still a value");

        let mark = painted
            .iter()
            .find(|(text, _, _)| text == status::NOTE_MARK)
            .unwrap_or_else(|| panic!("the note was not marked; got {painted:?}"));
        assert_eq!(mark.1, theme::WARN);
        assert!(mark.2 > 0, "the mark laid out to no glyphs");
    }

    /// A row draws its fields left to right in their fixed order — the note
    /// before the name, the sample in the middle, the toggle last of the
    /// texts. The grip, the dot and the audition triangle paint no text and
    /// so are not in this list.
    #[test]
    fn a_rows_fields_are_drawn_in_the_specs_order() {
        let mut model = Model::new();
        for id in model.lanes().iter().skip(1).map(|lane| lane.id).collect::<Vec<u32>>() {
            assert!(model.delete_lane(id), "leaving one lane to look at");
        }
        assert!(model.set_volume_db(0, -6.0) && model.set_pan(0, -0.5));
        assert!(model.set_choke(0, 3) && model.set_mode(0, LaneMode::Gate));

        let painted = painted_strings(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        assert_eq!(painted, ["C1", "Lane 1", status::DROP_HINT, "-6 dB", "L50", "3", GATE_LABEL]);
    }

    /// With no lanes at all the window says the one thing it can, in the
    /// theme's secondary text and at the hint's own size — and says nothing
    /// else: no empty row, no stray leading.
    ///
    /// The vertex count is the point of the last assertion: egui bundles no
    /// font unless an integration asks for it and egui-baseview deliberately
    /// does not, so chopdrop enables egui's `default_fonts` itself
    /// (`chopdrop/Cargo.toml`). Without that feature this text lays out to
    /// nothing and the editor is a blank grey window — which is the one way
    /// the look can break that no colour assertion would notice.
    #[test]
    fn an_empty_list_draws_the_big_hint_and_nothing_else() {
        let mut model = model_with_no_lanes();
        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        let texts: Vec<&str> = painted.iter().map(|(text, _, _)| text.as_str()).collect();
        assert_eq!(texts, [EMPTY_HINT]);
        assert_eq!(painted[0].1, theme::TEXT_DIM, "the hint is secondary text");
        let glyphs = painted[0].2;
        assert!(glyphs > 0, "the hint laid out to no glyphs — is egui's `default_fonts` on?");
    }

    /// A list far taller than its window draws every row it is given and
    /// scrolls, rather than squeezing the rows or growing the window: the
    /// rows below the fold are laid out all the same, which is what makes
    /// this checkable without a display.
    #[test]
    fn a_long_list_keeps_every_row() {
        let mut model = Model::new();
        while model.lanes().len() < 40 {
            assert!(model.add_lane().is_some(), "a fortieth lane is well under `MAX_LANES`");
        }

        let painted = painted_strings(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        for lane in model.lanes() {
            assert!(painted.contains(&lane.name), "lane {} was not drawn", lane.id);
        }
    }

    /// Every row has a note field, each one its own column wide and its row
    /// tall, showing the lane's note under Bitwig's name for it.
    ///
    /// The width is the point: the field is one of egui's widgets and egui
    /// sizes a widget from what is in it, so a field that took its own size
    /// would put the eight rows' columns at eight different offsets. The
    /// shortest name ("C1") and the longest ("C#-2") have to come out at the
    /// one width.
    #[test]
    fn every_rows_note_field_is_one_column_wide_whatever_it_says() {
        let mut list = List::new();
        assert!(list.model.set_note(list.model.lanes()[0].id, 1), "C#-2, the longest name");
        list.idle();

        assert_eq!(list.notes.len(), DEFAULT_LANE_COUNT as usize, "one field per lane");
        for (row, field) in list.notes.iter().enumerate() {
            assert_eq!(field.width(), theme::HEADER_KEY_WIDTH, "row {row} is out of its column");
            assert_eq!(field.height(), theme::ROW_HEIGHT, "row {row} is not a row tall");
        }
        let left = list.notes[0].left();
        assert!(list.notes.iter().all(|field| field.left() == left), "the column is not straight");

        assert!(list.painted.contains(&"C#-2".to_string()), "got {:?}", list.painted);
        let second = "C#1".to_string();
        assert!(list.painted.contains(&second), "the second lane; got {:?}", list.painted);
    }

    /// A drag over the field moves the note — right and up for higher, left
    /// and down for lower — and the whole drag is one undo step, closed when
    /// the button comes up and not once a frame.
    #[test]
    fn dragging_a_note_field_moves_the_note_and_is_one_undo_step() {
        let mut list = List::new();
        list.idle();
        let first = list.notes[0].center();
        assert_eq!(list.note(0), 36, "C1, the first default lane");
        assert!(!list.model.can_undo(), "nothing has been done yet");

        // By hand rather than through `drag`, because what the undo stack
        // holds *during* the drag is half of what is being tested.
        list.frame(vec![egui::Event::PointerMoved(first), pointer(first, true)]);
        list.frame(vec![egui::Event::PointerMoved(first + vec2(20.0, 0.0))]);
        list.frame(vec![egui::Event::PointerMoved(first + vec2(40.0, 0.0))]);

        let during = list.note(0);
        assert!(during > 36, "a drag to the right did not raise the note: {during}");
        assert!(during <= 36 + 10, "40 points at a quarter of a note each is ten: {during}");
        assert_eq!(list.model.undo_depth(), 0, "the step closes at the release, not before");
        assert!(list.model.can_undo(), "but the drag has changed something");

        list.frame(vec![pointer(first + vec2(40.0, 0.0), false)]);
        assert_eq!(list.note(0), during, "the release moved the note further");
        assert_eq!(list.model.undo_depth(), 1, "one drag, one step");

        // And the other way: down lowers it, as right raised it.
        list.drag(first, first + vec2(0.0, 20.0));
        assert!(list.note(0) < during, "a drag downwards did not lower the note");
        assert_eq!(list.model.undo_depth(), 2);

        assert!(list.model.undo() && list.model.undo(), "both drags are there to undo");
        assert_eq!(list.note(0), 36, "undone twice, the lane is back on C1");
    }

    /// A drag far past either end of MIDI's range stops at the end rather
    /// than wrapping round or saturating the `u8` behind the field.
    #[test]
    fn a_drag_stops_at_the_ends_of_the_midi_range() {
        let mut list = List::new();
        list.idle();
        let first = list.notes[0].center();

        list.drag(first, first + vec2(2_000.0, 0.0));
        assert_eq!(list.note(0), notes::HIGHEST_NOTE, "G8 is as high as a lane goes");

        list.drag(first, first - vec2(4_000.0, 0.0));
        assert_eq!(list.note(0), 0, "C-2 is as low as a lane goes");
    }

    /// A notch of the wheel over the field is a semitone, and the list it
    /// sits in does not scroll under it — the field takes the scroll out of
    /// egui's input, which is what makes the two gestures tell apart at all.
    #[test]
    fn a_notch_of_the_wheel_is_a_semitone_and_leaves_the_list_where_it_was() {
        let mut list = List::new();
        // Enough lanes that the list is taller than the window and so has
        // somewhere to scroll to.
        while list.model.lanes().len() < 40 {
            assert!(list.model.add_lane().is_some(), "well under `MAX_LANES`");
        }
        list.idle();

        let first = list.notes[0].center();
        let top = list.notes[0].top();

        list.scroll(first, 1.0);
        assert_eq!(list.note(0), 37, "a notch up is one semitone up");
        assert_eq!(list.notes[0].top(), top, "the list scrolled under the pointer as well");
        assert_eq!(list.model.undo_depth(), 1, "a notch is a gesture of its own");

        list.scroll(first, -3.0);
        assert_eq!(list.note(0), 34, "three notches down are three semitones down");
        assert_eq!(list.notes[0].top(), top, "the list scrolled under the pointer as well");

        // Half a notch is no semitone at all, and the leftovers are kept:
        // the second half of it steps the note.
        let note = list.note(0);
        list.scroll(first, 0.5);
        assert_eq!(list.note(0), note, "half a notch moved the note");
        list.scroll(first, 0.5);
        assert_eq!(list.note(0), note + 1, "the two halves did not add up to a notch");

        // And the wheel anywhere else in the row is the list's own, which is
        // what the field is taking the scroll away from.
        let elsewhere = pos2(list.notes[0].right() + 150.0, first.y);
        list.scroll(elsewhere, -2.0);
        assert_eq!(list.note(0), note + 1, "the wheel changed a note it was not over");
        assert!(list.notes[0].top() < top, "the wheel below the field did not scroll the list");
    }

    /// A click opens the field as a text box with the note's name in it,
    /// and the name typed in its place is parsed back to a note through the
    /// writer API — one undo step for the edit.
    #[test]
    fn clicking_a_note_field_types_a_name_into_it() {
        let mut list = List::new();
        list.idle();
        let second = list.notes[1].center();
        assert_eq!(list.note(1), 37, "C#1, the second default lane");

        list.click(second);
        // The box is open and holds the name it had, selected, so what is
        // typed next replaces it rather than joining it.
        list.idle();
        assert!(list.painted.contains(&"C#1".to_string()), "got {:?}", list.painted);

        list.frame(vec![egui::Event::Text("f#3".to_string())]);
        list.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)]);
        list.idle();

        assert_eq!(list.note(1), 66, "F#3, typed in lower case");
        assert_eq!(list.model.undo_depth(), 1, "the edit is one step");
        assert!(list.painted.contains(&"F#3".to_string()), "got {:?}", list.painted);
        assert!(
            !list.painted.contains(&"f#3".to_string()),
            "the field kept the user's spelling; got {:?}",
            list.painted
        );
    }

    /// A name typed and then left behind — the user clicks away without
    /// pressing Enter — still lands, and still as one undo step.
    ///
    /// Its own path through [`DragValue`]: the text box is gone by the frame
    /// the value is applied, so the gesture ends on the field's *button*
    /// having lost the focus rather than on the box having lost it. A
    /// gesture that ended unnoticed would leave the change uncommitted, to
    /// be swept into whatever the user did next.
    #[test]
    fn a_name_left_without_an_enter_still_lands_as_one_step() {
        let mut list = List::new();
        list.idle();
        let first = list.notes[0].center();

        list.click(first);
        list.idle();
        list.frame(vec![egui::Event::Text("G1".to_string())]);

        // Somewhere else in the window, which is what takes the focus off
        // the box: the sample column of another row, which answers nothing
        // of its own yet.
        let elsewhere = pos2(list.notes[2].right() + 150.0, list.notes[2].center().y);
        list.click(elsewhere);
        list.idle();

        assert_eq!(list.note(0), 43, "G1 was typed and left");
        // A step on the stack is the whole of it: an edit the commit had
        // missed would still be sitting uncommitted, with nothing there.
        assert_eq!(list.model.undo_depth(), 1, "the edit is one step, closed when left");
        assert!(list.model.undo(), "and one undo puts it back");
        assert_eq!(list.note(0), 36);
    }

    /// A name that is not a note is refused: the lane keeps the note it had
    /// and the field goes back to showing it, with nothing on the undo stack
    /// to put right.
    #[test]
    fn a_name_that_is_not_a_note_leaves_the_lane_alone() {
        let mut list = List::new();
        list.idle();
        let first = list.notes[0].center();

        for garbage in ["H9", "", "banana", "C99"] {
            list.click(first);
            list.idle();
            list.frame(vec![egui::Event::Text(garbage.to_string())]);
            list.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)]);
            list.idle();

            assert_eq!(list.note(0), 36, "{garbage:?} reached the lane");
            assert_eq!(list.model.undo_depth(), 0, "{garbage:?} made an undo step");
            assert!(list.painted.contains(&"C1".to_string()), "got {:?}", list.painted);
        }
    }

    /// A double click on a name opens a box over the cell — at the cell's
    /// own size, so the column stays straight — and the name typed into it
    /// reaches the lane through the writer API as one undo step.
    #[test]
    fn double_clicking_a_name_types_a_new_one_into_the_lane() {
        let mut list = List::new();
        list.idle();
        let cell = list.names[0];
        let first = cell.center();
        assert_eq!(list.name(0), "Lane 1", "the first default lane");

        // Once is not the gesture: the name is a label until it is asked for
        // twice, so a single click leaves it alone.
        list.click(first);
        list.idle();
        assert_eq!(list.rename.editing(), None, "one click opened the box");

        list.double_click(first);
        list.idle();
        assert_eq!(list.rename.editing(), Some(list.order()[0]), "the box is open on the lane");
        assert_eq!(list.names[0].size(), cell.size(), "the box is not the cell's size");
        // The box holds the name it had, selected, so what is typed next
        // replaces it rather than joining it.
        assert!(list.painted.contains(&"Lane 1".to_string()), "got {:?}", list.painted);

        // The frame the box takes up the focus it asked for, and then the
        // typing.
        list.idle();
        list.frame(vec![egui::Event::Text("Kick".to_string())]);
        list.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)]);
        list.idle();

        assert_eq!(list.name(0), "Kick");
        assert_eq!(list.rename.editing(), None, "the box stayed open after it was taken");
        assert_eq!(list.model.undo_depth(), 1, "the rename is one step");
        assert!(list.painted.contains(&"Kick".to_string()), "got {:?}", list.painted);

        assert!(list.model.undo(), "and one undo puts the name back");
        assert_eq!(list.name(0), "Lane 1");
    }

    /// A rename abandoned by Escape, and one taken with nothing in it, both
    /// leave the lane named as it was — and neither makes an undo step to
    /// put right.
    ///
    /// The empty one is this window's rule rather than the model's: a name
    /// is a lane's one handle in the list, and a row with nothing in that
    /// column could not be found again to be renamed back.
    #[test]
    fn a_rename_escaped_or_emptied_keeps_the_name_the_lane_had() {
        let mut list = List::new();
        list.idle();
        let first = list.names[0].center();

        for (typed, key_pressed) in [("Snare", egui::Key::Escape), ("   ", egui::Key::Enter)] {
            list.double_click(first);
            list.idle();
            list.idle();
            list.frame(vec![egui::Event::Text(typed.to_string())]);
            list.frame(vec![key(key_pressed, true), key(key_pressed, false)]);
            list.idle();

            assert_eq!(list.name(0), "Lane 1", "{typed:?} reached the lane");
            assert_eq!(list.rename.editing(), None, "{typed:?} left the box open");
            assert_eq!(list.model.undo_depth(), 0, "{typed:?} made an undo step");
        }
    }

    /// A row dragged by its grip moves down the list as the pointer passes
    /// the rows below it, carrying its own note with it — display order and
    /// nothing else — and the whole drag is one undo step, closed at the
    /// release.
    #[test]
    fn dragging_a_row_by_its_grip_reorders_the_list_as_one_undo_step() {
        let mut list = List::new();
        list.idle();
        let before = list.order();
        // The places the first three rows occupy, which do not move: a
        // reorder changes which lane is drawn in a row, not where the row is.
        let places: Vec<Pos2> = list.grips.iter().take(3).map(|grip| grip.center()).collect();

        // By hand rather than through `drag`, because what the undo stack
        // holds *during* the drag is half of what is being tested.
        list.frame(vec![egui::Event::PointerMoved(places[0]), pointer(places[0], true)]);
        list.frame(vec![egui::Event::PointerMoved(places[1])]);

        assert_eq!(list.order()[..2], [before[1], before[0]], "the row has not moved yet");
        assert_eq!(list.model.undo_depth(), 0, "the step closes at the release, not before");
        assert!(list.model.can_undo(), "but the drag has changed something");

        list.frame(vec![egui::Event::PointerMoved(places[2])]);
        assert_eq!(list.order()[..3], [before[1], before[2], before[0]], "two rows passed");

        list.frame(vec![pointer(places[2], false)]);
        assert_eq!(list.order()[..3], [before[1], before[2], before[0]], "the release moved it on");
        assert_eq!(list.model.undo_depth(), 1, "one drag, one step");

        // Display order only: every lane kept its own note, so the column of
        // notes is now in the order the rows are.
        assert_eq!(list.note(0), 37, "C#1 — the second default lane, now the first");
        assert_eq!(list.model.lane(before[0]).expect("the dragged lane").note, 36, "still C1");

        assert!(list.model.undo(), "and one undo puts the order back");
        assert_eq!(list.order(), before);
    }

    /// A row dragged far past the end of the list stops in the last place
    /// rather than running off it, and one dragged nowhere moves nothing.
    #[test]
    fn a_row_dragged_past_the_end_of_the_list_stops_in_the_last_place() {
        let mut list = List::new();
        list.idle();
        let before = list.order();
        let first = list.grips[0].center();

        list.drag(first, first + vec2(0.0, 10_000.0));

        let mut expected = before.clone();
        expected.rotate_left(1);
        assert_eq!(list.order(), expected, "the row did not stop in the last place");
        assert_eq!(list.model.undo_depth(), 1, "one drag, one step");

        // And a grip merely clicked is no reorder at all: nothing moved, so
        // `commit` has no step to make.
        list.click(first);
        list.idle();
        assert_eq!(list.order(), expected, "a click on the grip moved the row");
        assert_eq!(list.model.undo_depth(), 1, "a click on the grip made an undo step");
    }

    /// The elastic column takes what the fixed fields leave, and stops at
    /// its minimum rather than going negative when the window is narrower
    /// than a row.
    #[test]
    fn the_sample_column_takes_what_is_left_down_to_its_minimum() {
        assert_eq!(sample_column_width(ROW_MIN_WIDTH), theme::HEADER_SAMPLE_MIN_WIDTH);
        assert_eq!(
            sample_column_width(ROW_MIN_WIDTH + 100.0),
            theme::HEADER_SAMPLE_MIN_WIDTH + 100.0
        );

        for narrow in [ROW_MIN_WIDTH - 1.0, 0.0, -100.0, f32::NAN] {
            assert_eq!(
                sample_column_width(narrow),
                theme::HEADER_SAMPLE_MIN_WIDTH,
                "{narrow} squeezed the column past its minimum"
            );
        }
    }

    /// Volumes as the field prints them: whole decibels, a sign only when
    /// there is a cut, and the ends of the range in range.
    #[test]
    fn volumes_print_as_whole_decibels() {
        assert_eq!(format_db(0.0), "0 dB");
        assert_eq!(format_db(-0.0), "0 dB");
        assert_eq!(format_db(-0.4), "0 dB", "a hair under unity is not a cut");
        assert_eq!(format_db(-6.0), "-6 dB");
        assert_eq!(format_db(-6.4), "-6 dB");
        assert_eq!(format_db(-6.5), "-7 dB", "rounded to the nearest, not truncated");
        assert_eq!(format_db(12.0), "12 dB");
        assert_eq!(format_db(LANE_VOLUME_DB_MIN), "-60 dB");
        assert_eq!(format_db(LANE_VOLUME_DB_MAX), "12 dB");

        // Clamped, not trusted: these cannot come off a lane, only out of a
        // project file or an automation curve that got past a setter.
        assert_eq!(format_db(99.0), "12 dB");
        assert_eq!(format_db(-999.0), "-60 dB");
        assert_eq!(format_db(f32::NAN), "0 dB", "a lost number is the lane default");
        assert_eq!(format_db(f32::INFINITY), "12 dB");
    }

    /// Pans as the field prints them: a side and a percentage, `C` in the
    /// middle.
    #[test]
    fn pans_print_as_a_side_and_a_percentage() {
        // The row's own composition: the float to its field's percentage,
        // then the percentage to its face.
        let face = |pan: f32| format_pan_percent(pan_percent(pan));
        assert_eq!(face(0.0), "C");
        assert_eq!(face(-0.0), "C");
        assert_eq!(face(-1.0), "L100");
        assert_eq!(face(1.0), "R100");
        assert_eq!(face(-0.5), "L50");
        assert_eq!(face(0.235), "R24", "rounded to the nearest percent");
        assert_eq!(face(-0.004), "C", "a hair off centre is still centre");

        assert_eq!(face(-9.0), "L100");
        assert_eq!(face(9.0), "R100");
        assert_eq!(face(f32::NAN), "C");
    }

    /// The choke field shows the group, and `0` for a lane in none — the
    /// field's own convention.
    #[test]
    fn the_choke_field_shows_the_group_number() {
        assert_eq!(format_choke(CHOKE_NONE), "0");
        assert_eq!(format_choke(1), "1");
        assert_eq!(format_choke(16), "16");
    }

    /// The two modes wear different faces, each one character wide, so the
    /// column can be read down.
    #[test]
    fn the_mode_toggle_shows_one_letter_per_mode() {
        assert_eq!(mode_label(LaneMode::OneShot), ONE_SHOT_LABEL);
        assert_eq!(mode_label(LaneMode::Gate), GATE_LABEL);
        assert_ne!(ONE_SHOT_LABEL, GATE_LABEL);
        for face in [ONE_SHOT_LABEL, GATE_LABEL] {
            assert_eq!(face.chars().count(), 1, "{face} does not fit one square");
        }
    }

    /// Every row's volume, pan, choke and mode field is its own column wide
    /// and its row tall, whatever is in it.
    ///
    /// The point the note field's own test makes, for the four fields added
    /// beside it: they are egui widgets and egui sizes a widget from its
    /// contents, so a field that took its own size would put the rows'
    /// columns at as many offsets as there are values. The widest value of
    /// each ("-60 dB", "L100", "16", and a lit "G") has to come out at the
    /// same width as the narrowest.
    #[test]
    fn every_rows_number_fields_are_one_column_wide_whatever_they_say() {
        let mut list = List::new();
        let first = list.model.lanes()[0].id;
        assert!(list.model.set_volume_db(first, LANE_VOLUME_DB_MIN), "-60 dB, the widest");
        assert!(list.model.set_pan(first, LANE_PAN_MIN), "L100, the widest");
        assert!(list.model.set_choke(first, CHOKE_GROUPS), "16, the widest");
        assert!(list.model.set_mode(first, LaneMode::Gate), "the lit face");
        list.idle();

        let columns: [(&str, f32, &Vec<Rect>); 4] = [
            ("volume", theme::HEADER_VOLUME_WIDTH, &list.volumes),
            ("pan", theme::HEADER_PAN_WIDTH, &list.pans),
            ("choke", theme::HEADER_CHOKE_WIDTH, &list.chokes),
            ("mode", theme::HEADER_TOGGLE_WIDTH, &list.modes),
        ];
        for (name, width, fields) in columns {
            assert_eq!(fields.len(), DEFAULT_LANE_COUNT as usize, "one {name} per lane");
            for (row, field) in fields.iter().enumerate() {
                assert_eq!(field.width(), width, "{name} of row {row} is out of its column");
                assert_eq!(field.height(), theme::ROW_HEIGHT, "{name} of row {row} is not a row");
            }
            let left = fields[0].left();
            assert!(
                fields.iter().all(|field| field.left() == left),
                "the {name} column is not straight"
            );
        }

        for face in ["-60 dB", "L100", "16", GATE_LABEL] {
            assert!(list.painted.contains(&face.to_string()), "{face}; got {:?}", list.painted);
        }
    }

    /// A drag over the volume field moves the lane's volume — right and up
    /// for louder — and the whole drag is one undo step, closed when the
    /// button comes up and not once a frame.
    #[test]
    fn dragging_a_volume_field_moves_the_volume_and_is_one_undo_step() {
        let mut list = List::new();
        list.idle();
        let first = list.volumes[0].center();
        assert_eq!(list.volume(0), 0, "a new lane plays at unity");
        assert!(!list.model.can_undo(), "nothing has been done yet");

        // By hand rather than through `drag`, because what the undo stack
        // holds *during* the drag is half of what is being tested.
        list.frame(vec![egui::Event::PointerMoved(first), pointer(first, true)]);
        list.frame(vec![egui::Event::PointerMoved(first - vec2(0.0, 20.0))]);
        list.frame(vec![egui::Event::PointerMoved(first - vec2(0.0, 40.0))]);

        let during = list.volume(0);
        assert!(during > 0, "a drag upwards did not raise the volume: {during}");
        assert!(during <= 10, "40 points at a quarter of a decibel each is ten: {during}");
        assert_eq!(list.model.undo_depth(), 0, "the step closes at the release, not before");
        assert!(list.model.can_undo(), "but the drag has changed something");

        list.frame(vec![pointer(first - vec2(0.0, 40.0), false)]);
        assert_eq!(list.volume(0), during, "the release moved the volume further");
        assert_eq!(list.model.undo_depth(), 1, "one drag, one step");
        assert!(
            list.painted.contains(&format_db(during as f32)),
            "the field shows {during} dB; got {:?}",
            list.painted
        );

        // And the other way: left lowers it, as up raised it.
        list.drag(first, first - vec2(20.0, 0.0));
        assert!(list.volume(0) < during, "a drag to the left did not lower the volume");
        assert_eq!(list.model.undo_depth(), 2);

        assert!(list.model.undo() && list.model.undo(), "both drags are there to undo");
        assert_eq!(list.volume(0), 0, "undone twice, the lane is back at unity");
    }

    /// A drag far past either end of a field's range stops at the end rather
    /// than running away with the lane's setting.
    ///
    /// The three ranges at once, because they are three ends of the same
    /// arrangement: the field's own range bounds the drag, and the model's
    /// setter clamps whatever gets past it.
    #[test]
    fn a_drag_stops_at_the_ends_of_every_fields_range() {
        let mut list = List::new();
        list.idle();
        let (volume, pan, choke) =
            (list.volumes[0].center(), list.pans[0].center(), list.chokes[0].center());

        list.drag(volume, volume + vec2(4_000.0, 0.0));
        assert_eq!(list.volume(0), whole_db(LANE_VOLUME_DB_MAX), "+12 dB is as loud as it goes");
        list.drag(volume, volume - vec2(8_000.0, 0.0));
        assert_eq!(list.volume(0), whole_db(LANE_VOLUME_DB_MIN), "-60 dB is as quiet as it goes");

        list.drag(pan, pan + vec2(4_000.0, 0.0));
        assert_eq!(list.pan(0), pan_percent(LANE_PAN_MAX), "hard right is as far as a pan goes");
        list.drag(pan, pan - vec2(8_000.0, 0.0));
        assert_eq!(list.pan(0), pan_percent(LANE_PAN_MIN), "hard left is as far as it goes");

        list.drag(choke, choke + vec2(4_000.0, 0.0));
        assert_eq!(list.choke(0), CHOKE_GROUPS, "16 is the last group");
        list.drag(choke, choke - vec2(8_000.0, 0.0));
        assert_eq!(list.choke(0), CHOKE_NONE, "and 0 is no group at all");
    }

    /// A notch of the wheel over a number field is one of that field's own
    /// units — a decibel, a percent of pan, a choke group — and the list it
    /// sits in does not scroll under it, for the reason the note field's own
    /// wheel test gives.
    #[test]
    fn a_notch_of_the_wheel_is_one_unit_of_whichever_field_it_is_over() {
        let mut list = List::new();
        // Enough lanes that the list is taller than the window and so has
        // somewhere to scroll to.
        while list.model.lanes().len() < 40 {
            assert!(list.model.add_lane().is_some(), "well under `MAX_LANES`");
        }
        list.idle();
        let top = list.volumes[0].top();

        list.scroll(list.volumes[0].center(), 1.0);
        assert_eq!(list.volume(0), 1, "a notch up is a decibel up");
        assert_eq!(list.model.undo_depth(), 1, "a notch is a gesture of its own");

        list.scroll(list.pans[0].center(), -1.0);
        assert_eq!(list.pan(0), -1, "a notch down is a percent to the left");
        assert_eq!(list.model.undo_depth(), 2);

        list.scroll(list.chokes[0].center(), 1.0);
        assert_eq!(list.choke(0), 1, "a notch up is the next choke group");
        assert_eq!(list.model.undo_depth(), 3, "three notches, three steps");

        // Several notches in one turn are several units, however many frames
        // egui smooths them over — and each of them a gesture, so the stack
        // grows by more than one.
        list.scroll(list.volumes[0].center(), -5.0);
        assert_eq!(list.volume(0), -4, "five notches down from a decibel up");
        assert!(list.model.undo_depth() > 3, "the turn made no step at all");

        assert_eq!(list.volumes[0].top(), top, "the list scrolled under the pointer as well");
    }

    /// A click opens a number field as a text box, and what is typed into it
    /// reaches the lane through the writer API as one undo step — in the
    /// spelling the field itself prints, and in the sloppier ones a user
    /// would try.
    #[test]
    fn clicking_a_volume_field_types_decibels_into_it() {
        let mut list = List::new();
        list.idle();
        let first = list.volumes[0].center();

        for (typed, expected) in [("-12 dB", -12), ("-6", -6), ("3db", 3), ("-6.4", -6)] {
            list.click(first);
            list.idle();
            list.frame(vec![egui::Event::Text(typed.to_string())]);
            list.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)]);
            list.idle();

            assert_eq!(list.volume(0), expected, "{typed:?} did not reach the lane");
            assert!(
                list.painted.contains(&format_db(expected as f32)),
                "{typed:?} left the field showing something else; got {:?}",
                list.painted
            );
        }
        assert_eq!(list.model.undo_depth(), 4, "one step per edit");

        // A text with no number in it is refused, and the field goes back to
        // the volume the lane has.
        for garbage in ["", "loud", "dB"] {
            let before = list.volume(0);
            list.click(first);
            list.idle();
            list.frame(vec![egui::Event::Text(garbage.to_string())]);
            list.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)]);
            list.idle();

            assert_eq!(list.volume(0), before, "{garbage:?} reached the lane");
            assert_eq!(list.model.undo_depth(), 4, "{garbage:?} made an undo step");
        }
    }

    /// The pan field takes a side and a distance typed into it, in either
    /// case, and the centre by its letter.
    #[test]
    fn clicking_a_pan_field_types_a_side_and_a_distance_into_it() {
        let mut list = List::new();
        list.idle();
        let first = list.pans[0].center();

        for (typed, expected) in [("L50", -50), ("r25", 25), ("C", 0), ("-80", -80), ("R200", 100)]
        {
            list.click(first);
            list.idle();
            list.frame(vec![egui::Event::Text(typed.to_string())]);
            list.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)]);
            list.idle();

            assert_eq!(list.pan(0), expected, "{typed:?} did not reach the lane");
            assert!(
                list.painted.contains(&format_pan_percent(expected)),
                "{typed:?} left the field showing something else; got {:?}",
                list.painted
            );
        }
    }

    /// A click on the one-shot/gate toggle swaps the lane's mode, one undo
    /// step a click, and the square it is in says which mode the lane is now
    /// in.
    #[test]
    fn clicking_the_mode_toggle_swaps_the_mode_as_one_undo_step() {
        let mut list = List::new();
        list.idle();
        let first = list.modes[0].center();
        assert_eq!(list.mode(0), LaneMode::OneShot, "a new lane plays whole samples");
        assert!(list.painted.contains(&ONE_SHOT_LABEL.to_string()), "got {:?}", list.painted);

        list.click(first);
        list.idle();
        assert_eq!(list.mode(0), LaneMode::Gate, "the click did not swap the mode");
        assert_eq!(list.model.undo_depth(), 1, "one click, one step");
        assert!(list.painted.contains(&GATE_LABEL.to_string()), "got {:?}", list.painted);

        list.click(first);
        list.idle();
        assert_eq!(list.mode(0), LaneMode::OneShot, "the second click did not swap it back");
        assert_eq!(list.model.undo_depth(), 2);

        assert!(list.model.undo() && list.model.undo(), "both clicks are there to undo");
        assert_eq!(list.mode(0), LaneMode::OneShot);

        // And it is the row's own toggle: the lane below keeps its mode.
        let second = list.modes[1].center();
        list.click(second);
        list.idle();
        assert_eq!(list.mode(1), LaneMode::Gate);
        assert_eq!(list.mode(0), LaneMode::OneShot, "the click reached the wrong row");
    }

    /// A lane's volume in the whole decibels its field works in: rounded to
    /// the nearest, and clamped — a value out of a project file cannot make
    /// the field start a drag from a number the lane could not hold.
    #[test]
    fn whole_decibels_are_rounded_and_clamped() {
        assert_eq!(whole_db(0.0), 0);
        assert_eq!(whole_db(-6.4), -6);
        assert_eq!(whole_db(-6.5), -7);
        assert_eq!(whole_db(LANE_VOLUME_DB_MIN), -60);
        assert_eq!(whole_db(LANE_VOLUME_DB_MAX), 12);
        assert_eq!(whole_db(-999.0), -60);
        assert_eq!(whole_db(99.0), 12);
        assert_eq!(whole_db(f32::NAN), 0, "a lost number is the lane default");
        assert_eq!(whole_db(f32::NEG_INFINITY), -60);
    }

    /// Decibels as the field parses them: the unit is optional, fractions
    /// round, and only a text with no number at all is refused.
    #[test]
    fn typed_decibels_are_parsed_loosely_and_rounded() {
        assert_eq!(parse_db("0"), Some(0));
        assert_eq!(parse_db("-6"), Some(-6));
        assert_eq!(parse_db("-6 dB"), Some(-6));
        assert_eq!(parse_db("-6dB"), Some(-6));
        assert_eq!(parse_db("  12 DB  "), Some(12));
        assert_eq!(parse_db("-6.5"), Some(-7), "rounded to the nearest, not truncated");
        assert_eq!(parse_db("+3"), Some(3));

        // Past the ends is not refused — the field's own range clamps it,
        // which is what makes "-80" mean "as quiet as it goes".
        assert_eq!(parse_db("-80"), Some(-80));

        assert_eq!(parse_db(""), None);
        assert_eq!(parse_db("dB"), None);
        assert_eq!(parse_db("loud"), None);
        assert_eq!(parse_db("inf"), None, "an infinity must not reach the cast");
        assert_eq!(parse_db("NaN"), None);
    }

    /// Pans as the field keeps, prints and parses them, round trip: the
    /// percentage a lane's pan comes to, the text that percentage is shown
    /// as, and the percentage that text parses back to.
    #[test]
    fn pans_go_round_from_a_lane_through_the_field_and_back() {
        for (pan, percent, face) in [
            (0.0, 0, "C"),
            (-1.0, -100, "L100"),
            (1.0, 100, "R100"),
            (-0.5, -50, "L50"),
            (0.24, 24, "R24"),
        ] {
            assert_eq!(pan_percent(pan), percent, "{pan} is not {percent}%");
            assert_eq!(format_pan_percent(percent), face);
            assert_eq!(parse_pan(face), Some(percent), "{face} did not parse back");
            assert_eq!(pan_from_percent(percent), pan, "{percent}% is not {pan}");
        }

        assert_eq!(pan_percent(f32::NAN), 0, "a lost number is the centre");
        assert_eq!(pan_percent(-9.0), -100, "clamped, not trusted");

        // The one percentage with no positive twin: public, so not bounded
        // by the field's range.
        assert_eq!(format_pan_percent(i32::MIN), format!("L{}", i32::MIN.unsigned_abs()));
    }

    /// Pans as the field parses them: either case, a bare percentage, a
    /// stray per-cent sign — and a text that names no side and no number is
    /// refused.
    #[test]
    fn typed_pans_take_a_side_a_number_or_both() {
        assert_eq!(parse_pan("c"), Some(0));
        assert_eq!(parse_pan(" C "), Some(0));
        assert_eq!(parse_pan("l50"), Some(-50));
        assert_eq!(parse_pan("R 25"), Some(25));
        assert_eq!(parse_pan("L50%"), Some(-50));
        assert_eq!(parse_pan("0"), Some(0));
        assert_eq!(parse_pan("-40"), Some(-40), "a bare negative is to the left");
        assert_eq!(parse_pan("40"), Some(40));
        assert_eq!(parse_pan("R24.4"), Some(24), "rounded, as the field is whole percents");

        // Clamped by the field's range rather than refused, as a volume is.
        assert_eq!(parse_pan("L200"), Some(-200));

        assert_eq!(parse_pan(""), None);
        assert_eq!(parse_pan("C50"), None, "the centre has no distance");
        assert_eq!(parse_pan("left"), None);
        assert_eq!(parse_pan("L"), None);
        assert_eq!(parse_pan("R inf"), None);
    }

    /// The number a field shows, out of the fractional one a drag value
    /// works in: rounded, held inside the field's own range, and never a
    /// panic or a saturating cast on a value that is not a number.
    #[test]
    fn a_dragged_number_is_rounded_and_held_in_the_fields_range() {
        let range = -60..=12;
        assert_eq!(dragged_value(0.0, &range), 0);
        assert_eq!(dragged_value(-6.4, &range), -6);
        assert_eq!(dragged_value(-6.5, &range), -7);
        assert_eq!(dragged_value(1e9, &range), 12);
        assert_eq!(dragged_value(-1e9, &range), -60);
        assert_eq!(dragged_value(f64::INFINITY, &range), 12);
        assert_eq!(dragged_value(f64::NAN, &range), 0, "a lost number is nought");
    }

    /// The toggle's two faces, their hover texts and the mode each click
    /// asks for — three pure reads of one two-valued setting.
    #[test]
    fn the_mode_toggle_swaps_faces_hovers_and_modes() {
        assert_eq!(other_mode(LaneMode::OneShot), LaneMode::Gate);
        assert_eq!(other_mode(LaneMode::Gate), LaneMode::OneShot);

        assert_eq!(mode_hover(LaneMode::OneShot), ONE_SHOT_HOVER);
        assert_eq!(mode_hover(LaneMode::Gate), GATE_HOVER);
        assert_ne!(ONE_SHOT_HOVER, GATE_HOVER);
        for hover in [ONE_SHOT_HOVER, GATE_HOVER] {
            assert!(hover.contains("click"), "{hover} does not say it is a switch");
        }
    }

    // ---- a drag's landing target ----

    /// A drag over a row outlines that row and nothing else: the promise the
    /// drop then keeps, made against the rows as the window really laid them
    /// out ([`drops::route`] states the same rule over bare geometry).
    #[test]
    fn a_drag_outlines_the_row_it_is_over() {
        let mut list = List::new();
        // A first frame to lay the rows out — a drag arrives over a window
        // that is already open.
        list.idle();
        let rows = list.rows.clone();
        assert_eq!(rows.len(), DEFAULT_LANE_COUNT as usize, "one row per lane");

        for row in &rows {
            list.hover(row.rect.center(), &["/nonexistent/BD 606.wav"]);

            assert_eq!(
                list.outlined,
                [row.rect],
                "the drag over lane {} outlined {:?}",
                row.id,
                list.outlined
            );
        }
    }

    /// A drag under the last row outlines the space there instead — where
    /// the lanes it would add are going to appear.
    #[test]
    fn a_drag_below_the_list_outlines_the_space_under_it() {
        let mut list = List::new();
        let crowd: Vec<u32> = list.model.lanes().iter().skip(3).map(|lane| lane.id).collect();
        for id in crowd {
            assert!(list.model.delete_lane(id), "leaving three rows and room under them");
        }
        list.idle();

        let last = *list.rows.last().expect("three rows");
        let below = Pos2::new(list.list.center().x, last.rect.bottom() + theme::ROW_PITCH);
        assert!(list.list.contains(below), "the hover has to be inside the list");

        list.hover(below, &["/nonexistent/kick.wav"]);

        assert_eq!(list.outlined.len(), 1, "got {:?}", list.outlined);
        let outline = list.outlined[0];
        assert_eq!(outline.top(), last.rect.bottom() + theme::ROW_GAP, "under the last row");
        assert_eq!(outline.bottom(), list.list.bottom(), "down to the end of the list");
        assert_eq!(outline.x_range(), last.rect.x_range(), "and in line with the rows");
    }

    /// With no lanes at all the whole list is the landing space — the window
    /// the big hint is inviting a drag into.
    #[test]
    fn a_drag_over_an_empty_list_outlines_the_whole_list() {
        let mut list = List::new();
        list.model = model_with_no_lanes();
        list.idle();

        let at = list.list.center();
        list.hover(at, &["/nonexistent/kick.wav"]);

        assert_eq!(list.outlined, [list.list]);
        assert!(list.painted.contains(&EMPTY_HINT.to_string()), "the hint is still there");
    }

    /// The outline belongs to the drag and not to the pointer: a row under
    /// the pointer with nothing in hand is no landing target, and an open
    /// window with no pointer in it is not either.
    #[test]
    fn nothing_is_outlined_without_a_drag() {
        let mut list = List::new();
        list.idle();
        assert!(list.outlined.is_empty(), "an idle window outlines nothing");

        let over_a_row = list.rows[2].rect.center();
        list.frame(vec![egui::Event::PointerMoved(over_a_row)]);
        assert!(list.outlined.is_empty(), "got {:?}", list.outlined);

        // And it is gone the frame the drag leaves the window.
        list.hover(over_a_row, &["/nonexistent/kick.wav"]);
        assert_eq!(list.outlined.len(), 1, "the drag is over the list");
        list.idle();
        assert!(list.outlined.is_empty(), "the drag left");
    }

    // ---- the audition button and the activity light ---------------------

    /// A click on a loaded lane's play button asks for that lane and no
    /// other, on the frame of the click and on no other frame.
    #[test]
    fn clicking_a_play_button_asks_for_its_own_lane() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        assert_eq!(list.asked, None, "an idle window asks for nothing");

        let loaded = list.model.lanes()[0].id;
        list.click(list.auditions[0].center());

        assert_eq!(list.asked, Some(loaded), "the lane the button belongs to");

        // The click is a frame, not a state: the next frame asks again for
        // nothing, so an audition is heard once per click.
        list.idle();
        assert_eq!(list.asked, None);
    }

    /// A click on an empty lane's play button asks for nothing: there is no
    /// published lane behind it to sound, and the ring it would take a place
    /// in is small and best-effort ([`audition_button`]).
    #[test]
    fn clicking_an_empty_lanes_play_button_asks_for_nothing() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        assert!(list.model.lanes()[1].sample.is_none(), "the second lane is empty");

        list.click(list.auditions[1].center());

        assert_eq!(list.asked, None);
    }

    /// The triangle says which buttons can do something: primary text on a
    /// lane with a sample, the dimmer tone on a lane with nothing to play.
    #[test]
    fn a_play_button_is_dim_on_a_lane_with_nothing_to_play() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();

        assert_eq!(list.triangles.len(), DEFAULT_LANE_COUNT as usize, "one per row");
        assert_eq!(list.triangles[0], theme::TEXT, "the loaded lane can play");
        for (row, triangle) in list.triangles.iter().enumerate().skip(1) {
            assert_eq!(*triangle, theme::TEXT_DIM, "row {row} has nothing to play");
        }
    }

    /// Every row's play button sits up front, right after the colour dot and
    /// just before the note, a row tall and in line down the list
    /// ([`theme::HEADER_AUDITION_WIDTH`]).
    #[test]
    fn the_play_buttons_lead_every_row() {
        let mut list = List::new();
        list.idle();

        assert_eq!(list.auditions.len(), DEFAULT_LANE_COUNT as usize);
        for (row, button) in list.auditions.iter().enumerate() {
            assert_eq!(button.width(), theme::HEADER_AUDITION_WIDTH, "row {row}");
            assert_eq!(button.height(), theme::ROW_HEIGHT, "row {row}");
            assert_eq!(button.left(), list.auditions[0].left(), "row {row} is out of line");
            assert_eq!(
                button.right() + theme::HEADER_FIELD_GAP,
                list.notes[row].left(),
                "row {row} is not just before its note"
            );
            let left = list.rows[row].rect.left();
            let fields = theme::HEADER_GRIP_WIDTH + theme::HEADER_DOT_WIDTH;
            let after_dot = left + fields + 2.0 * theme::HEADER_FIELD_GAP;
            assert_eq!(button.left(), after_dot, "row {row}: not right after the dot");
        }
    }

    /// A lane the engine has struck draws its dot brightened, and only that
    /// lane's; a few frames later the light has gone out and the dot is the
    /// lane's plain colour again.
    ///
    /// The window is kept drawing by the fade itself here, which is the
    /// repaint the editor asks for while [`Lights::fading`] holds.
    #[test]
    fn a_struck_lane_brightens_its_dot_and_fades_back() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();

        let plain = theme::rgb(list.model.lanes()[0].colour);
        assert_eq!(list.dots[0], plain, "nothing has played yet");

        list.strike(0);
        list.idle();

        assert_eq!(list.dots[0], theme::activity_dot(plain, 1.0), "struck this frame");
        assert!(list.dots[0] != plain, "the light has to be visible");
        for (row, dot) in list.dots.iter().enumerate().skip(1) {
            assert_eq!(*dot, theme::rgb(list.model.lanes()[row].colour), "row {row} played");
        }

        // egui's own clock advances a frame at a time, so the fade is over
        // within a handful of them whatever the frame rate.
        for _ in 0..200 {
            list.idle();
            if !list.lights.fading() {
                break;
            }
        }
        assert!(!list.lights.fading(), "the light never went out");
        assert_eq!(list.dots[0], plain, "and the dot is the lane's own colour again");
    }

    /// An audition is the other half of the same loop: the click asks, the
    /// engine counts, and the dot lights. The engine is not in this test, so
    /// the count stands in for it — what is pinned here is that the light
    /// belongs to the lane the button asked for.
    #[test]
    fn an_audition_lights_the_lane_it_asked_for() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();

        list.click(list.auditions[0].center());
        let asked = list.asked.expect("the click asked for a lane");
        assert_eq!(asked, list.model.lanes()[0].id);

        // What the engine does with it, one block later.
        list.strike(0);
        list.idle();

        let plain = theme::rgb(list.model.lanes()[0].colour);
        assert_eq!(list.dots[0], theme::activity_dot(plain, 1.0));
    }

    /// The audition button answers the pointer and the rest of the row does
    /// not: the button lightens under a hover, and the name cell beside it —
    /// which senses clicks of its own, to catch a double one — keeps the flat
    /// grey every painted cell in the row shares ([`control_box`]'s
    /// `reactive`).
    ///
    /// Pinned because the two look the same until a pointer is over them: a
    /// `control_box` that read its visuals off `sense` alone would light the
    /// name cell too and make a row ripple as the pointer crossed it, which
    /// is a change to the name field and not to this one.
    #[test]
    fn only_the_audition_button_answers_the_pointer() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        let button = list.auditions[0];
        let name = list.names[0];
        let (resting_button, resting_name) = (list.fill_at(button), list.fill_at(name));

        // Over the button: it lifts, and nothing else in the row moves.
        list.point_at(button.center());
        assert_ne!(list.fill_at(button), resting_button, "the button is dead under the pointer");
        assert_eq!(list.fill_at(name), resting_name, "the name cell moved with it");

        // Over the name cell: neither of them moves.
        list.point_at(name.center());
        assert_eq!(list.fill_at(name), resting_name, "the name cell lit on a hover");
        assert_eq!(list.fill_at(button), resting_button, "the button lit from across the row");
    }

    /// A lane that was never published cannot light up at all: there is no
    /// counter behind an empty lane, so its dot is its colour for as long as
    /// it stays empty.
    #[test]
    fn an_empty_lanes_dot_never_lights() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        // The frame that sees the lanes before the hit, so that the hit is
        // a change and not the history a window opens on
        // ([`crate::editor::activity`]).
        list.idle();
        list.strike(0);
        list.idle();

        assert_eq!(list.dots.len(), DEFAULT_LANE_COUNT as usize, "one dot per row");
        for (row, dot) in list.dots.iter().enumerate().skip(1) {
            assert_eq!(*dot, theme::rgb(list.model.lanes()[row].colour), "row {row}");
        }
        assert!(list.lights.fading(), "the loaded lane is lit, though");
    }
    /// A secondary click anywhere over a row opens that row's menu, with all
    /// of its items in it — and over no other row (one menu at a time).
    #[test]
    fn a_right_click_over_a_row_opens_that_rows_menu() {
        let mut list = List::new();
        list.idle();
        assert!(!list.menu_open(), "a window opens with no menu in it");

        // Over the row's name cell, which is a widget of its own: a
        // right-click on a field is still a right-click on the row
        // (`editor/menu.rs`).
        list.right_click(list.names[0].center());

        assert!(list.menu_open(), "the right-click opened nothing");
        for label in [
            menu::RENAME_LABEL,
            menu::COLOUR_LABEL,
            menu::DUPLICATE_LABEL,
            menu::DELETE_LABEL,
            menu::RELOAD_LABEL,
            menu::CLEAR_LABEL,
            menu::OWN_OUTPUTS_LABEL,
        ] {
            assert!(
                list.painted.iter().any(|text| text == label),
                "the menu has no {label:?} in it; got {:?}",
                list.painted
            );
        }

        assert_eq!(
            list.menu().swatches.len(),
            theme::LANE_SWATCHES.len(),
            "the palette's swatches, all of them"
        );
    }

    /// Escape closes the menu, as it leaves any field in this window — and
    /// so does a click on the window outside it.
    #[test]
    fn the_menu_closes_on_escape_and_on_a_click_outside_it() {
        let mut list = List::new();
        list.idle();

        list.right_click(list.names[0].center());
        assert!(list.menu_open());
        list.escape();
        assert!(!list.menu_open(), "Escape left the menu open");

        list.right_click(list.names[0].center());
        assert!(list.menu_open());
        // The grip of the row at the other end of the list: a menu is
        // nearly as tall as the window, so "outside it" has to be a column
        // the popup does not cover as well as a row it does not.
        let outside = list.grips[DEFAULT_LANE_COUNT as usize - 1].center();
        assert!(!list.menu_rect().contains(outside), "the test clicked inside the menu");
        list.click_menu(outside);
        assert!(!list.menu_open(), "a click elsewhere left the menu open");
    }

    /// "rename" opens the row's own rename box — the same box a double
    /// click on the name opens, with the lane's name in it.
    #[test]
    fn the_menus_rename_opens_the_rows_rename_box() {
        let mut list = List::new();
        list.idle();
        let id = list.model.lanes()[0].id;

        list.right_click(list.names[0].center());
        let rename = list.menu().rename;
        list.click_menu(rename.center());

        assert_eq!(list.rename.editing(), Some(id), "the box is not open on the lane");
        assert!(!list.menu_open(), "the menu stayed open over the box it just opened");

        // And it is a box like any other: typed into and taken by Enter.
        list.frame(vec![egui::Event::Text(String::from("Kick")), key(egui::Key::Enter, true)]);
        assert_eq!(list.name(0), "Kick");
    }

    /// A swatch gives the lane its colour, as one undo step, and the lane's
    /// own swatch wears the accent ring that says which one it is.
    #[test]
    fn a_swatch_recolours_the_lane() {
        let mut list = List::new();
        list.idle();
        let was = list.model.lanes()[0].colour;

        list.right_click(list.names[0].center());
        // The fifth swatch, which no default lane is wearing in the first
        // row (lane *n* opens with palette colour *n*).
        let swatch = list.menu().swatches[4];
        list.click_menu(swatch.center());

        let wanted = theme::lane_swatch_rgb(4);
        assert_ne!(wanted, was, "the test picked the colour the lane already had");
        assert_eq!(list.model.lanes()[0].colour, wanted, "the swatch painted nothing");
        assert!(!list.menu_open(), "the menu stayed open over the colour it just set");

        // One gesture, one step back.
        assert!(list.model.undo());
        assert_eq!(list.model.lanes()[0].colour, was);
    }

    /// The accent ring is on the swatch the lane is wearing and on no
    /// other: the one thing the palette says beyond the ten colours.
    #[test]
    fn the_lanes_own_swatch_wears_the_accent_ring() {
        let mut list = List::new();
        list.idle();
        // The second row, so the ringed swatch is not the first of the ten
        // and a test that ringed them all would fail here.
        list.right_click(list.names[1].center());

        let worn = list.menu().swatches[1];
        assert_eq!(list.rings.len(), 1, "one swatch is ringed, found {}", list.rings.len());
        assert_eq!(list.rings[0], worn, "the ring is on the wrong swatch");
        // Which colour it is drawn in is [`menu::swatch_ring`]'s own test:
        // egui fades a popup in, so a ring read off a frame is the theme's
        // accent multiplied by however far through that fade the frame is.
        assert_eq!(
            menu::swatch_ring(list.model.lanes()[1].colour, theme::lane_swatch_rgb(1), false),
            Some(theme::ACCENT)
        );
    }

    /// "duplicate" puts a copy of the lane directly below it, named after
    /// it, as one undo step.
    #[test]
    fn duplicate_copies_the_lane_below_itself() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        let was = list.order();

        list.right_click(list.names[0].center());
        let duplicate = list.menu().duplicate;
        list.click_menu(duplicate.center());

        assert_eq!(list.model.lanes().len(), was.len() + 1, "no copy was made");
        assert_eq!(list.name(1), "Lane 1 copy", "the copy is not below its original");
        assert!(
            list.model.lanes()[1].sample.is_some(),
            "the copy came without the sample it was copied from"
        );
        assert!(!list.menu_open());

        assert!(list.model.undo());
        assert_eq!(list.order(), was, "one undo step did not take the copy back");
    }

    /// At [`crate::model::MAX_LANES`] "duplicate" is drawn but dead: a
    /// click on it copies nothing and leaves the menu open, as the grey
    /// file items do on an empty lane — and hovering it says why, in
    /// `+ lane`'s own words.
    #[test]
    fn duplicate_is_disabled_on_a_full_list() {
        let mut model = Model::new();
        while model.lanes().len() < crate::model::MAX_LANES as usize {
            assert!(model.add_lane().is_some(), "the list filled before `MAX_LANES`");
        }
        let mut list = List::over(model);
        list.idle();
        let was = list.order();

        list.right_click(list.names[0].center());
        let duplicate = list.menu().duplicate;
        assert!(duplicate.width() > 0.0, "the item is not drawn at all");

        // Rest the pointer on it past egui's tooltip delay: one move, then
        // a second of still frames (each one advances the input clock by
        // egui's predicted frame time).
        list.frame(vec![egui::Event::PointerMoved(duplicate.center())]);
        for _ in 0..60 {
            list.idle();
        }
        assert!(
            list.painted.iter().any(|text| text == crate::editor::top_bar::ADD_LANE_FULL_HOVER),
            "the dead item does not say why"
        );

        list.click_menu(duplicate.center());
        assert_eq!(list.order(), was, "a full list took a copy");
        assert!(list.menu_open(), "a dead item closed the menu");
    }

    /// Delete asks twice: the first click arms the item and leaves the menu
    /// open wearing the question, the second one deletes — as one undo step.
    #[test]
    fn delete_asks_twice_before_it_takes_a_lane() {
        let mut list = List::new();
        list.idle();
        let was = list.order();

        list.right_click(list.names[0].center());
        let delete = list.menu().delete;
        list.click_menu(delete.center());

        assert_eq!(list.order(), was, "the first click deleted the lane");
        assert!(list.menu_open(), "the menu closed under the question it was asking");
        assert!(
            list.painted.iter().any(|text| text == menu::DELETE_CONFIRM_LABEL),
            "the item does not say a second click will do it; got {:?}",
            list.painted
        );

        list.click_menu(list.menu().delete.center());
        assert_eq!(list.model.lanes().len(), was.len() - 1, "the second click deleted nothing");
        assert_eq!(list.order(), was[1..], "the wrong lane went");
        assert!(!list.menu_open());

        assert!(list.model.undo());
        assert_eq!(list.order(), was, "one undo step did not bring the lane back");
    }

    /// The armed delete lives as long as its own menu and no longer: a menu
    /// reopened after one unanswered click asks from the start again
    /// ([`menu::Confirm::follow`]).
    #[test]
    fn an_unanswered_delete_is_forgotten_with_its_menu() {
        let mut list = List::new();
        list.idle();
        let id = list.model.lanes()[0].id;

        list.right_click(list.names[0].center());
        list.click_menu(list.menu().delete.center());
        assert!(list.confirm.armed(id), "the first click armed nothing");

        list.escape();
        assert!(!list.confirm.armed(id), "the question outlived its menu");

        list.right_click(list.names[0].center());
        assert!(
            list.painted.iter().any(|text| text == menu::DELETE_LABEL),
            "the reopened menu is still armed; got {:?}",
            list.painted
        );
        // And that click is the first of two all over again.
        list.click_menu(list.menu().delete.center());
        assert_eq!(list.model.lanes()[0].id, id, "a reopened menu deleted on one click");
    }

    /// "clear sample" takes the audio off the lane and leaves the lane:
    /// its name, its note and its colour stay, and the row goes back to the
    /// hint for an empty one.
    #[test]
    fn clear_sample_empties_the_lane_but_keeps_it() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        let (id, name, note) = {
            let lane = &list.model.lanes()[0];
            (lane.id, lane.name.clone(), lane.note)
        };

        list.right_click(list.names[0].center());
        list.click_menu(list.menu().clear.center());

        let lane = &list.model.lanes()[0];
        assert_eq!(lane.id, id, "the lane went with its sample");
        assert!(lane.sample.is_none(), "the sample is still on the lane");
        assert_eq!((lane.name.as_str(), lane.note), (name.as_str(), note));
        assert!(
            list.painted.iter().any(|text| text == status::DROP_HINT),
            "the emptied row does not invite a sample"
        );

        assert!(list.model.undo());
        assert!(list.model.lanes()[0].sample.is_some(), "one undo step did not bring it back");
    }

    /// "reload from file" hands the window the lane and the path to read
    /// again — the loader's job, not the list's (`editor/menu.rs`).
    #[test]
    fn reload_hands_the_window_the_lanes_own_path() {
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        let id = list.model.lanes()[0].id;

        list.right_click(list.names[0].center());
        list.click(list.menu().reload.center());

        assert_eq!(
            list.reload,
            Some((id, PathBuf::from("/home/dev/kit/BD 606.wav"))),
            "the window was not asked to reload the lane's own file"
        );
        // Nothing was written: the sample the lane is playing stays until a
        // decode replaces it.
        assert!(list.model.lanes()[0].sample.is_some());
    }

    /// A lane with nothing in it offers neither file action: both items are
    /// drawn, so the menu is the same shape on every lane, and neither does
    /// anything when clicked.
    #[test]
    fn an_empty_lane_offers_no_reload_and_no_clear() {
        let mut list = List::new();
        list.idle();

        list.right_click(list.names[0].center());
        let (reload, clear) = (list.menu().reload, list.menu().clear);
        assert!(reload.width() > 0.0 && clear.width() > 0.0, "the items are not drawn at all");

        list.click_menu(reload.center());
        assert_eq!(list.reload, None, "an empty lane asked the loader for a file");
        list.click_menu(clear.center());
        assert!(list.menu_open(), "a dead item closed the menu");
        assert_eq!(list.model.lanes().len(), DEFAULT_LANE_COUNT as usize, "a lane went");
    }

    /// A model whose first lane is a chop cut from `/songs/Eye.wav`,
    /// 30.120 s–32.090 s at 48 kHz, and the origin it remembers.
    fn model_with_a_chop_first_lane() -> (Model, ChopOrigin) {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        let origin = ChopOrigin {
            source_path: String::from("/songs/Eye.wav"),
            start_frame: 1_445_760,
            end_frame: 1_540_320,
        };
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 16].into() };
        let path = Path::new("/songs/Eye.wav");
        let sample =
            SampleRef::with_origin(model.handle(), path, vec![0], data, Some(origin.clone()));
        assert!(model.set_sample(id, sample));
        model.commit();
        (model, origin)
    }

    /// "open source in editor" is on a chop lane's menu and hands the
    /// window the lane's origin — a read, not a write.
    #[test]
    fn open_source_hands_the_window_the_lanes_origin() {
        let (model, origin) = model_with_a_chop_first_lane();
        let mut list = List::over(model);
        list.idle();

        list.right_click(list.names[0].center());
        let item = list.menu().open_source.expect("a chop lane offers its source");
        list.click(item.center());
        assert_eq!(list.opened, Some(origin));

        list.idle();
        assert!(!list.menu_open(), "the item closes the menu");
        assert_eq!(list.opened, None, "asked for once");
        assert!(list.model.lanes()[0].sample.is_some(), "nothing was written");
    }

    /// A lane that was not cut in the chop editor has no such item at all,
    /// loaded or empty.
    #[test]
    fn a_lane_without_an_origin_offers_no_open_source() {
        for model in [Model::new(), model_with_a_loaded_first_lane()] {
            let mut list = List::over(model);
            list.idle();
            list.right_click(list.names[0].center());
            assert_eq!(list.menu().open_source, None);
            assert_eq!(list.opened, None);
        }
    }

    /// "re-render" is on a Stretch lane's menu only, and its click asks for a
    /// fresh render — a new generation, pending, no error — as no undo step;
    /// the row's sample column then says "rendering…".
    #[test]
    fn rerender_asks_a_stretch_lane_for_a_fresh_render() {
        let mut model = model_with_a_loaded_first_lane();
        let id = model.lanes()[0].id;
        let mut list = List::over(model_with_a_loaded_first_lane());
        list.idle();
        list.right_click(list.names[0].center());
        assert_eq!(list.menu().rerender, None, "an Off lane has nothing to render");

        assert!(model.set_time_mode(id, TimeMode::Stretch));
        model.commit();
        let generation = model.lanes()[0].render_generation;
        assert!(model.set_render_error(id, generation, "stretch failed"));
        let depth = model.undo_depth();
        let mut list = List::over(model);
        list.idle();
        let hover = sample_hover(&list.model.lanes()[0]);
        assert_eq!(hover.lines().nth(1), Some(status::RENDER_FAILED_HOVER));

        list.right_click(list.names[0].center());
        let item = list.menu().rerender.expect("a Stretch lane offers a re-render");
        list.click(item.center());
        let lane = &list.model.lanes()[0];
        assert!(lane.render_generation > generation, "a new generation");
        assert!(lane.render_pending);
        assert_eq!(lane.render_error, None);
        assert_eq!(list.model.undo_depth(), depth, "a re-render is not an undo step");
        assert_eq!(status::sample_status(lane), status::SampleStatus::Rendering);
        list.idle();
        assert!(!list.menu_open(), "the item closes the menu");
    }

    /// The sample cell's hover names where a chop lane was cut from, before
    /// the browse hint; any other lane's hover has no origin line.
    #[test]
    fn the_sample_hover_shows_a_chop_lanes_origin() {
        let (model, origin) = model_with_a_chop_first_lane();
        let expected = crate::chop::format_origin(&origin, 48_000, "Eye.wav");
        assert_eq!(
            sample_hover(&model.lanes()[0]),
            format!("{expected}\n{BROWSE_HOVER}")
        );
        assert_eq!(sample_hover(&model.lanes()[1]), BROWSE_HOVER);
        let plain = model_with_a_loaded_first_lane();
        assert_eq!(sample_hover(&plain.lanes()[0]), BROWSE_HOVER);
    }

    // ---- The browser's way in --------------------------------------------

    #[test]
    fn a_click_on_a_sample_cell_asks_to_browse_for_that_lane() {
        let mut list = List::new();
        list.frame(Vec::new());
        let third = list.model.lanes()[2].id;
        let at = list.samples[2].center();
        list.frame(vec![egui::Event::PointerMoved(at), pointer(at, true)]);
        list.frame(vec![pointer(at, false)]);
        assert_eq!(list.browse, Some(third));
        list.frame(Vec::new());
        assert_eq!(list.browse, None, "asked once, on the click");
    }

    #[test]
    fn browse_in_a_lane_menu_asks_to_browse_for_that_lane() {
        let mut list = List::new();
        list.frame(Vec::new());
        let second = list.model.lanes()[1].id;
        list.right_click(list.names[1].center());
        let item = list.menu().browse.center();
        list.frame(vec![egui::Event::PointerMoved(item), pointer(item, true)]);
        list.frame(vec![pointer(item, false)]);
        assert_eq!(list.browse, Some(second));
    }

    #[test]
    fn the_lane_the_browser_loads_into_is_outlined() {
        let mut list = List::new();
        list.frame(Vec::new());
        list.target = Some(list.model.lanes()[4].id);
        list.frame(Vec::new());
        assert_eq!(list.outlined, [list.samples[4]]);
    }

    /// "give each its own output" on a three-lane selection gives the three
    /// Out 2–4 (outputs 1–3), in display order, as one undo step, and leaves
    /// the unselected lanes on the main pair.
    #[test]
    fn give_each_its_own_output_spreads_the_selection_over_out_2_to_4() {
        let mut list = List::new();
        list.idle();
        list.click(list.names[1].center());
        list.click_with(list.names[2].center(), ctrl());
        list.click_with(list.names[3].center(), ctrl());
        assert_eq!(list.selection.len(), 3);
        assert!(!list.model.can_undo(), "selecting is not a step");

        // On a selected row, so the whole selection moves.
        list.right_click(list.names[2].center());
        assert!(
            list.painted.iter().any(|text| text == menu::OWN_OUTPUTS_LABEL),
            "the menu has no {:?}; got {:?}",
            menu::OWN_OUTPUTS_LABEL,
            list.painted
        );
        list.click_menu(list.menu().own_outputs.center());

        let outputs: Vec<u8> = list.model.lanes().iter().map(|lane| lane.output).collect();
        let mut expected = vec![0; outputs.len()];
        expected[1..4].copy_from_slice(&[1, 2, 3]);
        assert_eq!(outputs, expected);
        assert_eq!(list.kept, 0, "every lane found an output");
        assert_eq!(list.drops.message(), None, "so the bar says nothing");
        assert!(!list.menu_open(), "the item closes the menu");

        assert!(list.model.undo());
        assert!(list.model.lanes().iter().all(|lane| lane.output == 0), "one undo step");
        assert!(!list.model.can_undo());
    }

    /// On a lane that is not selected, the item moves that lane alone and
    /// leaves the selection where it was.
    #[test]
    fn give_each_its_own_output_on_an_unselected_lane_moves_only_that_lane() {
        let mut list = List::new();
        list.idle();
        list.click(list.names[0].center());
        list.click_with(list.names[1].center(), ctrl());

        list.right_click(list.names[5].center());
        list.click_menu(list.menu().own_outputs.center());

        let outputs: Vec<u8> = list.model.lanes().iter().map(|lane| lane.output).collect();
        let mut expected = vec![0; outputs.len()];
        expected[5] = 1;
        assert_eq!(outputs, expected);
    }

    /// With no free output left, the lanes that found none keep theirs and
    /// the top bar's status slot says so for [`drops::MESSAGE_SHOWN_FOR`].
    #[test]
    fn give_each_its_own_output_with_no_free_output_says_so_for_a_while() {
        let mut model = Model::new();
        while model.lanes().len() < 17 {
            model.add_lane().expect("room for seventeen lanes");
        }
        // The last fifteen lanes take Out 2–16 between them.
        let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
        for (output, &id) in (1..=15).zip(&ids[2..]) {
            assert!(model.set_output(id, output));
        }
        model.commit();
        let mut list = List::over(model);
        list.idle();
        list.click(list.names[0].center());
        list.click_with(list.names[1].center(), ctrl());

        list.right_click(list.names[0].center());
        list.click_menu(list.menu().own_outputs.center());

        assert_eq!(list.model.lanes()[0].output, 0, "no free output: it kept its own");
        assert_eq!(list.model.lanes()[1].output, 0);
        let message = "2 lanes kept their output: no free outputs";
        assert_eq!(list.drops.message(), Some(message));
        assert_eq!(list.drops.message_hover(), Some(crate::editor::status::OUTPUTS_FULL_HOVER));

        // Still up a moment before its time, gone once it is up.
        let shown = list.ctx.input(|input| input.time);
        let span = drops::MESSAGE_SHOWN_FOR.as_secs_f64();
        list.frame_of(egui::RawInput { time: Some(shown + span - 0.5), ..list.window() });
        assert_eq!(list.drops.message(), Some(message));
        list.frame_of(egui::RawInput { time: Some(shown + span), ..list.window() });
        assert_eq!(list.drops.message(), None, "the message takes itself away");
    }

    fn ctrl() -> egui::Modifiers {
        egui::Modifiers { ctrl: true, command: true, ..egui::Modifiers::NONE }
    }

    fn shift() -> egui::Modifiers {
        egui::Modifiers { shift: true, ..egui::Modifiers::NONE }
    }

    /// Where the gap between a row's note field and its name cell is: the
    /// row's own background.
    fn background(list: &List, row: usize) -> Pos2 {
        let (note, name) = (list.notes[row], list.names[row]);
        pos2((note.max.x + name.min.x) / 2.0, name.center().y)
    }

    #[test]
    fn clicking_a_rows_name_selects_that_lane_and_writes_nothing() {
        let mut list = List::new();
        list.idle();
        let before: Vec<String> = list.model.lanes().iter().map(|l| format!("{l:?}")).collect();
        let third = list.order()[2];
        list.click(list.names[2].center());
        assert_eq!(list.selection.ids(), &[third]);
        assert_eq!(list.selected, Some(third), "the window is told, to open the lane tab");
        let after: Vec<String> = list.model.lanes().iter().map(|l| format!("{l:?}")).collect();
        assert_eq!(after, before, "a selection is not a write");
        assert!(!list.model.can_undo(), "nor an undo step");
        assert_eq!(list.rename.editing(), None, "one click does not rename");

        list.click(list.names[5].center());
        assert_eq!(list.selection.ids(), &[list.order()[5]]);
    }

    #[test]
    fn clicking_a_rows_background_selects_that_lane() {
        let mut list = List::new();
        list.idle();
        list.click(background(&list, 4));
        assert_eq!(list.selection.ids(), &[list.order()[4]]);
        assert_eq!(list.selected, Some(list.order()[4]));
    }

    #[test]
    fn ctrl_toggles_and_shift_ranges_over_the_display_order() {
        let mut list = List::new();
        list.idle();
        let order = list.order();
        list.click(list.names[1].center());
        list.click_with(list.names[3].center(), ctrl());
        assert_eq!(list.selection.ids(), &[order[1], order[3]]);
        list.click_with(background(&list, 1), ctrl());
        assert_eq!(list.selection.ids(), &[order[3]]);

        // Shift from the anchor (row 1, the last Ctrl+click) to row 5.
        list.click_with(list.names[5].center(), shift());
        assert_eq!(list.selection.ids(), &order[1..=5]);
        // And back up past the anchor.
        list.click_with(list.names[0].center(), shift());
        assert_eq!(list.selection.ids(), &order[0..=1]);
    }

    #[test]
    fn row_widgets_act_on_their_own_lane_and_leave_the_selection_alone() {
        let mut list = List::new();
        list.idle();
        let order = list.order();
        list.click(list.names[0].center());
        list.click_with(list.names[1].center(), shift());
        let selected = list.selection.clone();
        list.selected = None;

        // The mode toggle of an unselected row: that lane changes, alone.
        list.click(list.modes[4].center());
        assert_eq!(list.mode(4), LaneMode::Gate);
        assert_eq!(list.mode(0), LaneMode::OneShot);
        assert_eq!(list.mode(1), LaneMode::OneShot);
        assert_eq!(list.selection, selected, "a field's click is not a row click");
        assert_eq!(list.selected, None);

        // A selected row's own toggle: still only its lane.
        list.click(list.modes[0].center());
        assert_eq!(list.mode(0), LaneMode::Gate);
        assert_eq!(list.mode(1), LaneMode::OneShot);

        // The sample cell browses, and the play button and number fields
        // keep their clicks too.
        list.click(list.samples[6].center());
        assert_eq!(list.browse, Some(order[6]));
        list.click(list.auditions[6].center());
        list.click(list.volumes[6].center());
        list.escape();
        assert_eq!(list.selection, selected);
    }

    #[test]
    fn selected_rows_are_filled_under_their_fields() {
        let mut list = List::new();
        list.idle();
        // Row-sized fills only: the fields' own boxes are a control's grey.
        let filled = |list: &List| -> Vec<Rect> {
            list.fills
                .iter()
                .filter(|(rect, fill)| {
                    *fill == theme::SELECTED_ROW && list.rows.iter().any(|row| row.rect == *rect)
                })
                .map(|(rect, _)| *rect)
                .collect()
        };
        assert!(filled(&list).is_empty(), "nothing is selected yet");
        list.click(list.names[2].center());
        list.idle();
        assert_eq!(filled(&list), [list.rows[2].rect]);
        // Under the fields: painted before the row's first field box.
        let at = |rect: Rect| list.fills.iter().position(|(drawn, _)| *drawn == rect);
        assert!(at(list.rows[2].rect) < at(list.notes[2]), "the fill covers the fields");
    }

    #[test]
    fn a_deleted_lane_leaves_the_selection() {
        let mut list = List::new();
        list.idle();
        let order = list.order();
        list.click(list.names[2].center());
        list.click_with(list.names[3].center(), ctrl());
        list.model.delete_lane(order[3]);
        list.model.commit();
        list.idle();
        assert_eq!(list.selection.ids(), &[order[2]]);
        assert_eq!(list.selection.anchor(), None, "the anchor went with its lane");
    }

    #[test]
    fn row_marker_shows_a_synced_length_and_an_output_off_the_main_pair() {
        use crate::time::NoteLength;

        let mut lane = Lane::new(1, "Kick", 36, 0);
        assert_eq!(row_marker(&lane), "", "a fresh lane is marked with nothing");

        // A length with the time mode off plays as cut, so it is not shown.
        lane.length = Length::Beats(NoteLength::Bar);
        assert_eq!(row_marker(&lane), "");
        lane.time_mode = TimeMode::Repitch;
        assert_eq!(row_marker(&lane), "1 bar");
        lane.time_mode = TimeMode::Stretch;
        assert_eq!(row_marker(&lane), "1 bar");

        // "As cut" is no length to show, synced or not.
        lane.length = Length::AsCut;
        assert_eq!(row_marker(&lane), "");

        lane.time_mode = TimeMode::Off;
        lane.output = 4;
        assert_eq!(row_marker(&lane), "out 5", "Out numbers are 1-based");

        lane.time_mode = TimeMode::Stretch;
        lane.length = Length::Beats(NoteLength::EighthTriplet);
        lane.output = 2;
        assert_eq!(row_marker(&lane), "1/8t out 3");
    }

    #[test]
    fn a_marked_row_paints_its_marker_dim() {
        use crate::time::NoteLength;

        let mut model = Model::new();
        let id = model.lanes()[0].id;
        assert!(model.set_output(id, 4));
        assert!(model.set_time_mode(id, TimeMode::Repitch));
        assert!(model.set_length(id, Length::Beats(NoteLength::Bar)));

        let painted = painted_text(|ui| {
            lane_list(
                ui,
                &mut model,
                &mut Rename::default(),
                &mut Confirm::default(),
                &mut Selection::default(),
                &Lights::default(),
                None,
            );
        });

        let drawn = painted
            .iter()
            .find(|(text, _, _)| text == "1 bar out 5")
            .unwrap_or_else(|| panic!("no row drew its marker; got {painted:?}"));
        assert_eq!(drawn.1, theme::TEXT_DIM, "the marker is secondary text");
        assert!(drawn.2 > 0, "the marker laid out to no glyphs");
        assert_eq!(
            painted.iter().filter(|(text, _, _)| text.contains("out ")).count(),
            1,
            "only the marked lane carries a marker; got {painted:?}"
        );
    }
}
