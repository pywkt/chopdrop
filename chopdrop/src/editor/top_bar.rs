//! The strip across the top of the window: the master volume, the "bitstep
//! Sample pad" switch and `+ lane`.
//!
//! The three controls of the bar are the three things in this plugin that
//! belong to the instance rather than to one lane, and they are of three
//! different kinds, which is most of what this module has to say:
//!
//! - the **master volume** is the one host parameter, so it is not written
//!   here at all: the rail ([`MasterVolume`]) tells the host about the
//!   gesture ([`ParamSetter::begin_set_parameter`] and its pair) and the
//!   host writes the value, which is what makes the knob automatable and
//!   what makes it move when the automation does;
//! - the **switch** is instance-wide model state saved with the project, so
//!   it goes through the model's writer API like any lane edit, and the
//!   hover text is where it explains itself — it only makes sense next to
//!   bitstep's own Sample pad setting;
//! - **`+ lane`** is a writer call whose whole behaviour (the name, the
//!   note, the colour) is [`Model::add_lane`]'s and is pinned by the model's
//!   own tests: the seventeenth lane of a fresh instance is "Lane 17" on note
//!   52 in palette colour 6 (id 16 wrapped past the ten colours).
//!
//! Both gestures end in [`Model::commit`], which is what makes them one
//! undo step each, which Ctrl+Z reaches. The volume's does not: a host
//! parameter is the host's history and not the model's, so the rail commits
//! nothing and Ctrl+Z over this window never moves it.
//!
//! # The rail is chopdrop's own
//!
//! The volume is drawn here rather than by nice-plug-egui's `ParamSlider`,
//! for the look and for the gesture. The look: a fixed-width rail in the
//! theme's language — a control's grey under the fill the value has reached,
//! the theme's hairline round it, its corners, its height, and the figure
//! drawn *inside* it rather than in a second box beside it, which is what
//! makes the bar read as three controls and not five.
//!
//! The gesture: a press opens the host's automation gesture, every frame
//! that moves the value sets it, and the release closes it — and a bare
//! click, which is the fastest way to set a volume, closes its own gesture in
//! that one press and release. `ParamSlider` cannot: it opens the gesture on
//! the press and closes it on egui's `drag_stopped`, which never comes for a
//! press that did not move, so the host is left recording a gesture nobody is
//! making. Its drag memory is private, so a wrapper around it cannot close
//! the gesture either — hence this module keeps the little state that gesture
//! needs ([`MasterVolume`]).
//!
//! Beside the three of them, in the room they leave over, is the window's
//! one status that is about the instance rather than a lane. Two things are
//! said in that slot, both of them a line of text with the sentence on its
//! hover and neither of them a refusal:
//!
//! - how much sample a save would write into the project, once that is more
//!   than a project should quietly carry
//!   ([`crate::editor::status::embed_warning`]) — advice, since every save
//!   still saves everything, and simply absent below the threshold;
//! - how many files the last drop had to leave behind at [`MAX_LANES`]
//!   ([`crate::editor::status::drop_overflow`]), which the editor hands in
//!   and which goes away by itself
//!   ([`crate::editor::drops::MESSAGE_SHOWN_FOR`]).
//!
//! The drop's message wins the slot while it is up: it is news, it is about
//! to go away, and the size warning is a standing fact that will still be
//! there after it.

use egui::{
    Align, Align2, Label, Rect, Response, RichText, Sense, StrokeKind, TextEdit, Ui, Vec2, emath,
};
use nice_plug::context::gui::ParamSetter;
use nice_plug::params::{FloatParam, Param};

use crate::editor::{status, theme};
use crate::model::{MAX_LANES, Model};

/// The label beside the master volume's rail.
///
/// "Master" rather than "Volume", which is what the host calls the
/// parameter: in this window every lane has a volume of its own, and the one
/// in the bar is the one they all pass through.
pub const MASTER_LABEL: &str = "Master";

/// The switch's name, spelled out in full — it names the bitstep setting it
/// is there to match, so it is not abbreviated.
pub const SAMPLE_PAD_LABEL: &str = "bitstep Sample pad";

/// What the switch says when the pointer rests on it.
///
/// The one hover text the bar is asked for, because the switch is the one
/// control whose name does not say what it does: off, a lane plays its
/// sample at the sample's own pitch whatever note struck it.
pub const SAMPLE_PAD_HOVER: &str =
    "cancels bitstep's Sample pad retune, so bitstep lanes with Sample pad on (bitstep's \
     default) play each sample at its own pitch — turn off if they have it off";

/// The add-lane button's face.
pub const ADD_LANE_LABEL: &str = "+ lane";

/// Why `+ lane` is greyed out: the list is at [`MAX_LANES`].
///
/// Shown only while the button is disabled, which is the one state of it a
/// user could otherwise mistake for the plugin having stopped answering.
pub const ADD_LANE_FULL_HOVER: &str = "the lane list is full";

/// The height the bar takes out of the window.
///
/// The theme's [`theme::TOP_BAR_HEIGHT`] — one control inside the panel's
/// padding — and the hairline egui paints along a panel's inner edge, which
/// is the bar's lower edge ([`theme::GRID_LINE`]). The line is drawn in room
/// the panel reserves inside its own size, so a bar asked for exactly
/// `TOP_BAR_HEIGHT` would draw its controls a point short of it; the window
/// allows for this height instead ([`crate::editor::DEFAULT_HEIGHT`]).
pub const BAR_HEIGHT: f32 = theme::TOP_BAR_HEIGHT + theme::HAIRLINE;

/// What the master volume's rail is in the middle of: the automation
/// gesture it has open, and the value being typed into it.
///
/// Per window and not per instance, like the lane list's rename box
/// ([`crate::editor::lanes::Rename`]) and for the same reason — a hand over
/// a rail belongs to the window it is over, so a window that opens again
/// starts with neither ([`crate::editor::ChopdropEditor`]'s `build`).
#[derive(Default)]
pub struct MasterVolume {
    /// Whether the host has been told a gesture is under way and not yet
    /// that it is over: [`ParamSetter::begin_set_parameter`] sent,
    /// [`ParamSetter::end_set_parameter`] still owed.
    ///
    /// Kept here rather than read off egui's own drag state, which is the
    /// whole reason this rail exists (see the module header): egui calls a
    /// press that never moved a *click* and not a drag, so a widget that
    /// ends its gesture on `drag_stopped` never ends the gesture a bare
    /// click began.
    held: bool,

    /// What has been typed into the rail, while the rail is a text box;
    /// `None` whenever it is a rail. Empty when the box opens — the figure
    /// the box replaced is its hint, so the first thing typed is the whole
    /// of the new value and nothing has to be selected away first.
    typed: Option<String>,

    /// Whether that box still has to ask for the keyboard focus — set when
    /// it opens and spent on the first frame it is drawn, exactly as the
    /// rename box spends its own. A box that asked every frame could never
    /// be left: it would take the focus back in the same frame Escape or a
    /// click elsewhere gave it up.
    focus: bool,
}

impl MasterVolume {
    /// Whether the rail is a text box this frame rather than a rail.
    pub fn typing(&self) -> bool {
        self.typed.is_some()
    }

    /// Turns the rail into an empty box asking for a value.
    fn open(&mut self) {
        self.typed = Some(String::new());
        self.focus = true;
    }

    /// Puts the rail back, keeping nothing of what was in the box.
    fn close(&mut self) {
        self.typed = None;
        self.focus = false;
    }
}

/// The bar's three controls and its one status, as egui answered them this
/// frame.
///
/// Handed back rather than dropped so that what the bar drew can be asked
/// where it landed, which is what the tests below look at.
pub struct TopBar {
    /// The master volume's rail (the slider, not the value text beside it).
    pub volume: Response,
    /// The "bitstep Sample pad" switch.
    pub sample_pad: Response,
    /// `+ lane`, disabled once the list is full.
    pub add_lane: Response,
    /// The embedded-size warning, when this instance is carrying enough
    /// sample to want one ([`status::embed_warning`]); `None` below the
    /// threshold, and `None` while a drop's message has the slot.
    pub embed_warning: Option<Response>,

    /// What the last drop had to leave behind, while the editor is still
    /// showing it ([`crate::editor::drops::DropIntake::message`]); `None`
    /// in every frame but the few after a drop onto a full list.
    pub drop_message: Option<Response>,
}

/// Draws the bar at the top of the window and answers the gestures over it.
///
/// `setter` is the window's host context, which is how a parameter is
/// written (see the module header); `model` is the instance's lanes, behind
/// the writer lock the editor's frame already holds.
///
/// `message` is what the last drop (or "give each its own output") left to
/// say and its hover, if it is still saying it
/// ([`crate::editor::drops::DropIntake::message`]): passed in rather than
/// read off anything here, because it belongs to the window the drag was
/// over and not to the instance the other three controls are of.
///
/// `master` is the rail's half-made gesture ([`MasterVolume`]), which is the
/// window's and not the frame's — the only thing this bar keeps between two
/// of them.
pub fn top_bar(
    ui: &mut Ui,
    volume: &FloatParam,
    setter: Option<&ParamSetter>,
    model: &mut Model,
    message: Option<(&str, &str)>,
    master: &mut MasterVolume,
) -> TopBar {
    let frame = egui::Frame::new()
        .fill(theme::PANEL)
        .inner_margin(egui::Margin::same(theme::PANEL_PADDING as i8));

    egui::Panel::top("chopdrop_top_bar")
        // Exactly one control tall, as the lane list below it is a whole
        // number of rows tall: the window's default height is a sum over
        // both ([`crate::editor::DEFAULT_HEIGHT`]).
        .exact_size(BAR_HEIGHT)
        // The bar's lower edge, and the point of [`BAR_HEIGHT`]. Said out
        // loud although it is egui's default, because the height depends on
        // it.
        .show_separator_line(true)
        .frame(frame)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                // The bar's own rhythm rather than a lane row's tighter one:
                // these three controls are three separate things.
                ui.spacing_mut().item_spacing.x = theme::ITEM_SPACING;

                let volume = master_volume(ui, volume, setter, master);
                let sample_pad = sample_pad_switch(ui, model);
                let add_lane = add_lane_button(ui, model);

                // One slot, two things that could be in it (see the module
                // header for why the drop wins it).
                let (embed_warning, drop_message) = match message {
                    Some((message, hover)) => {
                        (None, Some(drop_message_label(ui, message, hover)))
                    }
                    None => (embed_warning_label(ui, model), None),
                };

                TopBar { volume, sample_pad, add_lane, embed_warning, drop_message }
            })
            .inner
        })
        .inner
}

/// The master volume: the label and the rail beside it, which a drag or a
/// click moves and a right-click turns into a box to type a value into.
///
/// Without a host context there is nothing to write a parameter through, so
/// the rail is drawn, reads the parameter and answers nothing. Only
/// reachable before [`crate::editor::ChopdropEditor`]'s `build` has run —
/// which nice-plug-egui never draws a frame without — and in this module's
/// own tests.
fn master_volume(
    ui: &mut Ui,
    volume: &FloatParam,
    setter: Option<&ParamSetter>,
    master: &mut MasterVolume,
) -> Response {
    ui.label(MASTER_LABEL);

    match setter {
        Some(setter) => volume_rail(ui, volume, setter, master),
        None => ui
            .add_enabled_ui(false, |ui| {
                let (_, response) = allocate_rail(ui, Sense::hover());
                paint_rail(ui, volume, &response);
                response
            })
            .inner,
    }
}

/// The rail, and the one gesture over it.
///
/// Three things reach the host from here and they are three calls, not one:
/// the press says a move has begun, every frame that moves the value sets
/// it, and letting go says the move is over. The press and the release are
/// read off the *primary* button rather than off egui's drag — which is what
/// closes a bare click's gesture in the frame the button comes back up (see
/// the module header) — and they are bracketed around the value and not the
/// other way about, because a host that is told a value before it is told a
/// gesture began records the value as an edit rather than as a move.
///
/// The rail is **absolute**: the value is where along it the pointer is, so
/// a click lands on the value under the pointer and a drag follows the hand.
/// A drag that has left the rail keeps being answered here (egui goes on
/// reporting the widget the button went down on) and the ends of the range
/// are where it clamps, which is what lets the hand overshoot a rail 96
/// points wide and still arrive at silence or at +12 dB.
fn volume_rail(
    ui: &mut Ui,
    volume: &FloatParam,
    setter: &ParamSetter,
    master: &mut MasterVolume,
) -> Response {
    if master.typing() {
        return volume_entry(ui, volume, setter, master);
    }

    let (rect, response) = allocate_rail(ui, Sense::click_and_drag());

    // The primary button alone: the other one opens the box below, and a
    // press of it over the rail must not move the volume on its way there.
    let pressed = response.is_pointer_button_down_on() && primary_down(ui);

    if pressed && !master.held {
        setter.begin_set_parameter(volume);
        master.held = true;
    }

    if let (true, Some(pointer)) = (pressed, response.interact_pointer_pos()) {
        set_value(volume, setter, emath::remap_clamp(pointer.x, rect.x_range(), 0.0..=1.0));
    }

    // Whether or not the pointer ended up over the rail, and whether or not
    // the frame it came up in was the frame it went down in: the gesture is
    // this module's to close, so the button going back up closes it.
    if master.held && !primary_down(ui) {
        setter.end_set_parameter(volume);
        master.held = false;
    }

    // After the release above, so that the one hand that could ask for both
    // at once — the other button clicked without letting the first one go —
    // does not leave the box open over a gesture nothing can close.
    if response.secondary_clicked() && !master.held {
        master.open();
    }

    paint_rail(ui, volume, &response);
    response
}

/// The rail as a box to type a value into, which a right-click opened.
///
/// A right-click and not a click or a double click, because every press of
/// the primary button over the rail *is* a value: the box has to be reached
/// by a gesture that does not move the volume on the way in, or Escape could
/// not put the volume back where it was.
///
/// Enter or a click elsewhere takes what was typed, Escape abandons it, and
/// a figure the parameter cannot read is simply not taken — the lane list's
/// own volume field refuses a typed value the same way
/// (`editor/lanes.rs`), and the parameter's parser is the same one the host
/// uses, so "-6", "-6 dB" and "3db" all land.
///
/// What it takes is bracketed by the begin and end a drag sends, because a
/// typed value is an automation gesture too: a host told of a value outside
/// one would record it as an edit of the automation lane rather than as a
/// move of the knob.
fn volume_entry(
    ui: &mut Ui,
    volume: &FloatParam,
    setter: &ParamSetter,
    master: &mut MasterVolume,
) -> Response {
    let typed = master.typed.as_mut().expect("the box is open");

    let response = ui.add(
        TextEdit::singleline(typed)
            // The bar's one geometry: a box that sized itself to what had
            // been typed into it would move the switch and `+ lane` along
            // the bar as the value grew.
            .min_size(Vec2::new(theme::TOP_BAR_VOLUME_WIDTH, theme::CONTROL_HEIGHT))
            .desired_width(theme::TOP_BAR_VOLUME_WIDTH)
            .margin(theme::CONTROL_PADDING)
            .align(Align2::CENTER_CENTER)
            .clip_text(true)
            // The figure the box replaced, as its hint: the box opens empty,
            // so the first thing typed is the whole of the new value, and
            // the volume is still readable while it is being typed over.
            .hint_text(volume.to_string()),
    );

    if std::mem::take(&mut master.focus) {
        response.request_focus();
    }

    if response.lost_focus() {
        // Escape leaves a text box in egui by clearing the focus, so it
        // arrives as the same frame a taken box does and is told apart by
        // the key itself — the rename box in `editor/lanes.rs` reads its own
        // the same way, and so does egui's `DragValue`.
        let escaped = ui.input(|input| input.key_pressed(egui::Key::Escape));
        let typed = master.typed.take().unwrap_or_default();
        master.close();

        if let (false, Some(normalized)) =
            (escaped, volume.string_to_normalized_value(typed.trim()))
        {
            setter.begin_set_parameter(volume);
            set_value(volume, setter, normalized);
            setter.end_set_parameter(volume);
        }
    }

    response
}

/// The room the rail takes in the bar, and the gestures it answers there.
///
/// [`theme::TOP_BAR_VOLUME_WIDTH`] by [`theme::CONTROL_HEIGHT`], like the
/// switch and `+ lane` beside it: the bar is exactly one control tall
/// ([`BAR_HEIGHT`]) and its three controls are fixed widths, so what the
/// window's width decides is only how much of the status beside them is
/// readable.
fn allocate_rail(ui: &mut Ui, sense: Sense) -> (Rect, Response) {
    ui.allocate_exact_size(Vec2::new(theme::TOP_BAR_VOLUME_WIDTH, theme::CONTROL_HEIGHT), sense)
}

/// Draws the rail: the control's box, the fill the value has reached inside
/// it, and the figure over both.
///
/// Every colour is the theme's own ([`theme::visuals`]) rather than this
/// module's: the box is a control's grey and lightens under the pointer and
/// darkens under a press exactly as the switch beside it does, the hairline
/// round it is [`theme::GRID_LINE`], and the fill is the colour a lit
/// control takes (`selection.bg_fill`, the dimmed [`theme::ACCENT`]) — which
/// is what makes a rail nobody has styled read as part of the same window.
///
/// The fill is a whole rail's worth of rounded rectangle under a clip the
/// value's width, and not a rectangle that width: a fill of its own would
/// round its right-hand edge in the middle of the rail and read as a pill
/// sliding along it rather than as a level filling it.
///
/// The figure goes inside, centred and truncated, and in the theme's primary
/// text — egui paints a non-interactive widget's text in the dimmer of the
/// two tones, which is right for the "Master" label beside the rail and
/// wrong for the value in it.
fn paint_rail(ui: &mut Ui, volume: &FloatParam, response: &Response) {
    let rect = response.rect;
    let visuals = *ui.style().interact(response);

    let edge = visuals.bg_stroke;
    ui.painter().rect(rect, theme::CONTROL_CORNER, visuals.bg_fill, edge, StrokeKind::Inside);

    // `max`-then-`min` is the NaN guard, as `voices.rs` does it: `clamp`
    // hands a NaN straight back, and a NaN width would reach the clip rect.
    #[allow(clippy::manual_clamp)]
    let filled = volume.modulated_normalized_value().max(0.0).min(1.0) * rect.width();
    if filled > 0.0 {
        let clip = Rect::from_min_size(rect.min, Vec2::new(filled, rect.height()));
        ui.painter().with_clip_rect(clip).rect_filled(
            rect,
            theme::CONTROL_CORNER,
            ui.visuals().selection.bg_fill,
        );
        // The hairline again, because the fill was painted over the half of
        // it that is inside the box.
        ui.painter().rect_stroke(rect, theme::CONTROL_CORNER, edge, StrokeKind::Inside);
    }

    ui.place(
        rect.shrink2(theme::CONTROL_PADDING),
        Label::new(RichText::new(volume.to_string()).color(theme::TEXT))
            .truncate()
            .halign(Align::Center)
            .selectable(false),
    );
}

/// Asks the host for `normalized`, unless that is the value the parameter
/// already has.
///
/// Set *on change*, which is what keeps a held slider quiet: a host records
/// what it is told, and a frame in which nothing moved has nothing to say.
/// Asked as the plain value and not the normalized one, so the value the
/// host ends up with is one the parameter could have reached on its own
/// ([`ParamSetter::set_parameter`] converts it back).
///
/// The clamp is this module's last line rather than its only one — the rail
/// clamps where it maps the pointer and the parameter clamps what it is
/// given — and it is `max`-then-`min`, so a position that arrived as a NaN
/// becomes silence instead of propagating.
fn set_value(volume: &FloatParam, setter: &ParamSetter, normalized: f32) {
    // `max`-then-`min` for the reason every clamp here is that way round:
    // `f32::max` returns the non-NaN operand, so a position that arrived as
    // a NaN becomes silence rather than a NaN gain.
    #[allow(clippy::manual_clamp)]
    let value = volume.preview_plain(normalized.max(0.0).min(1.0));
    if value != volume.modulated_plain_value() {
        setter.set_parameter(volume, value);
    }
}

/// Whether the primary mouse button is down anywhere this frame.
///
/// Asked of the frame's input and not of the rail's response, because the
/// release this closes a gesture on may well arrive with the pointer
/// somewhere else entirely.
fn primary_down(ui: &Ui) -> bool {
    ui.input(|input| input.pointer.primary_down())
}

/// The "bitstep Sample pad" switch: lit from the model, written through it.
///
/// A framed button rather than egui's `toggle_value`, which hides its frame
/// while it is off: a switch that reads as a label until the pointer finds
/// it is a switch nobody finds. Lit, it takes the theme's selection colour
/// with its text still at full brightness ([`theme::visuals`]).
fn sample_pad_switch(ui: &mut Ui, model: &mut Model) -> Response {
    let on = model.sample_pad_switch();

    let response = ui
        .add(
            egui::Button::new(SAMPLE_PAD_LABEL)
                .selected(on)
                // The label fits ([`theme::TOP_BAR_SWITCH_WIDTH`]); this is
                // for fonts and scalings other than the ones the tests use.
                .truncate()
                .min_size(Vec2::new(theme::TOP_BAR_SWITCH_WIDTH, theme::CONTROL_HEIGHT)),
        )
        .on_hover_text(SAMPLE_PAD_HOVER);

    if response.clicked() {
        model.set_sample_pad_switch(!on);
        // One gesture, one undo step — and the switch is part of what an
        // undo entry holds.
        model.commit();
    }

    response
}

/// `+ lane`: one lane at the end of the list, named, noted and coloured by
/// [`Model::add_lane`].
///
/// Disabled at [`MAX_LANES`], where the writer would refuse anyway: a button
/// that answers nothing is worse than one that is plainly not available, and
/// the hover text says which it is.
fn add_lane_button(ui: &mut Ui, model: &mut Model) -> Response {
    let room = model.lanes().len() < MAX_LANES as usize;

    let response = ui
        .add_enabled(
            room,
            egui::Button::new(ADD_LANE_LABEL)
                .min_size(Vec2::new(theme::TOP_BAR_ADD_WIDTH, theme::CONTROL_HEIGHT)),
        )
        .on_disabled_hover_text(ADD_LANE_FULL_HOVER);

    if response.clicked() && model.add_lane().is_some() {
        model.commit();
    }

    response
}

/// The embedded-size warning, in the room the three controls left over:
/// the figure in [`theme::WARN`] with [`status::EMBED_WARN_HOVER`] behind
/// it, or nothing at all while the project is nowhere near the threshold.
///
/// Truncated and never wrapped, like a row's fields: the bar is exactly one
/// control tall ([`BAR_HEIGHT`]) and a second line of text would push the
/// lane list down by however much the window had been narrowed. On the
/// narrowest window the three controls leave room for only the figure
/// itself, which is the part worth seeing — the rest of the line, and the
/// whole of what it means, are on the hover.
///
/// Read off the model each frame rather than kept: the total is a sum over
/// at most [`MAX_LANES`] samples ([`Model::embedded_bytes`]), which is
/// cheaper than a figure a writer could leave stale.
fn embed_warning_label(ui: &mut Ui, model: &Model) -> Option<Response> {
    let warning = status::embed_warning(model.embedded_bytes())?;

    Some(
        ui.add(Label::new(RichText::new(warning).color(theme::WARN)).truncate())
            .on_hover_text(status::EMBED_WARN_HOVER),
    )
}

/// What the last drop had to leave behind (or the lanes "give each its own
/// output" could not move), in the slot the size warning otherwise has: the
/// count in [`theme::WARN`] with its hover — [`status::DROP_FULL_HOVER`],
/// [`status::OUTPUTS_FULL_HOVER`] — behind it.
///
/// Drawn exactly like the warning beside it — truncated, never wrapped, one
/// control tall — because the bar's height is the lane list's top edge and a
/// message that pushed the list down would be worse than a message cut
/// short. The count is the part worth seeing and it is at the front of the
/// line; the rest is on the hover.
fn drop_message_label(ui: &mut Ui, message: &str, hover: &str) -> Response {
    ui.add(Label::new(RichText::new(message).color(theme::WARN)).truncate()).on_hover_text(hover)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, MutexGuard};

    use std::path::Path;

    use egui::{Pos2, Rect, pos2, vec2};
    use nice_plug::context::PluginApi;
    use nice_plug::context::gui::{GuiContext, GuiContextInner};
    use nice_plug::params::Param;
    use nice_plug::params::internals::ParamPtr;
    use nice_plug::plugin::PluginState;

    use super::*;
    use crate::editor::tests::fresh_params;
    use crate::editor::{DEFAULT_WIDTH, MIN_HEIGHT, MIN_WIDTH};
    use crate::model::{
        DEFAULT_LANE_COUNT, SAMPLE_PAD_SWITCH_DEFAULT, SampleRef, lock_model,
    };
    use crate::sample::{MAX_SAMPLE_BYTES, SampleData};
    use crate::state::EMBED_WARN_BYTES;
    use crate::state::LANE_PALETTE;
    use crate::{ChopdropParams, VOLUME_MAX_DB};

    /// A host that records what the GUI asked it to do to a parameter, in
    /// the order it asked.
    ///
    /// The one thing this module cannot be tested without: the master
    /// volume is written by the *host* and not by the plugin, so a slider
    /// with nothing behind it would prove only that a rail was painted.
    #[derive(Default)]
    struct RecordingHost {
        /// `begin` / `set <normalized>` / `end`, which is the shape of one
        /// automation gesture.
        calls: Mutex<Vec<String>>,
    }

    impl RecordingHost {
        fn push(&self, call: String) {
            self.calls.lock().expect("the recording host is never poisoned").push(call);
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("the recording host is never poisoned").clone()
        }

        /// The normalized values it was told to set, in order.
        fn values(&self) -> Vec<f32> {
            self.calls()
                .iter()
                .filter_map(|call| call.strip_prefix("set "))
                .map(|value| value.parse().expect("a normalized value"))
                .collect()
        }
    }

    impl GuiContextInner for RecordingHost {
        fn plugin_api(&self) -> PluginApi {
            PluginApi::Clap
        }

        unsafe fn raw_begin_set_parameter(&self, _param: ParamPtr) {
            self.push("begin".to_string());
        }

        unsafe fn raw_set_parameter_normalized(&self, _param: ParamPtr, normalized: f32) {
            self.push(format!("set {normalized}"));
        }

        unsafe fn raw_end_set_parameter(&self, _param: ParamPtr) {
            self.push("end".to_string());
        }

        fn get_state(&self) -> PluginState {
            PluginState { version: String::new(), params: BTreeMap::new(), fields: BTreeMap::new() }
        }

        fn set_state(&self, _state: PluginState) {}

        fn request_restart(&self) {}
    }

    /// The bar under test: a themed context the size of the narrowest window
    /// the host allows, an instance's parameters and lanes, and a host
    /// listening for parameter gestures.
    ///
    /// Display-free — egui lays out and tessellates without a window — which
    /// is what makes the editor's gestures testable without a display
    /// at all.
    struct Bar {
        ctx: egui::Context,
        params: Arc<ChopdropParams>,
        host: Arc<RecordingHost>,
        gui_ctx: GuiContext,
        /// How wide the window is drawn: the narrowest it may be dragged,
        /// unless a test asks for the width it opens at ([`Bar::wide`]).
        width: f32,
        /// Where the bar's three controls landed in the last frame, in the
        /// order they are drawn.
        rects: [Rect; 3],
        /// Whether each of them answered gestures in the last frame.
        enabled: [bool; 3],
        /// Where the embedded-size warning landed in the last frame, if the
        /// bar drew one at all.
        warning: Option<Rect>,
        /// Where a drop's message landed in the last frame, if the editor
        /// handed one in ([`Bar::message`]).
        drop_message: Option<Rect>,
        /// What the editor is saying about the last drop, as
        /// [`crate::editor::drops::DropIntake::message`] would hand it over:
        /// `None` in every frame but the few after a drop onto a full list.
        message: Option<String>,
        /// The rail's own half-made gesture, which the window holds between
        /// frames ([`MasterVolume`]) and which this harness holds for the
        /// same reason: a press and the release that closes it are two
        /// frames.
        master: MasterVolume,
        /// Every text painted in the last frame, in the order it was
        /// painted.
        painted: Vec<String>,
        /// How much of the rail the fill reached in the last frame: the clip
        /// the fill was painted under ([`paint_rail`]), which is the rail's
        /// left-hand end and the value's share of its width.
        fill: Option<Rect>,
    }

    impl Bar {
        fn new() -> Bar {
            let ctx = egui::Context::default();
            theme::apply(&ctx);

            let host = Arc::new(RecordingHost::default());
            let gui_ctx = GuiContext::new(Arc::clone(&host) as Arc<dyn GuiContextInner>);

            Bar {
                ctx,
                params: fresh_params(),
                host,
                gui_ctx,
                width: MIN_WIDTH,
                rects: [Rect::NOTHING; 3],
                enabled: [false; 3],
                warning: None,
                drop_message: None,
                message: None,
                master: MasterVolume::default(),
                painted: Vec::new(),
                fill: None,
            }
        }

        /// The same bar in the window the host opens
        /// ([`crate::editor::DEFAULT_WIDTH`]) rather than in the narrowest
        /// one it may be dragged to.
        ///
        /// The one thing in the bar that depends on which: the three
        /// controls are fixed widths and the warning takes what they leave,
        /// so at the minimum width it is truncated to a few characters and
        /// at the default width it is drawn whole.
        fn wide() -> Bar {
            Bar { width: DEFAULT_WIDTH, ..Bar::new() }
        }

        /// Puts a sample of `byte_len` file bytes on each of the first
        /// `lanes` lanes, as a project that size would restore.
        ///
        /// One sample cloned onto each of them, which is exactly what
        /// duplicating a lane does and what the save counts once per lane
        /// ([`Model::embedded_bytes`]) — so a 64 MiB file on three lanes is
        /// 192 MiB of project and one allocation in this test. The bytes are
        /// zeroed and never read: the accounting is about how big the file
        /// is.
        fn load(&mut self, lanes: usize, byte_len: usize) {
            let mut model = self.model();
            let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 16].into() };
            let path = Path::new("/home/dev/kit/long take.wav");
            let sample = SampleRef::new(model.handle(), path, vec![0; byte_len], data);
            let ids: Vec<u32> = model.lanes().iter().take(lanes).map(|lane| lane.id).collect();
            for id in ids {
                assert!(model.set_sample(id, sample.clone()), "a lane the model just listed");
            }
        }

        /// One frame of the window, with `events` as its input.
        ///
        /// `host` is whether the editor has been handed its `GuiContext`
        /// yet — true in every frame nice-plug-egui ever draws, and false in
        /// the one test that asks what the bar does without one.
        fn frame(&mut self, events: Vec<egui::Event>, host: bool) {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(self.width, MIN_HEIGHT))),
                events,
                ..Default::default()
            };

            let volume = &self.params.volume;
            let setter = self.gui_ctx.param_setter();
            let setter = host.then_some(&setter);
            let message = self.message.as_deref().map(|text| (text, status::DROP_FULL_HOVER));
            let mut model = lock_model(self.params.lanes.model());

            let master = &mut self.master;

            let mut drawn = None;
            let output = self.ctx.run_ui(input, |ui| {
                let bar = top_bar(ui, volume, setter, &mut model, message, master);
                drawn = Some((
                    [bar.volume.rect, bar.sample_pad.rect, bar.add_lane.rect],
                    [bar.volume.enabled(), bar.sample_pad.enabled(), bar.add_lane.enabled()],
                    bar.embed_warning.map(|warning| warning.rect),
                    bar.drop_message.map(|message| message.rect),
                ));
            });
            drop(model);

            let (rects, enabled, warning, drop_message) =
                drawn.expect("the bar is drawn every frame");
            self.rects = rects;
            self.enabled = enabled;
            self.warning = warning;
            self.drop_message = drop_message;
            self.painted = output
                .shapes
                .iter()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                    _ => None,
                })
                .collect();
            // The one shape in the bar painted in the colour a lit control
            // takes, under the clip that is the point of it.
            self.fill = output.shapes.iter().find_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) if rect.fill == theme::dim(theme::ACCENT, 0.55) => {
                    Some(clipped.clip_rect)
                }
                _ => None,
            });
            // `TexturesDelta` panics when dropped with the first frame's
            // font upload still in it.
            output.drop_without_applying_deltas();
        }

        /// A frame with no input, as a window that is merely open draws.
        fn idle(&mut self) {
            self.frame(Vec::new(), true);
        }

        /// A press and a release over `at`, one frame each, as a window
        /// receives a click.
        fn click(&mut self, at: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(at), pointer(at, true)], true);
            self.frame(vec![pointer(at, false)], true);
        }

        /// A press and a release of the *other* button over `at`, which is
        /// how a value is typed into the rail ([`volume_entry`]).
        fn right_click(&mut self, at: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(at), secondary(at, true)], true);
            self.frame(vec![secondary(at, false)], true);
        }

        /// A press at `from`, a frame of movement to `to`, and a release
        /// there: the three frames egui needs to call something a drag
        /// rather than a click (it decides by how far the pointer went).
        fn drag(&mut self, from: Pos2, to: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(from), pointer(from, true)], true);
            self.frame(vec![egui::Event::PointerMoved(to)], true);
            self.frame(vec![pointer(to, false)], true);
        }

        fn model(&self) -> MutexGuard<'_, Model> {
            lock_model(self.params.lanes.model())
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

    /// A project carrying no more sample than it should says nothing about
    /// it: the warning is absent and not merely quiet.
    #[test]
    fn a_project_within_the_limit_is_not_warned_about() {
        let mut bar = Bar::wide();
        bar.load(1, MAX_SAMPLE_BYTES);
        bar.idle();

        assert!(bar.model().embedded_bytes() < EMBED_WARN_BYTES, "64 MiB is well inside it");
        assert_eq!(bar.warning, None, "nothing was drawn to warn with");
        assert!(
            !bar.painted.iter().any(|text| text.contains("MiB of samples")),
            "a figure was drawn anyway; got {:?}",
            bar.painted
        );
    }

    /// Past the threshold the bar says how much sample the project is
    /// carrying, beside its three controls, with the explanation on the
    /// hover.
    #[test]
    fn a_project_over_the_threshold_is_warned_about_beside_the_controls() {
        let mut bar = Bar::wide();
        // Three lanes of one 64 MiB file: 192 MiB of project, which is what
        // a save would write and what the warning is set in.
        bar.load(3, MAX_SAMPLE_BYTES);
        bar.idle();

        assert_eq!(bar.model().embedded_bytes(), 3 * MAX_SAMPLE_BYTES);
        let expected = status::embed_warning(3 * MAX_SAMPLE_BYTES).expect("past the threshold");
        assert_eq!(expected, "192 MiB of samples");
        assert!(
            bar.painted.contains(&expected),
            "the figure was not drawn whole; got {:?}",
            bar.painted
        );

        let warning = bar.warning.expect("the warning was drawn");
        assert!(
            warning.min.x > bar.rects[2].max.x,
            "the warning took room from `+ lane` rather than the room left over"
        );
        assert!(warning.max.x <= DEFAULT_WIDTH, "and it stayed inside the window");
    }

    /// A drop that lost files is said in the same slot the warning uses,
    /// beside the three controls and with its sentence on the hover.
    #[test]
    fn a_drop_that_lost_files_is_said_beside_the_controls() {
        let mut bar = Bar::wide();
        bar.message = status::drop_overflow(3);
        bar.idle();

        let message = bar.message.clone().expect("three files lost");
        assert_eq!(message, "3 files didn't fit");
        assert!(
            bar.painted.contains(&message),
            "the count was not drawn whole; got {:?}",
            bar.painted
        );

        let drawn = bar.drop_message.expect("the message was drawn");
        assert!(
            drawn.min.x > bar.rects[2].max.x,
            "the message took room from `+ lane` rather than the room left over"
        );
        assert!(drawn.max.x <= DEFAULT_WIDTH, "and it stayed inside the window");
    }

    /// While it is up it has the slot to itself: the project's size is a
    /// standing fact and will still be there when the message has gone.
    #[test]
    fn a_drops_message_has_the_slot_while_it_is_up() {
        let mut bar = Bar::wide();
        bar.load(3, MAX_SAMPLE_BYTES);
        bar.message = status::drop_overflow(1);
        bar.idle();

        assert!(bar.drop_message.is_some(), "the drop's message was drawn");
        assert_eq!(bar.warning, None, "and the size warning gave up the slot");
        assert!(
            !bar.painted.iter().any(|text| text.contains("MiB of samples")),
            "the figure was drawn alongside it; got {:?}",
            bar.painted
        );

        // And it comes back the moment the editor stops saying it, since the
        // figure is a read of the model and not a flag.
        bar.message = None;
        bar.idle();
        assert!(bar.warning.is_some(), "the size warning has the slot again");
        assert_eq!(bar.drop_message, None);
    }

    /// A lane losing its sample takes the warning with it: the figure is a
    /// read of the model every frame and not a flag a writer sets.
    #[test]
    fn clearing_a_lane_takes_the_warning_away_again() {
        let mut bar = Bar::wide();
        bar.load(3, MAX_SAMPLE_BYTES);
        bar.idle();
        assert!(bar.warning.is_some(), "the project is over the threshold");

        let ids: Vec<u32> = bar.model().lanes().iter().map(|lane| lane.id).collect();
        for id in ids.iter().take(2) {
            assert!(bar.model().clear_sample(*id), "a lane the model just listed");
        }
        bar.idle();

        assert_eq!(bar.model().embedded_bytes(), MAX_SAMPLE_BYTES, "one lane still loaded");
        assert_eq!(bar.warning, None, "and the bar has stopped saying anything");
    }

    /// The bar says what it holds: the volume with its value, the switch by
    /// its whole name, and the add button.
    #[test]
    fn the_bar_draws_the_volume_the_switch_and_the_add_button() {
        let mut bar = Bar::new();
        bar.idle();

        for label in [MASTER_LABEL, SAMPLE_PAD_LABEL, ADD_LANE_LABEL] {
            assert!(
                bar.painted.contains(&label.to_string()),
                "{label:?} was not drawn whole; got {:?}",
                bar.painted
            );
        }
        assert!(
            bar.painted.contains(&"0.00 dB".to_string()),
            "the master volume reads unity on a fresh instance; got {:?}",
            bar.painted
        );
    }

    /// The three controls fit the narrowest window the host will allow, in
    /// their fixed order, inside a bar exactly [`BAR_HEIGHT`] tall.
    ///
    /// The widths are the theme's, but how wide a label comes out is egui's
    /// — so this is a real layout pass rather than arithmetic over the
    /// constants.
    #[test]
    fn the_bars_controls_fit_the_narrowest_window() {
        let mut bar = Bar::new();
        bar.idle();

        let [volume, switch, add] = bar.rects;

        assert!(volume.right() <= switch.left(), "the volume is left of the switch");
        assert!(switch.right() <= add.left(), "`+ lane` is last");
        assert!(
            add.right() <= MIN_WIDTH - theme::PANEL_PADDING,
            "`+ lane` is clipped off the narrowest window: {add:?}"
        );
        for control in bar.rects {
            assert!(control.top() >= 0.0, "{control:?} is above the window");
            assert!(control.bottom() <= BAR_HEIGHT, "{control:?} reaches into the lane list");
            assert!(control.width() > 0.0 && control.height() > 0.0, "{control:?}");
        }
        assert_eq!(volume.width(), theme::TOP_BAR_VOLUME_WIDTH, "the rail is a fixed width");
        assert_eq!(volume.height(), theme::CONTROL_HEIGHT, "one control tall, as its neighbours");
        assert_eq!(switch.width(), theme::TOP_BAR_SWITCH_WIDTH);
        assert_eq!(add.width(), theme::TOP_BAR_ADD_WIDTH);
        assert!(bar.enabled.iter().all(|enabled| *enabled), "every control answers");
    }

    /// `+ lane` on a fresh instance adds the seventeenth lane as
    /// [`Model::add_lane`] makes it — "Lane 17", note 52, palette colour 6
    /// (id 16 wrapped past the ten) — and the click is one undo step, not none and
    /// not one per frame.
    #[test]
    fn clicking_add_lane_adds_the_seventeenth_lane_through_the_writer_api() {
        let mut bar = Bar::new();
        bar.idle();
        assert_eq!(bar.model().lanes().len(), DEFAULT_LANE_COUNT as usize);
        assert!(!bar.model().can_undo(), "nothing has been done yet");

        let add = bar.rects[2].center();
        bar.click(add);

        let model = bar.model();
        assert_eq!(model.lanes().len(), DEFAULT_LANE_COUNT as usize + 1, "one lane per click");

        let added = model.lanes().last().expect("the lane just added");
        assert_eq!(added.name, "Lane 17");
        assert_eq!(added.note, 52, "one above the sixteenth default's 51");
        assert_eq!(added.colour, LANE_PALETTE[6], "id 16 wrapped past the ten palette colours");
        assert!(added.sample.is_none(), "an added lane is empty");

        assert!(model.can_undo(), "the click is one undo step");
        assert_eq!(model.undo_depth(), 1);
    }

    /// The switch writes the model and is undoable; a second click puts it
    /// back.
    #[test]
    fn clicking_the_switch_toggles_it_and_is_one_step_each_way() {
        let mut bar = Bar::new();
        bar.idle();
        assert_eq!(bar.model().sample_pad_switch(), SAMPLE_PAD_SWITCH_DEFAULT);

        let switch = bar.rects[1].center();

        // A new instance starts with it on (SAMPLE_PAD_SWITCH_DEFAULT), so
        // the first click turns it off and the second back on.
        bar.click(switch);
        assert!(!bar.model().sample_pad_switch(), "the switch did not go off");
        assert_eq!(bar.model().undo_depth(), 1);

        bar.click(switch);
        assert!(bar.model().sample_pad_switch(), "the switch did not go on again");
        assert_eq!(bar.model().undo_depth(), 2);
    }

    /// The master volume is not written here: the slider asks the *host* to
    /// set the parameter, bracketed by the begin and end an automatable
    /// gesture needs, so that a drag of it is recorded as one move of one
    /// knob. The rail is absolute, so a drag that ends at its right-hand end
    /// asks for the top of the range — +12 dB.
    #[test]
    fn the_volume_slider_asks_the_host_to_set_the_parameter() {
        let mut bar = Bar::new();
        bar.idle();
        assert!(bar.host.calls().is_empty(), "drawing a slider is not a gesture");

        let rail = bar.rects[0];
        let middle = rail.center();
        bar.drag(middle, pos2(rail.right(), middle.y));

        let calls = bar.host.calls();
        assert_eq!(calls.first().map(String::as_str), Some("begin"), "got {calls:?}");
        assert_eq!(calls.last().map(String::as_str), Some("end"), "got {calls:?}");
        assert_eq!(calls.iter().filter(|call| *call == "begin").count(), 1, "got {calls:?}");
        assert_eq!(calls.iter().filter(|call| *call == "end").count(), 1, "got {calls:?}");

        let values = bar.host.values();
        assert!(!values.is_empty(), "the host was never asked to set a value: {calls:?}");
        assert!(
            (values[0] - 0.5).abs() < 0.01,
            "the rail is absolute: its middle is half way along, not {:?}",
            values[0]
        );
        assert_eq!(
            values.last().copied(),
            Some(1.0),
            "the right-hand end of the rail is the top of the range; got {values:?}"
        );

        let top = nice_plug::util::gain_to_db(bar.params.volume.preview_plain(1.0));
        assert!((top - VOLUME_MAX_DB).abs() < 0.01, "the top of the range is +12 dB, not {top}");
        assert_eq!(bar.model().undo_depth(), 0, "a host parameter is not a model gesture");
    }

    /// A bare click is a whole gesture: the volume jumps to where the pointer
    /// landed, and the host is told the move began, where it went and that it
    /// is over — in that order and once each. This is the one `ParamSlider`
    /// could not do: it opens its gesture on the press and closes it on a
    /// drag that a bare click never becomes.
    #[test]
    fn a_bare_click_on_the_rail_is_one_whole_gesture() {
        let mut bar = Bar::new();
        bar.idle();

        let rail = bar.rects[0];
        bar.click(pos2(rail.right(), rail.center().y));

        let calls = bar.host.calls();
        assert_eq!(calls.len(), 3, "a click is a begin, a set and an end; got {calls:?}");
        assert_eq!(calls[0], "begin", "got {calls:?}");
        assert_eq!(calls[2], "end", "the gesture was left open; got {calls:?}");
        assert_eq!(
            bar.host.values(),
            vec![1.0],
            "the click landed on the right-hand end of the rail; got {calls:?}"
        );
        assert!(!bar.master.held, "and nothing is owed the host after it");
    }

    /// The release that closes the gesture need not land on the rail: a hand
    /// that has dragged the volume past the window's edge lets go wherever
    /// it is, and the value it dragged to is the end of the range.
    #[test]
    fn a_release_away_from_the_rail_still_closes_the_gesture() {
        let mut bar = Bar::new();
        bar.idle();

        let rail = bar.rects[0];
        let away = pos2(rail.right() + 400.0, rail.bottom() + 300.0);
        bar.drag(rail.center(), away);

        let calls = bar.host.calls();
        assert_eq!(calls.iter().filter(|call| *call == "begin").count(), 1, "got {calls:?}");
        assert_eq!(calls.iter().filter(|call| *call == "end").count(), 1, "got {calls:?}");
        assert_eq!(calls.last().map(String::as_str), Some("end"), "got {calls:?}");
        assert_eq!(
            bar.host.values().last().copied(),
            Some(1.0),
            "a drag past the right-hand end clamps there; got {calls:?}"
        );
        assert!(!bar.master.held);
    }

    /// The rail says its value twice, as a slider should: the figure drawn
    /// inside it, and how far along it the fill has reached.
    #[test]
    fn the_rail_fills_as_far_as_the_value_and_says_it_inside() {
        let mut bar = Bar::new();
        bar.idle();

        let rail = bar.rects[0];
        let unity = bar.params.volume.modulated_normalized_value();
        assert!(unity > 0.0 && unity < 1.0, "a fresh instance is somewhere along the rail");

        let fill = bar.fill.expect("the fill was drawn");
        assert_eq!(fill.left(), rail.left(), "the fill starts at the rail's own end");
        assert!(
            (fill.width() - rail.width() * unity).abs() < 1.0,
            "the fill reached {} of a {} rail, not the value's {unity}",
            fill.width(),
            rail.width()
        );
        assert!(bar.painted.contains(&"0.00 dB".to_string()), "got {:?}", bar.painted);
    }

    /// The rail has room for the longest figure it can draw, since the figure
    /// is inside it: at the bottom of its range the volume reads "-60.00 dB",
    /// and a rail that truncated that would leave the bar's one number as the
    /// one thing in the window a user could not read.
    #[test]
    fn the_rail_has_room_for_the_whole_of_the_lowest_value() {
        let mut bar = Bar::new();
        // One frame, because egui has no fonts to measure with until it has
        // laid something out at least once.
        bar.idle();

        let lowest = bar.params.volume.normalized_value_to_string(0.0, true);
        assert_eq!(lowest, "-60.00 dB", "the bottom of the range");

        // The font a label in this window is laid out in, asked of the real
        // text engine: the widths here are the theme's, but how wide a
        // figure comes out in them is egui's.
        let body = bar.ctx.global_style().text_styles[&egui::TextStyle::Body].clone();
        let drawn =
            bar.ctx.fonts_mut(|fonts| fonts.layout_no_wrap(lowest.clone(), body, theme::TEXT));
        let room = theme::TOP_BAR_VOLUME_WIDTH - 2.0 * theme::CONTROL_PADDING.x;
        assert!(
            drawn.rect.width() <= room,
            "{lowest:?} draws {} points wide and the rail leaves {room} for it",
            drawn.rect.width()
        );
    }

    /// The other button turns the rail into a box to type into, and moves
    /// nothing on its way there — which is the whole reason it is the other
    /// button ([`volume_entry`]). The figure it replaced is still readable
    /// behind the cursor, as the box's hint.
    #[test]
    fn a_right_click_opens_a_box_without_moving_the_volume() {
        let mut bar = Bar::new();
        bar.idle();

        bar.right_click(bar.rects[0].center());
        bar.idle();

        assert!(bar.master.typing(), "the rail is still a rail");
        assert!(bar.host.calls().is_empty(), "the host was written to: {:?}", bar.host.calls());
        assert!(bar.painted.contains(&"0.00 dB".to_string()), "got {:?}", bar.painted);
        assert_eq!(
            bar.rects[0].width(),
            theme::TOP_BAR_VOLUME_WIDTH,
            "the box took the rail's room, so the bar did not move"
        );
    }

    /// A typed value is a gesture too, and the same three calls in the same
    /// order: the parameter's own parser reads it, so the units the lane
    /// fields take are the units this takes.
    #[test]
    fn a_typed_value_is_one_whole_gesture_too() {
        let mut bar = Bar::new();
        bar.idle();

        bar.right_click(bar.rects[0].center());
        // The frame the box is first drawn in asks for the keyboard, and the
        // frame after it is the first with the keyboard in it.
        bar.idle();
        bar.idle();
        bar.frame(vec![egui::Event::Text("-6 dB".to_string())], true);
        bar.frame(vec![key(egui::Key::Enter, true), key(egui::Key::Enter, false)], true);
        bar.idle();

        let calls = bar.host.calls();
        assert_eq!(calls.len(), 3, "typing is a begin, a set and an end; got {calls:?}");
        assert_eq!(calls[0], "begin", "got {calls:?}");
        assert_eq!(calls[2], "end", "got {calls:?}");

        let expected = bar
            .params
            .volume
            .string_to_normalized_value("-6 dB")
            .expect("the parameter reads its own unit");
        let asked = bar.host.values()[0];
        assert!((asked - expected).abs() < 0.001, "the host was asked for {asked}, not {expected}");

        assert!(!bar.master.typing(), "the box stayed open after it was taken");
        assert_eq!(bar.model().undo_depth(), 0, "a host parameter is not a model gesture");
    }

    /// Escape abandons the box and a figure the parameter cannot read is
    /// refused: either way nothing at all reaches the host, so the volume is
    /// exactly where it was before the box opened.
    #[test]
    fn an_abandoned_or_unreadable_value_never_reaches_the_host() {
        for (typed, pressed) in [("-6 dB", egui::Key::Escape), ("banana", egui::Key::Enter)] {
            let mut bar = Bar::new();
            bar.idle();

            bar.right_click(bar.rects[0].center());
            bar.idle();
            bar.idle();
            bar.frame(vec![egui::Event::Text(typed.to_string())], true);
            bar.frame(vec![key(pressed, true), key(pressed, false)], true);
            bar.idle();

            assert!(
                bar.host.calls().is_empty(),
                "{typed:?} reached the host: {:?}",
                bar.host.calls()
            );
            assert!(!bar.master.typing(), "{typed:?} left the box open");
            assert!(bar.painted.contains(&"0.00 dB".to_string()), "got {:?}", bar.painted);
        }
    }

    /// With no host context the volume is still readable and cannot be
    /// touched — the state the window would be in before nice-plug-egui has
    /// handed the editor its context.
    #[test]
    fn without_a_host_the_volume_is_shown_but_not_settable() {
        let mut bar = Bar::new();
        bar.frame(Vec::new(), false);

        assert!(!bar.enabled[0], "there is no host to set a parameter through");
        assert!(bar.enabled[1] && bar.enabled[2], "the model's own controls still answer");
        assert!(bar.painted.contains(&"0.00 dB".to_string()), "got {:?}", bar.painted);
        assert!(bar.host.calls().is_empty(), "nothing reached the host");
    }

    /// At [`MAX_LANES`] the button is drawn but disabled, and a click on it
    /// does nothing: the writer would refuse, and the list must not grow
    /// past the cap by way of the GUI.
    #[test]
    fn the_add_button_is_disabled_once_the_list_is_full() {
        let mut bar = Bar::new();
        {
            let mut model = bar.model();
            while model.lanes().len() < MAX_LANES as usize {
                assert!(model.add_lane().is_some(), "room up to the cap");
            }
        }

        bar.idle();
        assert_eq!(bar.rects[2].width(), theme::TOP_BAR_ADD_WIDTH, "the button is still drawn");
        assert!(!bar.enabled[2], "the full list leaves the button live");

        bar.click(bar.rects[2].center());
        assert_eq!(bar.model().lanes().len(), MAX_LANES as usize, "the cap held");
    }
}
