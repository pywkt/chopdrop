//! Undo and redo from the keyboard: the two keys the editor keeps back from
//! the host, taken as steps through the model's own history.
//!
//! The history itself is the model's, and so is every decision about it:
//! what a step contains, how deep the stacks go, that a new edit throws the
//! redone future away, and that a gesture which ended where it started is
//! no step at all ([`crate::model::Model::commit`]). Each gesture over the
//! window has already committed itself where it was applied — the release
//! of a drag in `editor/lanes.rs`, a click in `editor/menu.rs`, a dropped
//! file once the loader lands it. So nothing here is policy: this module
//! reads a frame's key events and asks the model to step.
//!
//! What it does decide is **when not to**. A lane's name is typed into this
//! window, and so is the master volume's value; while either box has the
//! keyboard, Ctrl+Z belongs to it: egui's own text box undoes the typing in
//! it, which is what a user reaching for the shortcut over a half-typed name
//! means by it. That is the `typing` argument of [`apply`], worked out by
//! [`typing`], and the reason it is an argument rather than a read of the
//! context is that it makes the whole dispatch testable without a window —
//! this module takes a slice of events and a model and nothing of egui's
//! layout.
//!
//! Two smaller details, both of them deliberate. Key *repeats* count, so
//! holding the shortcut walks back through the history the way it does in a
//! host. And a frame can carry more than one press — egui hands over
//! everything that arrived since the last frame at once — so each one is a
//! step, in the order it was pressed, rather than a frame being worth one
//! step however much was typed into it.

use egui::Event;

use crate::editor::keys::{self, HistoryKey};
use crate::model::Model;

/// Whether the keyboard belongs to a text box this frame — the `typing`
/// argument [`apply`] takes.
///
/// Any of three things makes it so: egui reports a text box focused; a
/// lane's rename box is open; or the master volume's rail has become its
/// entry box. The last two are asked of the boxes themselves and not only
/// of egui's focus, because a box asks for the focus in the frame it opens
/// and egui grants it a frame later — a Ctrl+Z pressed in between is meant
/// for the box the user has just opened, not for the edit before it.
pub fn typing(text_edit_focused: bool, renaming: bool, master_typing: bool) -> bool {
    text_edit_focused || renaming || master_typing
}

/// Takes the undo and redo steps a frame's `events` ask for, in the order
/// they were pressed, and reports how many of them found something to do.
///
/// `typing` is whether the keyboard belongs to a text box this frame — a
/// lane's rename field, or the value typed into the master volume's rail —
/// in which case the shortcut is that box's and no step is taken at all.
///
/// The count is for the tests and for a caller that wants to know whether
/// the window moved under the user; a step that had nowhere to go is not
/// counted, because the model reports that itself and asking for an undo
/// with an empty stack is simply nothing happening.
pub fn apply(events: &[Event], typing: bool, model: &mut Model) -> usize {
    if typing {
        return 0;
    }

    requested(events).filter(|step| take(*step, model)).count()
}

/// The steps `events` ask for, in order: every key press that
/// [`keys::history_key`] claims for the editor instead of passing it on to
/// the host.
///
/// Presses only. A release carries the same key and modifiers as the press
/// before it, so counting both would double every step.
fn requested(events: &[Event]) -> impl Iterator<Item = HistoryKey> + '_ {
    events.iter().filter_map(|event| match event {
        Event::Key { key, pressed: true, modifiers, .. } => keys::history_key(*key, *modifiers),
        _ => None,
    })
}

/// One step through the model's history, reporting whether there was one to
/// take.
fn take(step: HistoryKey, model: &mut Model) -> bool {
    match step {
        HistoryKey::Undo => model.undo(),
        HistoryKey::Redo => model.redo(),
    }
}

#[cfg(test)]
mod tests {
    use egui::{Key, Modifiers};

    use super::*;

    /// A held Ctrl, as X11 hands one to egui: both bits, which is what
    /// [`keys::history_key`] reads (`editor/keys.rs`).
    const CTRL: Modifiers = Modifiers::CTRL.plus(Modifiers::COMMAND);

    /// A key press, as egui-baseview hands one on.
    fn press(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
    }

    /// Its release.
    fn release(key: Key, modifiers: Modifiers) -> Event {
        Event::Key { key, physical_key: None, pressed: false, repeat: false, modifiers }
    }

    /// A model with one committed gesture behind it: the first lane's note
    /// moved from its own C1 to C3.
    fn edited() -> Model {
        let mut model = Model::new();
        assert!(model.set_note(0, 60));
        assert!(model.commit(), "the gesture changed something");
        model
    }

    /// The shape of the whole thing: Ctrl+Z steps back over the gesture and
    /// Ctrl+Shift+Z steps forward into it again.
    #[test]
    fn ctrl_z_steps_back_and_ctrl_shift_z_forward() {
        let mut model = edited();

        assert_eq!(apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
        assert_eq!(model.lane(0).unwrap().note, 36, "back to the lane's own note");

        assert_eq!(apply(&[press(Key::Z, CTRL | Modifiers::SHIFT)], false, &mut model), 1);
        assert_eq!(model.lane(0).unwrap().note, 60, "forward into the edit again");
    }

    /// Ctrl+Y redoes as well, as it does in Bitwig itself
    /// ([`keys::history_key`]).
    #[test]
    fn ctrl_y_redoes_too() {
        let mut model = edited();

        assert_eq!(apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
        assert_eq!(apply(&[press(Key::Y, CTRL)], false, &mut model), 1);
        assert_eq!(model.lane(0).unwrap().note, 60);
    }

    /// A frame carries everything that arrived since the last one, so two
    /// presses in it are two steps — and the order they are taken in is the
    /// order they were pressed.
    #[test]
    fn two_presses_in_one_frame_are_two_steps() {
        let mut model = Model::new();
        assert!(model.set_note(0, 60));
        assert!(model.commit());
        assert!(model.set_note(1, 61));
        assert!(model.commit());

        let frame = vec![press(Key::Z, CTRL), press(Key::Z, CTRL)];
        assert_eq!(apply(&frame, false, &mut model), 2);
        assert_eq!(model.lane(0).unwrap().note, 36);
        assert_eq!(model.lane(1).unwrap().note, 37);

        let frame = vec![press(Key::Z, CTRL | Modifiers::SHIFT), press(Key::Z, CTRL)];
        assert_eq!(apply(&frame, false, &mut model), 2, "forward and straight back again");
        assert_eq!(model.lane(0).unwrap().note, 36, "where the pair of them leaves it");
    }

    /// The release that follows a press is not a second step.
    #[test]
    fn a_release_is_not_a_step() {
        let mut model = edited();

        let frame = vec![press(Key::Z, CTRL), release(Key::Z, CTRL)];
        assert_eq!(apply(&frame, false, &mut model), 1);
        assert_eq!(model.undo_depth(), 0, "one step back, not two");
        assert_eq!(model.redo_depth(), 1);
    }

    /// While a lane's name is being typed the box has the shortcut: the
    /// model is not touched, and neither is its history.
    #[test]
    fn a_text_box_keeps_the_shortcut_to_itself() {
        let mut model = edited();

        assert_eq!(apply(&[press(Key::Z, CTRL)], true, &mut model), 0);
        assert_eq!(model.lane(0).unwrap().note, 60, "the edit is still there");
        assert_eq!((model.undo_depth(), model.redo_depth()), (1, 0), "and so is its step");
    }

    /// Each of the three boxes takes the keyboard on its own — egui's
    /// focus, an open rename box, the master volume's entry box — and only
    /// with none of them is the shortcut the history's.
    #[test]
    fn typing_is_any_of_the_three_boxes() {
        assert!(!typing(false, false, false), "nothing is being typed");
        assert!(typing(true, false, false), "a focused text box");
        assert!(typing(false, true, false), "a rename box in the frame it opens");
        assert!(typing(false, false, true), "the master box in the frame it opens");
        assert!(typing(true, true, true));
    }

    /// The master volume's box, in the frame between the right-click and
    /// egui granting it the focus, keeps the shortcut from the history.
    #[test]
    fn the_master_box_keeps_the_shortcut_before_it_has_the_focus() {
        let mut model = edited();

        let master_box_opening = typing(false, false, true);
        assert_eq!(apply(&[press(Key::Z, CTRL)], master_box_opening, &mut model), 0);
        assert_eq!(model.lane(0).unwrap().note, 60, "the edit is still there");
        assert_eq!((model.undo_depth(), model.redo_depth()), (1, 0), "and so is its step");
    }

    /// Nothing else a frame can carry is a step — not the keys the editor
    /// passes on to the host, and not the letter on its own.
    #[test]
    fn other_keys_are_no_business_of_the_history() {
        let mut model = edited();

        let frame = vec![
            press(Key::Z, Modifiers::NONE),
            press(Key::Y, CTRL | Modifiers::SHIFT),
            press(Key::S, CTRL),
            press(Key::Space, Modifiers::NONE),
            press(Key::Delete, Modifiers::NONE),
            Event::PointerGone,
        ];
        assert_eq!(apply(&frame, false, &mut model), 0);
        assert_eq!(model.lane(0).unwrap().note, 60);
        assert_eq!((model.undo_depth(), model.redo_depth()), (1, 0));
    }

    /// A shortcut with nowhere to go does nothing and says so — which is
    /// what a window whose history is empty answers every time.
    #[test]
    fn a_step_with_nowhere_to_go_is_not_counted() {
        let mut model = Model::new();

        assert_eq!(apply(&[press(Key::Z, CTRL)], false, &mut model), 0);
        assert_eq!(apply(&[press(Key::Z, CTRL | Modifiers::SHIFT)], false, &mut model), 0);
        assert_eq!((model.undo_depth(), model.redo_depth()), (0, 0));
    }

    /// An edit nobody marked the end of is still undoable: the model
    /// commits it itself, so a shortcut pressed mid-gesture lands on the
    /// state before it rather than on the gesture before that
    /// ([`crate::model::Model::undo`]).
    #[test]
    fn an_uncommitted_edit_is_the_first_step_back() {
        let mut model = Model::new();
        assert!(model.set_volume_db(0, -6.0));

        assert_eq!(apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
        assert_eq!(model.lane(0).unwrap().volume_db, 0.0, "where the drag started");
        assert_eq!((model.undo_depth(), model.redo_depth()), (0, 1));
    }
}
