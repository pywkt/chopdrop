//! The activity lights: which lane has just played, and how brightly its
//! colour dot is lit for it.
//!
//! The engine counts a lane's voice starts into the published snapshot and
//! says nothing about when they happened: a counter that moved is all the
//! audio thread can afford to leave behind, and a clock is the one thing it
//! cannot read. So the clock is here. Once a frame the window hands this
//! module every published lane's counter and the time ([`Lights::observe`]);
//! a lane whose count has changed since the last frame was struck *now*, and
//! its light is a read of how long ago that was ([`fade_at`]).
//!
//! Pure, and tested as such: no egui and no nice-plug, no clock of its own
//! and no colour either. The time arrives as a number of seconds — which is
//! what egui's own input offers (`egui::InputState::time`) and what a test
//! can simply pass — and what comes out is a number between one and nought
//! that [`crate::editor::theme::activity_dot`] turns into the lane's colour
//! brightened.
//!
//! # Why a count and not a flag
//!
//! A boolean "this lane is playing" would answer a different question: a
//! one-shot of a two-second crash is one hit, not two seconds of light, and
//! a hat struck sixteen times in a bar has to flash sixteen times rather
//! than stay on. A counter the GUI compares against the one it last saw
//! gives both — every change is one strike, however long the sound it
//! started lasts, and however fast the strikes come.
//!
//! It also means a light is never missed by a frame that did not happen: a
//! lane struck three times between two frames comes out as one flash
//! starting at the second of them, which is the honest answer when the
//! window was not being drawn. What a window that is not drawn at all needs
//! is a reason to draw — that is the audio thread's wake-up call
//! (`crate::Chopdrop::process`) and the repaint this module's
//! [`Lights::fading`] asks for while a light is still on its way out.
//!
//! # Lanes come and go
//!
//! Only lanes with a sample are published ([`crate::model::PublishedLanes`]),
//! so the table here is of playable lanes and not of rows: an empty lane has
//! no entry, a lane that loses its sample loses its entry with it, and a
//! lane seen for the first time starts out dark rather than flashing — a
//! window that lit every lane the moment it opened would be reporting hits
//! that happened before anyone was looking.

/// How long a lane's light takes to fade from freshly struck back to the
/// lane's own colour, in seconds.
///
/// Long enough to be seen at sixty frames a second (nine frames of it) and
/// short enough that a sixteenth-note hat at 170 bpm is sixteen separate
/// flashes rather than one smear.
pub const FADE: f64 = 0.150;

/// How bright a light struck `elapsed` seconds ago is: `1.0` the instant the
/// lane played, `0.0` once [`FADE`] has gone by, and the curve in between.
///
/// Squared, so the light leaves full brightness at once and the last of it
/// lingers: a lane struck on the beat reads as a strike and not as a pulse,
/// which a straight ramp — equal brightness lost per millisecond, its
/// midpoint still half lit — does not.
///
/// `min` before `max`, which is the NaN-safe clamp the way round
/// that leaves an unanswerable time *dark*: `f64::min` hands back the
/// number when one side is NaN, so a NaN elapsed comes out as [`FADE`] —
/// a light that has already gone out — rather than as one stuck on.
pub fn fade_at(elapsed: f64) -> f32 {
    // `min`-then-`max` is the NaN guard, as `voices.rs` does it; `clamp`
    // itself hands a NaN straight back, which would reach the dot's colour
    // as a NaN brightness.
    #[allow(clippy::manual_clamp)]
    let lit = elapsed.min(FADE).max(0.0);
    let left = 1.0 - lit / FADE;
    (left * left) as f32
}

/// One lane's light, as the last [`Lights::observe`] left it.
struct Light {
    /// The lane it belongs to ([`crate::model::Lane::id`]).
    lane: u32,

    /// The lane's activity counter as this entry last saw it. Compared and
    /// never added up: see the module header.
    hits: u32,

    /// When the counter was last seen to change, on the clock
    /// [`Lights::observe`] is given, and `None` for a lane this window has
    /// not yet seen play — which is every lane the frame the window opens,
    /// hits already on the counter or not (see the module header).
    struck: Option<f64>,

    /// Whether the lane was in the frame being observed — the mark
    /// [`Lights::observe`] drops the lanes that are gone by.
    seen: bool,
}

/// Every playable lane's light, and the frame they were last read at.
///
/// Owned by [`crate::editor::ChopdropEditor`] and built afresh for each
/// opened window, like everything else that belongs to a window rather than
/// to the instance: the counters are the instance's, the *times* are this
/// window's, and a window opening long after the last hit should open dark.
#[derive(Default)]
pub struct Lights {
    /// One entry per published lane, in no particular order — a handful of
    /// entries looked up by id, which is a scan and not a map (the same
    /// reasoning as [`crate::model::PublishedLanes::lane`]).
    lit: Vec<Light>,

    /// The time of the last [`observe`](Lights::observe), which every
    /// [`fade`](Lights::fade) is measured back from. Kept here rather than
    /// passed in again so that one frame cannot read its lights at two
    /// different instants.
    now: f64,

    /// The lane whose audition is sounding this frame, as the engine last
    /// reported it ([`crate::engine::AuditionRing::sounding`]): that row's
    /// play button is drawn as a stop button.
    auditioning: Option<u32>,
}

impl Lights {
    /// Reads this frame's counters: `lanes` is every published lane as
    /// `(id, activity counter)` and `now` is the time, in seconds on any
    /// clock that counts up (egui's `InputState::time`).
    ///
    /// Called once a frame, before the rows are drawn, and the only place
    /// the table changes. A lane whose counter has moved since the last
    /// frame is struck at `now`; a lane seen for the first time starts out
    /// dark; a lane that is no longer published is forgotten, so the table
    /// is as long as the playable lane list and no longer.
    pub fn observe(&mut self, lanes: impl Iterator<Item = (u32, u32)>, now: f64) {
        self.now = now;
        for light in &mut self.lit {
            light.seen = false;
        }

        for (lane, hits) in lanes {
            match self.lit.iter_mut().find(|light| light.lane == lane) {
                Some(light) => {
                    light.seen = true;
                    if light.hits != hits {
                        light.hits = hits;
                        light.struck = Some(now);
                    }
                }
                // Never struck rather than struck a fade ago: the lane's
                // hits so far happened before this window was looking at
                // it (see the module header).
                None => self.lit.push(Light { lane, hits, struck: None, seen: true }),
            }
        }

        self.lit.retain(|light| light.seen);
    }

    /// How brightly lane `lane`'s dot is lit as of the last
    /// [`observe`](Lights::observe): `1.0` on the frame it was struck, down
    /// to `0.0` once [`FADE`] has passed, and `0.0` for a lane with no
    /// sample, one this window has not seen play, or one that is not there
    /// at all.
    pub fn fade(&self, lane: u32) -> f32 {
        self.lit.iter().find(|light| light.lane == lane).map_or(0.0, |light| self.fade_of(light))
    }

    /// Whether any light is still on its way out, and so whether the window
    /// needs another frame to draw the rest of the fade with: the window
    /// keeps repainting while any light is fading.
    ///
    /// False when nothing has played, which is what keeps an idle window
    /// idle: the frames a fade costs are the nine it takes, and the audio
    /// thread is what wakes the window for the next hit
    /// (`crate::Chopdrop::process`).
    pub fn fading(&self) -> bool {
        self.lit.iter().any(|light| self.fade_of(light) > 0.0)
    }

    /// Sets the lane whose audition is sounding this frame. Called once a
    /// frame, beside [`observe`](Lights::observe).
    pub fn set_auditioning(&mut self, lane: Option<u32>) {
        self.auditioning = lane;
    }

    /// The lane whose audition is sounding this frame, if any.
    pub fn auditioning(&self) -> Option<u32> {
        self.auditioning
    }

    /// One entry's light at the frame's own instant — the one place the
    /// time and the curve meet, so a dot and the repaint that draws it
    /// cannot disagree about whether the light is out.
    fn fade_of(&self, light: &Light) -> f32 {
        light.struck.map_or(0.0, |struck| fade_at(self.now - struck))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The curve's three fixed points: full the instant a lane plays,
    /// nothing once the fade is over, and still fading in between.
    #[test]
    fn a_light_is_full_when_it_is_struck_and_out_when_the_fade_is_over() {
        assert_eq!(fade_at(0.0), 1.0, "the frame the lane played");
        assert_eq!(fade_at(FADE), 0.0, "the fade is over, to the millisecond");
        assert_eq!(fade_at(FADE * 2.0), 0.0, "and stays over");
        assert_eq!(fade_at(10.0), 0.0, "ten seconds later, no light");

        let halfway = fade_at(FADE / 2.0);
        assert!(halfway > 0.0 && halfway < 1.0, "halfway through, half gone: {halfway}");
    }

    /// The curve only ever falls, so a dot never brightens again on its own
    /// — and it falls faster at the start than at the end, which is what
    /// makes a hit read as a strike rather than as a pulse.
    #[test]
    fn the_fade_falls_from_the_strike_and_leaves_its_tail_behind() {
        let mut last = fade_at(0.0);
        for step in 1..=30 {
            let fade = fade_at(FADE * f64::from(step) / 30.0);
            assert!(fade <= last, "the light brightened again at step {step}");
            last = fade;
        }

        // Squared and not straight: the midpoint of a straight ramp is
        // exactly half lit, and this one is well under it.
        assert!(fade_at(FADE / 2.0) < 0.4, "the drop is front-loaded");
    }

    /// A time that is not a number leaves the dot dark rather than lit —
    /// the clamp order [`fade_at`] documents — and so does one from before
    /// the strike, which is a clock that has gone backwards.
    #[test]
    fn an_impossible_elapsed_time_leaves_the_light_out() {
        assert_eq!(fade_at(f64::NAN), 0.0, "a NaN is not a lane that just played");
        assert_eq!(fade_at(f64::INFINITY), 0.0);
        assert_eq!(fade_at(-1.0), 1.0, "before the strike is the strike itself");
        assert_eq!(fade_at(f64::NEG_INFINITY), 1.0);
    }

    /// A lane seen for the first time is dark, however many hits its
    /// counter already carries: those happened before this window opened.
    #[test]
    fn a_lane_seen_for_the_first_time_does_not_flash() {
        let mut lights = Lights::default();

        lights.observe([(1, 0), (2, 9_000)].into_iter(), 100.0);

        assert_eq!(lights.fade(1), 0.0);
        assert_eq!(lights.fade(2), 0.0, "nine thousand hits, none of them ours");
        assert!(!lights.fading(), "an opening window asks for no extra frames");
    }

    /// A counter that moves lights that lane's dot and only that lane's,
    /// and the light fades over [`FADE`] whether or not it moves again.
    #[test]
    fn a_hit_lights_its_own_lane_and_fades_over_the_fade() {
        let mut lights = Lights::default();
        lights.observe([(1, 0), (2, 0)].into_iter(), 0.0);

        lights.observe([(1, 1), (2, 0)].into_iter(), 1.0);
        assert_eq!(lights.fade(1), 1.0, "struck this frame");
        assert_eq!(lights.fade(2), 0.0, "the other lane did not play");
        assert!(lights.fading());

        // Two frames later, the counter unchanged: the light is on its way
        // out, and the window still wants frames to draw it with.
        lights.observe([(1, 1), (2, 0)].into_iter(), 1.0 + FADE / 2.0);
        let half = lights.fade(1);
        assert!(half > 0.0 && half < 1.0, "halfway out: {half}");
        assert!(lights.fading());

        // Past the fade and not exactly on it: the curve's own boundary is
        // pinned above ([`fade_at`]), and a sum of two tenths of a second
        // in binary floating point is not the boundary to the last bit.
        lights.observe([(1, 1), (2, 0)].into_iter(), 1.0 + FADE * 2.0);
        assert_eq!(lights.fade(1), 0.0, "the fade is over");
        assert!(!lights.fading(), "and the window goes back to being reactive");
    }

    /// A second hit while the first is still fading starts the light again
    /// from full — a hat struck twice flashes twice.
    #[test]
    fn a_second_hit_restrikes_a_light_that_is_still_fading() {
        let mut lights = Lights::default();
        lights.observe([(1, 0)].into_iter(), 0.0);
        lights.observe([(1, 1)].into_iter(), 1.0);
        lights.observe([(1, 1)].into_iter(), 1.0 + FADE * 0.9);
        assert!(lights.fade(1) > 0.0, "still fading");

        lights.observe([(1, 2)].into_iter(), 1.0 + FADE * 0.95);
        assert_eq!(lights.fade(1), 1.0, "struck again");
    }

    /// Several hits between two frames are one flash: the window was not
    /// being drawn, so there was nothing to see them one at a time.
    #[test]
    fn hits_the_window_slept_through_come_out_as_one_flash() {
        let mut lights = Lights::default();
        lights.observe([(1, 7)].into_iter(), 0.0);

        lights.observe([(1, 19)].into_iter(), 5.0);

        assert_eq!(lights.fade(1), 1.0, "twelve hits, one light");
    }

    /// A counter that wraps past `u32::MAX` is a change like any other, and
    /// a wrap is one more hit ([`crate::model::PublishedLane::activity`]).
    #[test]
    fn a_wrapped_counter_is_one_more_hit() {
        let mut lights = Lights::default();
        lights.observe([(1, u32::MAX)].into_iter(), 0.0);

        lights.observe([(1, 0)].into_iter(), 1.0);

        assert_eq!(lights.fade(1), 1.0);
    }

    /// A lane that stops being published — its sample cleared, the lane
    /// deleted — is forgotten, so the table stays the length of the
    /// playable lane list however long a window is left open.
    #[test]
    fn a_lane_that_stops_playing_is_forgotten() {
        let mut lights = Lights::default();
        lights.observe([(1, 0), (2, 0), (3, 0)].into_iter(), 0.0);
        lights.observe([(1, 1), (2, 1), (3, 1)].into_iter(), 1.0);
        assert_eq!(lights.lit.len(), 3);

        lights.observe([(2, 1)].into_iter(), 1.01);

        assert_eq!(lights.lit.len(), 1, "two lanes are gone and so are their lights");
        assert!(lights.fade(2) > 0.0, "the one that stayed kept its fade");
        assert_eq!(lights.fade(1), 0.0, "and a lane with no entry is dark");

        // And back again: a lane given a sample a second time starts dark,
        // like any lane seen for the first time.
        lights.observe([(1, 1), (2, 1)].into_iter(), 1.02);
        assert_eq!(lights.fade(1), 0.0);
    }

    /// An empty instance — no lane with a sample in it — asks for no
    /// repaints at all.
    #[test]
    fn no_playable_lanes_is_no_lights() {
        let mut lights = Lights::default();
        lights.observe([(1, 0)].into_iter(), 0.0);
        lights.observe([(1, 1)].into_iter(), 1.0);
        assert!(lights.fading(), "one lane, struck");

        lights.observe(std::iter::empty(), 1.0);

        assert!(!lights.fading());
        assert_eq!(lights.fade(1), 0.0);
    }
}
