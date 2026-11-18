//! What a project saves, and the colours a fresh instance hands its lanes.
//!
//! This module is the serde boundary: the versioned snapshot of every lane —
//! settings, and the sample as the original file's bytes — that nice-plug
//! writes into the host's project and reads back out of it. [`Snapshot`] is
//! that shape, [`Snapshot::of`] makes one out of the model's lanes, and
//! [`Snapshot::into_restore`] turns one back into lanes the model's writer
//! can take — plus, per lane, the [`PendingSample`] the loader has to work
//! through before that lane plays again. The model owns both ends of that
//! trip ([`crate::model::Model::snapshot`] and
//! [`crate::model::Model::load_snapshot`]); nothing here touches a `Model`.
//!
//! The project's size limits are here too, because they are facts about what
//! a project can carry: [`SAMPLE_NOT_EMBEDDED`] and the cap behind it (a
//! sample too big to embed is saved path-only, and its lane says so), and
//! [`EMBED_WARN_BYTES`] with [`embed_warning`], which the editor asks about
//! the total the model counts.
//!
//! The other thing here is [`LANE_PALETTE`], which the editor's theme reads.
//! It lives here rather than in `editor/theme.rs` because a lane's colour is
//! *saved state*, not a looked-up style: the palette's `0xRRGGBB` is written
//! into the project file, so a palette that drifted from the one the theme
//! paints with would repaint every reloaded patch. The theme converts this
//! list to `egui` colours ([`crate::editor::theme::LANE_SWATCHES`]) and keeps
//! no list of its own.
//!
//! Nothing here imports a nice-plug type: the snapshot is plain serde and
//! unit-tested without the framework.

use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

use crate::chop::{self, ChopEditor, ChopOrigin, View};
use crate::model::{Lane, LaneMode, MAX_LANES, SampleRef};
use crate::time::{Length, TimeMode};

/// The colours a new instance's lanes are given, as `0xRRGGBB`, in order:
/// lane *n* of a fresh instance gets `LANE_PALETTE[n]`, and `+ lane` carries
/// on round the list once past the end.
///
/// bitstep's palette, copied value for value, because the two plugins sit
/// side by side in one project and their lane colours have to read as one
/// instrument's. Ten colours walking the hue circle once — red through
/// green, then cyan through pink — all at about one brightness, so no lane
/// looks more important than another and all ten stay legible against
/// [`crate::editor::theme::PANEL`].
///
/// Only the low 24 bits are ever read (`theme::rgb`), so a stored colour from
/// a future version that sets the top byte cannot come out transparent.
pub const LANE_PALETTE: [u32; 10] = [
    0xE2_6E_6E, 0xE2_9A_5E, 0xE2_C9_5E, 0xA8_C9_6E, 0x6E_C9_8A, // red … green
    0x6E_C2_C9, 0x6E_96_D6, 0x8E_7E_D6, 0xC2_7E_D6, 0xD6_7E_A8, // cyan … pink
];

/// The version every save writes, and the one a load reads best.
///
/// Version 2 added the per-lane settings (attack, release, cut itself,
/// output, time mode, length, fine, pitch). Every one of them defaults to
/// what a version 1 lane did, so a version 1 project needs no branch on this
/// number to sound the same: a missing field *is* the v1 behaviour
/// ([`LaneSnapshot::default`]).
///
/// A load does **not** refuse a higher number: the fields are
/// forward-tolerant (unknown ones are ignored, missing ones default), so a
/// project saved by a later chopdrop opens here with whatever this version
/// understands of it, which is better for the user than an empty instrument.
/// The number is here to be *branched on* the day a field's meaning changes
/// rather than its name — the only kind of change the field names cannot
/// describe by themselves.
pub const CURRENT_VERSION: u32 = 2;

/// What a lane says when the sample saved with it could not be brought back:
/// base64 that is not base64, embedded bytes no decoder accepts.
///
/// One sentence for both halves of that failure, because they are one thing
/// as far as a user can act on it — the project's copy of the audio is
/// unusable, and the lane kept everything else. "Reload from file" is the way
/// out, which is why the path is still shown.
pub const SAMPLE_RESTORE_FAILED: &str = "saved sample couldn't be restored";

/// What a lane says when a path-only restore found nothing it could read at
/// the saved path, followed by the sample's file name
/// ("sample file missing: <file_name>").
///
/// A different sentence from [`SAMPLE_RESTORE_FAILED`] because the user's
/// move is different: this project never carried the audio, so the file —
/// moved, renamed or on the other machine — is the thing to go and find.
pub const SAMPLE_MISSING_PREFIX: &str = "sample file missing: ";

/// What a lane says when its sample is too big for the project to carry it:
/// over [`crate::sample::MAX_SAMPLE_BYTES`], so the save keeps the path and
/// not the bytes.
///
/// Not a failure — the lane plays, this session and every session that can
/// still reach the file — which is why it is phrased as what the project
/// does rather than as something that went wrong, and why
/// [`crate::model::Lane::embed_note`] hands it out beside the sample's name
/// instead of in place of it.
pub const SAMPLE_NOT_EMBEDDED: &str = "too big to save in the project — keeping the file path";

/// The embedded total a project should not quietly pass: 128 MiB of
/// original sample bytes, above which the editor warns
/// ([`embed_warning`]).
///
/// Deliberately conservative, and it is the *pre-base64* figure: bitstep
/// found that Bitwig refuses state over 256 MiB, and base64 costs about
/// 4/3× before zstd wins some of it back, so 128 MiB of samples is already
/// near enough that spending the rest should be a knowing decision.
/// Trivial to raise — one constant — and the warning is advice, never a
/// refusal: nothing here stops a save.
pub const EMBED_WARN_BYTES: usize = 128 * 1024 * 1024;

/// The longest `bytes_b64` that could still decode to a file the loader
/// would accept, beyond which an embed is left unread
/// ([`SampleSnapshot::into_pending`]).
///
/// Four base64 characters carry three bytes, so encoding
/// [`crate::sample::MAX_SAMPLE_BYTES`] takes `ceil(cap / 3) * 4`
/// characters; the `+ 2` blocks of slack keep that ceiling (and any
/// padding) comfortably inside the bound, because the one error this must
/// not make is turning away a sample that is actually within the cap. What
/// it buys is that a project claiming a gigabyte of embedded audio costs a
/// length check rather than a gigabyte-long decode the cap would then
/// refuse anyway.
const MAX_EMBED_B64_LEN: usize = (crate::sample::MAX_SAMPLE_BYTES / 3 + 2) * 4;

/// How [`LaneMode::OneShot`] is spelled in a project file.
const MODE_ONE_SHOT: &str = "one_shot";
/// How [`LaneMode::Gate`] is spelled in a project file.
const MODE_GATE: &str = "gate";

/// Everything a project must restore: the instance switch and every lane,
/// in display order, each with its sample's original file bytes.
///
/// This is the whole of what chopdrop saves. It goes into the host's project
/// as the one `#[persist = "lanes"]` blob, which nice-plug serialises as
/// JSON and compresses with zstd — compression that matters here precisely
/// because the blob embeds the sample files themselves.
///
/// Every field defaults, so a project file that is missing one — or whose
/// writer was an older or a newer chopdrop — loads as far as it can rather
/// than not at all. Unknown fields are ignored, which is serde's own
/// behaviour and the reason nothing here says `deny_unknown_fields`: a
/// field this version has never heard of is a field a later version added,
/// and it must not cost the user the lanes alongside it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    /// [`CURRENT_VERSION`] when this snapshot was written here.
    ///
    /// Missing in the file means 1 (see [`legacy_version`]) — not 0, and not
    /// the current version: an absent `version` can only come from a writer
    /// that predates the field, and 1 is what such a writer wrote.
    #[serde(default = "legacy_version")]
    pub version: u32,
    /// The "bitstep Sample pad" switch, instance-wide. A file that omits it
    /// loads it **off** (`bool`'s default), which is what every writer that
    /// predates the field meant — deliberately not
    /// [`crate::model::SAMPLE_PAD_SWITCH_DEFAULT`], which is on and only
    /// decides a fresh instance.
    #[serde(default)]
    pub sample_pad_switch: bool,
    /// The lanes in display order. Lane ids are **not** here: they are
    /// per-instance and a load hands out fresh ones.
    #[serde(default)]
    pub lanes: Vec<LaneSnapshot>,
    /// (v2) The chop editor's source path, view, selection and strip
    /// settings — `None` when the editor has nothing to remember (a v1
    /// file, or an editor never touched). Never audio: the source is
    /// reopened by path through the loader on load.
    #[serde(default)]
    pub chop_editor: Option<ChopEditorSnapshot>,
}

impl Snapshot {
    /// The snapshot that saves these lanes and this switch.
    ///
    /// Takes the lanes rather than a `Model` so that this module stays on
    /// its own side of the boundary (and testable without one);
    /// [`crate::model::Model::snapshot`] is the one caller.
    ///
    /// A lane's message and its "loading…" flag are not saved: both describe
    /// this session's attempt at a file, and the next load of this project
    /// makes its own.
    pub fn of(lanes: &[Lane], sample_pad_switch: bool) -> Snapshot {
        Snapshot {
            version: CURRENT_VERSION,
            sample_pad_switch,
            lanes: lanes.iter().map(LaneSnapshot::of).collect(),
            chop_editor: None,
        }
    }

    /// Turn a parsed snapshot into what a load needs: lanes for
    /// [`crate::model::Model::replace_lanes`], and the sample work beside
    /// them.
    ///
    /// Hostile input is the normal case here — this is a file anything could
    /// have written — so nothing in this direction can fail or panic:
    /// out-of-range values are clamped by the same functions the GUI's
    /// setters use ([`Lane::clamp`]), an unreadable `mode` becomes the
    /// default, and a sample that cannot be restored leaves its lane empty
    /// with [`SAMPLE_RESTORE_FAILED`] on it.
    ///
    /// Lanes past [`MAX_LANES`] are dropped **before** their base64 is
    /// touched: a file claiming a million lanes should cost a million
    /// discarded structs, not a million decoded megabytes.
    ///
    /// A version 1 file had no release: its gate lanes faded over
    /// [`crate::voices::FADE_MS`], so every lane of one loads at
    /// [`crate::model::RELEASE_MS_V1`] rather than the serde default.
    pub fn into_restore(mut self) -> Restore {
        self.lanes.truncate(MAX_LANES as usize);
        if self.version < 2 {
            for stored in &mut self.lanes {
                stored.release_ms = crate::model::RELEASE_MS_V1;
            }
        }

        let mut lanes = Vec::with_capacity(self.lanes.len());
        let mut samples = Vec::with_capacity(self.lanes.len());
        for stored in self.lanes {
            let (lane, pending) = stored.into_lane();
            lanes.push(lane);
            samples.push(pending);
        }

        let chop_editor = self.chop_editor.map(ChopEditorSnapshot::into_restore);
        Restore { lanes, sample_pad_switch: self.sample_pad_switch, samples, chop_editor }
    }
}

/// A snapshot float as a project file may hold it: a JSON number reads as
/// itself, anything else — `null` (what `serde_json` writes for a NaN), a
/// string, a bool, an array, an object — as NaN, which the field's load
/// clamp turns into its default. One bad number then costs one field, not
/// the whole project.
fn lenient_f64<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    deserializer.deserialize_any(LenientNumber)
}

/// [`lenient_f64`] for an `f32` field; a number past `f32`'s range reads
/// as an infinity, as serde's own `f32` would, and is clamped like one.
fn lenient_f32<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    lenient_f64(deserializer).map(|value| value as f32)
}

/// The visitor behind [`lenient_f64`]: numbers pass, everything else is
/// consumed whole (so the parse carries on after it) and reads as NaN.
struct LenientNumber;

impl<'de> serde::de::Visitor<'de> for LenientNumber {
    type Value = f64;

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str("a number (anything else reads as NaN)")
    }

    fn visit_f64<E>(self, value: f64) -> Result<f64, E> {
        Ok(value)
    }

    fn visit_i64<E>(self, value: i64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_u64<E>(self, value: u64) -> Result<f64, E> {
        Ok(value as f64)
    }

    fn visit_bool<E>(self, _: bool) -> Result<f64, E> {
        Ok(f64::NAN)
    }

    fn visit_str<E>(self, _: &str) -> Result<f64, E> {
        Ok(f64::NAN)
    }

    fn visit_bytes<E>(self, _: &[u8]) -> Result<f64, E> {
        Ok(f64::NAN)
    }

    fn visit_unit<E>(self) -> Result<f64, E> {
        Ok(f64::NAN)
    }

    fn visit_none<E>(self) -> Result<f64, E> {
        Ok(f64::NAN)
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, inner: D) -> Result<f64, D::Error> {
        inner.deserialize_any(self)
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<f64, A::Error> {
        while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
        Ok(f64::NAN)
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<f64, A::Error> {
        while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
        Ok(f64::NAN)
    }
}

/// The chop editor as a project file holds it: the source by path only,
/// the main view, the selection, snap and the strip's size. Looping is
/// never saved (it always loads off).
///
/// `#[serde(default)]` on the struct, so a missing field takes the fresh
/// editor's value (snap on, [`chop::STRIP_HEIGHT`]).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChopEditorSnapshot {
    /// The source's path; empty for no source.
    pub source_path: String,
    /// The main view's first frame.
    pub first: u64,
    /// The main view's zoom.
    #[serde(deserialize_with = "lenient_f64")]
    pub frames_per_pixel: f64,
    /// The selection, frames `[start, end)`.
    pub selection: Option<(u64, u64)>,
    pub snap: bool,
    pub collapsed: bool,
    #[serde(deserialize_with = "lenient_f32")]
    pub height_px: f32,
}

impl Default for ChopEditorSnapshot {
    /// A fresh editor: no source, nothing selected, snap on, expanded at
    /// the default height.
    fn default() -> Self {
        let editor = ChopEditor::default();
        ChopEditorSnapshot {
            source_path: String::new(),
            first: editor.view.first,
            frames_per_pixel: editor.view.frames_per_pixel,
            selection: None,
            snap: editor.snap,
            collapsed: editor.collapsed,
            height_px: editor.height_px,
        }
    }
}

impl ChopEditorSnapshot {
    /// What saving this editor writes, or `None` when there is nothing to
    /// remember (no source and the strip as a fresh one is).
    ///
    /// While the source is still loading (or could not be found) the view
    /// and selection it is waiting to apply are what is saved, so saving a
    /// project whose source has not landed yet does not forget them.
    pub fn of(editor: &ChopEditor) -> Option<ChopEditorSnapshot> {
        let (view, selection) = if editor.source.is_some() {
            (editor.view, editor.selection.map(|selection| (selection.start, selection.end)))
        } else {
            (editor.pending_view.unwrap_or(editor.view), editor.pending_selection)
        };
        let saved = ChopEditorSnapshot {
            source_path: editor.source_path.clone().unwrap_or_default(),
            first: view.first,
            frames_per_pixel: view.frames_per_pixel,
            selection,
            snap: editor.snap,
            collapsed: editor.collapsed,
            height_px: editor.height_px,
        };
        let untouched = ChopEditorSnapshot {
            first: saved.first,
            frames_per_pixel: saved.frames_per_pixel,
            ..ChopEditorSnapshot::default()
        };
        (saved != untouched).then_some(saved)
    }

    /// What a load applies, clamped: hostile input is the normal case.
    /// A selection that is empty or backwards is dropped; a zoom that is not
    /// a finite number of at least 1 frame/px means "fit"; the height is
    /// clamped to the strip's range. The view and selection are clamped
    /// again to the source when it lands.
    pub fn into_restore(self) -> ChopRestore {
        let fpp = self.frames_per_pixel;
        let view = (fpp.is_finite() && fpp >= 1.0)
            .then_some(View { first: self.first, frames_per_pixel: fpp });
        let selection = self.selection.filter(|&(start, end)| start < end);
        let source_path = Some(self.source_path).filter(|path| !path.is_empty());
        ChopRestore {
            view: source_path.as_ref().and(view),
            selection: source_path.as_ref().and(selection),
            source_path,
            snap: self.snap,
            collapsed: self.collapsed,
            height_px: chop::clamp_strip_height(self.height_px),
        }
    }
}

/// A saved chop editor, clamped and ready for
/// [`crate::model::Model::load_snapshot`] to apply: the source to reopen
/// (through the loader), the view and selection to apply once it lands,
/// and the strip settings to apply at once.
#[derive(Clone, Debug, PartialEq)]
pub struct ChopRestore {
    pub source_path: Option<String>,
    pub view: Option<View>,
    pub selection: Option<(u64, u64)>,
    pub snap: bool,
    pub collapsed: bool,
    pub height_px: f32,
}

/// One lane as a project file holds it: everything in [`Lane`] that outlives
/// the session, and nothing else.
///
/// `#[serde(default)]` on the struct, so a lane missing any field still
/// loads, taking that field from [`LaneSnapshot::default`] — a fresh lane's
/// value, not a zero: a lane whose `colour` went missing should look like a
/// lane, and one whose `note` did should answer to the note a first lane
/// answers to.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LaneSnapshot {
    /// The lane's name, up to [`crate::model::MAX_NAME_LEN`] characters once
    /// loaded (a longer one in the file is trimmed, not refused).
    pub name: String,
    /// The note that plays it, 0–127 (Bitwig names 60 as C3).
    pub note: u8,
    /// Its colour as `0xRRGGBB`.
    pub colour: u32,
    /// Its own volume in dB.
    #[serde(deserialize_with = "lenient_f32")]
    pub volume_db: f32,
    /// Where it sits in the stereo field, −1..=1.
    #[serde(deserialize_with = "lenient_f32")]
    pub pan: f32,
    /// Its choke group: 0 for none, otherwise 1..=[`crate::model::CHOKE_GROUPS`].
    pub choke: u8,
    /// `"one_shot"` or `"gate"`.
    ///
    /// A string and not a serde enum on purpose: a mode this version has
    /// never heard of has to load as *something*, and an enum would fail the
    /// whole lane — the whole project, in fact, since one bad lane would
    /// fail the parse. Anything unrecognised reads as
    /// [`LaneMode::OneShot`], the default.
    pub mode: String,
    /// The sample, if the lane had one.
    pub sample: Option<SampleSnapshot>,
    /// (v2) The fade-in from note-on in ms; 0 when missing.
    #[serde(deserialize_with = "lenient_f32")]
    pub attack_ms: f32,
    /// (v2) The Gate-mode fade after note-off in ms; 300 when missing, the
    /// fixed release v1 gate lanes had.
    #[serde(deserialize_with = "lenient_f32")]
    pub release_ms: f32,
    /// (v2) Whether a note-on fades out the lane's own sounding voices.
    ///
    /// `false` when missing — v1 lanes never cut themselves, and a v1
    /// project must sound the same — even though a lane made fresh in v2
    /// starts with it on ([`crate::model::CUT_ITSELF_DEFAULT`]).
    pub cut_itself: bool,
    /// (v2) The output index, 0 (Out 1) ..= [`crate::model::OUTPUT_MAX`];
    /// anything above lands on Out 1.
    pub output: u8,
    /// (v2) `"off"`, `"repitch"` or `"stretch"` ([`TimeMode::as_str`]).
    ///
    /// A string for the reason [`mode`](Self::mode) is: anything
    /// unrecognised reads as [`TimeMode::Off`] instead of failing the load.
    pub time_mode: String,
    /// (v2) `"as_cut"` or a note length's string ([`Length::as_str`]);
    /// anything unrecognised reads as [`Length::AsCut`].
    pub length: String,
    /// (v2) The fine tempo trim in percent; 0 when missing.
    #[serde(deserialize_with = "lenient_f32")]
    pub fine_pct: f32,
    /// (v2) The pitch in semitones; 0 when missing.
    #[serde(deserialize_with = "lenient_f32")]
    pub pitch_st: f32,
}

impl Default for LaneSnapshot {
    /// An empty lane as version 1 would have written one: no sample, the
    /// first palette colour, the first default note, 0 dB, centred, no
    /// choke group, one-shot — and every version 2 field at the value that
    /// reproduces v1's behaviour: no attack, the 300 ms release, no cutting
    /// itself, Out 1, time off, as cut, no fine trim, no pitch.
    ///
    /// Only a field-by-field fallback for a file that left something out —
    /// nothing in the plugin starts from this.
    fn default() -> LaneSnapshot {
        LaneSnapshot {
            name: String::new(),
            note: crate::model::DEFAULT_FIRST_NOTE,
            colour: LANE_PALETTE[0],
            volume_db: crate::model::LANE_VOLUME_DB_DEFAULT,
            pan: crate::model::LANE_PAN_DEFAULT,
            choke: crate::model::CHOKE_NONE,
            mode: String::from(MODE_ONE_SHOT),
            sample: None,
            attack_ms: crate::model::ATTACK_MS_DEFAULT,
            release_ms: crate::model::RELEASE_MS_DEFAULT,
            cut_itself: false,
            output: crate::model::OUTPUT_MAIN,
            time_mode: String::from(TimeMode::Off.as_str()),
            length: String::from(Length::AsCut.as_str()),
            fine_pct: crate::model::FINE_DEFAULT,
            pitch_st: crate::model::PITCH_DEFAULT,
        }
    }
}

impl LaneSnapshot {
    /// What saving this lane writes.
    fn of(lane: &Lane) -> LaneSnapshot {
        LaneSnapshot {
            name: lane.name.clone(),
            note: lane.note,
            colour: lane.colour,
            volume_db: lane.volume_db,
            pan: lane.pan,
            choke: lane.choke,
            mode: String::from(mode_name(lane.mode)),
            sample: lane.sample.as_ref().map(SampleSnapshot::of),
            attack_ms: lane.attack_ms,
            release_ms: lane.release_ms,
            cut_itself: lane.cut_itself,
            output: lane.output,
            time_mode: String::from(lane.time_mode.as_str()),
            length: String::from(lane.length.as_str()),
            fine_pct: lane.fine_pct,
            pitch_st: lane.pitch_st,
        }
    }

    /// The lane this describes, clamped into range, plus the sample work it
    /// leaves for the loader.
    ///
    /// The lane comes back with a **placeholder id of 0**: ids are
    /// per-instance and this one's comes from the model's counter, which
    /// [`crate::model::Model::replace_lanes`] stamps on every lane it takes.
    /// Nothing between here and there reads the id.
    ///
    /// A sample that cannot even be handed to the loader — base64 that will
    /// not decode, no bytes and no path to read — is reported on the lane
    /// itself and asks for no job: there is nothing for a worker to try.
    fn into_lane(self) -> (Lane, Option<PendingSample>) {
        let mut lane = Lane::new(0, &self.name, self.note, self.colour);
        lane.volume_db = self.volume_db;
        lane.pan = self.pan;
        lane.choke = self.choke;
        lane.mode = mode_from_name(&self.mode);
        lane.attack_ms = self.attack_ms;
        lane.release_ms = self.release_ms;
        // From the file, never from `Lane::new`'s v2 default: a v1 file has
        // no such field, and its serde default is v1's `false`.
        lane.cut_itself = self.cut_itself;
        lane.output = self.output;
        lane.time_mode = TimeMode::from_str(&self.time_mode).unwrap_or_default();
        lane.length = Length::from_str(&self.length).unwrap_or_default();
        lane.fine_pct = self.fine_pct;
        lane.pitch_st = self.pitch_st;
        lane.clamp();

        let Some(sample) = self.sample else {
            return (lane, None);
        };

        match sample.into_pending() {
            Some(pending) => (lane, Some(pending)),
            None => {
                lane.error = Some(String::from(SAMPLE_RESTORE_FAILED));
                (lane, None)
            }
        }
    }
}

/// A lane's sample as a project file holds it: where it came from, and —
/// normally — the file itself.
///
/// The embedded copy is the truth. A project that has travelled to another
/// machine, or whose samples have since been renamed, edited or deleted,
/// still plays exactly what it was saved with, and [`path`](Self::path) is
/// only what the lane shows and what "reload from file" re-reads.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SampleSnapshot {
    /// The file the sample was loaded from, for display and for reloading.
    pub path: String,
    /// The last component of [`path`](Self::path), saved rather than
    /// re-derived so that a lane keeps the name it showed even if the path
    /// in the file is something no filesystem would give a name to.
    pub file_name: String,
    /// The original file's bytes as standard, padded base64 — `None` when
    /// the save was path-only (over the embedding cap).
    ///
    /// base64 and not a byte array because this blob is JSON: a `Vec<u8>`
    /// would serialise as a list of numbers, which costs three to four
    /// characters per byte against base64's four per three.
    pub bytes_b64: Option<String>,
    /// Where a chop's audio was cut from; absent (v1 files, and every
    /// sample that is not a chop) is `None`. Not written when `None`, so an
    /// ordinary sample saves exactly as it did in version 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<OriginSnapshot>,
}

/// A chop sample's [`ChopOrigin`] as a project file holds it: the source
/// path and the frames `[start_frame, end_frame)` cut from it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OriginSnapshot {
    /// The source file the chop was cut from.
    pub source_path: String,
    /// The cut's first frame.
    pub start_frame: u64,
    /// One past the cut's last frame.
    pub end_frame: u64,
}

impl OriginSnapshot {
    /// What saving a chop's origin writes.
    fn of(origin: &ChopOrigin) -> OriginSnapshot {
        OriginSnapshot {
            source_path: origin.source_path.clone(),
            start_frame: origin.start_frame,
            end_frame: origin.end_frame,
        }
    }

    /// The origin this entry restores, or `None` when it describes no cut a
    /// chop could have come from (hostile input): an empty source path, or
    /// a region that is empty or runs backwards. Losing it costs only the
    /// hover text and "open source in editor" — the sample still plays.
    fn into_origin(self) -> Option<ChopOrigin> {
        if self.source_path.is_empty() || self.end_frame <= self.start_frame {
            return None;
        }
        Some(ChopOrigin {
            source_path: self.source_path,
            start_frame: self.start_frame,
            end_frame: self.end_frame,
        })
    }
}

impl SampleSnapshot {
    /// What saving this sample writes: the file's own bytes, base64'd — or
    /// nothing but the path, when the file is over the embedding cap.
    ///
    /// [`SampleRef::embeds`] is the one place that choice is made, so the
    /// bytes this writes, the total the model counts
    /// ([`crate::model::Model::embedded_bytes`]) and the note the lane
    /// carries ([`crate::model::Lane::embed_note`]) cannot disagree about
    /// which samples a project carries.
    ///
    /// A path-only entry is all a lane that big can be given: 64 MiB is
    /// already most of what a host will accept as state, and a save that
    /// embedded it anyway would risk the whole project — every other lane
    /// included — for the one sample. The path still restores it on this
    /// machine, and the lane says so.
    fn of(sample: &SampleRef) -> SampleSnapshot {
        SampleSnapshot {
            path: sample.path.clone(),
            file_name: sample.file_name.clone(),
            bytes_b64: sample.embeds().then(|| STANDARD.encode(&**sample.bytes)),
            origin: sample.origin.as_ref().map(OriginSnapshot::of),
        }
    }

    /// The loader's half of restoring this sample, or `None` if there is
    /// nothing a loader could do with it.
    ///
    /// `None` covers the two cases that are already lost before any audio
    /// decoding is tried: base64 that does not decode (truncated, re-wrapped
    /// or simply not base64), and base64 that decodes to nothing. A
    /// path-only entry with no path is lost the same way — there is no file
    /// to read and no bytes to decode.
    ///
    /// An embed longer than the cap allows ([`MAX_EMBED_B64_LEN`]) is not a
    /// failure, it is a path-only restore: the text is dropped unread and
    /// the loader is asked for the file instead, which is the one chance
    /// such a lane has of playing anything. With no path to fall back to,
    /// it joins the cases above.
    fn into_pending(self) -> Option<PendingSample> {
        self.into_pending_capped(MAX_EMBED_B64_LEN)
    }

    /// [`SampleSnapshot::into_pending`] against a given base64 length cap,
    /// so that the over-cap fallback is testable without building 85 MiB of
    /// base64 (`sample.rs`'s `read_file` / `read_file_with` split the same way).
    fn into_pending_capped(self, max_b64_len: usize) -> Option<PendingSample> {
        let bytes = match &self.bytes_b64 {
            Some(b64) if b64.len() > max_b64_len => None,
            Some(b64) => {
                let bytes = STANDARD.decode(b64.as_bytes()).ok()?;
                if bytes.is_empty() {
                    return None;
                }
                Some(bytes)
            }
            None => None,
        };

        if bytes.is_none() && self.path.is_empty() {
            return None;
        }

        let file_name = restored_file_name(&self.path, self.file_name);
        let origin = self.origin.and_then(OriginSnapshot::into_origin);
        Some(PendingSample { path: self.path, file_name, bytes, origin })
    }
}

// `bytes_b64` is up to 85 MiB of base64, and a failing test that printed it
// would be unreadable twice over. Its size is the part worth seeing.
impl std::fmt::Debug for SampleSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SampleSnapshot")
            .field("path", &self.path)
            .field("file_name", &self.file_name)
            .field("bytes_b64", &self.bytes_b64.as_ref().map(|b64| b64.len()))
            .field("origin", &self.origin)
            .finish()
    }
}

/// A parsed snapshot in the two pieces a load uses it in
/// ([`Snapshot::into_restore`]).
///
/// The lanes go to the model's writer, which clamps them again, assigns them
/// ids and publishes them — so a restored project is playable (if silent)
/// the moment it is loaded. The samples then arrive one by one as the loader
/// decodes them, which is why they travel separately: decoding tens of
/// megabytes on the thread the host is waiting on would hang the host.
#[derive(Debug)]
pub struct Restore {
    /// The lanes in display order, each with a placeholder id of 0 and,
    /// where its sample was already beyond saving, a message on it.
    pub lanes: Vec<Lane>,
    /// The "bitstep Sample pad" switch as the file had it.
    pub sample_pad_switch: bool,
    /// One entry per lane in [`lanes`](Self::lanes), in the same order and
    /// the same number of them: `None` where the lane has no sample to
    /// restore, either because it never had one or because what it had is
    /// unusable.
    pub samples: Vec<Option<PendingSample>>,
    /// The chop editor as the file had it, clamped; `None` for a file with
    /// none, which loads a fresh editor.
    pub chop_editor: Option<ChopRestore>,
}

/// One lane's sample, waiting for the loader (a loader job is a lane id and
/// a source).
///
/// The source is [`bytes`](Self::bytes) when the project embedded the file,
/// and the path otherwise. Both still have to be read by the loader's own
/// screening — size cap, Bitwig-clip magic, decode — because embedded bytes
/// are no more trustworthy than a dropped file: they come out of the same
/// project file every other value here does.
#[derive(Clone, PartialEq)]
pub struct PendingSample {
    /// Where the sample came from, for the lane to show and — when there
    /// are no bytes — for the loader to read.
    pub path: String,
    /// What to call the sample in the lane and in any message about it
    /// ("sample file missing: <file_name>").
    pub file_name: String,
    /// The embedded file, already out of base64; `None` for a path-only
    /// save, which means "read the path instead".
    pub bytes: Option<Vec<u8>>,
    /// The chop origin the restored [`SampleRef`] carries
    /// ([`SampleRef::with_origin`]); `None` for an ordinary sample.
    pub origin: Option<ChopOrigin>,
}

impl PendingSample {
    /// The words a lane shows when this restore does not come back with
    /// audio — whatever the decoder's own complaint was.
    ///
    /// Two sentences, chosen by where the sample was coming from rather
    /// than by what went wrong: embedded bytes that no decoder accepts are
    /// a lost copy ([`SAMPLE_RESTORE_FAILED`]), and a path the loader could
    /// not read, or read something unusable from, is a missing file
    /// ([`SAMPLE_MISSING_PREFIX`]). A restore is not a drop: the decoder's
    /// row of the taxonomy ("unsupported audio file") would describe a file
    /// the user has not chosen in this session and may no longer have,
    /// whereas both of these say what to do next.
    ///
    /// The loader is the caller: it passes this to
    /// [`crate::model::Model::set_error`] when a restore job fails.
    pub fn failure_message(&self) -> String {
        if self.bytes.is_some() {
            String::from(SAMPLE_RESTORE_FAILED)
        } else {
            format!("{SAMPLE_MISSING_PREFIX}{}", self.file_name)
        }
    }
}

// As `SampleSnapshot`'s: sizes, not 64 MiB of contents.
impl std::fmt::Debug for PendingSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingSample")
            .field("path", &self.path)
            .field("file_name", &self.file_name)
            .field("bytes", &self.bytes.as_ref().map(Vec::len))
            .field("origin", &self.origin)
            .finish()
    }
}

/// The version a file that has none is from: 1, the first there was
/// (see [`Snapshot::version`]).
fn legacy_version() -> u32 {
    1
}

/// Whether a project embedding this many sample bytes is big enough to warn
/// about: strictly over [`EMBED_WARN_BYTES`].
///
/// Takes the total rather than the lanes so that the threshold is one pure
/// comparison, testable at any size without owning that many bytes; the
/// editor asks it through [`crate::editor::status::embed_warning`], which
/// turns a `true` into the figure the top bar shows, over a total the model
/// counts the lanes for ([`crate::model::Model::embedded_bytes`]).
///
/// Advice and not a limit — every save still saves everything — so the
/// warning is the only consequence of crossing the threshold. One
/// instance's own total, since one instance is all a model knows: a project
/// with four of them can pass the figure the warning is derived from
/// without any one of them saying so, which is the trade for a check that
/// costs nothing and needs no host.
///
/// Strictly over, so a project sitting exactly on the threshold is quiet:
/// the number is a round one chosen with slack in it, and a warning at the
/// boundary would be a warning about the boundary rather than about the
/// project.
pub fn embed_warning(embedded_bytes: usize) -> bool {
    embedded_bytes > EMBED_WARN_BYTES
}

/// How a mode is written into a project file.
fn mode_name(mode: LaneMode) -> &'static str {
    match mode {
        LaneMode::OneShot => MODE_ONE_SHOT,
        LaneMode::Gate => MODE_GATE,
    }
}

/// The mode a project file's string means — [`LaneMode::OneShot`] for
/// anything this version does not recognise, including an empty string and a
/// mode a later chopdrop invented (see [`LaneSnapshot::mode`]).
fn mode_from_name(name: &str) -> LaneMode {
    match name {
        MODE_GATE => LaneMode::Gate,
        _ => LaneMode::OneShot,
    }
}

/// The name a restored sample shows: the saved one, or the path's last
/// component when the file saved none (an older writer, or a hand-edited
/// project), or the whole path when that has no last component either.
///
/// Matches what [`SampleRef::new`] derives for a freshly loaded file, so a
/// restored lane and a dropped one label the same file the same way.
fn restored_file_name(path: &str, saved: String) -> String {
    if !saved.is_empty() {
        return saved;
    }

    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Model;

    /// The palette is pinned, not derived: these ten numbers are written into
    /// saved projects (and into bitstep's), so changing one is a change to
    /// how an existing patch looks, which this test makes anyone say out
    /// loud.
    #[test]
    fn the_palette_is_bitsteps_ten_colours() {
        assert_eq!(
            LANE_PALETTE,
            [
                0xE2_6E_6E, 0xE2_9A_5E, 0xE2_C9_5E, 0xA8_C9_6E, 0x6E_C9_8A, 0x6E_C2_C9,
                0x6E_96_D6, 0x8E_7E_D6, 0xC2_7E_D6, 0xD6_7E_A8,
            ]
        );
    }

    /// Every entry is a plain `0xRRGGBB` with nothing in its top byte, and
    /// none of them is black or white: the colour dot has to say *which*
    /// lane it belongs to, and a default lane may not come out invisible
    /// against the panel.
    #[test]
    fn every_palette_colour_is_a_visible_rgb_triple() {
        for (index, &colour) in LANE_PALETTE.iter().enumerate() {
            assert_eq!(colour >> 24, 0, "colour {index} has bits above 24: {colour:#010X}");
            assert_ne!(colour, 0x00_00_00, "colour {index} is black");
            assert_ne!(colour, 0xFF_FF_FF, "colour {index} is white");
        }

        let mut seen = LANE_PALETTE.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), LANE_PALETTE.len(), "two lanes would start the same colour");
    }
    // ---- The snapshot ---------------------------------------------------

    /// Every persistent field of every lane as text, in display order: what
    /// a round trip has to preserve.
    ///
    /// Ids are left out, because a load hands out fresh ones, and so are a
    /// lane's message and its "loading…" flag, which belong to a session and
    /// not to a project. The sample is spelled out in full — the file's
    /// bytes *and* the decoded audio — since "lossless" is a claim about
    /// exactly those.
    fn lane_text(lanes: &[Lane]) -> Vec<String> {
        lanes
            .iter()
            .map(|lane| {
                let sample = lane.sample.as_ref().map(|sample| {
                    (&sample.path, &sample.file_name, sample.bytes.to_vec(), &*sample.data)
                });
                format!(
                    "{:?} {} {:06X} {} {} {} {:?} {} {} {} {} {:?} {:?} {} {} {sample:?}",
                    lane.name,
                    lane.note,
                    lane.colour,
                    lane.volume_db,
                    lane.pan,
                    lane.choke,
                    lane.mode,
                    lane.attack_ms,
                    lane.release_ms,
                    lane.cut_itself,
                    lane.output,
                    lane.time_mode,
                    lane.length,
                    lane.fine_pct,
                    lane.pitch_st,
                )
            })
            .collect()
    }

    /// A sample built the way the loader will build one: real WAV bytes and
    /// the audio symphonia decodes out of them (the generator is
    /// `sample.rs`'s own).
    fn a_wav_sample(handle: &basedrop::Handle, path: &str) -> SampleRef {
        let bytes = crate::sample::tests::wav_i16(1, 44_100, &[0, 1_000, -1_000, 32_767]);
        let data = crate::sample::decode(&bytes, Some("wav")).expect("a generated WAV decodes");
        SampleRef::new(handle, Path::new(path), bytes, data)
    }

    /// A one-lane snapshot around this sample, for the cases that are about
    /// the sample rather than about the lane.
    fn snapshot_of(sample: Option<SampleSnapshot>) -> Snapshot {
        Snapshot {
            version: CURRENT_VERSION,
            sample_pad_switch: false,
            chop_editor: None,
            lanes: vec![LaneSnapshot {
                name: String::from("Kick"),
                sample,
                ..LaneSnapshot::default()
            }],
        }
    }

    /// A chop's origin is saved with its sample under these names, and a
    /// load hands it to the loader so the restored lane keeps it.
    #[test]
    fn a_chop_s_origin_survives_the_round_trip_through_json() {
        let mut saved = Model::new();
        let handle = saved.handle().clone();
        let lane = saved.lanes()[0].id;
        let origin = ChopOrigin {
            source_path: String::from("/songs/Eye.wav"),
            start_frame: 1_445_760,
            end_frame: 1_540_320,
        };
        let plain = a_wav_sample(&handle, "/tmp/Eye 1.wav");
        let chop = SampleRef::with_origin(
            &handle,
            Path::new("/tmp/Eye 1.wav"),
            plain.bytes.to_vec(),
            (*plain.data).clone(),
            Some(origin.clone()),
        );
        assert!(saved.set_sample(lane, chop));

        let json = serde_json::to_string(&saved.snapshot()).expect("a snapshot serialises");
        assert!(
            json.contains(concat!(
                r#""origin":{"source_path":"/songs/Eye.wav","#,
                r#""start_frame":1445760,"end_frame":1540320}"#,
            )),
            "{json}"
        );
        let parsed: Snapshot = serde_json::from_str(&json).expect("and parses back");

        let mut loaded = Model::new();
        let pending = loaded.load_snapshot(parsed);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].1.origin, Some(origin));
    }

    /// An ordinary sample writes no origin and loads with none; an origin
    /// that describes no cut (hostile input) is dropped, the sample kept.
    #[test]
    fn a_missing_or_impossible_origin_restores_as_none() {
        let entry = |origin| SampleSnapshot {
            path: String::from("/tmp/kick.wav"),
            file_name: String::from("kick.wav"),
            bytes_b64: Some(String::from("AQID")),
            origin,
        };
        let restore = snapshot_of(Some(entry(None))).into_restore();
        assert_eq!(restore.samples[0].as_ref().expect("bytes to decode").origin, None);

        let hostile = [
            OriginSnapshot { source_path: String::from("/s.wav"), start_frame: 9, end_frame: 3 },
            OriginSnapshot { source_path: String::from("/s.wav"), start_frame: 5, end_frame: 5 },
            OriginSnapshot { source_path: String::new(), start_frame: 0, end_frame: 10 },
        ];
        for origin in hostile {
            let restore = snapshot_of(Some(entry(Some(origin.clone())))).into_restore();
            let pending = restore.samples[0].as_ref().expect("the sample still restores");
            assert_eq!(pending.origin, None, "for {origin:?}");
        }
    }

    /// The field names and the value spellings of version 2, pinned
    /// character for character (version 1's names are the first eight
    /// lane fields, unchanged; the v1 fixture test pins that they still
    /// load).
    ///
    /// This is a file format: these names are in every project anyone has
    /// saved, so renaming one is a change that silently drops whatever it
    /// used to carry (serde ignores what it does not recognise). The test
    /// exists to make that deliberate.
    #[test]
    fn version_two_writes_these_names() {
        let snapshot = Snapshot {
            version: CURRENT_VERSION,
            sample_pad_switch: true,
            chop_editor: Some(ChopEditorSnapshot {
                source_path: String::from("/songs/Eye.wav"),
                first: 4410,
                frames_per_pixel: 12.5,
                selection: Some((100, 900)),
                snap: false,
                collapsed: true,
                height_px: 240.0,
            }),
            lanes: vec![LaneSnapshot {
                name: String::from("Kick"),
                note: 36,
                colour: 0x12_34_56,
                volume_db: -3.5,
                pan: -0.25,
                choke: 2,
                mode: String::from("gate"),
                sample: Some(SampleSnapshot {
                    path: String::from("/tmp/kick.wav"),
                    file_name: String::from("kick.wav"),
                    bytes_b64: Some(String::from("AQID")),
                    origin: None,
                }),
                attack_ms: 5.5,
                release_ms: 120.0,
                cut_itself: true,
                output: 3,
                time_mode: String::from("stretch"),
                length: String::from("1/4"),
                fine_pct: -1.5,
                pitch_st: 2.25,
            }],
        };

        assert_eq!(
            serde_json::to_string(&snapshot).expect("a snapshot serialises"),
            concat!(
                r#"{"version":2,"sample_pad_switch":true,"lanes":[{"name":"Kick","note":36,"#,
                r#""colour":1193046,"volume_db":-3.5,"pan":-0.25,"choke":2,"mode":"gate","#,
                r#""sample":{"path":"/tmp/kick.wav","file_name":"kick.wav","#,
                r#""bytes_b64":"AQID"},"attack_ms":5.5,"release_ms":120.0,"cut_itself":true,"#,
                r#""output":3,"time_mode":"stretch","length":"1/4","fine_pct":-1.5,"#,
                r#""pitch_st":2.25}],"chop_editor":{"source_path":"/songs/Eye.wav","#,
                r#""first":4410,"frames_per_pixel":12.5,"selection":[100,900],"snap":false,"#,
                r#""collapsed":true,"height_px":240.0}}"#,
            )
        );
    }

    /// Saving reads the lane fields that outlive the session and leaves the
    /// two that do not out of the file entirely.
    #[test]
    fn saving_takes_every_persistent_field_and_nothing_else() {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        model.set_mode(id, LaneMode::Gate);
        model.set_volume_db(id, -6.0);
        model.set_error(id, "a file that would not open");
        model.set_loading(id, true);

        let snapshot = model.snapshot();
        assert_eq!(snapshot.version, CURRENT_VERSION);
        assert_eq!(snapshot.lanes.len(), 16, "the sixteen default lanes");
        assert!(snapshot.sample_pad_switch, "a new instance's switch, on, is saved");

        let lane = &snapshot.lanes[0];
        assert_eq!(lane.name, "Lane 1");
        assert_eq!(lane.note, 36);
        assert_eq!(lane.colour, LANE_PALETTE[0]);
        assert_eq!(lane.volume_db, -6.0);
        assert_eq!(lane.pan, 0.0);
        assert_eq!(lane.choke, 0);
        assert_eq!(lane.mode, "gate");
        assert_eq!(lane.sample, None, "no sample has been loaded");

        let json = serde_json::to_string(&snapshot).expect("a snapshot serialises");
        assert!(!json.contains("error"), "a lane's message is not project state: {json}");
        assert!(!json.contains("loading"), "a pending decode is not project state: {json}");
    }

    /// The full round trip: a model with edited lanes and a real sample in
    /// one of them, saved to JSON, parsed back, loaded into a fresh instance
    /// and its samples brought back the way the loader will — settings,
    /// display order, file bytes and decoded audio all identical.
    #[test]
    fn a_model_with_a_sample_survives_the_round_trip_through_json() {
        let mut saved = Model::new();
        let handle = saved.handle().clone();
        let kick = saved.lanes()[0].id;
        let quiet = saved.lanes()[1].id;

        saved.rename(kick, "Kick");
        saved.set_note(kick, 48);
        saved.set_colour(kick, 0x12_34_56);
        saved.set_volume_db(kick, -4.5);
        saved.set_pan(kick, 0.75);
        saved.set_choke(kick, 3);
        saved.set_mode(kick, LaneMode::Gate);
        saved.set_attack_ms(kick, 12.5);
        saved.set_release_ms(kick, 750.0);
        saved.set_cut_itself(kick, false);
        saved.set_output(kick, 7);
        saved.set_time_mode(kick, TimeMode::Stretch);
        saved.set_length(kick, Length::Beats(crate::model::NoteLength::Eighth));
        saved.set_fine_pct(kick, 2.5);
        saved.set_pitch_st(kick, -3.25);
        saved.set_cut_itself(quiet, true);
        saved.set_time_mode(quiet, TimeMode::Repitch);
        saved.set_sample(kick, a_wav_sample(&handle, "/tmp/kick.wav"));
        saved.set_sample_pad_switch(true);
        saved.reorder(0, 3);
        // Neither of these is saved; the load below must not invent them.
        saved.set_error(quiet, "this message is this session's");
        saved.set_loading(quiet, true);

        let json = serde_json::to_string(&saved.snapshot()).expect("a snapshot serialises");
        let parsed: Snapshot = serde_json::from_str(&json).expect("and parses back");

        let mut loaded = Model::new();
        let pending = loaded.load_snapshot(parsed);
        let handle = loaded.handle().clone();
        assert_eq!(pending.len(), 1, "one lane had a sample");
        for (id, sample) in pending {
            let bytes = sample.bytes.expect("the save embedded the file");
            let data = crate::sample::decode(&bytes, None).expect("the embedded bytes decode");
            loaded.set_sample(id, SampleRef::new(&handle, Path::new(&sample.path), bytes, data));
        }

        assert!(loaded.sample_pad_switch(), "the switch is instance state and is saved");
        assert_eq!(lane_text(loaded.lanes()), lane_text(saved.lanes()));

        let kick = &loaded.lanes()[3];
        assert_eq!(kick.name, "Kick");
        assert_eq!(kick.attack_ms, 12.5);
        assert_eq!(kick.release_ms, 750.0);
        assert!(!kick.cut_itself, "a saved `false` is not replaced by the new-lane default");
        assert_eq!(kick.output, 7);
        assert_eq!(kick.time_mode, TimeMode::Stretch);
        assert_eq!(kick.length, Length::Beats(crate::model::NoteLength::Eighth));
        assert_eq!(kick.fine_pct, 2.5);
        assert_eq!(kick.pitch_st, -3.25);

        let second = &loaded.lanes()[0];
        assert!(second.cut_itself);
        assert_eq!(second.time_mode, TimeMode::Repitch);
        assert_eq!(second.name, "Lane 2", "the reorder survived too");
        assert_eq!(second.error, None, "a message describes a session, not a project");
        assert!(!second.loading, "nothing of this instance's is decoding");
    }

    /// `mode` is a string so that a mode this version has never heard of
    /// costs one lane's playing style rather than the whole project.
    #[test]
    fn the_mode_string_round_trips_and_anything_else_is_one_shot() {
        assert_eq!(mode_name(LaneMode::OneShot), "one_shot");
        assert_eq!(mode_name(LaneMode::Gate), "gate");

        for (text, mode) in [
            ("one_shot", LaneMode::OneShot),
            ("gate", LaneMode::Gate),
            ("", LaneMode::OneShot),
            ("Gate", LaneMode::OneShot),
            ("loop_forward", LaneMode::OneShot),
        ] {
            assert_eq!(mode_from_name(text), mode, "mode {text:?}");
        }
    }

    /// A file that left everything out still loads: an absent `version` is
    /// version 1, an absent switch is off, and an absent lane field is a
    /// fresh lane's value rather than a zero.
    #[test]
    fn a_file_missing_everything_still_loads() {
        let snapshot: Snapshot =
            serde_json::from_str("{}").expect("an empty object is a snapshot");
        assert_eq!(snapshot.version, 1, "no version means the first one");
        assert!(!snapshot.sample_pad_switch);
        assert!(snapshot.lanes.is_empty());

        let lane: LaneSnapshot = serde_json::from_str("{}").expect("and so is an empty lane");
        assert_eq!(lane, LaneSnapshot::default());

        let restore = Snapshot { lanes: vec![lane], ..snapshot }.into_restore();
        let lane = &restore.lanes[0];
        assert_eq!(lane.name, "");
        assert_eq!(lane.note, crate::model::DEFAULT_FIRST_NOTE);
        assert_eq!(lane.colour, LANE_PALETTE[0], "not black");
        assert_eq!(lane.volume_db, crate::model::LANE_VOLUME_DB_DEFAULT);
        assert_eq!(lane.pan, crate::model::LANE_PAN_DEFAULT);
        assert_eq!(lane.choke, crate::model::CHOKE_NONE);
        assert_eq!(lane.mode, LaneMode::OneShot);
        assert!(lane.sample.is_none());
        assert_eq!(restore.samples, vec![None], "and nothing for the loader to do");
    }

    /// Fields this version does not know are ignored rather than fatal — a
    /// project a later chopdrop saved opens here, minus whatever that
    /// chopdrop added.
    #[test]
    fn fields_from_another_version_are_ignored() {
        let json = r#"{
            "version": 9,
            "sample_pad_switch": true,
            "swing": 0.5,
            "lanes": [{ "name": "Kick", "note": 40, "reverse": true }]
        }"#;

        let snapshot: Snapshot = serde_json::from_str(json).expect("the known fields parse");
        assert_eq!(snapshot.version, 9);
        let restore = snapshot.into_restore();
        assert!(restore.sample_pad_switch, "a version from the future still loads");
        assert_eq!(restore.lanes.len(), 1);
        assert_eq!(restore.lanes[0].name, "Kick");
        assert_eq!(restore.lanes[0].note, 40);
    }

    /// Every lane value from a file goes through the same clamps the GUI's
    /// setters use, because a project file is input like any other.
    #[test]
    fn a_hostile_lane_is_clamped_into_range() {
        let snapshot = Snapshot {
            version: CURRENT_VERSION,
            sample_pad_switch: false,
            chop_editor: None,
            lanes: vec![LaneSnapshot {
                name: "x".repeat(200),
                note: 200,
                colour: 0xFF_12_34_56,
                volume_db: f32::NAN,
                pan: -9.0,
                choke: 99,
                mode: String::from("gate"),
                sample: None,
                ..LaneSnapshot::default()
            }],
        };

        let restore = snapshot.into_restore();
        let lane = &restore.lanes[0];
        assert_eq!(lane.name.chars().count(), crate::model::MAX_NAME_LEN);
        assert_eq!(lane.note, 127);
        assert_eq!(lane.colour, 0x12_34_56, "the invented alpha byte is dropped");
        assert_eq!(lane.volume_db, crate::model::LANE_VOLUME_DB_DEFAULT, "NaN is unity");
        assert_eq!(lane.pan, crate::model::LANE_PAN_MIN);
        assert_eq!(lane.choke, crate::model::CHOKE_GROUPS);
        assert_eq!(lane.mode, LaneMode::Gate);
    }

    /// A file claiming more lanes than an instance may hold loses the
    /// excess, so a corrupt or malicious project cannot ask for unbounded
    /// lanes.
    #[test]
    fn lanes_past_the_limit_are_dropped() {
        let snapshot = Snapshot {
            version: CURRENT_VERSION,
            sample_pad_switch: false,
            chop_editor: None,
            lanes: vec![LaneSnapshot::default(); MAX_LANES as usize + 10],
        };

        let restore = snapshot.into_restore();
        assert_eq!(restore.lanes.len(), MAX_LANES as usize);
        assert_eq!(restore.samples.len(), restore.lanes.len(), "one entry per lane, always");
    }

    /// An embedded sample travels as standard padded base64 of the file's
    /// own bytes, and comes back as those bytes.
    #[test]
    fn embedded_bytes_travel_as_standard_base64() {
        let collector = basedrop::Collector::new();
        let sample = a_wav_sample(&collector.handle(), "/tmp/kick.wav");
        let bytes = sample.bytes.to_vec();

        let saved = SampleSnapshot::of(&sample);
        assert_eq!(saved.path, "/tmp/kick.wav");
        assert_eq!(saved.file_name, "kick.wav");
        assert_eq!(saved.bytes_b64.as_deref(), Some(STANDARD.encode(&bytes).as_str()));

        let restore = snapshot_of(Some(saved)).into_restore();
        let pending = restore.samples[0].clone().expect("there is a file to decode");
        assert_eq!(pending.bytes.as_deref(), Some(bytes.as_slice()));
        assert_eq!(pending.file_name, "kick.wav");
        assert_eq!(restore.lanes[0].error, None, "nothing has failed");
        assert!(restore.lanes[0].sample.is_none(), "the lane plays once the loader is done");
    }

    /// base64 that is not base64 — truncated, re-wrapped, or empty — leaves
    /// the lane empty with its settings and a message, and asks the loader
    /// for nothing: there is nothing left for a worker to try.
    #[test]
    fn base64_that_will_not_decode_leaves_an_empty_lane_with_a_message() {
        for b64 in ["not base64 at all", "AQI", "QUJ$", "", "AQID\nAQID"] {
            let restore = snapshot_of(Some(SampleSnapshot {
                path: String::from("/tmp/kick.wav"),
                file_name: String::from("kick.wav"),
                bytes_b64: Some(String::from(b64)),
                origin: None,
            }))
            .into_restore();

            let lane = &restore.lanes[0];
            assert_eq!(lane.name, "Kick", "the lane keeps everything but its sample: {b64:?}");
            assert!(lane.sample.is_none());
            assert_eq!(lane.error.as_deref(), Some(SAMPLE_RESTORE_FAILED), "for {b64:?}");
            assert_eq!(restore.samples, vec![None], "for {b64:?}");
        }
    }

    /// A path-only save (the over-cap case) asks the loader to read
    /// the file, and names it by the saved name — or by the path's own last
    /// component when the file saved none.
    #[test]
    fn a_path_only_save_asks_the_loader_to_read_the_path() {
        let named = snapshot_of(Some(SampleSnapshot {
            path: String::from("/samples/kick.wav"),
            file_name: String::from("the kick.wav"),
            bytes_b64: None,
            origin: None,
        }))
        .into_restore();
        let pending = named.samples[0].clone().expect("there is a file to read");
        assert_eq!(pending.path, "/samples/kick.wav");
        assert_eq!(pending.file_name, "the kick.wav", "the saved name is what the lane showed");
        assert_eq!(pending.bytes, None, "nothing was embedded — read the path");
        assert_eq!(named.lanes[0].error, None, "nothing has failed yet");

        let unnamed = snapshot_of(Some(SampleSnapshot {
            path: String::from("/samples/snare.wav"),
            file_name: String::new(),
            bytes_b64: None,
            origin: None,
        }))
        .into_restore();
        let pending = unnamed.samples[0].clone().expect("there is a file to read");
        assert_eq!(pending.file_name, "snare.wav", "derived the way a drop derives it");
    }

    /// A sample entry with neither bytes nor a path is not work for the
    /// loader, it is a lane whose sample is gone.
    #[test]
    fn a_sample_with_no_bytes_and_no_path_is_a_restore_failure() {
        let restore = snapshot_of(Some(SampleSnapshot::default())).into_restore();
        assert_eq!(restore.lanes[0].error.as_deref(), Some(SAMPLE_RESTORE_FAILED));
        assert_eq!(restore.samples, vec![None]);
    }

    /// A lane with no sample asks for nothing and says nothing: an empty
    /// lane in a project is an empty lane, not a failure.
    #[test]
    fn a_lane_that_never_had_a_sample_is_restored_empty_and_silent() {
        let restore = snapshot_of(None).into_restore();
        assert_eq!(restore.lanes[0].name, "Kick");
        assert!(restore.lanes[0].sample.is_none());
        assert_eq!(restore.lanes[0].error, None);
        assert_eq!(restore.samples, vec![None]);
    }

    // ---- Limits ---------------------------------------------------------

    /// A sample over the embedding cap is saved as a path and nothing else,
    /// and the lane holding it says so.
    ///
    /// Such a save is still a save: on a machine that has the file, the
    /// path is a restore, which is why the entry keeps every other field
    /// rather than the lane losing its sample at the save.
    #[test]
    fn a_sample_over_the_cap_is_saved_path_only() {
        let collector = basedrop::Collector::new();
        let data = crate::sample::SampleData {
            rate: 44_100,
            channels: 1,
            frames: vec![0.0].into(),
        };
        // Zeroed and never read past its length: the accounting is about how
        // big the file is, not what is in it.
        let bytes = vec![0_u8; crate::sample::MAX_SAMPLE_BYTES + 1];
        let path = Path::new("/samples/long take.wav");
        let sample = SampleRef::new(&collector.handle(), path, bytes, data);
        assert!(!sample.embeds(), "one byte over the cap");

        let mut lane = Lane::new(0, "Long", 36, LANE_PALETTE[0]);
        lane.sample = Some(sample);
        assert_eq!(lane.embed_note(), Some(SAMPLE_NOT_EMBEDDED), "the lane carries the note");

        let saved = Snapshot::of(&[lane], false);
        let stored = saved.lanes[0].sample.as_ref().expect("the sample is still described");
        assert_eq!(stored.bytes_b64, None, "64 MiB + 1 is not written into the project");
        assert_eq!(stored.path, "/samples/long take.wav", "the path is what restores it");
        assert_eq!(stored.file_name, "long take.wav");

        let restore = saved.into_restore();
        let pending = restore.samples[0].clone().expect("the path is work for the loader");
        assert_eq!(pending.bytes, None, "a path-only restore, like any other");
        assert_eq!(restore.lanes[0].error, None, "nothing has failed");
        assert_eq!(pending.failure_message(), format!("{SAMPLE_MISSING_PREFIX}long take.wav"));
    }

    /// The embedded-size warning fires strictly above
    /// [`EMBED_WARN_BYTES`] — tested at the threshold itself, which is the
    /// only size the rule can be wrong about.
    #[test]
    fn the_embedded_warning_fires_strictly_over_the_threshold() {
        assert_eq!(EMBED_WARN_BYTES, 128 * 1024 * 1024, "128 MiB, pinned");

        assert!(!embed_warning(0), "a project with no samples");
        assert!(!embed_warning(EMBED_WARN_BYTES - 1));
        assert!(!embed_warning(EMBED_WARN_BYTES), "exactly on the threshold is quiet");
        assert!(embed_warning(EMBED_WARN_BYTES + 1), "one byte past it warns");
        assert!(embed_warning(usize::MAX));
    }

    /// The two limits as a pair: no single sample can trip the warning by
    /// itself, and a project right on the threshold still has room for
    /// base64's 4/3 inside the 256 MiB bitstep found Bitwig refuses past —
    /// before zstd gives any of it back.
    ///
    /// Both sides are constants, so this is checked where a relationship
    /// between constants belongs — at compile time; the test exists so that
    /// the reasoning is written down next to it.
    #[test]
    fn the_threshold_leaves_room_for_base64_under_the_hosts_ceiling() {
        const {
            assert!(
                crate::sample::MAX_SAMPLE_BYTES < EMBED_WARN_BYTES,
                "one sample at the cap should not be a warning on its own",
            );
            assert!(
                EMBED_WARN_BYTES / 3 * 4 < 256 * 1024 * 1024,
                "base64 of a warning-sized project is already past the host's ceiling",
            );
        }
    }

    // ---- Hostile state --------------------------------------------------

    /// Base64 that decodes to bytes no decoder accepts is handed to the
    /// loader all the same — nothing on this side can tell a WAV from a
    /// love letter — and the decoder's refusal is what empties the lane:
    /// name, note, colour and settings kept, no sample, and the message
    /// that says the project's own copy is unusable.
    #[test]
    fn embedded_bytes_that_are_not_audio_leave_an_empty_lane_with_a_message() {
        let snapshot = snapshot_of(Some(SampleSnapshot {
            path: String::from("/tmp/kick.wav"),
            file_name: String::from("kick.wav"),
            bytes_b64: Some(STANDARD.encode(b"this is not a wav file")),
            origin: None,
        }));

        let mut model = Model::new();
        let pending = model.load_snapshot(snapshot);
        assert_eq!(pending.len(), 1, "the base64 decoded, so there is a job to try");

        // What the loader will do with that job, here by hand.
        let (id, sample) = pending.into_iter().next().expect("one job");
        let bytes = sample.bytes.clone().expect("the project embedded bytes");
        assert_eq!(
            crate::sample::decode(&bytes, Some("wav")),
            Err(crate::sample::DecodeError::Unsupported),
            "they are not audio",
        );
        assert!(model.set_error(id, &sample.failure_message()));

        let lane = &model.lanes()[0];
        assert_eq!(lane.name, "Kick", "the lane keeps everything but its sample");
        assert_eq!(lane.note, crate::model::DEFAULT_FIRST_NOTE);
        assert!(lane.sample.is_none());
        assert!(!lane.loading, "the job is over");
        assert_eq!(lane.error.as_deref(), Some(SAMPLE_RESTORE_FAILED));
    }

    /// A path-only restore that finds nothing at the saved path says which
    /// file it went looking for, because that is the one the user has to go
    /// and find.
    #[test]
    fn a_path_only_restore_that_cannot_be_read_names_the_missing_file() {
        let missing = "/nonexistent/chopdrop/kick.wav";
        let restore = snapshot_of(Some(SampleSnapshot {
            path: String::from(missing),
            file_name: String::from("kick.wav"),
            bytes_b64: None,
            origin: None,
        }))
        .into_restore();

        assert_eq!(
            crate::sample::read_file(Path::new(missing)),
            Err(crate::sample::DecodeError::Unreadable),
            "there is nothing at that path to read",
        );

        let pending = restore.samples[0].clone().expect("the path is the source");
        assert_eq!(pending.failure_message(), "sample file missing: kick.wav");
        assert_eq!(restore.lanes[0].name, "Kick", "and the lane keeps its settings");
        assert_eq!(restore.lanes[0].error, None, "the loader is the one that reports this");
    }

    /// An embed longer than any sample could be is dropped unread and the
    /// lane restores from its path instead: a project claiming a gigabyte
    /// of embedded audio costs a length check, not a gigabyte-long decode
    /// the cap then refuses.
    #[test]
    fn an_oversized_embed_restores_from_the_path_instead() {
        let huge = SampleSnapshot {
            path: String::from("/samples/kick.wav"),
            file_name: String::from("kick.wav"),
            bytes_b64: Some(STANDARD.encode([7_u8; 300])),
            origin: None,
        };

        let pending =
            huge.clone().into_pending_capped(64).expect("the path is still a source");
        assert_eq!(pending.bytes, None, "the embed was never decoded");
        assert_eq!(pending.path, "/samples/kick.wav", "so the loader reads the file");
        assert_eq!(pending.file_name, "kick.wav");
        assert_eq!(
            pending.failure_message(),
            "sample file missing: kick.wav",
            "and if that file is gone too, it is a missing file and not a lost copy",
        );

        let within = huge.clone().into_pending_capped(MAX_EMBED_B64_LEN);
        let within = within.expect("the same entry is in range under the real cap");
        assert_eq!(within.bytes.as_deref(), Some([7_u8; 300].as_slice()), "bytes win");

        let orphan = SampleSnapshot { path: String::new(), ..huge };
        assert_eq!(orphan.into_pending_capped(64), None, "nothing left to try");
    }

    /// The embed-length cap admits a sample of exactly
    /// [`crate::sample::MAX_SAMPLE_BYTES`] — the one error this screen must
    /// not make — while still holding a hostile project to within a few
    /// bytes of that cap.
    #[test]
    fn the_embed_length_cap_admits_a_sample_of_exactly_the_cap() {
        // Padded base64 is four characters per three bytes, rounded up:
        // pinned against the encoder on sizes a test can encode, then
        // applied below to a cap it cannot.
        for bytes in [0_usize, 1, 2, 3, 4, 5, 100, 1_000] {
            let encoded = STANDARD.encode(vec![0_u8; bytes]);
            assert_eq!(encoded.len(), bytes.div_ceil(3) * 4, "base64 of {bytes} bytes");
        }

        let cap = crate::sample::MAX_SAMPLE_BYTES;
        assert!(
            cap.div_ceil(3) * 4 <= MAX_EMBED_B64_LEN,
            "a sample of exactly the cap would be dropped unread",
        );
        assert!(
            MAX_EMBED_B64_LEN / 4 * 3 <= cap + 16,
            "the slack would let a hostile project decode well past the cap",
        );
    }

    /// What `into_restore` promises about every lane it produces, whatever
    /// the file said.
    fn assert_lane_is_in_range(lane: &Lane) {
        assert!(lane.name.chars().count() <= crate::model::MAX_NAME_LEN, "name {:?}", lane.name);
        assert!(lane.note <= 127, "note {}", lane.note);
        assert_eq!(lane.colour >> 24, 0, "colour {:#010X}", lane.colour);
        assert!(
            (crate::model::LANE_VOLUME_DB_MIN..=crate::model::LANE_VOLUME_DB_MAX)
                .contains(&lane.volume_db),
            "volume {}",
            lane.volume_db,
        );
        assert!(
            (crate::model::LANE_PAN_MIN..=crate::model::LANE_PAN_MAX).contains(&lane.pan),
            "pan {}",
            lane.pan,
        );
        assert!(lane.choke <= crate::model::CHOKE_GROUPS, "choke {}", lane.choke);
        assert!(
            (crate::model::ATTACK_MS_MIN..=crate::model::ATTACK_MS_MAX).contains(&lane.attack_ms),
            "attack {}",
            lane.attack_ms,
        );
        assert!(
            (crate::model::RELEASE_MS_MIN..=crate::model::RELEASE_MS_MAX)
                .contains(&lane.release_ms),
            "release {}",
            lane.release_ms,
        );
        assert!(lane.output <= crate::model::OUTPUT_MAX, "output {}", lane.output);
        assert!(
            (crate::model::FINE_MIN..=crate::model::FINE_MAX).contains(&lane.fine_pct),
            "fine {}",
            lane.fine_pct,
        );
        assert!(
            (crate::model::PITCH_MIN..=crate::model::PITCH_MAX).contains(&lane.pitch_st),
            "pitch {}",
            lane.pitch_st,
        );
    }

    /// Loading never panics. A project file is host-provided bytes: every
    /// one of these either is not a snapshot — an error, which the caller
    /// handles — or is one, and then every lane it lands is in range, with
    /// one sample entry per lane and no more lanes than an instance may
    /// hold.
    #[test]
    fn no_project_file_can_panic_a_load() {
        let deep = format!(r#"{{"lanes":{}{}}}"#, "[".repeat(300), "]".repeat(300));
        let many = format!(
            r#"{{"lanes":[{}]}}"#,
            vec![r#"{"note":127,"choke":16}"#; MAX_LANES as usize + 20].join(",")
        );
        let wide_name = format!(r#"{{"lanes":[{{"name":"{}"}}]}}"#, "🙂".repeat(300));

        // `null` (serde_json's NaN) or a non-number in a float field must
        // still parse.
        let lenient = [
            r#"{"lanes":[{"volume_db":null,"pan":null,"attack_ms":null,"release_ms":null}]}"#,
            r#"{"lanes":[{"fine_pct":null,"pitch_st":null},{"pan":"left","volume_db":true}]}"#,
            r#"{"lanes":[{"release_ms":[1,[2]],"attack_ms":{"a":{"b":1}},"pitch_st":"1"}]}"#,
            r#"{"chop_editor":{"frames_per_pixel":null,"height_px":null},"lanes":[{}]}"#,
            r#"{"chop_editor":{"source_path":"/x.wav","frames_per_pixel":"x","height_px":[]}}"#,
        ];
        for json in lenient {
            assert!(serde_json::from_str::<Snapshot>(json).is_ok(), "{json} parses");
        }

        let hostile = [
            "",
            "null",
            "[]",
            "7",
            r#""a string""#,
            "{",
            "{}",
            r#"{"version":-1}"#,
            r#"{"version":4294967295,"sample_pad_switch":null}"#,
            r#"{"version":1e9999,"lanes":[]}"#,
            r#"{"lanes":null}"#,
            r#"{"lanes":{}}"#,
            r#"{"lanes":[{"name":null}]}"#,
            r#"{"lanes":[{"note":255,"choke":255,"colour":4294967295}]}"#,
            r#"{"lanes":[{"volume_db":1e40,"pan":-1e40}]}"#,
            r#"{"lanes":[{"volume_db":-1e-40,"pan":1e-40,"mode":"🙂"}]}"#,
            r#"{"lanes":[{"name":"\ud800"}]}"#,
            r#"{"lanes":[{"attack_ms":1e40,"release_ms":-1e40,"fine_pct":1e40}]}"#,
            r#"{"lanes":[{"output":255,"pitch_st":-1e40,"time_mode":"","length":""}]}"#,
            r#"{"lanes":[{"output":-1,"cut_itself":"yes","time_mode":7,"length":null}]}"#,
            r#"{"lanes":[{"sample":null}]}"#,
            r#"{"lanes":[{"sample":{}}]}"#,
            r#"{"lanes":[{"sample":{"bytes_b64":"!!!!","path":"/x.wav"}}]}"#,
            r#"{"lanes":[{"sample":{"bytes_b64":null,"path":"","file_name":""}}]}"#,
            r#"{"lanes":[{"sample":{"path":"\u0000","file_name":"\u0000"}}]}"#,
            deep.as_str(),
            many.as_str(),
            wide_name.as_str(),
        ];

        for json in hostile.into_iter().chain(lenient) {
            let Ok(snapshot) = serde_json::from_str::<Snapshot>(json) else {
                continue;
            };

            let restore = snapshot.clone().into_restore();
            assert!(restore.lanes.len() <= MAX_LANES as usize, "lane count for {json}");
            assert_eq!(restore.samples.len(), restore.lanes.len(), "one each for {json}");
            for lane in &restore.lanes {
                assert_lane_is_in_range(lane);
            }

            let mut model = Model::new();
            model.load_snapshot(snapshot);
            for lane in model.lanes() {
                assert_lane_is_in_range(lane);
            }
        }
    }

    // ---- Version 2 ------------------------------------------------------

    /// A project saved by chopdrop v1, checked in once and never rewritten.
    const V1_PROJECT: &str = include_str!("../tests/fixtures/v1_project.json");

    /// A v1 project loads unchanged and sounds the same: its own settings as
    /// saved, and every v2 field at v1's behaviour — above all, lanes that do
    /// **not** cut themselves, although a lane made fresh in v2 does.
    #[test]
    fn the_v1_fixture_loads_with_v1_behaviour() {
        let snapshot: Snapshot = serde_json::from_str(V1_PROJECT).expect("the v1 fixture parses");
        assert_eq!(snapshot.version, 1, "the fixture is a v1 file");

        let mut model = Model::new();
        let pending = model.load_snapshot(snapshot);
        assert!(model.sample_pad_switch());
        assert_eq!(pending.len(), 1, "the kick's embedded sample goes to the loader");

        let lanes = model.lanes();
        assert_eq!(lanes.len(), 2);
        let (kick, hat) = (&lanes[0], &lanes[1]);
        assert_eq!(kick.name, "Kick");
        assert_eq!(kick.note, 36);
        assert_eq!(kick.colour, 0xE2_6E_6E);
        assert_eq!(kick.volume_db, -3.0);
        assert_eq!(kick.pan, -0.25);
        assert_eq!(kick.choke, 1);
        assert_eq!(kick.mode, LaneMode::OneShot);
        assert_eq!(hat.name, "Open hat");
        assert_eq!(hat.note, 46);
        assert_eq!(hat.mode, LaneMode::Gate);
        assert!(hat.sample.is_none() && hat.error.is_none());

        for lane in lanes {
            assert!(!lane.cut_itself, "{}: v1 lanes never cut themselves", lane.name);
            assert_eq!(lane.release_ms, 0.0, "{}: v1's 4 ms gate fade", lane.name);
            assert_eq!(lane.release_ms, crate::model::RELEASE_MS_V1, "{}", lane.name);
            assert_eq!(lane.attack_ms, 0.0, "{}", lane.name);
            assert_eq!(lane.output, 0, "{}: Out 1", lane.name);
            assert_eq!(lane.time_mode, TimeMode::Off, "{}", lane.name);
            assert_eq!(lane.length, Length::AsCut, "{}", lane.name);
            assert_eq!(lane.fine_pct, 0.0, "{}", lane.name);
            assert_eq!(lane.pitch_st, 0.0, "{}", lane.name);
        }
        assert!(Lane::new(0, "new", 36, 0).cut_itself, "while a fresh v2 lane does");

        let (_, sample) = &pending[0];
        let bytes = sample.bytes.as_ref().expect("the fixture embeds the kick");
        let data = crate::sample::decode(bytes, None).expect("and the embedded WAV decodes");
        assert_eq!(data.rate, 44_100);
        assert_eq!(data.frame_count(), 6);
    }

    /// The one lane a hostile v2 case lands in, through the whole load.
    fn restored(json: &str) -> Lane {
        let snapshot: Snapshot = serde_json::from_str(json).expect("the case parses");
        snapshot.into_restore().lanes.remove(0)
    }

    #[test]
    fn an_output_past_the_last_lands_on_out_1() {
        assert_eq!(restored(r#"{"version":2,"lanes":[{"output":200}]}"#).output, 0);
        assert_eq!(restored(r#"{"version":2,"lanes":[{"output":15}]}"#).output, 15);
    }

    #[test]
    fn an_unknown_time_mode_is_off() {
        let lane = restored(r#"{"version":2,"lanes":[{"time_mode":"banana"}]}"#);
        assert_eq!(lane.time_mode, TimeMode::Off);
        let lane = restored(r#"{"version":2,"lanes":[{"time_mode":"repitch"}]}"#);
        assert_eq!(lane.time_mode, TimeMode::Repitch, "while a known one is read");
    }

    #[test]
    fn an_unknown_length_is_as_cut() {
        assert_eq!(restored(r#"{"version":2,"lanes":[{"length":"3/7"}]}"#).length, Length::AsCut);
        let lane = restored(r#"{"version":2,"lanes":[{"length":"1/16"}]}"#);
        assert_eq!(lane.length, Length::Beats(crate::model::NoteLength::Sixteenth));
    }

    /// Only a version 1 file loads at the v1 fade — a version 2 file that
    /// leaves `release_ms` out gets the new-lane 300 ms, and one that saves
    /// it keeps it.
    #[test]
    fn a_v2_file_missing_the_release_loads_300_ms() {
        let lane = restored(r#"{"version":2,"lanes":[{"mode":"gate"}]}"#);
        assert_eq!(lane.release_ms, crate::model::RELEASE_MS_DEFAULT);
        assert_eq!(lane.release_ms, 300.0);
        let lane = restored(r#"{"version":2,"lanes":[{"mode":"gate","release_ms":0.0}]}"#);
        assert_eq!(lane.release_ms, 0.0, "a saved 0 ms is read as saved");
        let lane = restored(r#"{"version":1,"lanes":[{"mode":"gate","release_ms":900.0}]}"#);
        assert_eq!(lane.release_ms, crate::model::RELEASE_MS_V1, "a v1 file has no release");
        let lane = restored(r#"{"lanes":[{"mode":"gate"}]}"#);
        assert_eq!(lane.release_ms, crate::model::RELEASE_MS_V1, "no version is version 1");

        // And the v2 round trip: a saved v2 lane with the field taken out.
        let mut model = Model::new();
        model.add_lane();
        let mut json = serde_json::to_value(model.snapshot()).expect("a snapshot serializes");
        assert_eq!(json["version"], 2);
        json["lanes"][0].as_object_mut().expect("a lane object").remove("release_ms");
        let snapshot: Snapshot = serde_json::from_value(json).expect("and parses back");
        assert_eq!(snapshot.into_restore().lanes[0].release_ms, 300.0);
    }

    /// JSON has no NaN, so this case is the snapshot a hostile writer could
    /// still hand over in memory; the clamp gives it the default.
    #[test]
    fn a_nan_release_is_300_ms() {
        let snapshot = Snapshot {
            version: CURRENT_VERSION,
            sample_pad_switch: false,
            chop_editor: None,
            lanes: vec![LaneSnapshot { release_ms: f32::NAN, ..LaneSnapshot::default() }],
        };
        assert_eq!(snapshot.into_restore().lanes[0].release_ms, 300.0);
    }

    /// Lenient numbers: a `null` or a non-number where a lane float belongs
    /// costs that field its default, never the whole project — the lanes
    /// either side load as saved.
    #[test]
    fn a_bad_number_defaults_one_field_and_keeps_every_lane() {
        let json = r#"{"version":2,"lanes":[
            {"name":"Kick","release_ms":120,"pan":-0.5},
            {"name":"Snare","release_ms":null,"pan":"left","volume_db":-6},
            {"name":"Hat","release_ms":40.5,"pan":0.25}
        ]}"#;
        let snapshot: Snapshot = serde_json::from_str(json).expect("a bad number is no bad file");
        let lanes = snapshot.into_restore().lanes;
        assert_eq!(lanes.len(), 3, "all three lanes load");

        let (kick, snare, hat) = (&lanes[0], &lanes[1], &lanes[2]);
        assert_eq!((kick.name.as_str(), kick.release_ms, kick.pan), ("Kick", 120.0, -0.5));
        assert_eq!(snare.name, "Snare");
        assert_eq!(snare.release_ms, 300.0, "null release is the default");
        assert_eq!(snare.pan, 0.0, "a string pan is centre");
        assert_eq!(snare.volume_db, -6.0, "while its good numbers are kept");
        assert_eq!((hat.name.as_str(), hat.release_ms, hat.pan), ("Hat", 40.5, 0.25));
    }

    /// The editor's floats are as lenient: a `null` zoom fits, a `null`
    /// height is the default strip height, and the rest of the editor loads.
    #[test]
    fn a_bad_editor_number_defaults_one_field() {
        let restore = chop_restored(
            r#"{"chop_editor":{"source_path":"/x.wav","first":9,"frames_per_pixel":null,
                "selection":[1,5],"height_px":"tall"}}"#,
        );
        assert_eq!(restore.source_path.as_deref(), Some("/x.wav"));
        assert_eq!(restore.view, None, "no zoom means fit");
        assert_eq!(restore.selection, Some((1, 5)));
        assert_eq!(restore.height_px, chop::clamp_strip_height(f32::NAN));
        assert_eq!(restore.height_px, ChopEditorSnapshot::default().height_px);
    }

    /// The editor part of a JSON project, clamped for loading.
    fn chop_restored(json: &str) -> ChopRestore {
        let snapshot: Snapshot = serde_json::from_str(json).expect("the fixture parses");
        snapshot.into_restore().chop_editor.expect("the file has an editor")
    }

    /// A hostile strip height lands inside the strip's range.
    #[test]
    fn a_hostile_strip_height_is_clamped() {
        let huge = chop_restored(r#"{"chop_editor":{"height_px":1e9}}"#);
        assert_eq!(huge.height_px, chop::STRIP_MAX_HEIGHT);
        let tiny = chop_restored(r#"{"chop_editor":{"height_px":-5}}"#);
        assert_eq!(tiny.height_px, chop::STRIP_MIN_HEIGHT);
        let nan = ChopEditorSnapshot { height_px: f32::NAN, ..ChopEditorSnapshot::default() };
        assert_eq!(nan.into_restore().height_px, chop::STRIP_HEIGHT);
    }

    /// A backwards or empty selection is dropped; a sane one is kept (and
    /// clamped to the source once it lands).
    #[test]
    fn a_negative_length_selection_is_none() {
        let path = r#""source_path":"/songs/Eye.wav""#;
        let backwards = format!(r#"{{"chop_editor":{{{path},"selection":[900,100]}}}}"#);
        assert_eq!(chop_restored(&backwards).selection, None);
        let empty = format!(r#"{{"chop_editor":{{{path},"selection":[50,50]}}}}"#);
        assert_eq!(chop_restored(&empty).selection, None);
        let fine = format!(r#"{{"chop_editor":{{{path},"selection":[100,900]}}}}"#);
        assert_eq!(chop_restored(&fine).selection, Some((100, 900)));
    }

    /// A zoom under 1 frame/px or not finite means "fit" on landing; a
    /// selection or view with no source to apply to is dropped.
    #[test]
    fn a_hostile_view_fits_and_nothing_applies_without_a_source() {
        let path = r#""source_path":"/songs/Eye.wav""#;
        let zero = format!(r#"{{"chop_editor":{{{path},"frames_per_pixel":0.0}}}}"#);
        assert_eq!(chop_restored(&zero).view, None);
        let nan = ChopEditorSnapshot {
            source_path: String::from("/songs/Eye.wav"),
            frames_per_pixel: f64::NAN,
            ..ChopEditorSnapshot::default()
        };
        assert_eq!(nan.into_restore().view, None);
        let kept = format!(r#"{{"chop_editor":{{{path},"first":7,"frames_per_pixel":2.5}}}}"#);
        assert_eq!(chop_restored(&kept).view, Some(View { first: 7, frames_per_pixel: 2.5 }));

        let sourceless = chop_restored(r#"{"chop_editor":{"selection":[1,9],"first":7}}"#);
        assert_eq!(sourceless.source_path, None);
        assert_eq!((sourceless.selection, sourceless.view), (None, None));
    }

    /// A missing field takes the fresh editor's value: snap on, the default
    /// height, expanded.
    #[test]
    fn a_chop_editor_missing_everything_is_a_fresh_one() {
        let restore = chop_restored(r#"{"chop_editor":{}}"#);
        assert_eq!(restore, ChopEditorSnapshot::default().into_restore());
        assert!(restore.snap);
        assert!(!restore.collapsed);
        assert_eq!(restore.height_px, chop::STRIP_HEIGHT);
        assert_eq!(restore.source_path, None);
    }

    /// An editor nobody touched saves nothing; any remembered setting
    /// saves the lot.
    #[test]
    fn an_untouched_editor_is_not_saved() {
        let mut model = Model::new();
        assert_eq!(model.snapshot().chop_editor, None);
        model.set_strip(false, 250.0);
        let saved = model.snapshot().chop_editor.expect("a resized strip is remembered");
        assert_eq!(saved.height_px, 250.0);
        assert_eq!(saved.source_path, "");
    }

    /// Loading a project replaces the editor too: one with no editor (a v1
    /// file) leaves a fresh one, and a saved strip lands at once while its
    /// source waits for the loader.
    #[test]
    fn a_load_replaces_the_editor() {
        let mut model = Model::new();
        model.open_source("/songs/old.wav");
        model.set_strip(true, 400.0);
        model.set_snap(false);
        model.load_snapshot(Snapshot::of(&crate::model::default_lanes(), false));
        let editor = model.chop();
        assert_eq!(editor.source_path, None);
        assert!(!editor.loading && editor.snap && !editor.collapsed);
        assert_eq!(editor.height_px, chop::STRIP_HEIGHT);

        let json = concat!(
            r#"{"version":2,"chop_editor":{"source_path":"/songs/Eye.wav","first":40,"#,
            r#""frames_per_pixel":3.0,"selection":[10,20],"snap":false,"collapsed":true,"#,
            r#""height_px":300.0}}"#,
        );
        model.load_snapshot(serde_json::from_str(json).expect("it parses"));
        let editor = model.chop();
        assert_eq!(editor.source_path.as_deref(), Some("/songs/Eye.wav"));
        assert!(editor.loading, "waiting on the loader");
        assert_eq!(editor.pending_selection, Some((10, 20)));
        assert_eq!(editor.pending_view, Some(View { first: 40, frames_per_pixel: 3.0 }));
        assert!(!editor.snap && editor.collapsed);
        assert_eq!(editor.height_px, 300.0);
        let resaved = model.snapshot().chop_editor.expect("saved while loading");
        assert_eq!(resaved.selection, Some((10, 20)));
        assert_eq!((resaved.first, resaved.frames_per_pixel), (40, 3.0));
    }
}
