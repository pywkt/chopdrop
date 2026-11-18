//! What a lane says about its sample, and what the top bar says about the
//! project's size.
//!
//! Two surfaces, both of them a read of state somebody else owns, which is
//! why they are one module and why that module draws nothing:
//!
//! - a row's **sample column** is one slot with four things to say in it —
//!   the file's name, the hint for a lane with nothing in it, "loading…"
//!   while the background worker is still at the file, and the message left
//!   by whatever went wrong last (a failed decode, or a failed restore). One
//!   of the four is drawn per lane per frame, so choosing between them is a
//!   decision and [`sample_status`] is where it is made;
//! - the **top bar's status** is about the instance rather than a lane, and
//!   two things are said there: how many bytes of sample a save would write
//!   into the project, once it is more than a project should quietly carry
//!   ([`embed_warning`]), and how many files a drop had to leave behind at
//!   the lane cap ([`drop_overflow`]).
//!
//! # Why the loading state wins
//!
//! A lane can be loading *and* carry a message: a reload that failed leaves
//! the message behind ([`crate::model::Model::set_error`] keeps the sample
//! and the lane), and the next drop onto that lane sets it loading without
//! clearing it. The job in flight is the newer fact and the one the user is
//! waiting on, so it is what the row says; the message it is hiding either
//! goes away with the result or is written again by it, since a result is
//! the only thing that ends a loading state
//! ([`crate::model::Model::set_sample`] and `set_error` both clear it).
//!
//! # Why the note is not a message
//!
//! A sample over the embedding cap is not a failure — the lane plays, and
//! every session that can still reach the file plays it — so
//! [`crate::model::Lane::embed_note`] is carried *beside* the file name
//! ([`SampleStatus::Loaded`] holds both) rather than in place of it. In the
//! row that is [`NOTE_MARK`] at the column's right edge with the note itself
//! on the hover, because the sentence is longer than the column and the file
//! name is the thing worth reading there.
//!
//! # Why a lost file is said in the bar
//!
//! A drop truncated at [`crate::model::MAX_LANES`] has no lane to say so on
//! — the files that did not fit are exactly the ones with no row of their
//! own — so it is said where the project's own status is said, and it is
//! the one message in the window that goes away by itself
//! ([`crate::editor::drops::MESSAGE_SHOWN_FOR`]): it is about a gesture
//! that is over, not about a state anything can read back.
//!
//! Nothing here holds state of its own: every function is a pure read, so
//! the window's whole message selection is tested without a display.

use egui::Color32;

use crate::editor::theme;
use crate::model::Lane;
use crate::time::TimeMode;

/// What the sample column says while a loader job for the lane is still
/// running (set when the job is queued, cleared by its result).
///
/// Lower case and dimmed like the hint, with the ellipsis spelled as one
/// character: this is the one thing a row says that is about to stop being
/// true by itself, and a long MP3 is the only time it is on screen long
/// enough to read.
pub const LOADING: &str = "loading…";

/// What the sample column says while a Stretch lane's render is on its way
/// (`render_pending`). Dimmed like [`LOADING`], and for the same reason: it
/// stops being true by itself.
pub const RENDERING: &str = "rendering…";

/// What the sample column says after a Stretch lane's last render failed;
/// the reason is on the hover, beside [`RENDER_FAILED_HOVER`].
pub const RENDER_FAILED: &str = "render failed";

/// The hover line under a failed render's reason: how to ask again.
pub const RENDER_FAILED_HOVER: &str = "right-click the lane and choose re-render to try again";

/// What the sample column of a lane with no sample says.
///
/// Lower case and in [`theme::TEXT_DIM`], unlike a file name: eight of these
/// stacked up are what a fresh instance looks like, and they are an
/// invitation rather than eight warnings.
pub const DROP_HINT: &str = "drop a sample here";

/// The mark drawn at the right edge of a row's sample column for a lane
/// carrying a note about its sample ([`crate::model::Lane::embed_note`]):
/// one character of [`theme::WARN`], with the sentence itself on the hover.
///
/// A mark rather than the words because the column has no room for the
/// words — it is as narrow as [`theme::HEADER_SAMPLE_MIN_WIDTH`] and the
/// file name is what is worth reading in it.
///
/// An exclamation mark and not a warning sign, because egui's
/// `default_fonts` are Latin and a little punctuation: `⚠`, `▲` and every
/// other symbol tried for this draws as a box, which the test below is
/// there to keep true of whatever anyone spells a message with
/// ([`crate::state::SAMPLE_NOT_EMBEDDED`] and its neighbours included).
pub const NOTE_MARK: &str = "!";

/// What the top bar's size warning says when the pointer rests on it.
///
/// It has to answer "and now what?", because the figure beside it is not a
/// limit and nothing has gone wrong: the state is written whatever its size,
/// and a host that refuses it is the failure this is trying to get ahead of
/// (bitstep found Bitwig refuses state over 256 MiB, and this warns well
/// below that).
pub const EMBED_WARN_HOVER: &str =
    "this project carries its samples inside itself, and a host may refuse to save a project \
     much larger than this. Nothing is stopped — every save still saves every sample.";

/// What one lane's sample column has to say this frame, in the order
/// [`sample_status`] decides between them.
///
/// Borrowed from the lane rather than built into strings: the list is drawn
/// every frame under the writer lock the frame already holds, and a message
/// per lane per frame would be the one allocation in the window that grew
/// with the number of lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleStatus<'lane> {
    /// A loader job for this lane is still running (see the module header
    /// for why this is said over anything else).
    Loading,
    /// The last thing that went wrong for this lane, in the words the model
    /// was given ([`crate::model::Lane::error`]).
    Failed(&'lane str),
    /// A Stretch lane with a sample whose render is on its way
    /// ([`crate::model::Lane::render_pending`]); the lane plays its last
    /// render (or nothing) until it lands.
    Rendering,
    /// A Stretch lane with a sample whose last render failed, with the
    /// reason ([`crate::model::Lane::render_error`]).
    RenderFailed(&'lane str),
    /// The lane's sample, by the file's name — and the note the lane carries
    /// about what a save will do with it, when there is one
    /// ([`crate::model::Lane::embed_note`]).
    Loaded {
        /// The last component of the sample's path, never the path itself: a
        /// row is three hundred points wide and a path is not.
        file_name: &'lane str,
        /// [`crate::state::SAMPLE_NOT_EMBEDDED`] for a sample the project
        /// will keep only the path of, `None` for one it will carry.
        note: Option<&'static str>,
    },
    /// A lane with no sample and nothing pending: the invitation to drop one
    /// ([`DROP_HINT`]).
    Empty,
}

impl<'lane> SampleStatus<'lane> {
    /// The text the column is drawn with.
    pub fn text(&self) -> &'lane str {
        match self {
            SampleStatus::Loading => LOADING,
            SampleStatus::Failed(message) => message,
            SampleStatus::Rendering => RENDERING,
            SampleStatus::RenderFailed(_) => RENDER_FAILED,
            SampleStatus::Loaded { file_name, .. } => file_name,
            SampleStatus::Empty => DROP_HINT,
        }
    }

    /// The colour that text is drawn in: [`theme::WARN`] for a message,
    /// [`theme::TEXT`] for a file name — a value, like the numbers beside it
    /// — and [`theme::TEXT_DIM`] for the two states that are neither.
    pub fn colour(&self) -> Color32 {
        match self {
            SampleStatus::Failed(_) | SampleStatus::RenderFailed(_) => theme::WARN,
            SampleStatus::Loaded { .. } => theme::TEXT,
            SampleStatus::Loading | SampleStatus::Rendering | SampleStatus::Empty => {
                theme::TEXT_DIM
            }
        }
    }

    /// What the column says when the pointer rests on it, if anything: the
    /// whole of a message the column is too narrow for, or the note a
    /// [`NOTE_MARK`] stands for.
    ///
    /// The same words as the column itself for a message, which is the point
    /// — "Bitwig clips can't be loaded — drop a .wav instead" does not fit
    /// in a lane row at any window size this plugin opens at.
    pub fn hover(&self) -> Option<&'lane str> {
        match self {
            SampleStatus::Failed(message) | SampleStatus::RenderFailed(message) => Some(message),
            SampleStatus::Loaded { note, .. } => *note,
            SampleStatus::Loading | SampleStatus::Rendering | SampleStatus::Empty => None,
        }
    }

    /// Whether the column draws [`NOTE_MARK`] at its right edge: only a
    /// loaded sample the project will not carry has a note to stand for.
    pub fn marked(&self) -> bool {
        matches!(self, SampleStatus::Loaded { note: Some(_), .. })
    }
}

/// What this lane's sample column says: the job in flight, else the message
/// left behind, else — for a Stretch lane with a sample — its render on its
/// way or the reason the last one failed, else the sample, else the hint
/// (the module header says why in that order).
pub fn sample_status(lane: &Lane) -> SampleStatus<'_> {
    if lane.loading {
        return SampleStatus::Loading;
    }
    if let Some(message) = &lane.error {
        return SampleStatus::Failed(message);
    }
    if lane.time_mode == TimeMode::Stretch && lane.sample.is_some() {
        if lane.render_pending {
            return SampleStatus::Rendering;
        }
        if let Some(message) = &lane.render_error {
            return SampleStatus::RenderFailed(message);
        }
    }
    match &lane.sample {
        Some(sample) => {
            SampleStatus::Loaded { file_name: &sample.file_name, note: lane.embed_note() }
        }
        None => SampleStatus::Empty,
    }
}

/// The top bar's warning for an instance that would write `embedded_bytes`
/// of sample into the project, or `None` while that is a figure nobody needs
/// to think about.
///
/// The figure and nothing else: [`theme::WARN`] is what makes it a warning
/// and [`EMBED_WARN_HOVER`] is what explains it, which between them leave
/// the line short enough for the room the bar's three controls leave over.
///
/// The threshold is [`crate::state::embed_warning`]'s, asked here rather
/// than copied: the bar warns exactly when the limit says it should. This
/// is the only place the question is asked of a live project — the bar pairs
/// it with [`crate::model::Model::embedded_bytes`].
///
/// Whole mebibytes, which is as fine as a figure in the hundreds of
/// megabytes deserves — and the warning's own threshold is 128 of them, so
/// there is no rounding down to nothing to worry about.
pub fn embed_warning(embedded_bytes: usize) -> Option<String> {
    crate::state::embed_warning(embedded_bytes)
        .then(|| format!("{} MiB of samples", embedded_bytes / (1024 * 1024)))
}

/// What the bar's drop message says when the pointer rests on it: why the
/// files were left behind, and what to do about it.
///
/// The line itself is only the count ([`drop_overflow`]), because the room
/// it gets is whatever the bar's three controls leave over — the same room
/// the size warning gets, and the same division of labour between the line
/// and its hover.
///
/// It does not name the cap: the number is in `+ lane`'s own hover
/// ([`crate::editor::top_bar::ADD_LANE_FULL_HOVER`]) and in the user guide,
/// and a sentence this one has to fit a tooltip.
pub const DROP_FULL_HOVER: &str =
    "the lane list is full, so the files past the last lane were not loaded. Delete a lane \
     and drop them again, or put the rest on a second chopdrop.";

/// The bar's message for a drop that had to leave `missing` files behind at
/// the lane cap, or `None` for the drops that lost nothing.
///
/// Counted files and not named ones: a drop of twenty files that lost
/// nineteen of them would be nineteen names in a line the width of a
/// tooltip, and the one thing a user needs to know is that the gesture was
/// not all of it done ([`DROP_FULL_HOVER`] says the rest).
///
/// Singular for one file, because a plugin that says "1 files" reads as
/// unfinished.
pub fn drop_overflow(missing: usize) -> Option<String> {
    match missing {
        0 => None,
        1 => Some("1 file didn't fit".to_owned()),
        many => Some(format!("{many} files didn't fit")),
    }
}

/// What the bar's "kept their output" message says when the pointer rests on
/// it: why the lanes did not move, and how to make room.
pub const OUTPUTS_FULL_HOVER: &str =
    "every output from Out 2 to Out 16 already has a lane on it, so these lanes stayed where \
     they were. Move a lane back to Out 1 to free its output.";

/// The bar's message after "give each its own output" left `unplaced` lanes
/// on their old output because no free one was left, or `None` when every
/// lane found one.
///
/// Singular for one lane, as [`drop_overflow`] is for one file.
pub fn outputs_kept(unplaced: usize) -> Option<String> {
    match unplaced {
        0 => None,
        1 => Some("1 lane kept its output: no free outputs".to_owned()),
        many => Some(format!("{many} lanes kept their output: no free outputs")),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::model::{MAX_LANES, Model, SampleRef};
    use crate::sample::{DecodeError, MAX_SAMPLE_BYTES, MAX_SOURCE_BYTES, SampleData};
    use crate::state::{
        EMBED_WARN_BYTES, SAMPLE_MISSING_PREFIX, SAMPLE_NOT_EMBEDDED, SAMPLE_RESTORE_FAILED,
    };

    /// A model whose first lane holds a sample of `byte_len` file bytes — a
    /// model and not a hand-built lane, because a sample can only be made
    /// with the collector handle a model owns.
    ///
    /// The bytes are zeroed and never read past their length: what decides
    /// whether a project can carry a sample is how big the file is, not what
    /// is in it.
    fn model_with_a_sample_of(byte_len: usize) -> Model {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 16].into() };
        let path = Path::new("/home/dev/kit/BD 606.wav");
        let sample = SampleRef::new(model.handle(), path, vec![0; byte_len], data);
        assert!(model.set_sample(id, sample), "the first lane takes a sample");
        model
    }

    /// A fresh lane with a sample in it, as a drop that worked leaves it.
    fn loaded_lane(model: &Model) -> &Lane {
        &model.lanes()[0]
    }

    /// A lane nobody has dropped anything on yet says so, dimly.
    #[test]
    fn an_empty_lane_invites_a_drop() {
        let model = Model::new();
        let status = sample_status(&model.lanes()[0]);

        assert_eq!(status, SampleStatus::Empty);
        assert_eq!(status.text(), DROP_HINT);
        assert_eq!(status.colour(), theme::TEXT_DIM, "an invitation, not a value");
        assert_eq!(status.hover(), None, "the hint is already the whole of it");
        assert!(!status.marked());
    }

    /// A loaded lane names the file, in the colour of the values beside it,
    /// and has nothing more to add.
    #[test]
    fn a_loaded_lane_names_its_file() {
        let model = model_with_a_sample_of(1024);
        let status = sample_status(loaded_lane(&model));

        assert_eq!(status, SampleStatus::Loaded { file_name: "BD 606.wav", note: None });
        assert_eq!(status.text(), "BD 606.wav", "the name and not the path");
        assert_eq!(status.colour(), theme::TEXT);
        assert_eq!(status.hover(), None);
        assert!(!status.marked(), "a project this one carries needs no mark");
    }

    /// A sample too big for the project to carry keeps its name in the
    /// column and hands the note to the hover: the lane is working.
    #[test]
    fn a_sample_the_project_cannot_carry_keeps_its_name_and_adds_a_mark() {
        let model = model_with_a_sample_of(MAX_SAMPLE_BYTES + 1);
        let status = sample_status(loaded_lane(&model));

        assert_eq!(status.text(), "BD 606.wav", "the lane plays — this is not a failure");
        assert_eq!(status.colour(), theme::TEXT);
        assert_eq!(status.hover(), Some(SAMPLE_NOT_EMBEDDED), "the note is on the hover");
        assert!(status.marked(), "and a mark says there is a hover worth finding");
    }

    /// A load that failed replaces the name with its message, in the
    /// theme's warning colour, and offers the whole of it on the hover
    /// because the column is narrower than the sentence.
    #[test]
    fn a_failed_load_says_what_went_wrong_in_the_warning_colour() {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        let message = format!("{SAMPLE_MISSING_PREFIX}kick.wav");
        assert!(model.set_error(id, &message));
        let status = sample_status(&model.lanes()[0]);

        assert_eq!(status, SampleStatus::Failed(&message));
        assert_eq!(status.text(), message);
        assert_eq!(status.colour(), theme::WARN);
        assert_eq!(status.hover(), Some(&*message), "the whole message, truncated or not");
        assert!(!status.marked(), "the message is the mark");
    }

    /// A failed *reload* keeps the sample it had, and the message is still
    /// what the column says: the file name is back once a load works (keep
    /// the current sample and show the error).
    #[test]
    fn a_message_is_shown_over_the_sample_the_lane_kept() {
        let mut model = model_with_a_sample_of(1024);
        let id = model.lanes()[0].id;
        let rejected = "unsupported audio file";
        assert!(model.set_error(id, rejected));

        assert_eq!(sample_status(&model.lanes()[0]), SampleStatus::Failed(rejected));
        assert!(model.lanes()[0].sample.is_some(), "the lane is still playing something");
    }

    /// A job in flight is said over everything else, including a message a
    /// previous attempt left behind (see the module header).
    #[test]
    fn a_pending_job_is_said_over_a_name_and_over_an_old_message() {
        let mut model = model_with_a_sample_of(1024);
        let id = model.lanes()[0].id;
        let rejected = "unsupported audio file";
        assert!(model.set_error(id, rejected));
        assert!(model.set_loading(id, true));

        let status = sample_status(&model.lanes()[0]);
        assert_eq!(status, SampleStatus::Loading);
        assert_eq!(status.text(), LOADING);
        assert_eq!(status.colour(), theme::TEXT_DIM, "a state, not a value and not a failure");
        assert_eq!(status.hover(), None);

        // And the result is what ends it, in both directions: the writers
        // that land a sample or a message both clear the flag, which is why
        // nothing here has to time it out.
        assert!(model.set_loading(id, false));
        assert_eq!(sample_status(&model.lanes()[0]), SampleStatus::Failed(rejected));
    }

    /// A Stretch lane with a sample says "rendering…" while its render is on
    /// its way and "render failed" (reason on the hover) after one failed;
    /// a load message or a job in flight still wins, a lane not in Stretch
    /// never says either, and a landed render gives the name back.
    #[test]
    fn a_stretch_lane_says_its_render_status() {
        let mut model = model_with_a_sample_of(1024);
        let id = model.lanes()[0].id;
        let name = SampleStatus::Loaded { file_name: "BD 606.wav", note: None };

        // Off: render fields mean nothing.
        let mut off = model.lanes()[0].clone();
        off.render_pending = true;
        off.render_error = Some(String::from("x"));
        assert_eq!(sample_status(&off), name, "only Stretch lanes render");

        assert!(model.set_time_mode(id, TimeMode::Stretch));
        assert!(model.lanes()[0].render_pending, "Stretch asks for a render");
        let status = sample_status(&model.lanes()[0]);
        assert_eq!(status, SampleStatus::Rendering);
        assert_eq!(status.text(), RENDERING);
        assert_eq!(status.colour(), theme::TEXT_DIM);
        assert_eq!(status.hover(), None);

        let generation = model.lanes()[0].render_generation;
        assert!(model.set_render_error(id, generation, "stretch failed"));
        let status = sample_status(&model.lanes()[0]);
        assert_eq!(status, SampleStatus::RenderFailed("stretch failed"));
        assert_eq!(status.text(), RENDER_FAILED);
        assert_eq!(status.colour(), theme::WARN);
        assert_eq!(status.hover(), Some("stretch failed"));
        assert!(!status.marked());

        // Asked again, then landed: the name is back.
        assert!(model.request_render(id));
        assert_eq!(sample_status(&model.lanes()[0]), SampleStatus::Rendering);
        let generation = model.lanes()[0].render_generation;
        let data = SampleData { rate: 48_000, channels: 1, frames: vec![0.0; 16].into() };
        assert!(model.set_render(id, generation, data));
        assert_eq!(sample_status(&model.lanes()[0]), name);
        assert!(model.request_render(id));

        // Pending again: a load message and a job in flight are still said over it.
        assert!(model.set_error(id, "unsupported audio file"));
        let failed = SampleStatus::Failed("unsupported audio file");
        assert_eq!(sample_status(&model.lanes()[0]), failed);
        assert!(model.set_loading(id, true));
        assert_eq!(sample_status(&model.lanes()[0]), SampleStatus::Loading);
        assert!(model.set_loading(id, false));
    }

    /// Every one of the four is a different text, which is what makes the
    /// column readable at a glance down a list of ten lanes.
    #[test]
    fn the_four_states_say_four_different_things() {
        let model = model_with_a_sample_of(1024);
        let texts = [
            SampleStatus::Loading.text(),
            SampleStatus::Failed("can't read file").text(),
            sample_status(loaded_lane(&model)).text(),
            SampleStatus::Empty.text(),
        ];

        for (index, text) in texts.iter().enumerate() {
            assert!(!text.is_empty(), "state {index} says nothing at all");
            assert_eq!(
                texts.iter().filter(|other| other == &text).count(),
                1,
                "state {index} says what another one says: {texts:?}"
            );
        }
    }

    /// The bar's warning fires exactly at its threshold — strictly above it
    /// — and says the figure in whole mebibytes.
    #[test]
    fn the_size_warning_fires_above_the_threshold_and_names_the_figure() {
        assert_eq!(embed_warning(0), None, "a project with no samples");
        assert_eq!(embed_warning(EMBED_WARN_BYTES), None, "exactly on the threshold is quiet");

        let warning = embed_warning(EMBED_WARN_BYTES + 1).expect("one byte past it warns");
        assert!(warning.starts_with("128 MiB"), "the figure, in whole MiB: {warning}");

        assert_eq!(embed_warning(200 * 1024 * 1024).as_deref(), Some("200 MiB of samples"));
    }

    /// The warning is one short line, because the room it gets is whatever
    /// the top bar's three controls leave over
    /// ([`crate::editor::theme::TOP_BAR_VOLUME_WIDTH`] and its neighbours).
    /// The sentence lives on the hover instead.
    #[test]
    fn the_size_warning_is_short_and_its_sentence_is_on_the_hover() {
        let warning = embed_warning(EMBED_WARN_BYTES * 8).expect("a gigabyte of samples");
        assert!(warning.chars().count() <= 24, "too long for the bar: {warning}");
        assert!(EMBED_WARN_HOVER.len() > warning.len(), "the hover is where the words are");
        assert!(
            EMBED_WARN_HOVER.contains("Nothing is stopped"),
            "the hover has to say that a save still saves"
        );
    }

    /// The drop message counts the files that were lost and says nothing
    /// at all about the drops that lost none, which is every drop but the
    /// one made into a full list.
    #[test]
    fn the_drop_message_counts_the_files_that_did_not_fit() {
        assert_eq!(drop_overflow(0), None, "a drop that landed whole says nothing");
        assert_eq!(drop_overflow(1).as_deref(), Some("1 file didn't fit"), "singular");
        assert_eq!(drop_overflow(2).as_deref(), Some("2 files didn't fit"));
        assert_eq!(
            drop_overflow(MAX_LANES as usize - 1).as_deref(),
            Some("127 files didn't fit"),
            "the most a single drop can lose into a list of one free lane"
        );
    }

    /// Like the size warning, it is one short line with its sentence on the
    /// hover: the bar is one control tall and the line gets whatever the
    /// three controls leave over
    /// ([`crate::editor::theme::TOP_BAR_VOLUME_WIDTH`] and its neighbours).
    #[test]
    fn the_drop_message_is_short_and_its_sentence_is_on_the_hover() {
        let message = drop_overflow(127).expect("a drop that lost 127 files");
        assert!(message.chars().count() <= 24, "too long for the bar: {message}");
        assert!(DROP_FULL_HOVER.len() > message.len(), "the hover is where the words are");
        assert!(
            DROP_FULL_HOVER.contains("were not loaded"),
            "the hover has to say that the files did not land"
        );
    }

    /// "give each its own output" says how many lanes found no free output,
    /// in so many words, and nothing when all of them did.
    #[test]
    fn the_outputs_message_counts_the_lanes_that_kept_their_output() {
        assert_eq!(outputs_kept(0), None, "every lane found an output");
        assert_eq!(outputs_kept(1).as_deref(), Some("1 lane kept its output: no free outputs"));
        assert_eq!(
            outputs_kept(2).as_deref(),
            Some("2 lanes kept their output: no free outputs")
        );
        assert!(OUTPUTS_FULL_HOVER.contains("Out 1"), "the hover says how to free one");
    }

    /// Every text a status surface can draw is drawable: egui's
    /// `default_fonts` (which chopdrop enables itself — without the feature
    /// the editor draws no text) are Latin and a little punctuation, so a
    /// message spelled with a symbol would reach the user as a box.
    ///
    /// The whole set is checked and not just [`NOTE_MARK`], because the
    /// messages come from three files — the decoder's rejections, the
    /// restore failures, and this module's own — and the em dash two of them
    /// use is exactly the kind of character this is about. The ellipsis of
    /// [`LOADING`] is the other.
    #[test]
    fn every_message_is_spelled_in_glyphs_the_default_fonts_have() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        // A pass, so the context has built its fonts before they are asked.
        ctx.run_ui(egui::RawInput::default(), |_| {}).drop_without_applying_deltas();

        // The theme pins the dark style and sets both of egui's
        // ([`theme::apply`]), so the dark one is the one every text in this
        // window is laid out with.
        let style = ctx.style_of(egui::Theme::Dark);
        let body = egui::TextStyle::Body.resolve(&style);

        let size_warning = embed_warning(EMBED_WARN_BYTES + 1).expect("past the threshold");
        let rejections = [
            DecodeError::Unreadable,
            DecodeError::TooBig { max_bytes: MAX_SAMPLE_BYTES },
            DecodeError::TooBig { max_bytes: MAX_SOURCE_BYTES },
            DecodeError::BitwigClip,
            DecodeError::Unsupported,
            DecodeError::NoAudio,
            DecodeError::TooLong,
            DecodeError::SourceTooLong,
        ]
        .map(|rejected| rejected.message());
        let drop_message = drop_overflow(3).expect("three files lost");
        let outputs_message = outputs_kept(3).expect("three lanes kept theirs");
        let texts = [
            OUTPUTS_FULL_HOVER,
            &outputs_message,
            NOTE_MARK,
            LOADING,
            RENDERING,
            RENDER_FAILED,
            RENDER_FAILED_HOVER,
            DROP_HINT,
            EMBED_WARN_HOVER,
            &size_warning,
            DROP_FULL_HOVER,
            &drop_message,
            SAMPLE_RESTORE_FAILED,
            SAMPLE_MISSING_PREFIX,
            SAMPLE_NOT_EMBEDDED,
        ];

        for text in texts.iter().copied().chain(rejections.iter().map(|rejected| &**rejected)) {
            for character in text.chars() {
                assert!(
                    ctx.fonts_mut(|fonts| fonts.has_glyph(&body, character)),
                    "{character:?} of {text:?} would be drawn as a box"
                );
            }
        }
    }
}
