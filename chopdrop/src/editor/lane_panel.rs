//! The lane tab of the dock: the selected lanes' attack, release, cut itself,
//! output and time settings.
//!
//! Every setting is a control: each change applies to every selected lane,
//! and a gesture (a drag, a wheel notch, a typed value, a click, a pick) is
//! one undo step however many lanes it touched ([`apply`]). The length
//! dropdown words each entry with the change it makes to the chop at the
//! current tempo ([`length_entries`]); the pitch field is disabled in
//! Repitch and shows the pitch the speed change makes ([`shown_pitch`]).
//! With several lanes selected a setting
//! they all share shows its value and one they disagree on shows as mixed
//! ([`Shared::Mixed`]): "—" for a field or the output dropdown, an
//! indeterminate checkbox for cut itself, no lit choice for the time mode.
//! The helpers that decide and word that are pure and tested here; the
//! drawing reads the model afresh every frame.
//!
//! Below the settings, the read-only origin line of a chop lane — where its
//! sample was cut from ([`crate::model::SampleRef::origin_text`]) — shown for
//! a single selected lane with an origin only. Under it, for selected lanes
//! in Stretch, the render status ([`render_status`]): "rendering…" while a
//! render is pending, or "render failed — click to retry" after one failed,
//! whose click asks for a fresh render of every failed lane
//! ([`Model::request_render`]; never an undo step). Hidden otherwise.

use std::ops::RangeInclusive;

use egui::{Checkbox, ComboBox, DragValue, Grid, Label, Rect, RichText, Sense, Ui, Vec2};

use crate::editor::{lanes, theme};
use crate::model::{
    ATTACK_MS_MAX, ATTACK_MS_MIN, FINE_MAX, FINE_MIN, Lane, Model, OUTPUT_MAX, PITCH_MAX,
    PITCH_MIN, RELEASE_MS_MAX, RELEASE_MS_MIN,
};
use crate::time::{self, Length, NOTE_LENGTHS, TimeMode};

/// The panel with no lane selected.
pub const HINT: &str = "click a lane name to select it";
/// What a field shows when the selected lanes disagree.
pub const MIXED: &str = "\u{2014}";

/// The rows' labels, top to bottom.
pub const ATTACK_LABEL: &str = "attack";
pub const RELEASE_LABEL: &str = "release";
pub const CUT_ITSELF_LABEL: &str = "cut itself";
pub const OUTPUT_LABEL: &str = "output";
pub const TIME_LABEL: &str = "time";
pub const LENGTH_LABEL: &str = "length";
pub const FINE_LABEL: &str = "fine";
pub const PITCH_LABEL: &str = "pitch";

/// The output dropdown's hover.
pub const OUTPUT_HOVER: &str =
    "Out 2\u{2013}16 are silent in Bitwig until you click Add Missing Chains on the device";

/// The pitch field's hover while it is disabled (every selected lane in
/// Repitch).
pub const PITCH_REPITCH_HOVER: &str =
    "Repitch sets the pitch from the length; switch time to Off or Stretch to set it";

/// The status line while a selected Stretch lane's render is pending.
pub const RENDERING_TEXT: &str = "rendering\u{2026}";
/// The status line after a selected Stretch lane's render failed; a click
/// retries.
pub const RENDER_FAILED_TEXT: &str = "render failed \u{2014} click to retry";

/// What marks the length entry nearest the chop's own length.
pub const NEAREST_MARK: &str = "\u{2248}";

/// The time mode's three choices, in the order they are drawn.
pub const TIME_CHOICES: [(TimeMode, &str); 3] =
    [(TimeMode::Off, "Off"), (TimeMode::Repitch, "Repitch"), (TimeMode::Stretch, "Stretch")];

/// One setting across the selected lanes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shared<T> {
    /// No lane selected.
    None,
    /// Every selected lane has this value.
    Same(T),
    /// The selected lanes disagree.
    Mixed,
}

impl<T> Shared<T> {
    /// The shared value, if there is one.
    pub fn value(self) -> Option<T> {
        match self {
            Shared::Same(value) => Some(value),
            _ => None,
        }
    }
}

/// Whether `values` agree: [`Shared::Same`] when every one equals the first,
/// [`Shared::Mixed`] when any differs, [`Shared::None`] when there are none.
pub fn mixed<T: PartialEq>(values: impl IntoIterator<Item = T>) -> Shared<T> {
    let mut values = values.into_iter();
    let Some(first) = values.next() else {
        return Shared::None;
    };
    if values.all(|value| value == first) { Shared::Same(first) } else { Shared::Mixed }
}

/// The panel's header: the lane's name for one lane, "N lanes" for several,
/// nothing for none.
pub fn header(selected: &[&Lane]) -> String {
    match selected {
        [] => String::new(),
        [lane] => lane.name.clone(),
        several => format!("{} lanes", several.len()),
    }
}

/// `value` worded by `format`, or [`MIXED`] when it is not shared.
pub fn shared_label<T>(value: Shared<T>, format: impl FnOnce(T) -> String) -> String {
    value.value().map(format).unwrap_or_else(|| MIXED.to_string())
}

/// A time in milliseconds as the panel shows it: "0 ms", "300 ms", "12.5 ms"
/// (whole milliseconds unless the value has a fraction worth showing).
pub fn format_ms(ms: f32) -> String {
    let tenths = (ms * 10.0).round() / 10.0;
    // `-0.0` would print as "-0 ms".
    let tenths = if tenths == 0.0 { 0.0 } else { tenths };
    if tenths.fract() == 0.0 { format!("{tenths:.0} ms") } else { format!("{tenths:.1} ms") }
}

/// An output index as its name: 0 → "Out 1" … 15 → "Out 16".
pub fn format_output(output: u8) -> String {
    format!("Out {}", u32::from(output) + 1)
}

/// A lane's length as the length dropdown names it: "as cut", "1 bar",
/// "1/8t".
pub fn format_length(length: Length) -> String {
    match length {
        Length::AsCut => "as cut".to_string(),
        Length::Beats(note) => note.label().to_string(),
    }
}

/// The fine trim, one decimal with its sign: "0.0 %", "+1.5 %", "−2.0 %"
/// (a real minus sign, as the length menu prints changes).
pub fn format_fine(pct: f32) -> String {
    let tenths = (pct * 10.0).round() / 10.0;
    if tenths == 0.0 {
        "0.0 %".to_string()
    } else if tenths < 0.0 {
        format!("\u{2212}{:.1} %", -tenths)
    } else {
        format!("+{tenths:.1} %")
    }
}

/// The pitch in semitones, two decimals with its sign: "0.00 st",
/// "+12.00 st", "−0.50 st".
pub fn format_pitch(semis: f32) -> String {
    let cents = (semis * 100.0).round() / 100.0;
    if cents == 0.0 {
        "0.00 st".to_string()
    } else if cents < 0.0 {
        format!("\u{2212}{:.2} st", -cents)
    } else {
        format!("+{cents:.2} st")
    }
}

/// A lane's sample length in seconds; `None` without a sample.
pub fn natural_secs(lane: &Lane) -> Option<f64> {
    let sample = lane.sample.as_ref()?;
    Some(time::natural_secs(sample.data.frame_count(), sample.data.rate))
}

/// The pitch the field shows for `lane` at `tempo`: its own pitch in Off and
/// Stretch; in Repitch the pitch the speed change makes
/// (`speed_to_semis(repitch_speed(natural, target))`, as the engine plays
/// it), 0 on "as cut" or without a sample.
pub fn shown_pitch(lane: &Lane, tempo: f64) -> f32 {
    match (lane.time_mode, lane.length) {
        (TimeMode::Repitch, Length::Beats(note)) => match natural_secs(lane) {
            Some(natural) if natural > 0.0 => {
                let target = time::target_secs(note, lane.fine_pct, tempo);
                time::speed_to_semis(time::repitch_speed(natural, target))
            }
            _ => 0.0,
        },
        (TimeMode::Repitch, Length::AsCut) => 0.0,
        _ => lane.pitch_st,
    }
}

/// The length dropdown's entries, in order: "as cut", then every
/// [`NoteLength`](crate::time::NoteLength) in menu order. With a chop
/// `natural` seconds long, each note length is suffixed with the change it
/// makes at `tempo` ("1 bar −1.4 %", fine not counted — it trims on top) and
/// the nearest one is marked [`NEAREST_MARK`] ("≈ 1 bar −1.4 %"); without
/// one (no sample, or the selected lanes' samples differ) the bare labels.
pub fn length_entries(natural: Option<f64>, tempo: f64) -> Vec<(Length, String)> {
    let natural = natural.filter(|secs| secs.is_finite() && *secs > 0.0);
    let nearest = natural.map(|secs| time::nearest(secs, tempo));
    let mut entries = vec![(Length::AsCut, format_length(Length::AsCut))];
    for note in NOTE_LENGTHS {
        let label = match natural {
            Some(secs) => {
                let change = time::change_pct(secs, time::target_secs(note, 0.0, tempo));
                let change = time::format_change(change);
                if nearest == Some(note) {
                    format!("{NEAREST_MARK} {} {change}", note.label())
                } else {
                    format!("{} {change}", note.label())
                }
            }
            None => note.label().to_string(),
        };
        entries.push((Length::Beats(note), label));
    }
    entries
}

/// Every value the panel shows, worked out from the selected lanes.
#[derive(Debug, Clone, PartialEq)]
pub struct Values {
    pub header: String,
    pub attack: String,
    pub release: String,
    pub cut_itself: Shared<bool>,
    pub output: String,
    pub time_mode: Shared<TimeMode>,
    pub length: String,
    pub fine: String,
    pub pitch: String,
    /// The origin line: one selected lane whose sample has a chop origin,
    /// else `None` (hidden).
    pub origin: Option<String>,
}

impl Values {
    /// The panel's values for `selected` (in selection order) at `tempo`
    /// (BPM, for the pitch a Repitch lane shows).
    pub fn of(selected: &[&Lane], tempo: f64) -> Values {
        let each = |get: fn(&Lane) -> f32| mixed(selected.iter().map(|lane| get(lane)));
        Values {
            header: header(selected),
            attack: shared_label(each(|lane| lane.attack_ms), format_ms),
            release: shared_label(each(|lane| lane.release_ms), format_ms),
            cut_itself: mixed(selected.iter().map(|lane| lane.cut_itself)),
            output: shared_label(mixed(selected.iter().map(|lane| lane.output)), format_output),
            time_mode: mixed(selected.iter().map(|lane| lane.time_mode)),
            length: shared_label(mixed(selected.iter().map(|lane| lane.length)), format_length),
            fine: shared_label(each(|lane| lane.fine_pct), format_fine),
            pitch: shared_label(
                mixed(selected.iter().map(|lane| shown_pitch(lane, tempo))),
                format_pitch,
            ),
            origin: match selected {
                [lane] => lane.sample.as_ref().and_then(|sample| sample.origin_text()),
                _ => None,
            },
        }
    }
}

/// The render status line of the selected lanes. Only lanes in Stretch
/// count — no other mode renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderStatus {
    /// Some lane's render is on its way ("rendering…").
    Rendering,
    /// None is on its way and some lane's last render failed ("render
    /// failed — click to retry").
    Failed,
}

impl RenderStatus {
    /// The line's text.
    pub fn text(self) -> &'static str {
        match self {
            RenderStatus::Rendering => RENDERING_TEXT,
            RenderStatus::Failed => RENDER_FAILED_TEXT,
        }
    }
}

/// The status line for `selected`: rendering wins over failed (the retry
/// would only ask again); `None` (hidden) when every Stretch lane is
/// settled, or none is in Stretch.
pub fn render_status(selected: &[&Lane]) -> Option<RenderStatus> {
    let stretch = || selected.iter().filter(|lane| lane.time_mode == TimeMode::Stretch);
    if stretch().any(|lane| lane.render_pending) {
        Some(RenderStatus::Rendering)
    } else if stretch().any(|lane| lane.render_error.is_some()) {
        Some(RenderStatus::Failed)
    } else {
        None
    }
}

/// "Click to retry": a fresh render for every lane in `ids` whose last
/// render failed. Not an undo step — render state is outside the history.
/// Returns how many lanes were asked.
pub fn retry_renders(model: &mut Model, ids: &[u32]) -> usize {
    let failed: Vec<u32> = selected_lanes(model, ids)
        .into_iter()
        .filter(|lane| lane.render_error.is_some())
        .map(|lane| lane.id)
        .collect();
    failed.into_iter().filter(|&id| model.request_render(id)).count()
}

/// The selected lanes that still exist, in selection order.
pub fn selected_lanes<'a>(model: &'a Model, ids: &[u32]) -> Vec<&'a Lane> {
    let lanes = model.lanes();
    ids.iter().filter_map(|id| lanes.iter().find(|lane| lane.id == *id)).collect()
}

/// How many milliseconds a point of drag moves the attack or release.
pub const MS_DRAG_SPEED: f64 = 1.0;
/// How many milliseconds a notch of the wheel moves the attack or release.
pub const MS_WHEEL_STEP: f32 = 10.0;
/// How many percent a point of drag moves the fine trim.
pub const FINE_DRAG_SPEED: f64 = 0.1;
/// How many percent a notch of the wheel moves the fine trim.
pub const FINE_WHEEL_STEP: f32 = 0.1;
/// How many semitones a point of drag moves the pitch.
pub const PITCH_DRAG_SPEED: f64 = 0.1;
/// How many semitones a notch of the wheel moves the pitch.
pub const PITCH_WHEEL_STEP: f32 = 1.0;

/// One change a panel control makes, to every selected lane at once.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Edit {
    AttackMs(f32),
    ReleaseMs(f32),
    CutItself(bool),
    /// An output index, 0 (Out 1) ..= [`OUTPUT_MAX`] (Out 16).
    Output(u8),
    TimeMode(TimeMode),
    Length(Length),
    FinePct(f32),
    PitchSt(f32),
}

/// What one frame of the panel found: the change it made (if any) and
/// whether the gesture ended — a release, a wheel notch, a typed value left,
/// a click — which is when the one undo step closes.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Gesture {
    pub edit: Option<Edit>,
    pub ended: bool,
}

impl Gesture {
    fn merge(&mut self, other: Gesture) {
        if self.edit.is_none() {
            self.edit = other.edit;
        }
        self.ended |= other.ended;
    }
}

/// Where the panel's controls were drawn this frame (for tests and for the
/// window to know nothing more about them).
#[derive(Debug, Clone, Copy, Default)]
pub struct PanelOutput {
    pub attack: Option<Rect>,
    pub release: Option<Rect>,
    pub cut_itself: Option<Rect>,
    /// The output dropdown's button.
    pub output: Option<Rect>,
    /// The time mode's three choices, in [`TIME_CHOICES`] order.
    pub time: [Option<Rect>; 3],
    /// The length dropdown's button.
    pub length: Option<Rect>,
    pub fine: Option<Rect>,
    pub pitch: Option<Rect>,
    /// Whether the pitch field took input (not every selected lane in
    /// Repitch).
    pub pitch_enabled: bool,
    /// The render status line, when shown, and what it said.
    pub status: Option<(Rect, RenderStatus)>,
}

/// What a click on the cut itself checkbox sets every selected lane to: on
/// when they are mixed, otherwise the opposite of what they share.
pub fn cut_itself_click(current: Shared<bool>) -> bool {
    match current {
        Shared::Same(on) => !on,
        Shared::Mixed | Shared::None => true,
    }
}

/// A typed number with an optional unit `suffix`, either minus sign
/// (ASCII or U+2212) and an optional `+`. `None` without a finite number.
fn parse_with_unit(text: &str, suffix: &str) -> Option<f64> {
    let text = text.trim().to_lowercase().replace('\u{2212}', "-");
    let number = text.strip_suffix(suffix).unwrap_or(&text).trim();
    number.parse::<f64>().ok().filter(|value| value.is_finite())
}

/// A typed time in milliseconds: "12", "12.5", "12 ms", "12ms". `None` for
/// anything without a number (the field then goes back to what it showed).
pub fn parse_ms(text: &str) -> Option<f64> {
    parse_with_unit(text, "ms")
}

/// A typed fine trim in percent: "1.5", "+1.5 %", "−2%", "-2".
pub fn parse_pct(text: &str) -> Option<f64> {
    parse_with_unit(text, "%")
}

/// A typed pitch in semitones: "7", "+12.00 st", "−0.5st", "-3".
pub fn parse_st(text: &str) -> Option<f64> {
    parse_with_unit(text, "st")
}

/// `value` held to the tenth the panel shows (milliseconds, percent).
fn tenths(value: f32) -> f32 {
    (value * 10.0).round() / 10.0
}

/// `semis` held to the cent the panel shows.
fn cents(semis: f32) -> f32 {
    (semis * 100.0).round() / 100.0
}

/// Applies one frame's gesture to every lane in `ids`, committing once when
/// the gesture ended — so a drag or a click over several lanes is one undo
/// step. The setters clamp; a lane that no longer exists is skipped by
/// them.
pub fn apply(model: &mut Model, ids: &[u32], gesture: Gesture) {
    if let Some(edit) = gesture.edit {
        for &id in ids {
            match edit {
                Edit::AttackMs(ms) => model.set_attack_ms(id, ms),
                Edit::ReleaseMs(ms) => model.set_release_ms(id, ms),
                Edit::CutItself(on) => model.set_cut_itself(id, on),
                Edit::Output(output) => model.set_output(id, output),
                Edit::TimeMode(mode) => model.set_time_mode(id, mode),
                Edit::Length(length) => model.set_length(id, length),
                Edit::FinePct(pct) => model.set_fine_pct(id, pct),
                Edit::PitchSt(st) => model.set_pitch_st(id, st),
            };
        }
    }
    if gesture.ended {
        model.commit();
    }
}

/// Draws the panel for the lanes `ids` names — the hint when none of them
/// exists, otherwise the header and one row per setting — and applies
/// whatever its controls changed to every one of them.
pub fn show(ui: &mut Ui, model: &mut Model, ids: &[u32]) -> PanelOutput {
    let mut out = PanelOutput::default();
    let selected = selected_lanes(model, ids);
    ui.add_space(theme::ITEM_SPACING);
    if selected.is_empty() {
        ui.label(RichText::new(HINT).color(theme::TEXT_DIM));
        return out;
    }
    let tempo = model.tempo().get();
    let values = Values::of(&selected, tempo);
    // What a drag over a mixed field moves on from: the first lane's value.
    let attack = (selected[0].attack_ms, values.attack == MIXED);
    let release = (selected[0].release_ms, values.release == MIXED);
    let fine = (selected[0].fine_pct, values.fine == MIXED);
    // In Repitch the field shows (and cannot move) the resulting pitch.
    let pitch_enabled = values.time_mode != Shared::Same(TimeMode::Repitch);
    let pitch = if pitch_enabled {
        (selected[0].pitch_st, values.pitch == MIXED)
    } else {
        (shown_pitch(selected[0], tempo), values.pitch == MIXED)
    };
    let natural = mixed(selected.iter().map(|lane| natural_secs(lane))).value().flatten();
    let entries = length_entries(natural, tempo);
    let length = mixed(selected.iter().map(|lane| lane.length));
    let existing: Vec<u32> = selected.iter().map(|lane| lane.id).collect();

    ui.label(RichText::new(&values.header).strong());
    ui.add_space(theme::ITEM_SPACING);

    let mut gesture = Gesture::default();
    Grid::new("chopdrop_lane_panel")
        .num_columns(2)
        .spacing([2.0 * theme::ITEM_SPACING, theme::ITEM_SPACING])
        .show(ui, |ui| {
            name_cell(ui, ATTACK_LABEL);
            let (rect, found) = number_field(ui, attack, &ATTACK_FIELD, true);
            out.attack = Some(rect);
            gesture.merge(found);
            ui.end_row();

            name_cell(ui, RELEASE_LABEL);
            let (rect, found) = number_field(ui, release, &RELEASE_FIELD, true);
            out.release = Some(rect);
            gesture.merge(found);
            ui.end_row();

            name_cell(ui, CUT_ITSELF_LABEL);
            let mut checked = values.cut_itself.value().unwrap_or(false);
            let indeterminate = values.cut_itself == Shared::Mixed;
            let response =
                ui.add(Checkbox::without_text(&mut checked).indeterminate(indeterminate));
            out.cut_itself = Some(response.rect);
            if response.clicked() {
                let on = cut_itself_click(values.cut_itself);
                gesture.merge(Gesture { edit: Some(Edit::CutItself(on)), ended: true });
            }
            ui.end_row();

            name_cell(ui, OUTPUT_LABEL);
            let shared = mixed(selected.iter().map(|lane| lane.output));
            let (rect, found) = output_dropdown(ui, shared, &values.output);
            out.output = Some(rect);
            gesture.merge(found);
            ui.end_row();

            name_cell(ui, TIME_LABEL);
            ui.horizontal(|ui| {
                for (n, (mode, label)) in TIME_CHOICES.into_iter().enumerate() {
                    let lit = values.time_mode == Shared::Same(mode);
                    let response = ui.selectable_label(lit, label);
                    out.time[n] = Some(response.rect);
                    if response.clicked() {
                        let edit = Some(Edit::TimeMode(mode));
                        gesture.merge(Gesture { edit, ended: true });
                    }
                }
            });
            ui.end_row();

            name_cell(ui, LENGTH_LABEL);
            let (rect, found) = length_dropdown(ui, length, &values.length, &entries);
            out.length = Some(rect);
            gesture.merge(found);
            ui.end_row();

            name_cell(ui, FINE_LABEL);
            let (rect, found) = number_field(ui, fine, &FINE_FIELD, true);
            out.fine = Some(rect);
            gesture.merge(found);
            ui.end_row();

            name_cell(ui, PITCH_LABEL);
            let (rect, found) = number_field(ui, pitch, &PITCH_FIELD, pitch_enabled);
            out.pitch = Some(rect);
            out.pitch_enabled = pitch_enabled;
            gesture.merge(found);
            ui.end_row();
        });

    if let Some(origin) = &values.origin {
        ui.add_space(theme::ITEM_SPACING);
        ui.label(RichText::new(origin).color(theme::TEXT_DIM));
    }

    let mut retry = false;
    if let Some(status) = render_status(&selected) {
        ui.add_space(theme::ITEM_SPACING);
        let text = RichText::new(status.text()).color(theme::TEXT_DIM);
        let rect = match status {
            RenderStatus::Rendering => ui.label(text).rect,
            RenderStatus::Failed => {
                let reason = selected.iter().find_map(|lane| lane.render_error.as_deref());
                let mut response = ui.add(Label::new(text).sense(Sense::click()));
                if let Some(reason) = reason {
                    response = response.on_hover_text(reason);
                }
                retry = response.clicked();
                response.rect
            }
        };
        out.status = Some((rect, status));
    }

    apply(model, &existing, gesture);
    if retry {
        retry_renders(model, &existing);
    }
    out
}

/// How one of the panel's number fields reads, moves and words its value.
struct Field {
    range: RangeInclusive<f32>,
    /// Units a point of drag moves.
    speed: f64,
    /// Units a notch of the wheel moves.
    wheel_step: f32,
    /// Holds a value to the precision the field shows.
    round: fn(f32) -> f32,
    format: fn(f32) -> String,
    parse: fn(&str) -> Option<f64>,
    edit: fn(f32) -> Edit,
}

const ATTACK_FIELD: Field = Field {
    range: ATTACK_MS_MIN..=ATTACK_MS_MAX,
    speed: MS_DRAG_SPEED,
    wheel_step: MS_WHEEL_STEP,
    round: tenths,
    format: format_ms,
    parse: parse_ms,
    edit: Edit::AttackMs,
};

const RELEASE_FIELD: Field = Field {
    range: RELEASE_MS_MIN..=RELEASE_MS_MAX,
    speed: MS_DRAG_SPEED,
    wheel_step: MS_WHEEL_STEP,
    round: tenths,
    format: format_ms,
    parse: parse_ms,
    edit: Edit::ReleaseMs,
};

const FINE_FIELD: Field = Field {
    range: FINE_MIN..=FINE_MAX,
    speed: FINE_DRAG_SPEED,
    wheel_step: FINE_WHEEL_STEP,
    round: tenths,
    format: format_fine,
    parse: parse_pct,
    edit: Edit::FinePct,
};

const PITCH_FIELD: Field = Field {
    range: PITCH_MIN..=PITCH_MAX,
    speed: PITCH_DRAG_SPEED,
    wheel_step: PITCH_WHEEL_STEP,
    round: cents,
    format: format_pitch,
    parse: parse_st,
    edit: Edit::PitchSt,
};

/// A number field: drag, wheel or click and type, as `field` says.
/// `(value, mixed)` is what it moves on from and whether to show [`MIXED`]
/// until it is moved. A disabled field only shows `value` (and, for the
/// pitch, says why on hover).
fn number_field(
    ui: &mut Ui,
    (value, mixed): (f32, bool),
    field: &Field,
    enabled: bool,
) -> (Rect, Gesture) {
    let round = field.round;
    let format = field.format;
    let base = f64::from(round(value));
    let mut edited = base;
    let (low, high) = (f64::from(*field.range.start()), f64::from(*field.range.end()));
    let response = ui.add_enabled(
        enabled,
        DragValue::new(&mut edited)
            .range(low..=high)
            .speed(field.speed)
            .min_size(Vec2::new(MS_FIELD_WIDTH, theme::CONTROL_HEIGHT))
            .update_while_editing(false)
            .custom_formatter(move |number, _| {
                if mixed && number == base { MIXED.to_string() } else { format(number as f32) }
            })
            .custom_parser(field.parse),
    );
    if !enabled {
        let response = response.on_disabled_hover_text(PITCH_REPITCH_HOVER);
        return (response.rect, Gesture::default());
    }

    let wheel = lanes::wheel_steps(ui, &response);
    let moved = if wheel == 0 {
        round(edited as f32)
    } else {
        let stepped = round(round(value) + wheel as f32 * field.wheel_step);
        stepped.max(*field.range.start()).min(*field.range.end())
    };
    let ended = wheel != 0 || response.drag_stopped() || response.lost_focus();
    let changed = f64::from(moved) != base;
    (response.rect, Gesture { edit: changed.then(|| (field.edit)(moved)), ended })
}

/// The length dropdown: `entries` ([`length_entries`]), the shared length
/// ticked, showing `label` ([`MIXED`] when the lanes disagree). Picking an
/// entry is one whole gesture.
fn length_dropdown(
    ui: &mut Ui,
    current: Shared<Length>,
    label: &str,
    entries: &[(Length, String)],
) -> (Rect, Gesture) {
    let mut picked = None;
    let response = ComboBox::from_id_salt("chopdrop_lane_panel_length")
        .width(MS_FIELD_WIDTH)
        .height(entries.len() as f32 * 2.0 * theme::CONTROL_HEIGHT)
        .selected_text(label)
        .show_ui(ui, |ui| {
            ui.set_min_width(LENGTH_MENU_WIDTH);
            for (length, text) in entries {
                let ticked = current == Shared::Same(*length);
                if ui.selectable_label(ticked, text.as_str()).clicked() {
                    picked = Some(*length);
                }
            }
        })
        .response;
    let gesture = Gesture { edit: picked.map(Edit::Length), ended: picked.is_some() };
    (response.rect, gesture)
}

/// The output dropdown: "Out 1" … "Out 16", the shared one ticked, showing
/// `label` ([`MIXED`] when the lanes disagree). Picking an entry is one
/// whole gesture, even when it is the output they already share.
fn output_dropdown(ui: &mut Ui, current: Shared<u8>, label: &str) -> (Rect, Gesture) {
    let mut picked = None;
    let response = ComboBox::from_id_salt("chopdrop_lane_panel_output")
        .width(MS_FIELD_WIDTH)
        // Tall enough for all sixteen without a scroll bar (the window
        // still bounds it).
        .height(f32::from(OUTPUT_MAX + 1) * 2.0 * theme::CONTROL_HEIGHT)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for output in 0..=OUTPUT_MAX {
                let ticked = current == Shared::Same(output);
                if ui.selectable_label(ticked, format_output(output)).clicked() {
                    picked = Some(output);
                }
            }
        })
        .response
        .on_hover_text(OUTPUT_HOVER);
    let gesture = Gesture { edit: picked.map(Edit::Output), ended: picked.is_some() };
    (response.rect, gesture)
}

/// How wide the panel's number fields are.
const MS_FIELD_WIDTH: f32 = 72.0;
/// How wide the open length menu is at least, so "≈ 1/16t −83 %" fits.
const LENGTH_MENU_WIDTH: f32 = 120.0;

fn name_cell(ui: &mut Ui, label: &str) {
    ui.label(RichText::new(label).color(theme::TEXT_DIM));
}


#[cfg(test)]
mod tests {
    use egui::{Pos2, RawInput, Rect, vec2};

    use super::*;
    use crate::time::NoteLength;

    #[test]
    fn mixed_tells_none_same_and_mixed_apart() {
        assert_eq!(mixed(Vec::<f32>::new()), Shared::None);
        assert_eq!(mixed([3.0]), Shared::Same(3.0));
        assert_eq!(mixed([true, true, true]), Shared::Same(true));
        assert_eq!(mixed([1u8, 1, 2]), Shared::Mixed);
        assert_eq!(mixed([2u8, 1, 1]), Shared::Mixed);
        // NaN never equals itself; a lane with one reads as mixed, not as
        // a value — the model clamps NaN away before it gets here anyway.
        assert_eq!(mixed([f32::NAN, f32::NAN]), Shared::Mixed);
        assert_eq!(Shared::Same(4).value(), Some(4));
        assert_eq!(Shared::<u8>::Mixed.value(), None);
        assert_eq!(Shared::<u8>::None.value(), None);
    }

    #[test]
    fn the_header_names_one_lane_and_counts_several() {
        let a = Lane::new(1, "Kick", 36, 0);
        let b = Lane::new(2, "Snare", 38, 0);
        let c = Lane::new(3, "Hat", 42, 0);
        assert_eq!(header(&[]), "");
        assert_eq!(header(&[&a]), "Kick");
        assert_eq!(header(&[&a, &b]), "2 lanes");
        assert_eq!(header(&[&a, &b, &c]), "3 lanes");
    }

    #[test]
    fn labels_word_each_value() {
        assert_eq!(format_ms(0.0), "0 ms");
        assert_eq!(format_ms(-0.0), "0 ms");
        assert_eq!(format_ms(300.0), "300 ms");
        assert_eq!(format_ms(12.5), "12.5 ms");
        assert_eq!(format_ms(2000.0), "2000 ms");
        assert_eq!(format_output(0), "Out 1");
        assert_eq!(format_output(4), "Out 5");
        assert_eq!(format_output(15), "Out 16");
        assert_eq!(format_length(Length::AsCut), "as cut");
        assert_eq!(format_length(Length::Beats(NoteLength::Bar)), "1 bar");
        assert_eq!(format_length(Length::Beats(NoteLength::EighthTriplet)), "1/8t");
        assert_eq!(format_fine(0.0), "0.0 %");
        assert_eq!(format_fine(-0.04), "0.0 %");
        assert_eq!(format_fine(1.5), "+1.5 %");
        assert_eq!(format_fine(-2.0), "\u{2212}2.0 %");
        assert_eq!(format_pitch(0.0), "0.00 st");
        assert_eq!(format_pitch(12.0), "+12.00 st");
        assert_eq!(format_pitch(-0.5), "\u{2212}0.50 st");
        assert_eq!(shared_label(Shared::Same(4u8), format_output), "Out 5");
        assert_eq!(shared_label(Shared::<u8>::Mixed, format_output), MIXED);
        assert_eq!(shared_label(Shared::<u8>::None, format_output), MIXED);
    }

    #[test]
    fn values_show_shared_settings_and_dash_the_mixed_ones() {
        let mut a = Lane::new(1, "Kick", 36, 0);
        let mut b = Lane::new(2, "Snare", 38, 0);
        a.attack_ms = 10.0;
        b.attack_ms = 20.0;
        b.cut_itself = false;
        a.output = 3;
        b.output = 3;
        b.time_mode = TimeMode::Stretch;
        a.length = Length::Beats(NoteLength::Bar);
        b.length = Length::Beats(NoteLength::Bar);
        b.pitch_st = 1.0;

        let one = Values::of(&[&a], 120.0);
        assert_eq!(one.header, "Kick");
        assert_eq!(one.attack, "10 ms");
        assert_eq!(one.release, "300 ms");
        assert_eq!(one.cut_itself, Shared::Same(true));
        assert_eq!(one.time_mode, Shared::Same(TimeMode::Off));

        let both = Values::of(&[&a, &b], 120.0);
        assert_eq!(both.header, "2 lanes");
        assert_eq!(both.attack, MIXED);
        assert_eq!(both.release, "300 ms");
        assert_eq!(both.cut_itself, Shared::Mixed);
        assert_eq!(both.output, "Out 4");
        assert_eq!(both.time_mode, Shared::Mixed);
        assert_eq!(both.length, "1 bar");
        assert_eq!(both.fine, "0.0 %");
        assert_eq!(both.pitch, MIXED);
    }

    /// The texts one headless frame of the panel paints for `ids`.
    fn painted(model: &mut Model, ids: &[u32]) -> Vec<String> {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(320.0, 600.0))),
            ..RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| {
            show(ui, model, ids);
        });
        let texts = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Text(text) => Some(text.galley.text().to_string()),
                _ => None,
            })
            .collect();
        output.drop_without_applying_deltas();
        texts
    }

    #[test]
    fn the_panel_draws_for_none_one_and_three_lanes() {
        let mut model = Model::new();
        let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
        assert!(ids.len() >= 3, "the default model has lanes to select");

        let none = painted(&mut model, &[]);
        assert_eq!(none, [HINT]);
        // A selection of lanes that no longer exist is no selection.
        assert_eq!(painted(&mut model, &[u32::MAX]), [HINT]);

        let one = painted(&mut model, &ids[..1]);
        assert!(one.contains(&model.lanes()[0].name), "{one:?}");
        for label in [ATTACK_LABEL, RELEASE_LABEL, CUT_ITSELF_LABEL, OUTPUT_LABEL, TIME_LABEL] {
            assert!(one.iter().any(|text| text == label), "{label} in {one:?}");
        }
        assert!(one.iter().any(|text| text == "Out 1"), "{one:?}");

        model.set_attack_ms(ids[1], 50.0);
        let three = painted(&mut model, &ids[..3]);
        assert!(three.iter().any(|text| text == "3 lanes"), "{three:?}");
        assert!(three.iter().any(|text| text == MIXED), "attack is mixed: {three:?}");
        for (_, label) in TIME_CHOICES {
            assert!(three.iter().any(|text| text == label), "{label} in {three:?}");
        }
    }

    /// Gives lane `id` a chop sample cut from `/songs/Eye.wav`, frames
    /// 30.120 s–32.090 s at 48 kHz.
    fn give_origin(model: &mut Model, id: u32) -> crate::chop::ChopOrigin {
        let origin = crate::chop::ChopOrigin {
            source_path: String::from("/songs/Eye.wav"),
            start_frame: 1_445_760,
            end_frame: 1_540_320,
        };
        let data = crate::sample::SampleData {
            rate: 48_000,
            channels: 1,
            frames: vec![0.0; 4].into(),
        };
        let path = std::path::Path::new("/songs/Eye.wav");
        let sample = crate::model::SampleRef::with_origin(
            model.handle(),
            path,
            vec![0],
            data,
            Some(origin.clone()),
        );
        assert!(model.set_sample(id, sample));
        origin
    }

    /// One selected chop lane shows its origin line, worded by
    /// `format_origin`; a lane without an origin, or several lanes, show
    /// none.
    #[test]
    fn the_origin_line_shows_for_one_chop_lane_only() {
        let mut model = Model::new();
        let ids: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
        let origin = give_origin(&mut model, ids[0]);
        let expected = crate::chop::format_origin(&origin, 48_000, "Eye.wav");
        assert_eq!(expected, "from Eye.wav, 0:30.120\u{2013}0:32.090");

        let one = painted(&mut model, &ids[..1]);
        assert!(one.contains(&expected), "{one:?}");
        let values = Values::of(&selected_lanes(&model, &ids[..1]), 120.0);
        assert_eq!(values.origin, Some(expected.clone()));

        let plain = painted(&mut model, &ids[1..2]);
        assert!(!plain.iter().any(|text| text.starts_with("from ")), "{plain:?}");
        assert_eq!(Values::of(&selected_lanes(&model, &ids[1..2]), 120.0).origin, None);

        let both = painted(&mut model, &ids[..2]);
        assert!(!both.contains(&expected), "single selection only: {both:?}");
    }

    /// The panel alone in a window over a default model, with its first two
    /// lanes selected — and the frames that drive it.
    struct Panel {
        ctx: egui::Context,
        model: Model,
        ids: Vec<u32>,
        out: PanelOutput,
    }

    impl Panel {
        fn new() -> Panel {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let mut model = Model::new();
            let ids: Vec<u32> = model.lanes().iter().take(2).map(|lane| lane.id).collect();
            // Make the two lanes disagree on every control, as one step the
            // tests start counting after.
            model.set_attack_ms(ids[1], 50.0);
            model.set_release_ms(ids[1], 900.0);
            model.set_cut_itself(ids[1], false);
            model.commit();
            let mut panel = Panel { ctx, model, ids, out: PanelOutput::default() };
            panel.idle();
            panel
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(320.0, 600.0))),
                events,
                ..RawInput::default()
            };
            let (model, ids) = (&mut self.model, &self.ids);
            let mut out = PanelOutput::default();
            let output = self.ctx.run_ui(input, |ui| out = show(ui, model, ids));
            output.drop_without_applying_deltas();
            self.out = out;
        }

        fn idle(&mut self) {
            self.frame(Vec::new());
        }

        fn click(&mut self, at: Pos2) {
            self.frame(vec![egui::Event::PointerMoved(at), pointer(at, true)]);
            self.frame(vec![pointer(at, false)]);
        }

        fn drag(&mut self, from: Pos2, to: Pos2) {
            let halfway = from + (to - from) / 2.0;
            self.frame(vec![egui::Event::PointerMoved(from), pointer(from, true)]);
            self.frame(vec![egui::Event::PointerMoved(halfway)]);
            self.frame(vec![egui::Event::PointerMoved(to)]);
            self.frame(vec![pointer(to, false)]);
        }

        fn lane(&self, n: usize) -> &Lane {
            self.model.lanes().iter().find(|lane| lane.id == self.ids[n]).expect("lane")
        }

        fn attack(&self) -> Pos2 {
            self.out.attack.expect("attack field drawn").center()
        }

        fn release(&self) -> Pos2 {
            self.out.release.expect("release field drawn").center()
        }

        fn cut_itself(&self) -> Pos2 {
            self.out.cut_itself.expect("checkbox drawn").center()
        }

        fn output(&self) -> Pos2 {
            self.out.output.expect("output dropdown drawn").center()
        }

        /// Where the last frame painted `text` last (a popup paints over
        /// the panel, so its entries come after the dropdown's button).
        fn text_at(&mut self, text: &str) -> Option<Pos2> {
            let input = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(320.0, 600.0))),
                ..RawInput::default()
            };
            let (model, ids) = (&mut self.model, &self.ids);
            let output = self.ctx.run_ui(input, |ui| {
                show(ui, model, ids);
            });
            let found = output
                .shapes
                .iter()
                .rev()
                .filter_map(|clipped| match &clipped.shape {
                    egui::Shape::Text(shape) if shape.galley.text() == text => {
                        Some(shape.visual_bounding_rect().center())
                    }
                    _ => None,
                })
                .next();
            output.drop_without_applying_deltas();
            found
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

    fn key(key: egui::Key) -> Vec<egui::Event> {
        [true, false]
            .map(|pressed| egui::Event::Key {
                key,
                physical_key: None,
                pressed,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            })
            .to_vec()
    }

    #[test]
    fn dragging_attack_sets_both_lanes_in_one_undo_step() {
        let mut panel = Panel::new();
        let depth = panel.model.undo_depth();
        let at = panel.attack();
        panel.drag(at, at + vec2(40.0, 0.0));
        let attack = panel.lane(0).attack_ms;
        assert!(attack > 0.0, "a drag right raised the attack: {attack}");
        assert_eq!(panel.lane(1).attack_ms, attack, "both lanes moved together");
        assert_eq!(panel.model.undo_depth(), depth + 1, "one drag, one step");

        assert!(panel.model.undo());
        assert_eq!(panel.lane(0).attack_ms, 0.0);
        assert_eq!(panel.lane(1).attack_ms, 50.0, "the undo restores each lane's own");
    }

    #[test]
    fn dragging_release_sets_both_lanes_in_one_undo_step() {
        let mut panel = Panel::new();
        let depth = panel.model.undo_depth();
        let at = panel.release();
        panel.drag(at, at - vec2(0.0, 30.0));
        let release = panel.lane(0).release_ms;
        assert!(release > 300.0, "a drag up raised the release: {release}");
        assert_eq!(panel.lane(1).release_ms, release);
        assert_eq!(panel.model.undo_depth(), depth + 1);
    }

    #[test]
    fn a_wheel_notch_steps_both_lanes_in_one_undo_step() {
        let mut panel = Panel::new();
        let depth = panel.model.undo_depth();
        let at = panel.release();
        let wheel = egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Line,
            delta: vec2(0.0, 1.0),
            phase: egui::TouchPhase::Move,
            source: egui::MouseWheelSource::Unknown,
            modifiers: egui::Modifiers::default(),
        };
        panel.frame(vec![egui::Event::PointerMoved(at), wheel]);
        for _ in 0..200 {
            if !panel.ctx.input(|input| input.is_scrolling()) {
                break;
            }
            panel.idle();
        }
        // From the first lane's value, as a drag moves on from it.
        assert_eq!(panel.lane(0).release_ms, 300.0 + MS_WHEEL_STEP);
        assert_eq!(panel.lane(1).release_ms, 300.0 + MS_WHEEL_STEP);
        assert_eq!(panel.model.undo_depth(), depth + 1);
    }

    #[test]
    fn typing_an_attack_sets_both_lanes_in_one_undo_step() {
        let mut panel = Panel::new();
        let depth = panel.model.undo_depth();
        let at = panel.attack();
        panel.click(at);
        panel.idle();
        panel.frame(vec![egui::Event::Text("12.5 ms".to_string())]);
        panel.frame(key(egui::Key::Enter));
        panel.idle();
        assert_eq!(panel.lane(0).attack_ms, 12.5);
        assert_eq!(panel.lane(1).attack_ms, 12.5);
        assert_eq!(panel.model.undo_depth(), depth + 1);

        // Past the end is clamped, garbage is refused.
        panel.click(at);
        panel.idle();
        panel.frame(vec![egui::Event::Text("99999".to_string())]);
        panel.frame(key(egui::Key::Enter));
        panel.idle();
        assert_eq!(panel.lane(1).attack_ms, ATTACK_MS_MAX);
        panel.click(at);
        panel.idle();
        panel.frame(vec![egui::Event::Text("soon".to_string())]);
        panel.frame(key(egui::Key::Enter));
        panel.idle();
        assert_eq!(panel.lane(0).attack_ms, ATTACK_MS_MAX);
        assert_eq!(panel.model.undo_depth(), depth + 2, "garbage made no step");
    }

    #[test]
    fn the_indeterminate_checkbox_click_sets_all_on_in_one_undo_step() {
        let mut panel = Panel::new();
        let depth = panel.model.undo_depth();
        assert!(panel.lane(0).cut_itself && !panel.lane(1).cut_itself, "mixed to start");
        let at = panel.cut_itself();
        panel.click(at);
        assert!(panel.lane(0).cut_itself && panel.lane(1).cut_itself, "all on");
        assert_eq!(panel.model.undo_depth(), depth + 1);

        // Now shared and on: a click turns both off, another step.
        panel.click(at);
        assert!(!panel.lane(0).cut_itself && !panel.lane(1).cut_itself, "all off");
        assert_eq!(panel.model.undo_depth(), depth + 2);

        assert!(panel.model.undo() && panel.model.undo());
        assert!(panel.lane(0).cut_itself && !panel.lane(1).cut_itself, "back to mixed");
    }

    #[test]
    fn the_output_hover_names_add_missing_chains() {
        assert!(OUTPUT_HOVER.contains("Add Missing Chains"), "{OUTPUT_HOVER}");
        assert!(OUTPUT_HOVER.contains("Bitwig"), "{OUTPUT_HOVER}");
    }

    #[test]
    fn applying_an_output_sets_every_lane_in_one_undo_step() {
        let mut model = Model::new();
        let ids: Vec<u32> = model.lanes().iter().take(3).map(|lane| lane.id).collect();
        let depth = model.undo_depth();
        apply(&mut model, &ids, Gesture { edit: Some(Edit::Output(4)), ended: true });
        for lane in selected_lanes(&model, &ids) {
            assert_eq!(lane.output, 4, "{}", lane.name);
        }
        assert_eq!(model.lanes()[3].output, 0, "an unselected lane keeps Out 1");
        assert_eq!(model.undo_depth(), depth + 1);
        // Picking what they already share makes no step.
        apply(&mut model, &ids, Gesture { edit: Some(Edit::Output(4)), ended: true });
        assert_eq!(model.undo_depth(), depth + 1);
        // A hostile index lands on main, never on a silent output.
        apply(&mut model, &ids[..1], Gesture { edit: Some(Edit::Output(16)), ended: true });
        assert_eq!(selected_lanes(&model, &ids[..1])[0].output, 0);
    }

    #[test]
    fn selecting_out_5_sets_output_4_on_every_selected_lane() {
        let mut panel = Panel::new();
        panel.model.set_output(panel.ids[1], 2);
        panel.model.commit();
        panel.idle();
        let depth = panel.model.undo_depth();
        assert!(panel.text_at(MIXED).is_some(), "mixed outputs show a dash");

        let at = panel.output();
        panel.click(at);
        panel.idle();
        let entry = panel.text_at("Out 5").expect("the open dropdown lists Out 5");
        panel.click(entry);
        panel.idle();

        assert_eq!(panel.lane(0).output, 4);
        assert_eq!(panel.lane(1).output, 4);
        assert_eq!(panel.model.lanes()[2].output, 0, "outside the selection");
        assert_eq!(panel.model.undo_depth(), depth + 1, "one pick, one step");
        assert!(panel.text_at("Out 5").is_some(), "the button now shows Out 5");

        assert!(panel.model.undo());
        assert_eq!(panel.lane(0).output, 0);
        assert_eq!(panel.lane(1).output, 2);
    }

    #[test]
    fn lanes_outside_the_selection_are_untouched() {
        let mut panel = Panel::new();
        let third = panel.model.lanes()[2].clone();
        let at = panel.attack();
        panel.drag(at, at + vec2(40.0, 0.0));
        panel.click(panel.cut_itself());
        let after = &panel.model.lanes()[2];
        assert_eq!(after.attack_ms, third.attack_ms);
        assert_eq!(after.cut_itself, third.cut_itself);
    }

    /// Gives lane `id` a silent mono sample `frames` long at 48 kHz.
    fn give_sample(model: &mut Model, id: u32, frames: usize) {
        let data = crate::sample::SampleData {
            rate: 48_000,
            channels: 1,
            frames: vec![0.0; frames].into(),
        };
        let path = std::path::Path::new("/songs/chop.wav");
        let sample = crate::model::SampleRef::new(model.handle(), path, vec![0], data);
        assert!(model.set_sample(id, sample));
    }

    /// A worked example chop: 1.995 s at 122 BPM (95 760 frames at 48 kHz).
    const CHOP_FRAMES: usize = 95_760;
    const CHOP_TEMPO: f64 = 122.0;

    /// The length menu for a 1.995 s chop at 122 BPM, every entry pinned.
    #[test]
    fn the_length_entries_are_pinned_for_the_brief_chop() {
        let labels: Vec<String> =
            length_entries(Some(1.995), CHOP_TEMPO).into_iter().map(|(_, l)| l).collect();
        let m = "\u{2212}";
        let expected = [
            "as cut".to_string(),
            format!("1/16 {m}94 %"),
            format!("1/8 {m}88 %"),
            format!("1/4 {m}75 %"),
            format!("1/2 {m}51 %"),
            format!("\u{2248} 1 bar {m}1.4 %"),
            "2 bars +97 %".to_string(),
            "4 bars +294 %".to_string(),
            "8 bars +689 %".to_string(),
            format!("1/16d {m}91 %"),
            format!("1/8d {m}82 %"),
            format!("1/4d {m}63 %"),
            format!("1/2d {m}26 %"),
            format!("1/16t {m}96 %"),
            format!("1/8t {m}92 %"),
            format!("1/4t {m}84 %"),
            format!("1/2t {m}67 %"),
        ];
        assert_eq!(labels, expected);
        let lengths: Vec<Length> =
            length_entries(None, CHOP_TEMPO).into_iter().map(|(l, _)| l).collect();
        assert_eq!(lengths[0], Length::AsCut, "as cut first");
        assert_eq!(&lengths[1..], NOTE_LENGTHS.map(Length::Beats).as_slice());
        // Without a chop length (no sample, samples differing, 0 s): bare.
        for natural in [None, Some(0.0), Some(f64::NAN)] {
            let bare: Vec<String> =
                length_entries(natural, CHOP_TEMPO).into_iter().map(|(_, l)| l).collect();
            assert_eq!(bare[5], "1 bar", "{natural:?}");
        }
    }

    /// Repitch shows the pitch its speed change makes; Off and Stretch the
    /// lane's own.
    #[test]
    fn the_shown_pitch_follows_the_mode() {
        let mut model = Model::new();
        let id = model.lanes()[0].id;
        give_sample(&mut model, id, CHOP_FRAMES);
        model.set_pitch_st(id, 3.0);
        let lane = |model: &Model| model.lane(id).expect("lane").clone();
        assert_eq!(shown_pitch(&lane(&model), CHOP_TEMPO), 3.0);
        model.set_time_mode(id, TimeMode::Repitch);
        assert_eq!(shown_pitch(&lane(&model), CHOP_TEMPO), 0.0, "as cut plays at speed 1");
        model.set_length(id, Length::Beats(NoteLength::Bar));
        let bar = time::target_secs(NoteLength::Bar, 0.0, CHOP_TEMPO);
        let expected = time::speed_to_semis(time::repitch_speed(1.995, bar));
        assert_eq!(shown_pitch(&lane(&model), CHOP_TEMPO), expected);
        assert_eq!(format_pitch(expected), "+0.24 st");
        model.set_time_mode(id, TimeMode::Stretch);
        assert_eq!(shown_pitch(&lane(&model), CHOP_TEMPO), 3.0);
    }

    /// The time controls on two selected lanes sharing that example chop at
    /// 122 BPM: each pick, drag or typed value sets both, one undo step.
    #[test]
    fn the_time_controls_set_both_lanes_in_one_undo_step_each() {
        let mut panel = Panel::new();
        panel.model.tempo().set(CHOP_TEMPO);
        for id in panel.ids.clone() {
            give_sample(&mut panel.model, id, CHOP_FRAMES);
        }
        panel.model.commit();
        panel.idle();
        assert!(panel.out.pitch_enabled, "Off: the pitch is the lane's own");

        // Time: Repitch.
        let depth = panel.model.undo_depth();
        panel.click(panel.out.time[1].expect("Repitch drawn").center());
        panel.idle();
        assert_eq!(panel.lane(0).time_mode, TimeMode::Repitch);
        assert_eq!(panel.lane(1).time_mode, TimeMode::Repitch);
        assert_eq!(panel.model.lanes()[2].time_mode, TimeMode::Off, "outside the selection");
        assert_eq!(panel.model.undo_depth(), depth + 1);
        assert!(!panel.out.pitch_enabled, "Repitch disables the pitch");

        // Length: the open menu words each entry; pick the nearest.
        let at = panel.out.length.expect("length dropdown drawn").center();
        panel.click(at);
        panel.idle();
        let nearest = "\u{2248} 1 bar \u{2212}1.4 %";
        let entry = panel.text_at(nearest).expect("the open menu lists the nearest entry");
        assert!(panel.text_at("2 bars +97 %").is_some(), "and the others with their change");
        panel.click(entry);
        panel.idle();
        assert_eq!(panel.lane(0).length, Length::Beats(NoteLength::Bar));
        assert_eq!(panel.lane(1).length, Length::Beats(NoteLength::Bar));
        assert_eq!(panel.model.undo_depth(), depth + 2);
        assert!(panel.text_at("1 bar").is_some(), "the button shows the short label");

        // Pitch: disabled, showing the computed pitch; a drag moves nothing.
        assert!(panel.text_at("+0.24 st").is_some(), "the resulting pitch shows");
        let at = panel.out.pitch.expect("pitch drawn").center();
        panel.drag(at, at + vec2(40.0, 0.0));
        assert_eq!(panel.lane(0).pitch_st, 0.0);
        assert_eq!(panel.lane(1).pitch_st, 0.0);
        assert_eq!(panel.model.undo_depth(), depth + 2, "a disabled drag makes no step");

        // Fine: typed, the shown pitch follows it.
        let at = panel.out.fine.expect("fine drawn").center();
        panel.click(at);
        panel.idle();
        panel.frame(vec![egui::Event::Text("+1.5 %".to_string())]);
        panel.frame(key(egui::Key::Enter));
        panel.idle();
        assert_eq!(panel.lane(0).fine_pct, 1.5);
        assert_eq!(panel.lane(1).fine_pct, 1.5);
        assert_eq!(panel.model.undo_depth(), depth + 3);
        let shown = shown_pitch(panel.lane(0), CHOP_TEMPO);
        assert!(panel.text_at(&format_pitch(shown)).is_some(), "{shown}");
        assert_ne!(format_pitch(shown), "+0.24 st", "fine changes the speed");

        // Fine: a drag left lowers both, one step.
        panel.drag(at, at - vec2(20.0, 0.0));
        let fine = panel.lane(0).fine_pct;
        assert!(fine < 1.5, "a drag left lowered the fine: {fine}");
        assert_eq!(panel.lane(1).fine_pct, fine);
        assert_eq!(panel.model.undo_depth(), depth + 4);

        // Back to Off: the pitch takes input again; a drag sets both.
        panel.click(panel.out.time[0].expect("Off drawn").center());
        panel.idle();
        assert_eq!(panel.lane(1).time_mode, TimeMode::Off);
        assert!(panel.out.pitch_enabled);
        let at = panel.out.pitch.expect("pitch drawn").center();
        panel.drag(at, at + vec2(40.0, 0.0));
        let pitch = panel.lane(0).pitch_st;
        assert!(pitch > 0.0, "a drag right raised the pitch: {pitch}");
        assert_eq!(panel.lane(1).pitch_st, pitch);
        assert_eq!(panel.model.undo_depth(), depth + 6);

        // Stretch: both, and the pitch stays enabled.
        panel.click(panel.out.time[2].expect("Stretch drawn").center());
        panel.idle();
        assert_eq!(panel.lane(0).time_mode, TimeMode::Stretch);
        assert_eq!(panel.lane(1).time_mode, TimeMode::Stretch);
        assert!(panel.out.pitch_enabled);
        assert_eq!(panel.model.undo_depth(), depth + 7);

        // One undo takes both lanes back to Off.
        assert!(panel.model.undo());
        assert_eq!(panel.lane(0).time_mode, TimeMode::Off);
        assert_eq!(panel.lane(1).time_mode, TimeMode::Off);
    }

    #[test]
    fn applying_time_edits_sets_every_lane() {
        let mut model = Model::new();
        let ids: Vec<u32> = model.lanes().iter().take(2).map(|lane| lane.id).collect();
        let edits = [
            Edit::TimeMode(TimeMode::Stretch),
            Edit::Length(Length::Beats(NoteLength::EighthTriplet)),
            Edit::FinePct(12.0),
            Edit::PitchSt(-30.0),
        ];
        for edit in edits {
            apply(&mut model, &ids, Gesture { edit: Some(edit), ended: true });
        }
        for lane in selected_lanes(&model, &ids) {
            assert_eq!(lane.time_mode, TimeMode::Stretch);
            assert_eq!(lane.length, Length::Beats(NoteLength::EighthTriplet));
            assert_eq!(lane.fine_pct, FINE_MAX, "clamped");
            assert_eq!(lane.pitch_st, PITCH_MIN, "clamped");
        }
    }

    #[test]
    fn helpers_parse_times_and_decide_the_click() {
        assert_eq!(parse_ms("12"), Some(12.0));
        assert_eq!(parse_ms(" 12.5 ms "), Some(12.5));
        assert_eq!(parse_ms("3MS"), Some(3.0));
        assert_eq!(parse_ms("ms"), None);
        assert_eq!(parse_ms("NaN"), None);
        assert_eq!(parse_ms(""), None);
        assert!(cut_itself_click(Shared::Mixed));
        assert!(cut_itself_click(Shared::Same(false)));
        assert!(!cut_itself_click(Shared::Same(true)));
        assert_eq!(parse_pct("+1.5 %"), Some(1.5));
        assert_eq!(parse_pct("\u{2212}2%"), Some(-2.0));
        assert_eq!(parse_pct("-0.3"), Some(-0.3));
        assert_eq!(parse_pct("%"), None);
        assert_eq!(parse_st("+12.00 st"), Some(12.0));
        assert_eq!(parse_st("\u{2212}0.5st"), Some(-0.5));
        assert_eq!(parse_st("7"), Some(7.0));
        assert_eq!(parse_st("inf"), None);
    }

    /// A Stretch lane shows "rendering…" while its render is pending, then
    /// nothing once it lands; a failed render shows the retry line, whose
    /// click asks for a fresh render (no undo step) and shows "rendering…"
    /// again. Lanes not in Stretch never show a status.
    #[test]
    fn the_render_status_shows_while_pending_and_the_retry_click_requests() {
        let mut panel = Panel::new();
        panel.model.tempo().set(CHOP_TEMPO);
        let (a, b) = (panel.ids[0], panel.ids[1]);
        give_sample(&mut panel.model, a, CHOP_FRAMES);
        give_sample(&mut panel.model, b, CHOP_FRAMES);
        panel.model.commit();
        panel.idle();
        assert_eq!(panel.out.status, None, "Off: nothing renders");
        assert!(panel.text_at(RENDERING_TEXT).is_none());

        // Stretch on both: each is pending.
        panel.click(panel.out.time[2].expect("Stretch drawn").center());
        panel.idle();
        assert!(panel.lane(0).render_pending && panel.lane(1).render_pending);
        assert_eq!(panel.out.status.map(|(_, s)| s), Some(RenderStatus::Rendering));
        assert!(panel.text_at(RENDERING_TEXT).is_some(), "rendering… painted");
        // Clicking "rendering…" does nothing.
        let generation = panel.lane(0).render_generation;
        panel.click(panel.out.status.expect("status drawn").0.center());
        assert_eq!(panel.lane(0).render_generation, generation);

        // a lands; b fails: the retry line, with the reason on hover.
        let silent = |frames: usize| crate::sample::SampleData {
            rate: 48_000,
            channels: 1,
            frames: vec![0.0; frames].into(),
        };
        let gen_a = panel.lane(0).render_generation;
        let gen_b = panel.lane(1).render_generation;
        assert!(panel.model.set_render(a, gen_a, silent(1_000)));
        assert!(panel.model.set_render_error(b, gen_b, "stretch failed"));
        panel.idle();
        assert_eq!(panel.out.status.map(|(_, s)| s), Some(RenderStatus::Failed));
        assert!(panel.text_at(RENDER_FAILED_TEXT).is_some(), "retry line painted");
        assert!(panel.text_at(RENDERING_TEXT).is_none());

        // The click asks again for b only, as no undo step.
        let depth = panel.model.undo_depth();
        let landed = panel.lane(0).render_generation;
        panel.click(panel.out.status.expect("status drawn").0.center());
        assert!(panel.lane(1).render_pending, "the retry requested a render");
        assert!(panel.lane(1).render_generation > gen_b);
        assert_eq!(panel.lane(1).render_error, None);
        assert_eq!(panel.lane(0).render_generation, landed, "a had not failed");
        assert!(!panel.lane(0).render_pending);
        assert_eq!(panel.model.undo_depth(), depth, "a retry is not an undo step");
        panel.idle();
        assert_eq!(panel.out.status.map(|(_, s)| s), Some(RenderStatus::Rendering));

        // Both settled: hidden.
        let gen_b = panel.lane(1).render_generation;
        assert!(panel.model.set_render(b, gen_b, silent(1_000)));
        panel.idle();
        assert_eq!(panel.out.status, None);
        assert!(panel.text_at(RENDER_FAILED_TEXT).is_none());
    }

    #[test]
    fn render_status_counts_stretch_lanes_and_rendering_wins() {
        let mut lanes: Vec<Lane> = Model::new().lanes().iter().take(2).cloned().collect();
        lanes[0].render_error = Some(String::from("x"));
        lanes[1].render_pending = true;
        let both = |lanes: &[Lane]| render_status(&lanes.iter().collect::<Vec<_>>());
        assert_eq!(both(&lanes), None, "Off lanes show nothing");
        lanes[0].time_mode = TimeMode::Stretch;
        assert_eq!(both(&lanes), Some(RenderStatus::Failed));
        lanes[1].time_mode = TimeMode::Stretch;
        assert_eq!(both(&lanes), Some(RenderStatus::Rendering));
        assert_eq!(render_status(&[]), None);
        assert_eq!(RenderStatus::Rendering.text(), "rendering\u{2026}");
        assert_eq!(RenderStatus::Failed.text(), "render failed \u{2014} click to retry");
    }
}
