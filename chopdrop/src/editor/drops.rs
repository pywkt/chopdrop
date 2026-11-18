//! What a drag-and-drop looks like by the time the editor sees it, where it
//! would land, and what landing there does.
//!
//! Files reach this plugin the long way round: Bitwig's browser hands X11 a
//! `text/uri-list`, the patched baseview turns that into paths
//! (`vendor/UPSTREAMING.md` §Patch 1), the patched egui-baseview puts
//! them into egui's `RawInput` as `hovered_files` while the drag is over the
//! window and `dropped_files` in the one frame it is let go — and forwards
//! the pointer position each drag event carried, which an XDND drag sends
//! nowhere else (§Patch 2). Everything above this module is therefore
//! vendored and only verifiable in Bitwig; this is the first layer that is
//! ours, and the layer the container can test.
//!
//! Four jobs, kept apart on purpose:
//!
//! - [`hovered_paths`] and [`dropped_paths`] are pure — slices of egui's
//!   types in, an ordered list of paths out — so the rules about what counts
//!   as a usable drop are unit-tested without a window.
//! - [`DropIntake`] is the once-a-frame reader the editor owns: it takes
//!   this frame's hover, this frame's pointer and — in the one frame it
//!   arrives — the drop itself.
//! - [`route`] is pure too, and is the decision: a pointer position and the
//!   rows' geometry in, the [`Landing`] a drop would have out. Both answers
//!   ask it, the outline drawn while the files are still in the air
//!   ([`DropIntake::outline`]) and the lanes they land in
//!   ([`DropIntake::land`]), so what the highlight promised and what the
//!   drop did cannot come apart.
//! - [`apply`] is the writer side: the first path onto the row that was
//!   outlined, the rest into lanes of their own named after their files, and
//!   every one of them handed to the background loader. It is also what
//!   counts the files a full list left no room for, which the intake turns
//!   into the message the top bar shows ([`DropIntake::message`]) — the
//!   one thing a drop does that is not about one lane.
//!
//! One more drag lands through the same [`route`] and outline: a selection
//! carried out of the chop strip ([`ChopPayload`], egui's own in-window drag
//! payload rather than an OS drop) replaces a row's sample or adds a lane
//! ([`land_chop`]).
//!
//! Nothing here touches the file system. A path that parses but points at
//! something unreadable or unsupported is the loader's business, which
//! keeps the decision above pure and the frame callback free of IO.

use std::path::{Path, PathBuf};
use std::time::Duration;

use egui::{Pos2, Rect};

use crate::chop;
use crate::editor::{status, theme};
use crate::loader::{Loader, Source};
use crate::model::Model;

/// How long the top bar shows what a drop left behind
/// ([`DropIntake::message`]).
///
/// Long enough to notice a line appear in a bar nobody was looking at and
/// then read it, short enough that it is gone before the next gesture — it
/// is the news of one drop, and it hides the project's size warning while
/// it is up ([`crate::editor::top_bar`]).
///
/// It goes away by itself rather than on the next click because a drop onto
/// a full list is usually followed by exactly the gestures that would
/// dismiss it: deleting a lane, scrolling the list, dropping the rest
/// somewhere else.
pub const MESSAGE_SHOWN_FOR: Duration = Duration::from_secs(8);

/// The paths of the files hovering over the window, in the order the drag
/// source listed them.
///
/// egui's `HoveredFile::path` is an `Option` because its web backend only
/// learns the mime type of what is being dragged, never a path. On X11 the
/// patch always sets it, so a `None` here means a hover worth no highlight
/// and is dropped rather than represented.
pub fn hovered_paths(files: &[egui::HoveredFile]) -> Vec<PathBuf> {
    files.iter().filter_map(|file| file.path.clone()).filter(|path| is_usable(path)).collect()
}

/// The paths of the files dropped on the window, in the order the drag source
/// listed them — which is the order the lanes they land in are added in
/// (the first path to the row under the pointer, the rest to new lanes).
///
/// egui hands a dropped file over as a handle rather than a path so that it
/// stays independent of file APIs; the handle's `bytes()` is deliberately
/// unused here, since reading a sample in the frame callback is exactly what
/// the loader exists to avoid.
pub fn dropped_paths(files: &[egui::DroppedFileHandle]) -> Vec<PathBuf> {
    files.iter().map(|file| file.path().to_path_buf()).filter(|path| is_usable(path)).collect()
}

/// Whether a path is worth passing on at all.
///
/// The one thing that can be judged without touching the disk: a drag source
/// that sends a line parsing to nothing leaves an empty path, and a lane
/// pointed at `""` could only ever show an error. Anything else is let
/// through — the drop is refused only when *every* path fails this
/// (`vendor/UPSTREAMING.md` §Patch 1).
fn is_usable(path: &std::path::Path) -> bool {
    !path.as_os_str().is_empty()
}

/// A drop as the one frame it arrived in found it: the files, and where the
/// pointer was holding them.
///
/// Taken out of the intake once ([`DropIntake::take_drop`]) and answered in
/// that same frame, so there is no state here to go stale and no drop to be
/// answered twice.
///
/// Named for the files and not for the gesture, because `Drop` on its own is
/// the standard library's trait and a field typed with it would read as one.
#[derive(Debug, Default, PartialEq)]
pub struct DroppedFiles {
    /// The files dropped, in the drag source's own order: the first lands on
    /// the row under the pointer and the rest add lanes.
    pub paths: Vec<PathBuf>,

    /// Where the pointer was when they landed, in egui's points.
    ///
    /// `None` for a drop egui could place nowhere at all, which [`route`]
    /// reads as the space below the list: a drop whose target is unknown may
    /// add lanes, never overwrite one.
    pub pointer: Option<Pos2>,
}

/// The editor's view of the drag going on over its window.
///
/// Lives in the editor struct and is reset with it on each opened window,
/// like everything else that belongs to one window rather than to the
/// instance (nice-plug-egui 0.6 never calls `editor_closed`, so
/// `NiceEguiApp::build` is where that happens).
#[derive(Default)]
pub struct DropIntake {
    /// This frame's hover, for the landing-target highlight
    /// ([`DropIntake::outline`]). Empty whenever nothing is being dragged
    /// over the window.
    hovering: Vec<PathBuf>,

    /// Where egui last saw the pointer — during a drag, the position the
    /// patched egui-baseview took off the X11 drag event
    /// (`vendor/UPSTREAMING.md` §Patch 2), since a dragged file sends
    /// the window no pointer motion of its own.
    pointer: Option<Pos2>,

    /// The drop this frame carried, until the frame takes it
    /// ([`DropIntake::land`]).
    ///
    /// egui clears `dropped_files` after the frame it arrives in, so the
    /// paths have to be held somewhere for the length of a frame; they are
    /// not held for longer than that, because the answer to a drop — lanes
    /// added, jobs queued — is made in the frame it lands in.
    dropped: Option<DroppedFiles>,

    /// What the last drop had to leave behind, until it has been on screen
    /// for [`MESSAGE_SHOWN_FOR`] ([`DropIntake::message`]).
    ///
    /// The one thing in this struct that outlives the frame it was made in,
    /// and the only state the window keeps about a gesture that is over: a
    /// drop onto a full list is the one gesture in the window that cannot
    /// do all of what it was asked, and the lanes it could not make are
    /// exactly the ones with no row to say so on.
    message: Option<Message>,
}

/// A message the top bar is showing for a while, and the time it stops.
///
/// Both halves in egui's own seconds-since-the-window-opened clock
/// (`input.time`), as the activity lights use ([`crate::editor::activity`]):
/// the editor's per-window state is thrown away with the window, so a clock
/// that starts again with it is the one clock nothing here can read
/// backwards.
#[derive(Debug)]
struct Message {
    /// The line the bar draws ([`status::drop_overflow`],
    /// [`status::outputs_kept`]).
    text: String,
    /// What the line says when the pointer rests on it.
    hover: &'static str,
    /// The frame time it is drawn for the last time at.
    until: f64,
}

impl DropIntake {
    /// Takes this frame's drag state out of egui's input. Called once per
    /// frame, before anything draws, so the hover highlight and the drop both
    /// act on the same frame they arrive in.
    pub fn read_frame(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|input| {
            self.hovering.clear();
            self.hovering.extend(hovered_paths(&input.raw.hovered_files));
            self.pointer = input.pointer.latest_pos();

            // Only a frame that carried files has a drop in it: the frames
            // in between have an empty `dropped_files` and must not wipe a
            // drop this frame's own `land` has yet to answer.
            let paths = dropped_paths(&input.raw.dropped_files);
            if !paths.is_empty() {
                self.dropped = Some(DroppedFiles { paths, pointer: self.pointer });
            }

            input.time
        });

        // And the message the last drop left, while it is still worth
        // saying. Outside the closure above, because asking for a frame
        // takes a lock of the context egui is holding input's for.
        //
        // The frame is asked for because nothing else will ask: a message
        // that appeared with no pointer in the window would otherwise stay
        // there until something moved, and the bar would be telling the
        // user about a drop made minutes ago.
        self.expire_message(now);
        if let Some(message) = &self.message {
            ctx.request_repaint_after(Duration::from_secs_f64((message.until - now).max(0.0)));
        }
    }

    /// Drops the message once it has had its [`MESSAGE_SHOWN_FOR`] on
    /// screen.
    ///
    /// `>=` rather than `>`, so the frame asked for by [`read_frame`] is the
    /// frame it goes on; a `now` that has gone backwards (a window's clock
    /// starting again under a message kept from the last one, which
    /// [`crate::editor::ChopdropEditor`]'s `build` makes impossible) would
    /// leave it up for its whole span again rather than for ever.
    fn expire_message(&mut self, now: f64) {
        if self.message.as_ref().is_some_and(|message| now >= message.until) {
            self.message = None;
        }
    }

    /// What the last drop has to say about itself, for the top bar to draw
    /// — `None` in every frame but the few after a drop that could not have
    /// all the lanes it asked for.
    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|message| message.text.as_str())
    }

    /// The hover behind [`DropIntake::message`], while there is one.
    pub fn message_hover(&self) -> Option<&'static str> {
        self.message.as_ref().map(|message| message.hover)
    }

    /// Puts `text` in the top bar's status slot for [`MESSAGE_SHOWN_FOR`],
    /// with `hover` behind it, replacing whatever was there.
    ///
    /// The drop's own message comes through here ([`DropIntake::land`]), and
    /// so does the one other gesture that can fall short with no row to say
    /// so on: "give each its own output" leaving lanes where they were
    /// ([`status::outputs_kept`]). A repaint is asked for because the bar of
    /// this frame has already been drawn.
    pub fn show_message(&mut self, ctx: &egui::Context, text: String, hover: &'static str) {
        let until = ctx.input(|input| input.time) + MESSAGE_SHOWN_FOR.as_secs_f64();
        self.message = Some(Message { text, hover, until });
        ctx.request_repaint();
    }

    /// The drop this frame carried, taken out of the intake — `None` in
    /// every frame but the one the files land in.
    ///
    /// Taken rather than read, because a drop is answered exactly once: a
    /// drop left behind would add its lanes again on the next frame, and
    /// every frame after that.
    pub fn take_drop(&mut self) -> Option<DroppedFiles> {
        self.dropped.take()
    }

    /// Outlines what the files hovering over the window would land on, and
    /// paints nothing when nothing is being dragged.
    ///
    /// Called after the list has been drawn and with the list's own
    /// `painter`, so the outline goes over the row's fields rather than
    /// under them, and clipped to the list, so a row half scrolled out of
    /// the viewport is outlined only as far as it is shown.
    ///
    /// `rows` is where the rows were drawn, in display order, and `list` the
    /// rectangle holding them and the space below them
    /// ([`crate::editor::lanes::LaneList`]); `strip` is where the chop
    /// strip was drawn ([`crate::editor::chop_strip::StripOutput::rect`]).
    pub fn outline(&self, ui: &egui::Ui, rows: &[LaneRect], list: Rect, strip: Rect) {
        if self.hovering.is_empty() {
            return;
        }

        // Clipped to whichever region the landing is in: the strip lies
        // outside the list, and a row's outline must still stop at the
        // list's edge.
        let landing = route(self.pointer, rows, list, strip);
        let clip = if matches!(landing, Landing::Strip { .. }) { strip } else { list };
        paint_outline(ui, landing, clip);
    }

    /// Lands the drop this frame carried, if it carried one: routes it
    /// against the rows this frame drew and hands it to the model and the
    /// loader ([`apply`]).
    ///
    /// The repaint is asked for because nothing else will: the lanes a drop
    /// made are not in the frame being drawn — the rows were read before the
    /// writer ran — and the drag that brought them is over, so there is no
    /// further input coming to draw them.
    ///
    /// The same is true of the message a truncated drop leaves
    /// ([`DropIntake::message`]): the bar was drawn before the list this is
    /// routed against, so the line appears in the frame asked for here and
    /// not in this one.
    pub fn land(
        &mut self,
        ui: &egui::Ui,
        model: &mut Model,
        loader: &Loader,
        rows: &[LaneRect],
        list: Rect,
        strip: Rect,
    ) {
        let Some(drop) = self.take_drop() else {
            return;
        };

        let landing = route(drop.pointer, rows, list, strip);
        let missing = apply(model, loader, landing, &drop.paths);
        match status::drop_overflow(missing) {
            Some(text) => self.show_message(ui.ctx(), text, status::DROP_FULL_HOVER),
            None => self.message = None,
        }

        ui.ctx().request_repaint();
    }
}

/// Strokes `landing`'s outline in the drop-target accent, clipped to `clip`.
fn paint_outline(ui: &egui::Ui, landing: Landing, clip: Rect) {
    ui.painter().with_clip_rect(clip).rect_stroke(
        landing.outline(),
        theme::CONTROL_CORNER,
        theme::drop_target_stroke(),
        egui::StrokeKind::Inside,
    );
}

/// The chop strip's selection on its way to a lane: the frames of the open
/// source it was dragged out with, `[start, end)`.
///
/// It travels as egui's own drag payload ([`egui::DragAndDrop`]) rather
/// than through [`DropIntake`], because it is a drag inside the window: OS
/// drops come the long way round, through the patched baseview, and this
/// never leaves egui. Set by the strip's press inside the selection
/// ([`crate::editor::chop_strip`]), read here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChopPayload {
    pub start: u64,
    /// Exclusive.
    pub end: u64,
}

/// The chop being dragged over the window this frame, if one is — the
/// frame it is let go included.
pub fn chop_dragged(ctx: &egui::Context) -> Option<ChopPayload> {
    egui::DragAndDrop::payload::<ChopPayload>(ctx).map(|payload| *payload)
}

/// Where a chop let go at `pointer` lands: `None` — nothing happens — over
/// the strip it came from, else [`route`]'s answer with no strip to offer
/// (a row replaces its sample, anywhere else adds a lane).
pub fn chop_landing(
    pointer: Option<Pos2>,
    rows: &[LaneRect],
    list: Rect,
    strip: Rect,
) -> Option<Landing> {
    let pointer = pointer.filter(|pointer| pointer.is_finite());
    if pointer.is_some_and(|pointer| strip.contains(pointer)) {
        return None;
    }
    Some(route(pointer, rows, list, Rect::NOTHING))
}

/// Outlines what the chop being dragged would land on, as a file drag's
/// is ([`DropIntake::outline`]), and paints nothing when no chop is in the
/// air or the pointer is still over the strip.
pub fn outline_chop(ui: &egui::Ui, rows: &[LaneRect], list: Rect, strip: Rect) {
    if chop_dragged(ui.ctx()).is_none() {
        return;
    }
    let pointer = ui.input(|input| input.pointer.latest_pos());
    if let Some(landing) = chop_landing(pointer, rows, list, strip) {
        paint_outline(ui, landing, list);
    }
}

/// Lands the chop let go this frame, if one was: takes egui's payload,
/// routes it ([`chop_landing`]) against the rows this frame drew and writes
/// it ([`apply_chop`]). Answers the lane written, `None` when nothing was
/// let go, it was let go over the strip, or the model refused it.
pub fn land_chop(
    ui: &egui::Ui,
    model: &mut Model,
    rows: &[LaneRect],
    list: Rect,
    strip: Rect,
) -> Option<u32> {
    let (released, pointer) =
        ui.input(|input| (input.pointer.any_released(), input.pointer.latest_pos()));
    if !released {
        return None;
    }
    let payload = *egui::DragAndDrop::take_payload::<ChopPayload>(ui.ctx())?;
    let landing = chop_landing(pointer, rows, list, strip)?;
    let landed = apply_chop(model, landing, payload);
    // The rows of this frame were read before the write: draw the next.
    ui.ctx().request_repaint();
    landed
}

/// Writes a chop to where it landed: onto a row, the lane's sample is
/// replaced by the cut ([`Model::replace_with_chop`] — name, note, colour
/// and settings kept); anywhere else it is a new lane
/// ([`Model::add_chop_lane`]). One undo step when anything changed. `None`
/// — nothing changed — for no source open, an empty cut, a full list or a
/// lane gone; a [`Landing::Strip`] adds nothing.
pub fn apply_chop(model: &mut Model, landing: Landing, payload: ChopPayload) -> Option<u32> {
    let selection =
        chop::Selection { start: payload.start, end: payload.end, active: chop::Edge::End };
    let landed = match landing {
        Landing::Row { id, .. } => model.replace_with_chop(id, selection).then_some(id),
        Landing::NewLanes { .. } => model.add_chop_lane(selection),
        Landing::Strip { .. } => None,
    };
    if landed.is_some() {
        model.commit();
    }
    landed
}

/// One lane row's place on screen, which is all [`route`] needs of it: the
/// lane it is of, and where it was drawn.
///
/// A type of its own rather than [`crate::editor::lanes::LaneRow`], which
/// carries a frame's worth of egui responses with it
/// ([`LaneRow::lane_rect`](crate::editor::lanes::LaneRow::lane_rect) makes
/// one): a rule stated over geometry alone is a rule a test can state back.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LaneRect {
    /// The lane the row is of.
    pub id: u32,
    /// Where the row was drawn.
    pub rect: Rect,
}

/// Where the files a drag is carrying would land.
///
/// Three targets: a row, the space after the last of them, or the chop strip.
/// Each carries the rectangle to outline while the drag is still going,
/// because the promise the highlight makes and the lane the drop writes have
/// to be the one decision ([`route`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Landing {
    /// The row under the pointer: the first path replaces this lane's
    /// sample — the lane keeps its name and everything else about it — and
    /// any paths after it add lanes.
    Row {
        /// The lane, by [`crate::model::Lane::id`] and never by index: the
        /// list can be reordered between the drop and the decode.
        id: u32,
        /// The row as the list drew it.
        outline: Rect,
    },

    /// The space below the last row — and anywhere over the window that is
    /// not a row at all: every path adds a lane of its own.
    NewLanes {
        /// What is left of the list under the last row, and the whole list
        /// when there are no rows, which is what an instance with every lane
        /// deleted offers.
        outline: Rect,
    },

    /// The chop editor strip: the first path opens there as the source
    /// ([`Loader::open_source`]); the strip holds one source, so any paths
    /// after it are not used.
    Strip {
        /// The strip as it was drawn, header included.
        outline: Rect,
    },
}

impl Landing {
    /// The rectangle to outline while the files are still in the air.
    pub fn outline(&self) -> Rect {
        match *self {
            Landing::Row { outline, .. }
            | Landing::NewLanes { outline }
            | Landing::Strip { outline } => outline,
        }
    }
}

/// Where a drop at `pointer` would land, over a list whose rows are at
/// `rows` (display order, top to bottom) inside `list`, with the chop strip
/// at `strip` (`Rect::NOTHING` for a caller with no strip to offer, such as
/// a chop dragged out of it).
///
/// The pure routing decision for a drop, and the whole of it: a point and
/// some rectangles in, a target out, no egui context and no model.
///
/// The rule is that the rows divide the list's height between them, so that
/// every point of it belongs to something: a pointer is on the first row
/// whose bottom edge it has not passed — which gives the gap between two
/// rows to the lower of them — and anything past the last row's bottom edge
/// adds lanes. Nothing lands in a crack.
///
/// Three ways to miss the rows entirely, all of which add lanes rather than
/// overwrite one: a pointer egui could not place (`None`), a position that
/// is not a number, and a point outside the list — the top bar, say, where a
/// drop is not aimed at any row and the files are still worth keeping.
///
/// A finite pointer inside `strip` opens its file there instead; the strip
/// sits outside the list, so the two never compete for a point.
pub fn route(pointer: Option<Pos2>, rows: &[LaneRect], list: Rect, strip: Rect) -> Landing {
    let pointer = pointer.filter(|pointer| pointer.is_finite());
    if pointer.is_some_and(|pointer| strip.contains(pointer)) {
        return Landing::Strip { outline: strip };
    }

    let over_a_row = pointer
        .filter(|pointer| list.contains(*pointer))
        .and_then(|pointer| rows.iter().find(|row| pointer.y <= row.rect.bottom()));

    match over_a_row {
        Some(row) => Landing::Row { id: row.id, outline: row.rect },
        None => Landing::NewLanes { outline: new_lane_space(rows, list) },
    }
}

/// The space a drop below the list would fill: the list under its last row,
/// as wide as the rows are.
///
/// As wide as the rows and not as the list, because the list gives up six
/// points to the scroll bar once it is long enough to need one and an
/// outline that ignored that would be the one thing in the window out of
/// line with the rows.
fn new_lane_space(rows: &[LaneRect], list: Rect) -> Rect {
    let Some(last) = rows.last() else {
        return list;
    };

    // Held inside the list at both ends, because a scrolled list has rows
    // outside it: scrolled down, the last row is below the viewport, and
    // `max` before `min` is the clamp order a value that is not a number
    // survives.
    let top = (last.rect.bottom() + theme::ROW_GAP).max(list.top()).min(list.bottom());
    Rect::from_x_y_ranges(last.rect.x_range(), top..=list.bottom())
}

/// The name a lane a drop creates is given: the file's own, without its
/// extension.
///
/// `None` for a path with no file name to take, which leaves the lane the
/// "Lane N" of the model's own progression ([`Model::add_lane`]) — a lane
/// named after nothing would be worse than one named in sequence.
///
/// The extension goes because the sample column says it already, in the
/// field next to the name: "BD 606" and "BD 606.wav" side by side is one
/// thing said twice across the narrowest two fields in the row. The length
/// is [`Model::rename`]'s business, as it is for a name a user types.
pub fn lane_name(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_string_lossy();
    let name = stem.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Hands a drop to the model and the loader: the lanes it asks for, and a
/// job per file.
///
/// `model` is the caller's own writer guard and `loader` the instance's, so
/// a drop of eight files is one lock, one publish and one undo step — the
/// lanes added, their names, and the "loading…" each queued job leaves on
/// its lane ([`Loader::queue`]).
///
/// What a path gets here is a job and not a sample: the file is read and
/// decoded on the loader's thread, which writes the result into the lane
/// itself. So nothing in this function can fail in a way a user sees, and
/// nothing in it touches the disk — a file that cannot be read or decoded
/// ends as a message on its own lane, and the lane is there either way.
///
/// The commit is the whole drop's, which is what makes eight files one undo
/// step rather than eight. Each sample arriving later is a step of its own,
/// because by then the lanes exist for it to land on (`Loader`'s own
/// `deliver` commits a [`Source::File`] result).
///
/// Answers how many of `paths` found no lane to land in, which is anything
/// but zero only for a drop that reached [`crate::model::MAX_LANES`]: the
/// count the window says out loud ([`status::drop_overflow`]), counted here
/// because this is the only place that knows it — the model refuses a lane
/// past the cap silently and a count worked out from the list's length
/// afterwards would have to guess at whether the drop was aimed at a row.
///
/// A drop on the strip adds no lanes and is no undo step: its first path is
/// opened as the chop editor's source and the rest are left alone.
pub fn apply(model: &mut Model, loader: &Loader, landing: Landing, paths: &[PathBuf]) -> usize {
    if let Landing::Strip { .. } = landing {
        if let Some(first) = paths.first() {
            loader.open_source(model, first);
        }
        return 0;
    }

    let mut rest = paths;

    // The row that was outlined keeps its name, its note, its colour and
    // everything else: a drop on a row replaces the sample and nothing more.
    // It needs no lane of its own, so it is the one path a full list can
    // still take.
    if let Landing::Row { id, .. } = landing
        && let Some((first, tail)) = paths.split_first()
    {
        loader.queue(model, id, Source::File(first.clone()));
        rest = tail;
    }

    let mut missing = 0;
    for (index, path) in rest.iter().enumerate() {
        // Out of lanes. `MAX_LANES` is every note a note source can send and
        // the model refuses past it silently, so the files that fit have
        // landed and the rest — this one and everything after it — go no
        // further than the count below.
        let Some(id) = model.add_lane() else {
            missing = rest.len() - index;
            break;
        };
        if let Some(name) = lane_name(path) {
            model.rename(id, &name);
        }
        loader.queue(model, id, Source::File(path.clone()));
    }

    model.commit();
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use egui::{Rect, pos2, vec2};

    use crate::model::{DEFAULT_LANE_COUNT, MAX_LANES, SharedModel, lock_model};

    /// A dropped file as the vendored egui-baseview makes one: a path, and a
    /// `bytes()` nothing in the editor calls.
    #[derive(Debug)]
    struct TestFile {
        path: PathBuf,
    }

    impl egui::DroppedFile for TestFile {
        fn path(&self) -> &Path {
            &self.path
        }

        fn bytes(&self) -> Result<Vec<u8>, String> {
            Err("a test file has no bytes".to_owned())
        }
    }

    /// `paths` as egui would hand them to a frame it was dropped on.
    fn dropped(paths: &[&str]) -> Vec<egui::DroppedFileHandle> {
        paths
            .iter()
            .map(|path| {
                Arc::new(TestFile { path: PathBuf::from(path) }) as egui::DroppedFileHandle
            })
            .collect()
    }

    /// `paths` as egui would hand them to a frame they are being dragged
    /// over; `None` stands for a hover with no path behind it.
    fn hovered(paths: &[Option<&str>]) -> Vec<egui::HoveredFile> {
        paths
            .iter()
            .map(|path| egui::HoveredFile { path: path.map(PathBuf::from), mime: String::new() })
            .collect()
    }

    /// The order is the drag source's, because the first path goes to the row
    /// under the pointer and the rest add lanes after it.
    #[test]
    fn dropped_files_keep_the_order_they_arrived_in() {
        let files = dropped(&["/tmp/kick.wav", "/tmp/snare.wav", "/tmp/hat.wav"]);

        assert_eq!(
            dropped_paths(&files),
            vec![
                PathBuf::from("/tmp/kick.wav"),
                PathBuf::from("/tmp/snare.wav"),
                PathBuf::from("/tmp/hat.wav"),
            ]
        );
    }

    /// The same file twice is two paths: a drag source that lists it twice is
    /// asking for two lanes, and deciding otherwise is the routing step's
    /// business, not this one's.
    #[test]
    fn the_same_path_twice_stays_twice() {
        let files = dropped(&["/tmp/kick.wav", "/tmp/kick.wav"]);

        assert_eq!(dropped_paths(&files).len(), 2);
    }

    /// Nothing is resolved on disk — none of these paths exist, and a
    /// directory or a `.txt` is passed on for the loader to reject with a
    /// message the lane can show.
    #[test]
    fn paths_are_passed_on_unexamined() {
        let files = dropped(&["/nonexistent/kick.wav", "/tmp", "relative.wav", "/tmp/notes.txt"]);

        assert_eq!(dropped_paths(&files).len(), 4);
    }

    /// The one path a drop can carry that is worth nothing to any later step.
    #[test]
    fn empty_paths_are_skipped() {
        let files = dropped(&["", "/tmp/kick.wav", ""]);

        assert_eq!(dropped_paths(&files), vec![PathBuf::from("/tmp/kick.wav")]);
        assert!(dropped_paths(&dropped(&[""])).is_empty(), "nothing usable is no drop");
    }

    #[test]
    fn a_drop_with_no_files_is_no_paths() {
        assert!(dropped_paths(&[]).is_empty());
        assert!(hovered_paths(&[]).is_empty());
    }

    /// A hover egui knows no path for highlights nothing, so it is not
    /// counted among what is about to land.
    #[test]
    fn hovers_without_a_path_are_skipped() {
        let files = hovered(&[None, Some("/tmp/kick.wav"), Some(""), Some("/tmp/snare.wav")]);

        assert_eq!(
            hovered_paths(&files),
            vec![PathBuf::from("/tmp/kick.wav"), PathBuf::from("/tmp/snare.wav")]
        );
    }

    /// One frame of egui over `ctx`, with whatever drag state the test is
    /// about, read exactly where the editor reads it: inside the pass.
    /// Display-free — egui lays a frame out without a window.
    fn frame(ctx: &egui::Context, intake: &mut DropIntake, input: egui::RawInput) {
        ctx.run_ui(input, |ui| intake.read_frame(ui.ctx())).drop_without_applying_deltas();
    }

    /// The hover is this frame's and nothing more: it appears while the files
    /// are over the window and is gone the frame the drag leaves.
    #[test]
    fn the_hover_lasts_exactly_as_long_as_the_drag() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        assert!(intake.hovering.is_empty(), "nothing is being dragged yet");

        frame(
            &ctx,
            &mut intake,
            egui::RawInput {
                hovered_files: hovered(&[Some("/tmp/kick.wav")]),
                ..Default::default()
            },
        );
        assert_eq!(intake.hovering, [PathBuf::from("/tmp/kick.wav")]);

        // The drag left: egui clears `hovered_files`, and so does the intake.
        frame(&ctx, &mut intake, egui::RawInput::default());
        assert!(intake.hovering.is_empty());
    }

    /// A drop is there for the frame it arrives in — long enough for that
    /// frame to answer it — and is taken out of the intake when it is, so
    /// the lanes it asks for are added once and not once a frame.
    #[test]
    fn a_drop_is_taken_once_in_the_frame_it_arrived_in() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        assert_eq!(intake.take_drop(), None, "the window has not been dropped on");

        frame(
            &ctx,
            &mut intake,
            egui::RawInput {
                dropped_files: dropped(&["/tmp/kick.wav", "/tmp/snare.wav"]),
                ..Default::default()
            },
        );
        let landed = vec![PathBuf::from("/tmp/kick.wav"), PathBuf::from("/tmp/snare.wav")];
        assert_eq!(intake.take_drop().expect("the files landed").paths, landed);
        assert_eq!(intake.take_drop(), None, "the drop was answered already");

        // Frames with no drop in them make none up.
        frame(&ctx, &mut intake, egui::RawInput::default());
        assert_eq!(intake.take_drop(), None);

        // And the next drop is a drop of its own.
        frame(
            &ctx,
            &mut intake,
            egui::RawInput { dropped_files: dropped(&["/tmp/hat.wav"]), ..Default::default() },
        );
        let second = intake.take_drop().expect("a second drop");
        assert_eq!(second.paths, [PathBuf::from("/tmp/hat.wav")]);
    }

    /// A drop that is not taken in its own frame survives the frames after
    /// it rather than being lost: `dropped_files` is cleared by egui after
    /// one frame, and a frame that merely redraws must not wipe a drop the
    /// editor has yet to answer.
    #[test]
    fn a_drop_nobody_took_is_not_wiped_by_the_next_frame() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();

        frame(
            &ctx,
            &mut intake,
            egui::RawInput { dropped_files: dropped(&["/tmp/kick.wav"]), ..Default::default() },
        );
        frame(&ctx, &mut intake, egui::RawInput::default());

        assert_eq!(intake.take_drop().expect("the drop is still there").paths.len(), 1);
    }

    /// The pointer position a drop is routed by is the one egui had when the
    /// files landed — which on X11 only exists because the patched
    /// egui-baseview forwards the drag's own position
    /// (`vendor/UPSTREAMING.md` §Patch 2).
    #[test]
    fn a_drop_carries_the_pointer_it_landed_under() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();

        let at = pos2(120.0, 64.0);
        frame(
            &ctx,
            &mut intake,
            egui::RawInput {
                events: vec![egui::Event::PointerMoved(at)],
                hovered_files: hovered(&[Some("/tmp/kick.wav")]),
                ..Default::default()
            },
        );
        assert_eq!(intake.pointer, Some(at), "the hover is placed too");

        let onto = pos2(120.0, 98.0);
        frame(
            &ctx,
            &mut intake,
            egui::RawInput {
                events: vec![egui::Event::PointerMoved(onto)],
                dropped_files: dropped(&["/tmp/kick.wav"]),
                ..Default::default()
            },
        );
        assert_eq!(intake.take_drop().expect("the drop").pointer, Some(onto));
    }

    /// The shape of a real drop: hovered for a few frames, then let go. The
    /// frame the files land in is also the frame the hover ends.
    #[test]
    fn a_drag_that_ends_in_a_drop_hands_over_the_same_paths() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        let files = [Some("/tmp/BD 606.wav")];

        for _ in 0..3 {
            frame(
                &ctx,
                &mut intake,
                egui::RawInput { hovered_files: hovered(&files), ..Default::default() },
            );
            assert_eq!(intake.hovering.len(), 1);
            assert_eq!(intake.take_drop(), None, "the drag is still going");
        }

        frame(
            &ctx,
            &mut intake,
            egui::RawInput { dropped_files: dropped(&["/tmp/BD 606.wav"]), ..Default::default() },
        );
        assert!(intake.hovering.is_empty(), "the drag is over");
        assert_eq!(
            intake.take_drop().expect("the files landed").paths,
            [PathBuf::from("/tmp/BD 606.wav")]
        );
    }
    // ---- routing ----

    /// A list the shape the real one is: `rows` rows at the row pitch,
    /// starting at the very top of the list — the panel's padding is outside
    /// it (`editor/lanes.rs`) — with a lane's worth of space left under the
    /// last of them.
    ///
    /// Made by hand rather than laid out by egui, because the point of
    /// [`route`] is that it is geometry and nothing else — the same rules are
    /// played over a real layout in `editor/lanes.rs`'s own tests.
    fn list_of(rows: usize) -> (Vec<LaneRect>, Rect) {
        let height = (rows + 1) as f32 * theme::ROW_PITCH;
        let list = Rect::from_min_size(pos2(10.0, 40.0), vec2(300.0, height));
        (rows_down(list, rows, 0.0), list)
    }

    /// `rows` rows down `list` from `offset` points below its top, as the
    /// list lays them out: the row height each, the row gap between them.
    ///
    /// A negative `offset` is a list scrolled down, whose first rows are
    /// above the viewport and whose last ones run past the bottom of it.
    fn rows_down(list: Rect, rows: usize, offset: f32) -> Vec<LaneRect> {
        (0..rows)
            .map(|row| LaneRect {
                // Ids that are not indices, so a test that confuses the two
                // says so.
                id: 100 + row as u32,
                rect: Rect::from_min_size(
                    pos2(list.left(), list.top() + offset + row as f32 * theme::ROW_PITCH),
                    vec2(list.width(), theme::ROW_HEIGHT),
                ),
            })
            .collect()
    }

    /// The pointer over a row's middle lands on that row — on its lane's
    /// id, not on its place in the list.
    #[test]
    fn a_drop_on_a_row_lands_on_that_rows_lane() {
        let (rows, list) = list_of(8);

        for row in &rows {
            assert_eq!(
                route(Some(row.rect.center()), &rows, list, Rect::NOTHING),
                Landing::Row { id: row.id, outline: row.rect },
                "the pointer was over lane {}",
                row.id
            );
        }
    }

    /// Every point of the list belongs to something: the gap between two
    /// rows goes to the row below it, and so does the padding above the
    /// first row — a drop cannot land in a crack.
    #[test]
    fn the_gaps_between_rows_belong_to_the_row_below() {
        let (rows, list) = list_of(4);

        assert_eq!(
            route(Some(pos2(list.center().x, list.top())), &rows, list, Rect::NOTHING),
            Landing::Row { id: rows[0].id, outline: rows[0].rect },
            "the first point of the list is the first row's"
        );

        for pair in rows.windows(2) {
            let gap = pos2(list.center().x, pair[0].rect.bottom() + theme::ROW_GAP / 2.0);
            assert_eq!(
                route(Some(gap), &rows, list, Rect::NOTHING),
                Landing::Row { id: pair[1].id, outline: pair[1].rect },
                "the gap under lane {} is lane {}'s",
                pair[0].id,
                pair[1].id
            );
        }

        // And the edges of a row are the row's, not its neighbours'.
        assert_eq!(
            route(Some(pos2(list.center().x, rows[2].rect.bottom())), &rows, list, Rect::NOTHING),
            Landing::Row { id: rows[2].id, outline: rows[2].rect }
        );
    }

    /// Below the last row, a drop adds lanes — and the outline it promises
    /// is that space: under the last row, as wide as the rows, down to the
    /// bottom of the list.
    #[test]
    fn a_drop_below_the_last_row_adds_lanes() {
        let (rows, list) = list_of(3);
        let last = rows.last().expect("three rows").rect;

        let below = pos2(list.center().x, last.bottom() + theme::ROW_PITCH);
        let Landing::NewLanes { outline } = route(Some(below), &rows, list, Rect::NOTHING) else {
            panic!("a drop under the last row adds lanes");
        };

        assert_eq!(outline.top(), last.bottom() + theme::ROW_GAP, "it starts under the last row");
        assert_eq!(outline.bottom(), list.bottom(), "and reaches the bottom of the list");
        assert_eq!(outline.x_range(), last.x_range(), "and lines up with the rows");
        assert_eq!(
            route(Some(pos2(list.center().x, list.bottom())), &rows, list, Rect::NOTHING),
            Landing::NewLanes { outline },
            "the last point of the list is still the space below"
        );
    }

    /// With no lanes at all the whole list is the landing space — the window
    /// a user who has deleted every lane is looking at.
    #[test]
    fn an_empty_list_is_all_landing_space() {
        let (rows, list) = list_of(0);

        assert_eq!(
            route(Some(list.center()), &rows, list, Rect::NOTHING),
            Landing::NewLanes { outline: list },
            "there is no row to land on"
        );
    }

    /// Three ways to be aimed at no row, all of which add lanes: a pointer
    /// egui could not place, one that is not a number, and one outside the
    /// list — over the top bar, say. None of them may overwrite a lane's
    /// sample, which is the one thing a drop can destroy.
    #[test]
    fn a_drop_aimed_at_no_row_adds_lanes_rather_than_overwriting_one() {
        let (rows, list) = list_of(8);
        let space = Landing::NewLanes { outline: new_lane_space(&rows, list) };

        assert_eq!(route(None, &rows, list, Rect::NOTHING), space, "a drop egui could not place");
        assert_eq!(
            route(Some(pos2(f32::NAN, f32::NAN)), &rows, list, Rect::NOTHING),
            space,
            "a position that is not a number"
        );
        assert_eq!(
            route(Some(pos2(list.center().x, list.top() - 20.0)), &rows, list, Rect::NOTHING),
            space,
            "a drop on the top bar, above the list"
        );
        assert_eq!(
            route(Some(pos2(list.right() + 20.0, list.center().y)), &rows, list, Rect::NOTHING),
            space,
            "a drop beside the list"
        );
    }

    /// A list scrolled so that its rows run past both edges of the viewport:
    /// the pointer still lands on the row it is over, and the landing space
    /// below stays inside the list however far the rows reach.
    #[test]
    fn a_scrolled_list_routes_by_what_is_on_screen() {
        // Twelve rows run well past a list this tall, and the scroll offset
        // puts the first of them above its top: the shape of a long list
        // scrolled into its middle.
        let list = Rect::from_min_size(pos2(10.0, 40.0), vec2(300.0, 200.0));
        let rows = rows_down(list, 12, -90.0);

        let under = rows
            .iter()
            .find(|row| row.rect.contains(list.center()))
            .expect("some row is under the middle of the viewport");
        assert_eq!(
            route(Some(list.center()), &rows, list, Rect::NOTHING),
            Landing::Row { id: under.id, outline: under.rect }
        );

        // The rows reach past the bottom of the list, so there is no space
        // under them: the outline collapses onto the bottom edge rather than
        // inverting or spilling out of the list.
        let outline = new_lane_space(&rows, list);
        assert!(list.contains_rect(outline), "the landing space left the list");
        assert_eq!(outline.bottom(), list.bottom());
        assert_eq!(outline.height(), 0.0, "a full list has no space under it");
    }

    /// The strip above the list, as the editor lays it out: full width,
    /// ending where the list begins.
    fn strip_over(list: Rect) -> Rect {
        Rect::from_min_max(pos2(0.0, 0.0), pos2(list.right() + 100.0, list.top()))
    }

    /// A pointer anywhere in the strip — header or body — lands on the
    /// strip, whatever the list holds.
    #[test]
    fn a_pointer_in_the_strip_lands_on_the_strip() {
        let (rows, list) = list_of(4);
        let strip = strip_over(list);

        for pointer in [strip.center(), strip.left_top(), pos2(strip.right() - 1.0, 5.0)] {
            assert_eq!(
                route(Some(pointer), &rows, list, strip),
                Landing::Strip { outline: strip },
                "{pointer:?}"
            );
        }
        assert_eq!(Landing::Strip { outline: strip }.outline(), strip);
        // A collapsed strip is its header, and is still a target.
        let header = Rect::from_min_size(Pos2::ZERO, vec2(400.0, 24.0));
        assert_eq!(
            route(Some(pos2(200.0, 12.0)), &[], list, header),
            Landing::Strip { outline: header }
        );
    }

    /// Offering the strip changes nothing for a pointer that is not in it:
    /// rows still replace, the space below still adds, and a pointer that is
    /// nowhere still adds rather than opening anything.
    #[test]
    fn the_strip_leaves_row_and_below_list_routing_alone() {
        let (rows, list) = list_of(4);
        let strip = strip_over(list);
        let space = new_lane_space(&rows, list);

        for row in &rows {
            assert_eq!(
                route(Some(row.rect.center()), &rows, list, strip),
                Landing::Row { id: row.id, outline: row.rect }
            );
        }
        let below = pos2(list.center().x, list.bottom() - 2.0);
        assert_eq!(route(Some(below), &rows, list, strip), Landing::NewLanes { outline: space });
        assert_eq!(route(None, &rows, list, strip), Landing::NewLanes { outline: space });
        assert_eq!(
            route(Some(pos2(f32::NAN, f32::NAN)), &rows, list, strip),
            Landing::NewLanes { outline: space },
            "a pointer that is not a number is in no strip"
        );
        // `Rect::NOTHING`, what a chop dragged out of the strip passes,
        // contains no point at all.
        assert_eq!(
            route(Some(strip.center()), &rows, list, Rect::NOTHING),
            Landing::NewLanes { outline: space }
        );
    }

    // ---- the lanes a drop makes ----

    /// A lane a drop creates is named after its file, without the extension
    /// the sample column shows anyway.
    #[test]
    fn a_dropped_file_names_its_lane() {
        assert_eq!(lane_name(Path::new("/home/dev/kit/BD 606.wav")).as_deref(), Some("BD 606"));
        assert_eq!(lane_name(Path::new("relative/clap.flac")).as_deref(), Some("clap"));
        assert_eq!(lane_name(Path::new("/tmp/no-extension")).as_deref(), Some("no-extension"));
        // A dotted name keeps every dot but the last: "kick.v2" is a name.
        assert_eq!(lane_name(Path::new("/tmp/kick.v2.wav")).as_deref(), Some("kick.v2"));
    }

    /// A path with no file name in it leaves the lane the name the model's
    /// own progression gave it.
    #[test]
    fn a_path_with_no_name_leaves_the_lanes_own() {
        assert_eq!(lane_name(Path::new("/")), None);
        assert_eq!(lane_name(Path::new("")), None);
        assert_eq!(lane_name(Path::new("/tmp/..")), None);
        assert_eq!(lane_name(Path::new("/tmp/   ")), None, "a name of spaces is no name");
    }

    /// A model behind the writer mutex and the loader that writes into it,
    /// as an instance has them — [`apply`] takes both.
    ///
    /// The loader's worker is real and its jobs fail, because the paths
    /// these tests drop do not exist: what is under test is the lanes and
    /// the jobs, which is this side of the loader's thread
    /// (`loader.rs` owns the decoding tests).
    fn instance() -> (SharedModel, Loader) {
        let model: SharedModel = Arc::new(Mutex::new(Model::new()));
        (Arc::clone(&model), Loader::new(model))
    }

    /// `paths` dropped on `landing`, answered as the editor's frame answers
    /// one: the writer lock held for the whole drop, and released before the
    /// loader is waited on (`loader.rs`: never drop a loader holding it).
    /// Answers [`apply`]'s own count of the files that found no lane.
    fn drop_onto(
        model: &SharedModel,
        loader: Loader,
        landing: Landing,
        paths: &[&str],
    ) -> usize {
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let missing = apply(&mut lock_model(model), &loader, landing, &paths);
        loader.finish();
        missing
    }

    /// Adds lanes until the list has `free` of its [`MAX_LANES`] left, as a
    /// project built up to the cap would arrive.
    ///
    /// Committed as one step, so an undo depth read after a drop is the
    /// drop's own.
    fn fill_to(model: &SharedModel, free: u32) {
        let mut model = lock_model(model);
        while (model.lanes().len() as u32) < MAX_LANES - free {
            assert!(model.add_lane().is_some(), "the list is not full yet");
        }
        model.commit();
        assert_eq!(model.lanes().len() as u32, MAX_LANES - free);
    }

    /// A drop on the strip opens its first file as the chop editor's source
    /// and nothing else: no lane added or changed, no undo step, the paths
    /// after the first unused.
    #[test]
    fn a_drop_on_the_strip_opens_the_first_path_as_the_source() {
        let (model, loader) = instance();
        let before: Vec<(u32, String)> = lock_model(&model)
            .lanes()
            .iter()
            .map(|lane| (lane.id, lane.name.clone()))
            .collect();

        let missing = {
            let paths =
                [PathBuf::from("/nonexistent/Song.wav"), PathBuf::from("/nonexistent/B.wav")];
            let mut held = lock_model(&model);
            let missing =
                apply(&mut held, &loader, Landing::Strip { outline: Rect::ZERO }, &paths);
            // Under the same guard: the strip shows "loading…" for it.
            assert!(held.chop().loading, "the source is loading");
            assert_eq!(held.chop().source_path.as_deref(), Some("/nonexistent/Song.wav"));
            missing
        };
        loader.finish();
        assert_eq!(missing, 0, "a drop on the strip leaves no file out of a lane");

        let model = lock_model(&model);
        let after: Vec<(u32, String)> =
            model.lanes().iter().map(|lane| (lane.id, lane.name.clone())).collect();
        assert_eq!(after, before, "no lane was added or renamed");
        assert!(model.lanes().iter().all(|lane| !lane.loading), "no lane is loading");
        assert!(!model.can_undo(), "opening a source is no undo step");
        let editor = model.chop();
        assert_eq!(editor.source_path.as_deref(), Some("/nonexistent/Song.wav"));
        assert!(!editor.loading, "the loader answered");
        assert!(editor.error.is_some(), "a missing file ends as the strip's message");
    }

    /// An empty drop on the strip opens nothing.
    #[test]
    fn an_empty_drop_on_the_strip_opens_nothing() {
        let (model, loader) = instance();
        assert_eq!(drop_onto(&model, loader, Landing::Strip { outline: Rect::ZERO }, &[]), 0);
        let model = lock_model(&model);
        assert_eq!(model.chop().source_path, None);
        assert!(!model.chop().loading);
    }

    /// The whole landing on a row: the lane keeps everything about itself
    /// but its sample, which the loader is now fetching, and the files after
    /// the first add lanes of their own: the first path replaces that lane's
    /// sample (name unchanged), the remaining paths add new lanes.
    #[test]
    fn a_drop_on_a_row_replaces_that_lanes_sample_and_adds_the_rest() {
        let (model, loader) = instance();
        let (id, name, note) = {
            let model = lock_model(&model);
            let lane = &model.lanes()[2];
            (lane.id, lane.name.clone(), lane.note)
        };

        drop_onto(
            &model,
            loader,
            Landing::Row { id, outline: Rect::ZERO },
            &["/nonexistent/BD 606.wav", "/nonexistent/SD 606.wav", "/nonexistent/CH.wav"],
        );

        let model = lock_model(&model);
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize + 2, "two lanes were added");

        let lane = model.lane(id).expect("the lane that was dropped on");
        assert_eq!(lane.name, name, "the row that was dropped on keeps its name");
        assert_eq!(lane.note, note, "and its note");
        assert_eq!(model.index_of(id), Some(2), "and its place in the list");

        let added: Vec<&str> =
            model.lanes()[DEFAULT_LANE_COUNT as usize..].iter().map(|l| l.name.as_str()).collect();
        assert_eq!(added, ["SD 606", "CH"], "the rest are named after their files, in order");
    }

    /// Every path of a drop below the list adds a lane, in the order the
    /// drag source listed them, each continuing the model's own note and
    /// colour progression — and the whole drop is one undo step.
    #[test]
    fn a_drop_below_the_list_adds_a_lane_per_file() {
        let (model, loader) = instance();
        let before = {
            let model = lock_model(&model);
            assert!(!model.can_undo(), "a fresh instance has nothing to undo");
            model.lanes().last().expect("eight lanes").note
        };

        drop_onto(
            &model,
            loader,
            Landing::NewLanes { outline: Rect::ZERO },
            &["/nonexistent/kick.wav", "/nonexistent/snare.aiff"],
        );

        let model = lock_model(&model);
        let added = &model.lanes()[DEFAULT_LANE_COUNT as usize..];
        assert_eq!(added.len(), 2);
        assert_eq!(added[0].name, "kick");
        assert_eq!(added[1].name, "snare");
        assert_eq!(added[0].note, before + 1, "the note progression carries on");
        assert_eq!(added[1].note, before + 2);
        assert_eq!(model.undo_depth(), 1, "a drop of two files is one undo step");
    }

    /// A drop that cannot have all the lanes it asks for takes what fits:
    /// `MAX_LANES` is refused silently by the model, and the files past it
    /// go no further.
    #[test]
    fn a_drop_past_the_lane_cap_lands_what_fits() {
        let (model, loader) = instance();
        fill_to(&model, 1);

        let missing = drop_onto(
            &model,
            loader,
            Landing::NewLanes { outline: Rect::ZERO },
            &["/nonexistent/first.wav", "/nonexistent/second.wav", "/nonexistent/third.wav"],
        );

        let model = lock_model(&model);
        assert_eq!(model.lanes().len() as u32, MAX_LANES, "the list is full and no fuller");
        assert_eq!(model.lanes().last().expect("a full list").name, "first");
        assert_eq!(missing, 2, "and the two that went nowhere are counted");
    }

    /// What [`apply`] answers at the cap, which is what the window says out
    /// loud: nothing while the files fit, and
    /// everything past the last free lane once they do not — including a
    /// drop that could place none of them at all.
    #[test]
    fn the_files_a_full_list_leaves_no_room_for_are_counted() {
        let three = ["/nonexistent/a.wav", "/nonexistent/b.wav", "/nonexistent/c.wav"];
        let below = Landing::NewLanes { outline: Rect::ZERO };

        // Fits exactly: three files into the three lanes left.
        let (model, loader) = instance();
        fill_to(&model, 3);
        assert_eq!(drop_onto(&model, loader, below, &three), 0, "they all found a lane");
        assert_eq!(lock_model(&model).lanes().len() as u32, MAX_LANES);

        // One over: the third file arrives at a full list.
        let (model, loader) = instance();
        fill_to(&model, 2);
        assert_eq!(drop_onto(&model, loader, below, &three), 1);
        assert_eq!(
            lock_model(&model).lanes().last().expect("a full list").name,
            "b",
            "and the one that was lost is the last of them, not the first"
        );

        // All over: a list with nothing free takes none of them.
        let (model, loader) = instance();
        fill_to(&model, 0);
        assert_eq!(drop_onto(&model, loader, below, &three), 3);
        assert_eq!(lock_model(&model).lanes().len() as u32, MAX_LANES, "and added no lane");
    }

    /// A drop aimed at a *row* needs no lane for its first file, so a full
    /// list still takes one file whole — which is the one way a drop onto a
    /// full list can lose nothing.
    #[test]
    fn the_first_file_of_a_drop_on_a_row_needs_no_lane_of_its_own() {
        let (model, loader) = instance();
        fill_to(&model, 0);
        let id = lock_model(&model).lanes()[0].id;
        let onto = Landing::Row { id, outline: Rect::ZERO };

        assert_eq!(
            drop_onto(&model, loader, onto, &["/nonexistent/one.wav"]),
            0,
            "the row it replaces is a lane that already exists"
        );

        let (model, loader) = instance();
        fill_to(&model, 0);
        let id = lock_model(&model).lanes()[0].id;
        assert_eq!(
            drop_onto(
                &model,
                loader,
                Landing::Row { id, outline: Rect::ZERO },
                &["/nonexistent/one.wav", "/nonexistent/two.wav"],
            ),
            1,
            "and only the file after it wanted one"
        );
    }

    /// Every lane a drop touches is marked as loading, which is what the row
    /// draws "loading…" from — checked with the drop's jobs still queued,
    /// since the results clear the flag.
    #[test]
    fn a_dropped_lane_is_loading_until_its_job_lands() {
        let (model, loader) = instance();
        let id = lock_model(&model).lanes()[0].id;

        {
            let mut model = lock_model(&model);
            apply(
                &mut model,
                &loader,
                Landing::Row { id, outline: Rect::ZERO },
                &[PathBuf::from("/nonexistent/kick.wav"), PathBuf::from("/nonexistent/clap.wav")],
            );

            assert!(model.lane(id).expect("the dropped-on lane").loading);
            let added = model.lanes().last().expect("the lane the second file made");
            assert!(added.loading, "a new lane waits for its sample too");
        }

        // And the loader clears it, failure or not: every one of these files
        // is missing, so each lane ends with a message instead.
        loader.finish();
        let model = lock_model(&model);
        for lane in model.lanes() {
            assert!(!lane.loading, "lane {} is still waiting", lane.id);
        }
        assert!(model.lane(id).expect("the dropped-on lane").error.is_some());
    }

    // ---- the whole path the editor's frame takes ----

    /// The rows of [`list_of`] renamed to the lanes an instance actually
    /// has, in display order.
    ///
    /// What the editor hands [`route`] comes out of the model — a row's id
    /// *is* a lane id (`editor/lanes.rs`) — so a test of the whole path has
    /// to say so, where one of the routing rule alone does not care.
    fn over_the_lanes(model: &SharedModel, rows: &mut [LaneRect]) {
        let model = lock_model(model);
        for (row, lane) in rows.iter_mut().zip(model.lanes()) {
            row.id = lane.id;
        }
    }

    /// A frame of the editor's own shape: the drag state read out of egui's
    /// input, then the drop landed against `rows` — what
    /// `ChopdropEditor::ui` does either side of drawing the list.
    ///
    /// The `Ui` is real but windowless, as every test in this crate's editor
    /// is: egui runs a pass, reads input and takes a repaint request without
    /// a display.
    fn editor_frame(
        ctx: &egui::Context,
        intake: &mut DropIntake,
        model: &SharedModel,
        loader: &Loader,
        rows: &[LaneRect],
        list: Rect,
        input: egui::RawInput,
    ) {
        editor_frame_delay(ctx, intake, model, loader, rows, list, input);
    }

    /// The same frame, answering how soon it asked to be drawn again — and
    /// drawn at whatever `input.time` says, which is egui's own clock and
    /// the one a message that goes away by itself is timed against
    /// ([`MESSAGE_SHOWN_FOR`]).
    ///
    /// The delay is what that message depends on: `Duration::MAX` is egui
    /// saying it will draw again only when something happens, which for a
    /// window nobody is touching is never.
    fn editor_frame_delay(
        ctx: &egui::Context,
        intake: &mut DropIntake,
        model: &SharedModel,
        loader: &Loader,
        rows: &[LaneRect],
        list: Rect,
        input: egui::RawInput,
    ) -> Duration {
        let output = ctx.run_ui(input, |ui| {
            intake.read_frame(ui.ctx());
            intake.land(ui, &mut lock_model(model), loader, rows, list, Rect::NOTHING);
        });
        let delay = output
            .viewport_output
            .values()
            .map(|viewport| viewport.repaint_delay)
            .min()
            .expect("a frame is drawn into a viewport");
        output.drop_without_applying_deltas();
        delay
    }

    /// Drops end to end through the editor's own path: files let go over
    /// a row reach that row's lane, the files after the first become lanes
    /// named after themselves, and the frame asks for the repaint that will
    /// show them.
    #[test]
    fn a_drop_read_from_egui_lands_on_the_row_it_was_let_go_over() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        let (model, loader) = instance();
        let (mut rows, list) = list_of(DEFAULT_LANE_COUNT as usize);
        over_the_lanes(&model, &mut rows);

        // Let go over the fourth row, as a drag out of Bitwig's browser
        // arrives: the pointer where the patched stack says it is, the paths
        // in the drag source's order.
        let onto = rows[3];
        editor_frame(
            &ctx,
            &mut intake,
            &model,
            &loader,
            &rows,
            list,
            egui::RawInput {
                events: vec![egui::Event::PointerMoved(onto.rect.center())],
                dropped_files: dropped(&["/nonexistent/BD 606.wav", "/nonexistent/CH.wav"]),
                ..Default::default()
            },
        );

        {
            let model = lock_model(&model);
            let lane = model.lane(onto.id).expect("the row that was dropped on");
            // Either state is right here and which one is a race with the
            // worker: the lane is waiting for its job, or the job has
            // already failed on a file that does not exist. What matters is
            // that the lane has one, and the drained loader below says so.
            assert!(lane.loading || lane.error.is_some(), "the row's lane got no job");
            assert_eq!(lane.name, "Lane 4", "and keeps the name it had");
            assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize + 1);
            assert_eq!(model.lanes().last().expect("the added lane").name, "CH");
        }
        assert!(ctx.has_requested_repaint(), "nothing else will draw the lanes it made");

        // And it lands once: the frames after it find no drop to answer, so
        // no lane is added twice.
        for _ in 0..3 {
            editor_frame(
                &ctx,
                &mut intake,
                &model,
                &loader,
                &rows,
                list,
                egui::RawInput::default(),
            );
        }
        assert_eq!(
            lock_model(&model).lanes().len(),
            DEFAULT_LANE_COUNT as usize + 1,
            "the drop was answered more than once"
        );

        // Both jobs really ran, against the paths that were dropped: every
        // one of these files is missing, so each lane ends with the
        // decoder's own message instead of a sample.
        loader.finish();
        let model = lock_model(&model);
        for id in [onto.id, model.lanes().last().expect("the added lane").id] {
            let lane = model.lane(id).expect("a lane of the instance");
            assert!(!lane.loading, "lane {id} is still waiting");
            assert!(lane.error.is_some(), "lane {id} was given no job");
        }
    }

    /// A drop the lane cap truncated says so through the editor's own path,
    /// and stops saying it by itself.
    ///
    /// The whole of what the window does with the count: the message is
    /// made in the frame the drop lands in — after the bar of that frame has
    /// been drawn, which is why `land` asks for another — and is gone
    /// [`MESSAGE_SHOWN_FOR`] later, in a frame it asked for itself so that a
    /// window nobody is touching does not keep it up.
    #[test]
    fn a_truncated_drop_says_how_many_files_it_lost_and_then_stops() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        let (model, loader) = instance();
        // One lane free, and three files aimed below the rows.
        fill_to(&model, 1);
        let (mut rows, list) = list_of(3);
        over_the_lanes(&model, &mut rows);

        let below = pos2(list.center().x, list.bottom());
        let dropping = egui::RawInput {
            time: Some(0.0),
            events: vec![egui::Event::PointerMoved(below)],
            dropped_files: dropped(&[
                "/nonexistent/one.wav",
                "/nonexistent/two.wav",
                "/nonexistent/three.wav",
            ]),
            ..Default::default()
        };

        assert_eq!(intake.message(), None, "a window that has had no drop says nothing");
        editor_frame(&ctx, &mut intake, &model, &loader, &rows, list, dropping);
        assert_eq!(
            intake.message(),
            Some("2 files didn't fit"),
            "the two files the full list could not take"
        );
        assert!(ctx.has_requested_repaint(), "and the frame that shows it was asked for");

        // Still up a moment before its time, and the frame that will take it
        // down has been asked for — the window may have no input coming.
        let left = MESSAGE_SHOWN_FOR.as_secs_f64() - 0.5;
        let delay = editor_frame_delay(
            &ctx,
            &mut intake,
            &model,
            &loader,
            &rows,
            list,
            egui::RawInput { time: Some(left), ..Default::default() },
        );
        assert_eq!(intake.message(), Some("2 files didn't fit"));
        assert!(
            delay <= Duration::from_secs_f64(0.5),
            "nothing will draw the frame that takes it down: {delay:?}"
        );

        // And its time is up.
        editor_frame(
            &ctx,
            &mut intake,
            &model,
            &loader,
            &rows,
            list,
            egui::RawInput { time: Some(MESSAGE_SHOWN_FOR.as_secs_f64()), ..Default::default() },
        );
        assert_eq!(intake.message(), None, "the message takes itself away");

        loader.finish();
    }

    /// A drop that lost nothing leaves nothing behind, which is every drop
    /// but the one made into a full list.
    #[test]
    fn a_drop_that_landed_whole_leaves_no_message() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        let (model, loader) = instance();
        let (mut rows, list) = list_of(3);
        over_the_lanes(&model, &mut rows);

        let below = pos2(list.center().x, list.bottom());
        editor_frame(
            &ctx,
            &mut intake,
            &model,
            &loader,
            &rows,
            list,
            egui::RawInput {
                time: Some(0.0),
                events: vec![egui::Event::PointerMoved(below)],
                dropped_files: dropped(&["/nonexistent/kick.wav", "/nonexistent/snare.wav"]),
                ..Default::default()
            },
        );

        assert_eq!(intake.message(), None, "both files found a lane");
        loader.finish();
    }

    /// The same path with the pointer below the rows: every file becomes a
    /// lane of its own and the row it missed is left alone.
    #[test]
    fn a_drop_read_from_egui_below_the_rows_adds_lanes_only() {
        let ctx = egui::Context::default();
        let mut intake = DropIntake::default();
        let (model, loader) = instance();
        // Three rows, so there is list left under them to let go over.
        let (mut rows, list) = list_of(3);
        over_the_lanes(&model, &mut rows);
        let untouched: Vec<u32> = lock_model(&model).lanes().iter().map(|lane| lane.id).collect();

        let below = pos2(list.center().x, list.bottom());
        editor_frame(
            &ctx,
            &mut intake,
            &model,
            &loader,
            &rows,
            list,
            egui::RawInput {
                events: vec![egui::Event::PointerMoved(below)],
                dropped_files: dropped(&["/nonexistent/kick.wav", "/nonexistent/snare.wav"]),
                ..Default::default()
            },
        );

        let model = lock_model(&model);
        for id in untouched {
            let lane = model.lane(id).expect("a lane of the instance");
            assert!(!lane.loading, "lane {id} was dropped on and should not have been");
        }
        let added: Vec<&str> =
            model.lanes()[DEFAULT_LANE_COUNT as usize..].iter().map(|l| l.name.as_str()).collect();
        assert_eq!(added, ["kick", "snare"]);

        drop(model);
        loader.finish();
    }

    /// A chop let go over the strip it came from is a no-op; anywhere else
    /// it routes as a file would, minus the strip: a row, or new lanes for
    /// the space below the list, outside it and a pointer egui lost.
    #[test]
    fn a_chop_lands_where_a_file_would_but_never_on_the_strip() {
        let (rows, list) = list_of(3);
        let strip = Rect::from_min_size(pos2(10.0, 0.0), vec2(300.0, 38.0));
        let below = Landing::NewLanes { outline: new_lane_space(&rows, list) };

        assert_eq!(chop_landing(Some(strip.center()), &rows, list, strip), None);
        assert_eq!(
            chop_landing(Some(rows[2].rect.center()), &rows, list, strip),
            Some(Landing::Row { id: rows[2].id, outline: rows[2].rect })
        );
        let under = pos2(list.center().x, list.bottom() - 1.0);
        assert_eq!(chop_landing(Some(under), &rows, list, strip), Some(below));
        assert_eq!(chop_landing(Some(pos2(900.0, 900.0)), &rows, list, strip), Some(below));
        assert_eq!(chop_landing(None, &rows, list, strip), Some(below));
        let lost = pos2(f32::NAN, strip.center().y);
        assert_eq!(chop_landing(Some(lost), &rows, list, strip), Some(below));
    }

    /// With no source open a chop changes nothing and is no undo step,
    /// wherever it lands.
    #[test]
    fn a_chop_without_a_source_changes_nothing() {
        let mut model = Model::new();
        let lanes = model.lanes().len();
        let id = model.lanes()[0].id;
        let payload = ChopPayload { start: 0, end: 4_800 };
        let row = Landing::Row { id, outline: Rect::NOTHING };
        assert_eq!(apply_chop(&mut model, row, payload), None);
        let below = Landing::NewLanes { outline: Rect::NOTHING };
        assert_eq!(apply_chop(&mut model, below, payload), None);
        assert_eq!(model.lanes().len(), lanes);
        assert!(!model.can_undo());
    }
}
