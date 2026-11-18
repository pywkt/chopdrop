//! Which region of the window reads the unforwarded keys.
//!
//! The chop strip and the browser both want the arrows and Enter, so only
//! one of them may read them at a time: the region last clicked with the
//! primary button owns the keyboard. Anywhere else — the top bar, the lane
//! rows — gives it back to the lanes, which read no keys of their own. A new
//! window starts with the lanes owning it.
//!
//! This decides nothing about Bitwig's keys: `keys.rs` forwards Space,
//! F-keys and Ctrl+alnum exactly as before, and the undo keys
//! (`history.rs`) stay global. Nor about text boxes: an owner still reads no
//! keys while a text box that is not its own has the focus (the browser's
//! filter box is its own, see `browser_panel.rs`).

use egui::{Event, PointerButton, Pos2, Rect};

/// The region that reads the keyboard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyOwner {
    /// The lane list (and anywhere outside the other two): no keys read.
    #[default]
    Lanes,
    /// The chop editor strip.
    Chop,
    /// The docked sample browser.
    Browser,
}

/// Where the two key-reading regions were last laid out; `None` for one not
/// on screen.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Regions {
    /// The chop strip's rect.
    pub chop: Option<Rect>,
    /// The browser panel's rect.
    pub browser: Option<Rect>,
}

impl Regions {
    /// The region a primary press at `pos` gives the keyboard to.
    pub fn owner_at(&self, pos: Pos2) -> KeyOwner {
        if self.browser.is_some_and(|rect| rect.contains(pos)) {
            KeyOwner::Browser
        } else if self.chop.is_some_and(|rect| rect.contains(pos)) {
            KeyOwner::Chop
        } else {
            KeyOwner::Lanes
        }
    }
}

impl KeyOwner {
    /// The owner after a frame's `events`: each primary press hands the
    /// keyboard to the region under it, the last press winning. Other
    /// buttons and releases change nothing.
    pub fn after_events(self, events: &[Event], regions: &Regions) -> KeyOwner {
        events.iter().fold(self, |owner, event| match event {
            Event::PointerButton { pos, button: PointerButton::Primary, pressed: true, .. } => {
                regions.owner_at(*pos)
            }
            _ => owner,
        })
    }

    /// The owner once the regions on screen are `regions`: a region that has
    /// gone (the browser closed, the strip hidden) gives the keyboard back
    /// to the lanes.
    pub fn still_on_screen(self, regions: &Regions) -> KeyOwner {
        match self {
            KeyOwner::Browser if regions.browser.is_none() => KeyOwner::Lanes,
            KeyOwner::Chop if regions.chop.is_none() => KeyOwner::Lanes,
            owner => owner,
        }
    }

    /// Whether `region` may read keys this frame: it owns the keyboard and no
    /// text box but its own has the focus (`foreign_text_focus`).
    pub fn reads_keys(self, region: KeyOwner, foreign_text_focus: bool) -> bool {
        self == region && region != KeyOwner::Lanes && !foreign_text_focus
    }
}

#[cfg(test)]
mod tests {
    use egui::{Modifiers, pos2};

    use super::*;

    fn regions() -> Regions {
        Regions {
            chop: Some(Rect::from_min_max(pos2(0.0, 300.0), pos2(500.0, 400.0))),
            browser: Some(Rect::from_min_max(pos2(500.0, 0.0), pos2(800.0, 400.0))),
        }
    }

    fn press(x: f32, y: f32, button: PointerButton, pressed: bool) -> Event {
        Event::PointerButton { pos: pos2(x, y), button, pressed, modifiers: Modifiers::NONE }
    }

    fn click(x: f32, y: f32) -> Event {
        press(x, y, PointerButton::Primary, true)
    }

    #[test]
    fn a_new_window_starts_with_the_lanes() {
        assert_eq!(KeyOwner::default(), KeyOwner::Lanes);
    }

    #[test]
    fn a_primary_press_gives_the_keyboard_to_the_region_under_it() {
        let regions = regions();
        let owner = KeyOwner::Lanes.after_events(&[click(600.0, 50.0)], &regions);
        assert_eq!(owner, KeyOwner::Browser);
        let owner = owner.after_events(&[click(100.0, 350.0)], &regions);
        assert_eq!(owner, KeyOwner::Chop);
        let owner = owner.after_events(&[click(100.0, 50.0)], &regions);
        assert_eq!(owner, KeyOwner::Lanes);
    }

    #[test]
    fn other_buttons_releases_and_moves_change_nothing() {
        let regions = regions();
        let events = [
            press(100.0, 350.0, PointerButton::Secondary, true),
            press(100.0, 350.0, PointerButton::Middle, true),
            press(100.0, 350.0, PointerButton::Primary, false),
            Event::PointerMoved(pos2(100.0, 350.0)),
        ];
        assert_eq!(KeyOwner::Browser.after_events(&events, &regions), KeyOwner::Browser);
        assert_eq!(KeyOwner::Browser.after_events(&[], &regions), KeyOwner::Browser);
    }

    #[test]
    fn the_last_press_of_a_frame_wins() {
        let events = [click(600.0, 50.0), click(100.0, 350.0)];
        assert_eq!(KeyOwner::Lanes.after_events(&events, &regions()), KeyOwner::Chop);
    }

    #[test]
    fn a_region_not_on_screen_takes_no_clicks() {
        let regions = Regions { chop: None, ..regions() };
        let owner = KeyOwner::Browser.after_events(&[click(100.0, 350.0)], &regions);
        assert_eq!(owner, KeyOwner::Lanes);
    }

    #[test]
    fn a_region_that_goes_away_gives_the_keyboard_back_to_the_lanes() {
        let gone = Regions::default();
        assert_eq!(KeyOwner::Browser.still_on_screen(&gone), KeyOwner::Lanes);
        assert_eq!(KeyOwner::Chop.still_on_screen(&gone), KeyOwner::Lanes);
        assert_eq!(KeyOwner::Browser.still_on_screen(&regions()), KeyOwner::Browser);
        assert_eq!(KeyOwner::Chop.still_on_screen(&regions()), KeyOwner::Chop);
        let browser_only = Regions { chop: None, ..regions() };
        assert_eq!(KeyOwner::Browser.still_on_screen(&browser_only), KeyOwner::Browser);
    }

    #[test]
    fn only_the_owner_reads_keys_and_never_under_a_foreign_text_box() {
        assert!(KeyOwner::Browser.reads_keys(KeyOwner::Browser, false));
        assert!(!KeyOwner::Browser.reads_keys(KeyOwner::Browser, true));
        assert!(!KeyOwner::Browser.reads_keys(KeyOwner::Chop, false));
        assert!(!KeyOwner::Lanes.reads_keys(KeyOwner::Browser, false));
        assert!(KeyOwner::Chop.reads_keys(KeyOwner::Chop, false));
        assert!(!KeyOwner::Chop.reads_keys(KeyOwner::Browser, false));
        assert!(!KeyOwner::Lanes.reads_keys(KeyOwner::Lanes, false));
    }
}
