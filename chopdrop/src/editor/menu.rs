//! A lane's right-click menu.
//!
//! Ten things a user can do to a lane that a row has no column for: rename
//! it, recolour it from the palette, duplicate it, delete it, browse for its
//! sample, reload its sample from the file on disk, take the sample off it,
//! reopen the source a chop lane was cut from in the chop editor, render a
//! Stretch lane's audio again, or give it (and the lanes selected with it)
//! an output of its own. A row is ten fields wide already and each of
//! these is a once-in-a-while gesture, so they live behind a secondary
//! click anywhere over the row — not over one field of it, which is why the
//! popup hangs off the row's own scope
//! response ([`lane_menu`]) rather than off a widget inside it.
//!
//! # What this module decides and what it does not
//!
//! Nothing here writes to the model. One frame of an open menu comes out as
//! at most one [`Action`] ([`MenuFrame`]), which the lane list applies
//! through the writer API exactly as it applies a drag or a rename
//! (`editor/lanes.rs`) — so the menu is drawn, read and asserted on without
//! a model to write into, and the one place a lane is written stays the one
//! place.
//!
//! Two of the six are not writes at all. "rename" opens the row's own rename
//! box ([`crate::editor::lanes::Rename`]), which is the same box a double
//! click on the name opens and keeps all of that box's behaviour. "reload
//! from file" belongs to the background loader, so it leaves the list the way
//! an audition does — handed out to the window, which has the loader
//! (`editor/mod.rs`).
//!
//! # The second click
//!
//! Delete asks twice (as in bitstep): the first click arms the item and the
//! menu stays open, the second one deletes. The arming is per window and
//! belongs to no lane — [`Confirm`] — and it is dropped the moment the menu
//! closes or another lane's menu opens ([`Confirm::follow`]), so a menu
//! reopened later never opens with a loaded gun in it. Everything else in the
//! menu closes it on the one click, which is why this module closes the popup
//! itself ([`PopupCloseBehavior::CloseOnClickOutside`]) instead of letting
//! egui close it on any click inside: egui's own default would take the menu
//! away under the first half of the one gesture that needs two.
//!
//! # The palette
//!
//! The ten [`theme::LANE_SWATCHES`] in a row, drawn by hand because egui
//! has no swatch: a square of the colour, and an [`theme::ACCENT`] ring
//! around the one the lane is wearing. The comparison is masked with
//! [`theme::RGB_MASK`], the same bits the painter reads, so a lane whose
//! colour came out of a project file with a top byte set still lights the
//! swatch it was given.

use egui::{Popup, PopupCloseBehavior, Rect, Response, RichText, SetOpenCommand, Sense, Ui, Vec2};

use crate::editor::theme;
use crate::editor::top_bar::ADD_LANE_FULL_HOVER;
use crate::model::{Lane, MAX_LANES};
use crate::time::TimeMode;

/// What the "rename" item does: opens the row's rename box.
pub const RENAME_LABEL: &str = "rename";

/// The label over the palette swatches — the one item of the menu that is
/// not a line to click but a row of them.
pub const COLOUR_LABEL: &str = "colour";

/// What the "duplicate" item does: a copy of the lane below it, sample and
/// note and all ([`crate::model::Model::duplicate_lane`]).
pub const DUPLICATE_LABEL: &str = "duplicate";

/// Whether "duplicate" has room to do anything: a list of `lane_count`
/// lanes takes one more only below [`MAX_LANES`], where
/// [`crate::model::Model::duplicate_lane`] would refuse anyway. At the limit
/// the item is drawn grey with `+ lane`'s own hover
/// ([`ADD_LANE_FULL_HOVER`]), so it explains itself the way `+ lane` does
/// instead of answering a click with nothing.
pub fn duplicate_enabled(lane_count: usize) -> bool {
    lane_count < MAX_LANES as usize
}

/// The delete item at rest, before it has been asked once.
pub const DELETE_LABEL: &str = "delete";

/// The delete item once it is armed: the same item, saying what a second
/// click on it will do: the second click confirms.
///
/// In [`theme::WARN`], which in this window means "this is the thing that
/// loses work": the one menu item whose outcome cannot be had back except
/// through undo.
pub const DELETE_CONFIRM_LABEL: &str = "delete — click again";

/// What the "reload from file" item does: reads the lane's own path again,
/// for a sample the user has edited since it was loaded (the project plays
/// its embedded copy, and this is how the file on disk gets back in).
pub const RELOAD_LABEL: &str = "reload from file";

/// What the pointer resting on a dead "reload from file" says on a chop lane.
/// A chop's path is its source's, and re-reading it would put the whole
/// source on the lane in place of the cut.
pub const RELOAD_CHOP_HOVER: &str = "a chop has no file of its own — use open source in editor";

/// Whether "reload from file" is live on the lane: it has a sample, and
/// that sample is a file of its own rather than a chop
/// ([`offers_open_source`]), whose path is the source it was cut from.
pub fn reload_enabled(lane: &Lane) -> bool {
    lane.sample.as_ref().is_some_and(|sample| sample.origin.is_none())
}

/// The "browse…" item: opens the sample browser on this lane. On every
/// lane, loaded or not.
pub const BROWSE_LABEL: &str = "browse…";

/// What the "clear sample" item does: leaves the lane, its note, its name
/// and its colour, and takes the audio off it.
pub const CLEAR_LABEL: &str = "clear sample";

/// The "open source in editor" item: only on a lane cut in the chop editor,
/// it reopens the file the lane was cut from with the cut selected and in
/// view.
pub const OPEN_SOURCE_LABEL: &str = "open source in editor";

/// Whether the lane's menu offers "open source in editor": only a lane
/// whose sample remembers where it was cut from has a source to open.
/// Unlike the file items it is left out, not greyed, on every other lane —
/// most lanes never come from the chop editor.
pub fn offers_open_source(lane: &Lane) -> bool {
    lane.sample.as_ref().is_some_and(|sample| sample.origin.is_some())
}

/// The "re-render" item: a fresh render of a Stretch lane's sample, whether
/// or not the last one failed ([`crate::model::Model::request_render`]; never
/// an undo step).
pub const RERENDER_LABEL: &str = "re-render";

/// What the pointer resting on "re-render" says.
pub const RERENDER_HOVER: &str =
    "renders this lane's stretched audio again from its sample. Renders happen by themselves; \
     this is for one that failed or sounds wrong.";

/// Whether the lane's menu offers "re-render": only a lane in Stretch with
/// a sample has a render to rebuild. Left out, not greyed, on every other
/// lane, like "open source in editor" — most lanes never stretch.
pub fn offers_rerender(lane: &Lane) -> bool {
    lane.time_mode == TimeMode::Stretch && lane.sample.is_some()
}

/// The "give each its own output" item (v2): every selected lane — or just
/// this one, when it is not selected — moves to the lowest aux output no
/// other lane uses ([`crate::model::Model::assign_own_outputs`]).
pub const OWN_OUTPUTS_LABEL: &str = "give each its own output";

/// What the pointer resting on "give each its own output" says: where the
/// lanes go, and what Bitwig needs before it hears them there.
pub const OWN_OUTPUTS_HOVER: &str =
    "moves the selected lanes (or this one) to free outputs, Out 2 to Out 16, one lane each. \
     Click Add Missing Chains on the device to hear them in Bitwig.";

/// What the pointer resting on the lane's own path says — the whole path,
/// which the row's sample column has no room for
/// (`editor/lanes.rs`'s `sample_cell`).
///
/// On the two items that are about the file, because this menu is the one
/// place in the window with room to say where a lane's audio came from.
pub fn path_hover(lane: &Lane) -> Option<&str> {
    lane.sample.as_ref().map(|sample| sample.path.as_str())
}

/// One thing a user asked of a lane through its menu.
///
/// At most one a frame: the menu closes on the click that makes one (bar
/// the first of delete's two), so there is never a second to find.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Open the row's rename box, as a double click on the name does.
    Rename,

    /// Give the lane this palette colour, as `0xRRGGBB`.
    Colour(u32),

    /// Copy the lane, placing the copy directly below it.
    Duplicate,

    /// Delete the lane — the second click of two.
    Delete,

    /// Open the sample browser on this lane.
    Browse,

    /// Read the lane's sample from its path again, through the loader.
    Reload,

    /// Take the sample off the lane, leaving the lane.
    ClearSample,

    /// Open the source the lane's sample was cut from in the chop editor,
    /// with the cut selected (only offered on a lane with an origin).
    OpenSource,

    /// Render the Stretch lane's audio again (only offered on a Stretch lane
    /// with a sample, [`offers_rerender`]).
    ReRender,

    /// Give the selected lanes — or this lane, when it is not selected —
    /// an aux output each.
    OwnOutputs,
}

/// Whether a lane's delete item has been clicked once and is waiting for
/// its second click.
///
/// Per opened window, like [`crate::editor::lanes::Rename`] and for the
/// same reason: it is a half-finished gesture rather than anything about a
/// lane, so it belongs to the window the gesture is being made in and is
/// built afresh in [`crate::editor::ChopdropEditor`]'s `build`.
///
/// One lane at a time, because one menu is open at a time.
#[derive(Default)]
pub struct Confirm {
    /// The lane whose delete item is armed, and `None` when none is.
    lane: Option<u32>,
}

impl Confirm {
    /// Whether this lane's delete item is the armed one, and so whether the
    /// next click on it deletes.
    pub fn armed(&self, lane: u32) -> bool {
        self.lane == Some(lane)
    }

    /// Arms this lane's delete item: the next click on it is the second.
    fn arm(&mut self, lane: u32) {
        self.lane = Some(lane);
    }

    /// Disarms whatever was armed.
    pub fn clear(&mut self) {
        self.lane = None;
    }

    /// Keeps the arming only while the menu it was made in is still the open
    /// one — `open` is the lane whose menu drew itself this frame, and
    /// `None` is a window with no menu open.
    ///
    /// Which is what makes the confirm a gesture and not a state: letting
    /// the menu go, clicking elsewhere, pressing Escape or opening another
    /// lane's menu all end it, and the next menu opens with "delete" rather
    /// than with "click again" on an item nobody has clicked.
    pub fn follow(&mut self, open: Option<u32>) {
        if self.lane != open {
            self.clear();
        }
    }
}

/// What one frame of a lane's menu came to.
pub struct MenuFrame {
    /// Whether the menu is open for this lane this frame — what
    /// [`Confirm::follow`] is told, and the one thing a *closed* menu says.
    pub open: bool,

    /// The one thing the frame was asked for, if it was asked for anything.
    pub action: Option<Action>,

    /// Where the open menu drew its items, for the tests — the same reason
    /// [`crate::editor::lanes::LaneRow`] carries its row's fields: a
    /// gesture is worth playing over the layout the user will actually see.
    /// `None` while the menu is closed.
    pub items: Option<Items>,
}

impl MenuFrame {
    /// The frame of a lane whose menu is not open.
    fn closed() -> MenuFrame {
        MenuFrame { open: false, action: None, items: None }
    }
}

/// Where an open menu's items landed this frame.
pub struct Items {
    /// The "rename" item.
    pub rename: Rect,

    /// The ten palette swatches, in [`theme::LANE_SWATCHES`] order.
    pub swatches: Vec<Rect>,

    /// The "duplicate" item.
    pub duplicate: Rect,

    /// The "delete" item, whatever face it is wearing.
    pub delete: Rect,

    /// The "browse…" item.
    pub browse: Rect,

    /// The "reload from file" item — drawn on every lane and enabled only
    /// on one with a sample of its own to re-read ([`reload_enabled`]).
    pub reload: Rect,

    /// The "clear sample" item, enabled under the same condition.
    pub clear: Rect,

    /// The "open source in editor" item, `None` on a lane with no origin
    /// ([`offers_open_source`]).
    pub open_source: Option<Rect>,

    /// The "re-render" item, `None` unless [`offers_rerender`].
    pub rerender: Option<Rect>,

    /// The "give each its own output" item.
    pub own_outputs: Rect,
}

/// The lane's menu, opened by a secondary click anywhere over its row and
/// drawn at the pointer.
///
/// `row` is the row's own scope response (`editor/lanes.rs`'s `row_scope`),
/// which is what makes the whole row the right-click target: the row's
/// fields are widgets of their own and a secondary click on one of them is
/// still a click on the row, which is exactly what
/// [`Response::container_secondary_clicked`] answers.
///
/// The popup is built here rather than through
/// [`Response::container_context_menu`] for one reason: that helper lets
/// egui close the menu on any click inside it, and the delete item needs
/// two (see the module header). Everything else about it is the same —
/// opened by the secondary click, closed by a primary click anywhere else
/// over the row, drawn at the pointer in the theme's menu style.
///
/// `room` is [`duplicate_enabled`] for the list the lane is in.
pub fn lane_menu(row: &Response, lane: &Lane, room: bool, confirm: &mut Confirm) -> MenuFrame {
    // A secondary click anywhere over the row — on one of its fields as
    // much as on the gap between two of them, which is what `container_`
    // means ([`Response::container_secondary_clicked`]).
    let opening = row.container_secondary_clicked();

    let shown = Popup::menu(row)
        .open_memory(opening.then_some(SetOpenCommand::Bool(true)))
        // At the pointer and pinned there, so the menu does not walk about
        // under the hand that is reading it.
        .at_pointer_fixed()
        // Every way this menu closes is below: a click outside it, an item
        // that acted, or Escape (which egui answers itself). None of
        // egui's own click behaviours will do — they close a menu on the
        // first click *inside* it, and the delete item needs two.
        .close_behavior(PopupCloseBehavior::IgnoreClicks)
        .show(|ui| items(ui, lane, room, confirm));

    let Some(inner) = shown else {
        return MenuFrame::closed();
    };

    if !opening && clicked_outside(&inner.response) {
        // Which is the one rule egui's own popups keep that this one has to
        // keep for itself, having turned its click handling off: a menu
        // left open over the window the user has gone back to clicking
        // would be a menu they cannot get rid of.
        //
        // Not on the frame the menu *opened* in: the popup is drawn from
        // the pointer down, so the click that opened it is a click inside
        // it by a point or two, and the menu would shut itself before it
        // was ever read.
        Popup::close_id(&row.ctx, Popup::default_response_id(row));
    }

    let (action, items) = inner.inner;
    MenuFrame { open: true, action, items: Some(items) }
}

/// Whether a click this frame landed somewhere other than in the open menu.
///
/// `menu` is the popup's own response, so "in the menu" is a question about
/// the popup's layer and not about the row underneath it: the popup is
/// drawn over the row it belongs to, and a click on one of its items must
/// not read as a click on the field it happens to be covering.
fn clicked_outside(menu: &Response) -> bool {
    menu.ctx.input(|input| input.pointer.any_click()) && !menu.contains_pointer()
}

/// The menu's contents, top to bottom in their fixed order, and the one
/// action a click on them asks for.
///
/// Every item but the first delete click closes the menu itself
/// ([`Ui::close`]): the gesture is over, and a menu left open over the row
/// it has just changed would hide the change.
fn items(
    ui: &mut Ui,
    lane: &Lane,
    room: bool,
    confirm: &mut Confirm,
) -> (Option<Action>, Items) {
    let mut action = None;

    let rename = ui.button(RENAME_LABEL);
    if rename.clicked() {
        action = Some(Action::Rename);
        ui.close();
    }

    ui.separator();

    // Not a button: the swatches under it are the item, and a label that
    // lit up under the pointer would read as a seventh thing to click.
    ui.label(RichText::new(COLOUR_LABEL).color(theme::TEXT_DIM));
    let (swatches, picked) = palette(ui, lane);
    if let Some(colour) = picked {
        action = Some(Action::Colour(colour));
        ui.close();
    }

    ui.separator();

    let duplicate = ui
        .add_enabled(room, egui::Button::new(DUPLICATE_LABEL))
        .on_disabled_hover_text(ADD_LANE_FULL_HOVER);
    if duplicate.clicked() {
        action = Some(Action::Duplicate);
        ui.close();
    }

    let armed = confirm.armed(lane.id);
    let delete = ui.button(delete_label(armed));
    if delete.clicked() {
        if armed {
            action = Some(Action::Delete);
            confirm.clear();
            ui.close();
        } else {
            // The menu stays open, wearing the question
            // ([`DELETE_CONFIRM_LABEL`]).
            confirm.arm(lane.id);
        }
    }

    ui.separator();

    // The two items about the file, dead on a lane that has none: a lane
    // with no sample has no path to re-read and nothing to clear, and an
    // item that did nothing when clicked would be worse than one that says
    // so by being grey. The hover is the lane's whole path, which the row
    // itself has no room for.
    let browse = ui.button(BROWSE_LABEL);
    if browse.clicked() {
        action = Some(Action::Browse);
        ui.close();
    }

    let loaded = lane.sample.is_some();
    let hover = path_hover(lane);

    // A chop lane's reload is dead too, and says why instead of the path:
    // the path it would re-read is the whole source it was cut from.
    let reload = if offers_open_source(lane) {
        ui.add_enabled(false, egui::Button::new(RELOAD_LABEL))
            .on_disabled_hover_text(RELOAD_CHOP_HOVER)
    } else {
        enabled_item(ui, RELOAD_LABEL, reload_enabled(lane), hover)
    };
    if reload.clicked() {
        action = Some(Action::Reload);
        ui.close();
    }

    let clear = enabled_item(ui, CLEAR_LABEL, loaded, hover);
    if clear.clicked() {
        action = Some(Action::ClearSample);
        ui.close();
    }

    let open_source = offers_open_source(lane).then(|| {
        let item = ui.button(OPEN_SOURCE_LABEL);
        let item = match lane.sample.as_ref().and_then(|sample| sample.origin_text()) {
            Some(origin) => item.on_hover_text(origin),
            None => item,
        };
        if item.clicked() {
            action = Some(Action::OpenSource);
            ui.close();
        }
        item.rect
    });

    let rerender = offers_rerender(lane).then(|| {
        let item = ui.button(RERENDER_LABEL).on_hover_text(RERENDER_HOVER);
        if item.clicked() {
            action = Some(Action::ReRender);
            ui.close();
        }
        item.rect
    });

    ui.separator();

    let own_outputs = ui.button(OWN_OUTPUTS_LABEL).on_hover_text(OWN_OUTPUTS_HOVER);
    if own_outputs.clicked() {
        action = Some(Action::OwnOutputs);
        ui.close();
    }

    let items = Items {
        rename: rename.rect,
        swatches,
        duplicate: duplicate.rect,
        delete: delete.rect,
        browse: browse.rect,
        reload: reload.rect,
        clear: clear.rect,
        open_source,
        rerender,
        own_outputs: own_outputs.rect,
    };
    (action, items)
}

/// The face the delete item is wearing: the plain word, or the question a
/// second click answers, in [`theme::WARN`].
fn delete_label(armed: bool) -> RichText {
    if armed {
        RichText::new(DELETE_CONFIRM_LABEL).color(theme::WARN)
    } else {
        RichText::new(DELETE_LABEL)
    }
}

/// One menu item that is only there to be clicked when the lane has a
/// sample, with the lane's path on its hover.
///
/// Drawn whether or not it is enabled, so that the menu is the same six
/// items tall on every lane and a user learns where each of them is.
fn enabled_item(ui: &mut Ui, label: &str, enabled: bool, hover: Option<&str>) -> Response {
    let response = ui.add_enabled(enabled, egui::Button::new(label));
    match hover {
        Some(path) => response.on_hover_text(path),
        None => response,
    }
}

/// The ten palette swatches in a row, and the colour a click on one of them
/// asks for.
///
/// Hand-drawn squares rather than buttons: a swatch *is* its colour, so
/// there is no fill for a widget's visuals to decide, and the one thing it
/// has to say beyond the colour is whether it is the lane's own — an
/// [`theme::ACCENT`] ring, the same accent the drop highlight uses for "the
/// thing this gesture is about".
fn palette(ui: &mut Ui, lane: &Lane) -> (Vec<Rect>, Option<u32>) {
    let mut picked = None;
    let mut rects = Vec::with_capacity(theme::LANE_SWATCHES.len());

    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(theme::HEADER_FIELD_GAP);
        for (index, &swatch) in theme::LANE_SWATCHES.iter().enumerate() {
            let colour = theme::lane_swatch_rgb(index);
            let (rect, response) =
                ui.allocate_exact_size(Vec2::splat(theme::SWATCH_SIZE), Sense::click());
            rects.push(rect);

            ui.painter().rect_filled(rect, theme::CELL_CORNER, swatch);
            if let Some(ring) = swatch_ring(lane.colour, colour, response.hovered()) {
                ui.painter().rect_stroke(
                    rect,
                    theme::CELL_CORNER,
                    egui::Stroke::new(theme::SWATCH_RING_WIDTH, ring),
                    egui::StrokeKind::Inside,
                );
            }

            if response.clicked() {
                picked = Some(colour);
            }
        }
    });

    (rects, picked)
}

/// The ring a swatch wears, if it wears one: [`theme::ACCENT`] on the
/// colour the lane is already wearing, [`theme::TEXT_DIM`] on the one under
/// the pointer, and nothing on the other eight.
///
/// Both colours are masked with [`theme::RGB_MASK`] before they are
/// compared — the same bits the painter reads — so a lane whose stored
/// colour has its top byte set (a project from some other version, or a
/// hostile one) still lights the swatch it names rather than none of them.
///
/// A function of three values and no egui state, so which swatch is ringed
/// is settled without a window; where they are drawn is [`palette`]'s.
pub fn swatch_ring(lane_colour: u32, swatch: u32, hovered: bool) -> Option<egui::Color32> {
    if lane_colour & theme::RGB_MASK == swatch & theme::RGB_MASK {
        Some(theme::ACCENT)
    } else if hovered {
        Some(theme::TEXT_DIM)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::chop::ChopOrigin;
    use crate::model::SampleRef;
    use crate::sample::SampleData;

    /// "duplicate" is live while the list has room for one more lane and
    /// dead at the limit, the same line `+ lane` is drawn on.
    #[test]
    fn duplicate_is_enabled_only_below_max_lanes() {
        assert!(duplicate_enabled(0));
        assert!(duplicate_enabled(MAX_LANES as usize - 1), "the last lane's room is refused");
        assert!(!duplicate_enabled(MAX_LANES as usize), "a full list still offers a copy");
        assert!(!duplicate_enabled(MAX_LANES as usize + 1));
    }

    /// The ten swatches are the palette the lanes are coloured from, in its
    /// order — a menu offering some other list would be a menu that could
    /// not say which colour a fresh lane is wearing.
    #[test]
    fn the_swatches_are_the_lane_palette_in_order() {
        assert_eq!(theme::LANE_SWATCHES.len(), 10, "ten swatches");
        for index in 0..theme::LANE_SWATCHES.len() {
            assert_eq!(
                theme::rgb(theme::lane_swatch_rgb(index)),
                theme::LANE_SWATCHES[index],
                "swatch {index} paints the colour it would set"
            );
        }
    }

    /// The confirm is armed by a click and dropped by the menu closing, by
    /// another lane's menu opening, and by being cleared outright.
    #[test]
    fn the_delete_confirm_lives_as_long_as_its_own_menu() {
        let mut confirm = Confirm::default();
        assert!(!confirm.armed(3), "nothing is armed to begin with");

        confirm.arm(3);
        assert!(confirm.armed(3));
        assert!(!confirm.armed(4), "one lane at a time");

        // The menu is still open on lane 3: the arming stands.
        confirm.follow(Some(3));
        assert!(confirm.armed(3));

        // Lane 4's menu opens instead — lane 3's question goes with its
        // menu rather than waiting for a click meant for another lane.
        confirm.follow(Some(4));
        assert!(!confirm.armed(3));

        confirm.arm(4);
        // And the menu closes.
        confirm.follow(None);
        assert!(!confirm.armed(4));

        confirm.arm(1);
        confirm.clear();
        assert!(!confirm.armed(1));
    }

    /// The ring is on the colour the lane is wearing, on the one under the
    /// pointer, and on nothing else — and a stored colour with its top byte
    /// set still finds its swatch.
    #[test]
    fn the_ring_marks_the_lanes_own_colour_and_the_hovered_one() {
        let worn = theme::lane_swatch_rgb(3);
        let other = theme::lane_swatch_rgb(4);
        assert_ne!(worn, other, "the palette repeats itself");

        assert_eq!(swatch_ring(worn, worn, false), Some(theme::ACCENT));
        assert_eq!(swatch_ring(worn, other, false), None, "an unworn swatch at rest");
        assert_eq!(swatch_ring(worn, other, true), Some(theme::TEXT_DIM), "under the pointer");
        assert_eq!(
            swatch_ring(worn, worn, true),
            Some(theme::ACCENT),
            "the lane's own colour says so even under the pointer"
        );

        // The top byte is not part of a colour (`theme::rgb`).
        assert_eq!(swatch_ring(0xFF00_0000 | worn, worn, false), Some(theme::ACCENT));
    }

    /// A lane's path is what the file items hover, and a lane with no
    /// sample has nothing to say about one.
    #[test]
    fn the_path_hover_is_the_lanes_own_path() {
        let lane = Lane::new(0, "Lane 1", 36, 0xE2_6E_6E);
        assert_eq!(path_hover(&lane), None, "an empty lane has no path");
    }

    /// The menu's items alone in a window, over one lane — and the frames
    /// that drive them, the same way the lane list's own tests do: what
    /// was painted and what the last frame's click asked for.
    struct Menu {
        ctx: egui::Context,
        lane: Lane,
        confirm: Confirm,
        items: Option<Items>,
        action: Option<Action>,
        painted: Vec<String>,
    }

    impl Menu {
        fn over(lane: Lane) -> Menu {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let mut menu = Menu {
                ctx,
                lane,
                confirm: Confirm::default(),
                items: None,
                action: None,
                painted: Vec::new(),
            };
            menu.frame(Vec::new());
            menu
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(400.0, 600.0),
                )),
                events,
                ..egui::RawInput::default()
            };
            let (lane, confirm) = (&self.lane, &mut self.confirm);
            let mut drawn = None;
            let output = self.ctx.run_ui(input, |ui| drawn = Some(items(ui, lane, true, confirm)));
            let (action, items) = drawn.expect("the items were drawn");
            self.action = action;
            self.items = Some(items);
            self.painted = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            output.drop_without_applying_deltas();
        }

        fn reload(&self) -> Rect {
            self.items.as_ref().expect("a frame was run").reload
        }

        /// Rests the pointer on the reload item past egui's tooltip delay.
        fn hover_reload(&mut self) {
            self.frame(vec![egui::Event::PointerMoved(self.reload().center())]);
            for _ in 0..60 {
                self.frame(Vec::new());
            }
        }

        /// Clicks the reload item; the action is the release frame's.
        fn click_reload(&mut self) {
            let pos = self.reload().center();
            let button = |pressed| egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            };
            self.frame(vec![egui::Event::PointerMoved(pos), button(true)]);
            self.frame(vec![button(false)]);
        }
    }

    /// A lane loaded from `path`, cut from a source when `origin` is given.
    fn loaded(handle: &basedrop::Handle, path: &str, origin: Option<ChopOrigin>) -> Lane {
        let mut lane = Lane::new(0, "Lane 1", 36, 0xE2_6E_6E);
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 4].into() };
        lane.sample = Some(SampleRef::with_origin(handle, Path::new(path), vec![0], data, origin));
        lane
    }

    /// "reload from file" is dead on a chop lane and says why — re-reading
    /// its path would put the whole source on the lane — while on a plain
    /// loaded lane it is live, hovers the lane's path and asks to reload.
    #[test]
    fn reload_is_disabled_with_the_chop_hover_on_a_lane_with_an_origin() {
        let collector = basedrop::Collector::new();
        let origin = ChopOrigin {
            source_path: String::from("/songs/Eye.wav"),
            start_frame: 100,
            end_frame: 900,
        };

        let chop = loaded(&collector.handle(), "/songs/Eye.wav", Some(origin));
        assert!(!reload_enabled(&chop));
        let mut menu = Menu::over(chop);
        assert!(menu.reload().width() > 0.0, "the item is drawn on a chop lane");
        menu.hover_reload();
        assert!(
            menu.painted.iter().any(|text| text == RELOAD_CHOP_HOVER),
            "the dead item does not say why: {:?}",
            menu.painted
        );
        assert!(
            !menu.painted.iter().any(|text| text == "/songs/Eye.wav"),
            "a chop's reload offers the source's path"
        );
        menu.click_reload();
        assert!(menu.action.is_none(), "a chop lane reloaded: {:?}", menu.action);

        let plain = loaded(&collector.handle(), "/kits/bd.wav", None);
        assert!(reload_enabled(&plain));
        let mut menu = Menu::over(plain);
        menu.hover_reload();
        assert!(!menu.painted.iter().any(|text| text == RELOAD_CHOP_HOVER), "{:?}", menu.painted);
        assert!(
            menu.painted.iter().any(|text| text == "/kits/bd.wav"),
            "the live item hovers the lane's path: {:?}",
            menu.painted
        );
        menu.click_reload();
        assert!(matches!(menu.action, Some(Action::Reload)), "{:?}", menu.action);

        let empty = Lane::new(0, "Lane 1", 36, 0xE2_6E_6E);
        assert!(!reload_enabled(&empty), "an empty lane has nothing to re-read");
    }

    /// "re-render" is offered only on a Stretch lane with a sample.
    #[test]
    fn rerender_is_offered_only_on_a_stretch_lane_with_a_sample() {
        let mut lane = Lane::new(0, "Lane 1", 36, 0xE2_6E_6E);
        lane.time_mode = TimeMode::Stretch;
        assert!(!offers_rerender(&lane), "nothing to render without a sample");
    }
}
