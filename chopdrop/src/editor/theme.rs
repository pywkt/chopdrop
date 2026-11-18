//! The only place the look of the editor lives.
//!
//! Every colour and every size the lane list is drawn with is named here, so
//! a later restyling is an edit to this file rather than a hunt through the
//! drawing code. Nothing here reads or writes the model: the helpers take the
//! value they colour (a lane's `0xRRGGBB`, an amount to brighten by) and hand
//! back an `egui` colour or stroke.
//!
//! This is bitstep's `editor/theme.rs` trimmed to a sample player, keeping
//! every name and value the two plugins share so that they read as a pair
//! when they sit side by side in one project. What a step grid alone needed —
//! step cells, velocity fills, ratchet and probability markers, the playhead,
//! the song grid, the parameter panel — is gone; chopdrop's own four
//! additions (the activity light, the drop highlight, error text, the
//! sample-name column) are in the same style, next to the constants they are
//! built from.
//!
//! The lane palette is [`crate::state::LANE_PALETTE`] converted to
//! [`Color32`], not a second list of its own: the colours a fresh instance
//! hands its lanes are saved into the host's project, so a palette that
//! drifted from the one in `state.rs` would repaint every reloaded patch.

use egui::{Color32, CornerRadius, Shadow, Stroke, Vec2, Visuals};

use crate::state::LANE_PALETTE;

// ---- greys (Bitwig-like dark greys) ----

/// Behind everything; the darkest tone, so panels read as raised.
pub const WINDOW: Color32 = Color32::from_gray(24);

/// Top bar and lane rows — the chrome the list is drawn on.
pub const PANEL: Color32 = Color32::from_gray(33);

/// Controls sitting on [`PANEL`] (buttons, drag values, dropdowns).
pub const CONTROL: Color32 = Color32::from_gray(45);

/// A control under the pointer.
pub const CONTROL_HOVER: Color32 = Color32::from_gray(56);

/// A control being clicked or held.
pub const CONTROL_ACTIVE: Color32 = Color32::from_gray(66);

/// Separators and outlines: the top bar's lower edge, the line under each
/// lane row, and the line egui draws around every widget.
pub const GRID_LINE: Color32 = Color32::from_gray(54);

/// egui's faint background — what the alternate rows of anything it stripes
/// are filled with.
///
/// bitstep's shaded step column, kept at its value because the two windows
/// are read next to each other: this is the one tone of the chrome that is
/// handed to egui rather than painted by hand, and a lane list striped a
/// shade off the sequencer's grid would be the first thing to show.
pub const CELL_SHADED: Color32 = Color32::from_gray(47);

/// Normal label text.
pub const TEXT: Color32 = Color32::from_gray(205);

/// Secondary text: a lane's sample file name, units, the empty-lane hint.
pub const TEXT_DIM: Color32 = Color32::from_gray(138);

/// The one non-grey of the chrome: a lit toggle, a focused control, the
/// outline on whatever a drag is about to land on. Lane colours are the
/// palette's job, not this one's.
pub const ACCENT: Color32 = Color32::from_rgb(0x55, 0x9D, 0xD6);

/// Text and items that have to say something is wrong or about to be: a
/// lane's reject message or missing-file note, and a `delete` or `clear`
/// waiting for its second click.
///
/// A red of the same family as the palette's, so it sits in the window
/// without matching any one lane: the point is that the row has something to
/// report, not that the report belongs to anything.
pub const WARN: Color32 = Color32::from_rgb(0xE8, 0x6A, 0x6A);

/// A lane row's drag grip — the dots a row is picked up by to reorder it.
///
/// Dimmer than [`TEXT`] and brighter than [`GRID_LINE`]: a grip is an
/// affordance rather than content, so it has to be findable without being the
/// first thing in the row the eye lands on.
pub const GRIP: Color32 = Color32::from_gray(104);

/// The fill under a selected lane row: lighter than [`PANEL`] by the step
/// [`CONTROL`] is, so a selected row reads as one raised bar with its fields'
/// hairlines still drawn on it. Grey and not the accent, which is kept for
/// drop and browse-target outlines.
pub const SELECTED_ROW: Color32 = {
    let step = CONTROL.r() - PANEL.r();
    Color32::from_gray(PANEL.r() + step)
};

/// The wash drawn over the lane row being dragged to a new position, so the
/// row the hand is holding reads as picked up.
pub const ROW_DRAG: Color32 = Color32::from_rgba_premultiplied(40, 40, 40, 40);

/// What is thrown under a menu or a dropdown.
///
/// Tighter and more squarely under its surface than egui's stock shadow,
/// which is offset ten points to the right and twenty down: a lane's context
/// menu opens over the rows below it, and a shadow cast that far sideways
/// darkens the very row the pointer is aiming at next. Straight down and a
/// little stronger instead, so the surface still lifts off the list.
pub const SHADOW: Shadow =
    Shadow { offset: [0, 4], blur: 12, spread: 0, color: Color32::from_black_alpha(140) };

/// How far towards white a lane's colour dot is pushed while the lane is
/// playing — the activity light at its brightest, before it fades back
/// (the fade curve between the two is [`crate::editor::activity`]'s).
///
/// bitstep's `PLAYHEAD_BRIGHTEN`, at its value and by the same means
/// ([`brighten`]): the two plugins are watched together while a beat plays,
/// so a hit has to read as the same amount of "now" in both.
pub const ACTIVITY_BRIGHTEN: f32 = 0.35;

// ---- sizes and spacing, in logical points ----

/// Every control of the chrome — a button in the top bar, a drag value in a
/// lane row, an item in a menu — is drawn exactly this tall, so the window's
/// two bands read as one list instead of a toolbar stacked on a column.
///
/// bitstep's step cell, and therefore its lane header: a row of chopdrop is
/// the same height as a row of the sequencer.
pub const CONTROL_HEIGHT: f32 = 30.0;

/// A lane row is one line of fields, a little taller than a control
/// elsewhere ([`CONTROL_HEIGHT`]), with [`ROW_GAP`] to the next one. Taller
/// for the play button at its front (the user's, 2026-11-16: the old one
/// was too small and too far off to the side; two-line rows were tried the
/// same day and were far too big).
pub const ROW_HEIGHT: f32 = 36.0;
pub const ROW_GAP: f32 = 4.0;
pub const ROW_PITCH: f32 = ROW_HEIGHT + ROW_GAP;

/// Inside a control, between its frame and its text.
///
/// Half of egui's stock horizontal padding, because a lane row spends its
/// width on nine fields: at 8 points a side the note field had 20 left for a
/// note name and clipped the lowest of them.
pub const CONTROL_PADDING: Vec2 = Vec2::new(4.0, 2.0);

/// The narrowest egui may lay a control out at, and the one size here that
/// is egui's own rather than this list's: only the controls that size
/// themselves see it (a menu item, a dropdown), since everything in the top
/// bar and a lane row is placed into a rect of its own.
pub const CONTROL_MIN_WIDTH: f32 = 40.0;

/// Padding inside the top bar and a lane row.
pub const PANEL_PADDING: f32 = 6.0;

/// The master volume / switch / `+ lane` strip across the top: one
/// [`CONTROL_HEIGHT`] control with [`PANEL_PADDING`] above and below it, so
/// the bar is as tall as a lane row and no taller.
pub const TOP_BAR_HEIGHT: f32 = CONTROL_HEIGHT + 2.0 * PANEL_PADDING;

/// Space between controls inside a panel.
pub const ITEM_SPACING: f32 = 4.0;

/// The top bar's three controls, in the order the bar draws them.
///
/// Widths rather than weights, for the reason the lane row's fields are
/// widths: the bar is read next to the list under it, and a control that
/// grew with its own text would move the two beside it whenever the switch
/// lit or the volume read "-60 dB" instead of "0 dB". Each holds its whole
/// label once [`CONTROL_PADDING`] has taken its share, and the three of them
/// together fit the narrowest window the host will allow
/// ([`crate::editor::MIN_WIDTH`], which is one lane row wide).
///
/// The master volume's is the whole control, since the rail holds the value
/// inside it: wide enough for a coarse drag and for the longest figure it
/// draws ("-60.00 dB"), and no wider — the bar's three controls leave the
/// rest of the window to the status beside them.
pub const TOP_BAR_VOLUME_WIDTH: f32 = 96.0;
/// The "bitstep Sample pad" switch, wide enough for its whole name: the
/// switch says what it does to every lane in the window, so it is spelled
/// out rather than abbreviated or hidden behind an icon.
pub const TOP_BAR_SWITCH_WIDTH: f32 = 132.0;
/// The `+ lane` button.
pub const TOP_BAR_ADD_WIDTH: f32 = 56.0;

/// A lane row's fields, in the order the row draws them: grip, colour dot,
/// audition button, note, name, sample, volume, pan, choke group,
/// one-shot/gate.
///
/// Widths rather than weights: every lane's row has to line its columns up
/// with every other's. Four of these are bitstep's lane-header widths
/// unchanged — grip, colour dot, note, name — as are the volume field and
/// the square toggle. Each holds the longest thing it has to show once
/// [`CONTROL_PADDING`] has taken its share (the lowest note name, "-60
/// dB"), because every one of these fields truncates rather than wraps.
pub const HEADER_GRIP_WIDTH: f32 = 10.0;
pub const HEADER_DOT_WIDTH: f32 = 12.0;
pub const HEADER_KEY_WIDTH: f32 = 38.0;
pub const HEADER_NAME_WIDTH: f32 = 64.0;
/// The lane volume field, wide enough for "-60 dB".
pub const HEADER_VOLUME_WIDTH: f32 = 48.0;
/// The lane pan field, wide enough for the widest position it shows
/// ("L100"). Chopdrop's own: bitstep pans steps, not lanes.
pub const HEADER_PAN_WIDTH: f32 = 44.0;
/// The choke-group field: a group number, 0 for none, up to
/// [`crate::voices::CHOKE_GROUPS`]. Chopdrop's own, and narrower than the
/// others because two digits is the most it ever holds.
pub const HEADER_CHOKE_WIDTH: f32 = 34.0;
/// One square-ish toggle — chopdrop's one-shot/gate switch, where bitstep has
/// its Mute and Solo.
pub const HEADER_TOGGLE_WIDTH: f32 = 19.0;
/// The audition button, right after the colour dot: wider than a toggle so
/// it is an easy target, and up front where the eye starts a row (the
/// user's, 2026-11-16).
pub const HEADER_AUDITION_WIDTH: f32 = 40.0;

/// The sample column: the loaded file's name, or the "drop a sample here"
/// hint, or the lane's error message.
///
/// The only field of a row that is chopdrop's alone, and the only one that is
/// given whatever width is left over — a file name is the longest text in the
/// window and the one most worth reading. This is the least it may be
/// squeezed to before the window itself stops shrinking
/// ([`crate::editor::MIN_WIDTH`]): enough for a short name ("kick.wav")
/// rather than for every name, since the slot truncates like the rest.
pub const HEADER_SAMPLE_MIN_WIDTH: f32 = 96.0;

/// The room the sample column keeps at its right end for the mark that
/// stands for a lane's note about its sample
/// ([`crate::editor::status::NOTE_MARK`]).
///
/// Taken off the column before the name is laid out, so that a long name is
/// truncated short of the mark instead of drawn under it. One glyph and the
/// gap before it, which is why this is not a field width of its own: the
/// mark lives inside the sample column's own box.
pub const SAMPLE_MARK_WIDTH: f32 = 14.0;

/// The strip a vertical scroll bar takes out of the lane list's width once
/// the list is long enough to need one — egui's own
/// [`egui::style::ScrollStyle::thin`] figure, which [`apply`] installs.
///
/// Named here so that the window's minimum width can allow for it
/// ([`crate::editor::MIN_WIDTH`]) without reaching into a style that is only
/// built when a context exists.
pub const SCROLL_BAR_STRIP: f32 = 6.0;

/// Space between a lane row's fields. Tighter than [`ITEM_SPACING`]: ten
/// controls share one row, and they belong to one lane rather than reading as
/// ten separate things.
pub const HEADER_FIELD_GAP: f32 = 3.0;

/// The lane's colour dot in its row, which doubles as its activity light.
pub const LANE_DOT_RADIUS: f32 = 5.0;

/// One dot of a lane row's drag grip ([`GRIP`]).
pub const GRIP_DOT_RADIUS: f32 = 1.4;

/// The triangle on a row's audition button, drawn rather than typed: egui
/// bundles no font that is certain to carry a play glyph, and a missing one
/// would come out as a blank box on the one control whose meaning is its
/// shape.
pub const PLAY_GLYPH: Vec2 = Vec2::new(11.0, 14.0);

/// Anything drawn by hand: the activity dot's box, the drop highlight, a
/// waveform later.
pub const CELL_CORNER: CornerRadius = CornerRadius::same(3);

/// Buttons and other panel controls.
pub const CONTROL_CORNER: CornerRadius = CornerRadius::same(4);

/// Hairlines: outlines, separators.
pub const HAIRLINE: f32 = 1.0;

/// How wide the outline around a drop target is drawn — the row a dragged
/// sample would land on, or the space below the list that would gain lanes.
///
/// Twice a [`HAIRLINE`], because it says where a file is about to go and has
/// to be seen from wherever on the window the pointer is, unlike the
/// hairlines that merely separate things.
pub const DROP_OUTLINE_WIDTH: f32 = 2.0;

/// The caret in the fields the editor types into — a lane's rename box and
/// its note field. Two points, as egui's own, because a hairline caret is
/// lost in the text it sits between.
pub const TEXT_CURSOR_WIDTH: f32 = 2.0;

/// The size the empty-state hint is drawn at — the one text in the editor
/// that is not a widget's, shown when there are no lanes at all. A little
/// above the body text, because it is all there is to read.
pub const HINT_SIZE: f32 = 15.0;

// ---- the lane palette ----

/// [`LANE_PALETTE`] as `egui` colours, in the same order — index *n* here is
/// the colour lane *n* of a fresh instance was given.
pub const LANE_SWATCHES: [Color32; LANE_PALETTE.len()] = {
    let mut swatches = [Color32::BLACK; LANE_PALETTE.len()];
    let mut index = 0;
    while index < LANE_PALETTE.len() {
        swatches[index] = rgb(LANE_PALETTE[index]);
        index += 1;
    }
    swatches
};

/// The bits of a stored lane colour that are the colour — see [`rgb`].
///
/// Named here because the context menu compares a lane's colour against the
/// palette to light the swatch it came from, and it has to mask exactly what
/// the painter reads.
pub const RGB_MASK: u32 = 0x00FF_FFFF;

/// One colour swatch in a lane's context menu. Square, and large enough to
/// hit without the ten of them outgrowing the menu's width.
pub const SWATCH_SIZE: f32 = 18.0;

/// The ring drawn around a colour swatch: [`ACCENT`] on the one the lane is
/// wearing, [`TEXT_DIM`] on the one under the pointer
/// (`editor/menu.rs`).
///
/// Two points, not the [`HAIRLINE`] a field's edge gets: the ring has to
/// read against ten saturated squares rather than against the panel's grey,
/// and it is the only thing in the menu that says which colour is already
/// the lane's.
pub const SWATCH_RING_WIDTH: f32 = 2.0;

/// A stored `0xRRGGBB` lane colour as an opaque `egui` colour.
///
/// Only the low 24 bits are read: a snapshot from a future version (or a
/// hostile one) that sets the top byte must not come out as a different
/// colour, let alone a transparent one.
pub const fn rgb(colour: u32) -> Color32 {
    let [_, r, g, b] = colour.to_be_bytes();
    Color32::from_rgb(r, g, b)
}

/// The palette colour for a lane position as the `0xRRGGBB` the model
/// stores, for handing to the model's `add_lane` / `set_colour` — cycling
/// once past the end of the palette, since there may be more lanes than
/// colours. On screen it is [`rgb`] of this, which is [`LANE_SWATCHES`] at
/// the same position.
pub fn lane_swatch_rgb(index: usize) -> u32 {
    LANE_PALETTE[index % LANE_PALETTE.len()]
}

// ---- lane colours ----

/// Pushes a colour towards white — a lane's dot while it is playing.
pub fn brighten(colour: Color32, amount: f32) -> Color32 {
    colour.lerp_to_gamma(Color32::WHITE, amount.clamp(0.0, 1.0))
}

/// Takes a colour down without tinting it.
pub fn dim(colour: Color32, factor: f32) -> Color32 {
    colour.gamma_multiply(factor.clamp(0.0, 1.0))
}

/// A lane's colour dot, `fade` of the way through its activity light: 1.0 the
/// moment the lane is struck, 0.0 once the light has gone out and the dot is
/// its plain colour again.
///
/// The curve `fade` follows over the ~150 ms is
/// [`crate::editor::activity::fade_at`]'s; the two ends of it are here, with
/// [`ACTIVITY_BRIGHTEN`], so that the light is the lane's own colour
/// brightened rather than a colour of its own.
pub fn activity_dot(lane: Color32, fade: f32) -> Color32 {
    brighten(lane, ACTIVITY_BRIGHTEN * fade.clamp(0.0, 1.0))
}

/// The outline drawn around what a dragged sample would land on.
pub fn drop_target_stroke() -> Stroke {
    Stroke::new(DROP_OUTLINE_WIDTH, ACCENT)
}

// ---- egui's own styling ----

/// The [`Visuals`] the editor's widgets inherit: egui's dark theme with this
/// module's greys and accent substituted, so stock widgets (buttons, combo
/// boxes, menus) match the hand-drawn parts without being styled one by one.
pub fn visuals() -> Visuals {
    let mut visuals = Visuals::dark();

    visuals.panel_fill = PANEL;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = WINDOW;
    visuals.faint_bg_color = CELL_SHADED;
    visuals.window_stroke = Stroke::new(HAIRLINE, GRID_LINE);
    visuals.window_corner_radius = CONTROL_CORNER;
    visuals.menu_corner_radius = CONTROL_CORNER;
    visuals.window_shadow = SHADOW;
    visuals.popup_shadow = SHADOW;
    visuals.hyperlink_color = ACCENT;
    visuals.warn_fg_color = WARN;
    visuals.text_cursor.stroke = Stroke::new(TEXT_CURSOR_WIDTH, ACCENT);
    visuals.selection.bg_fill = dim(ACCENT, 0.55);
    // A selected control — a lit one-shot/gate toggle, the bitstep switch —
    // keeps its text at full brightness over that fill.
    visuals.selection.stroke = Stroke::new(HAIRLINE, TEXT);

    for (widget, fill) in [
        (&mut visuals.widgets.noninteractive, PANEL),
        (&mut visuals.widgets.inactive, CONTROL),
        (&mut visuals.widgets.hovered, CONTROL_HOVER),
        (&mut visuals.widgets.active, CONTROL_ACTIVE),
        (&mut visuals.widgets.open, CONTROL_HOVER),
    ] {
        widget.bg_fill = fill;
        widget.weak_bg_fill = fill;
        widget.bg_stroke = Stroke::new(HAIRLINE, GRID_LINE);
        widget.corner_radius = CONTROL_CORNER;
        widget.fg_stroke = Stroke::new(HAIRLINE, TEXT);
        // Widgets growing under the pointer would pull one lane row's fields
        // out of line with the next one's.
        widget.expansion = 0.0;
    }
    // Everything above is a control's own text. This is the rest: a label
    // beside a drag value, a heading inside a menu, a lane's sample name.
    // egui takes a *non-interactive* widget's foreground as the colour for
    // all of it, so this one line is the editor's whole text hierarchy — and
    // the reason `override_text_color` is left `None`, since a value there
    // would flatten the two back into one tone.
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(HAIRLINE, TEXT_DIM);

    visuals
}

/// Installs [`visuals`] and this module's spacing on a context. Called once
/// per window, from the editor's `build`.
///
/// The plugin is dark whatever the desktop is doing, so the theme is pinned
/// and *both* of egui's styles are written: were the host or the OS to flip
/// the preference mid-session, the light style it switched to would otherwise
/// be egui's stock one.
pub fn apply(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.all_styles_mut(|style| {
        style.visuals = visuals();

        // The spacing egui lays out *with*, as against the geometry this
        // module hands the lane rows: the rows and the top bar are placed
        // rect by rect, but a menu and a dropdown lay themselves out, and
        // they have to come out at the same rhythm as the rest of the window
        // rather than at egui's stock one.
        let spacing = &mut style.spacing;
        spacing.item_spacing = egui::vec2(ITEM_SPACING, ITEM_SPACING);
        spacing.window_margin = egui::Margin::same(PANEL_PADDING as i8);
        spacing.menu_margin = egui::Margin::same(PANEL_PADDING as i8);
        spacing.menu_spacing = ITEM_SPACING;
        spacing.button_padding = CONTROL_PADDING;
        spacing.icon_spacing = ITEM_SPACING;
        spacing.interact_size = egui::vec2(CONTROL_MIN_WIDTH, CONTROL_HEIGHT);
        // Thin scroll bars rather than egui's stock floating ones, which
        // hover *over* what they scroll: a bar that overlapped the audition
        // button at the end of a row would take the click meant for it.
        // These reserve their own strip, so the lane list simply gives the
        // sample column six points less once a ninth lane makes the list
        // scroll (`editor/lanes.rs`).
        spacing.scroll = egui::style::ScrollStyle::thin();
        debug_assert_eq!(
            spacing.scroll.allocated_width(),
            SCROLL_BAR_STRIP,
            "the width the lane list leaves for the bar is not the width egui takes"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The palette is `state`'s, converted — not a second list — and it keeps
    /// handing out colours past its end so a tenth lane is not black.
    #[test]
    fn the_palette_mirrors_state_and_cycles() {
        assert_eq!(LANE_SWATCHES.len(), LANE_PALETTE.len());
        for (index, &colour) in LANE_PALETTE.iter().enumerate() {
            let expected =
                Color32::from_rgb((colour >> 16) as u8, (colour >> 8) as u8, colour as u8);
            assert_eq!(LANE_SWATCHES[index], expected, "swatch {index}");
            assert_eq!(rgb(lane_swatch_rgb(index)), expected, "drawn {index}");
            assert_eq!(lane_swatch_rgb(index), colour, "rgb {index}");
        }

        let len = LANE_SWATCHES.len();
        for index in 0..len {
            let first = lane_swatch_rgb(index);
            assert_eq!(lane_swatch_rgb(index + len), first, "wrap at {index}");
            assert_eq!(lane_swatch_rgb(index + 3 * len), first, "wrap thrice at {index}");
        }

        assert!(LANE_SWATCHES.iter().all(Color32::is_opaque), "lane colours are opaque");
    }

    /// A stored colour reaches the screen as itself, and a stray top byte
    /// (a future version's flag, a corrupt snapshot) changes nothing — which
    /// is exactly [`RGB_MASK`]'s promise to the colour menu, since it decides
    /// which swatch a lane's colour lights by comparing through it.
    #[test]
    fn rgb_reads_only_the_low_twenty_four_bits() {
        assert_eq!(rgb(0x12_34_56), Color32::from_rgb(0x12, 0x34, 0x56));
        assert_eq!(rgb(0xFF_12_34_56), rgb(0x12_34_56));
        assert_eq!(rgb(0), Color32::BLACK);
        assert_eq!(rgb(0xFF_FF_FF), Color32::WHITE);

        for stored in [0x12_34_56, 0xFF_12_34_56, 0xAB_C2_7E_D6] {
            assert_eq!(rgb(stored), rgb(stored & RGB_MASK), "mask and painter disagree");
        }
    }

    /// Clamping, so a caller cannot produce a colour outside the range by
    /// over- or under-shooting.
    #[test]
    fn brighten_and_dim_clamp_their_amounts() {
        let lane = LANE_SWATCHES[4];

        assert_eq!(brighten(lane, 5.0), Color32::WHITE);
        assert_eq!(brighten(lane, -1.0), lane);
        assert_eq!(dim(lane, 5.0), lane);
        assert_eq!(dim(lane, -1.0), lane.gamma_multiply(0.0));
    }

    /// The activity light is the lane's own colour, brightened while the lane
    /// sounds and back to itself once the light has gone out — for every
    /// palette colour, since any lane may be the one that is playing.
    #[test]
    fn the_activity_light_brightens_a_lanes_own_colour() {
        for (index, &swatch) in LANE_SWATCHES.iter().enumerate() {
            let struck = activity_dot(swatch, 1.0);
            let faded = activity_dot(swatch, 0.0);
            let halfway = activity_dot(swatch, 0.5);

            assert_eq!(faded, swatch, "lane {index}'s dot must return to its colour");
            assert_eq!(struck, brighten(swatch, ACTIVITY_BRIGHTEN), "lane {index}");
            assert!(struck.intensity() > faded.intensity(), "lane {index} does not light up");
            assert!(halfway.intensity() > faded.intensity(), "lane {index} mid-fade");
            assert!(struck.intensity() > halfway.intensity(), "lane {index} mid-fade");

            // A fade outside 0..=1 — a clock that jumped, a frame that came
            // late — may not overshoot into white or undershoot into black.
            assert_eq!(activity_dot(swatch, 9.0), struck, "lane {index} overshot");
            assert_eq!(activity_dot(swatch, -9.0), faded, "lane {index} undershot");
            assert!(struck.is_opaque(), "lane {index}");
        }
    }

    /// The drop highlight is the accent at a width that can be seen across
    /// the window, and plainly not one of the hairlines that separate things.
    #[test]
    fn the_drop_target_outline_is_the_accent() {
        let stroke = drop_target_stroke();
        assert_eq!(stroke.color, ACCENT);
        assert_eq!(stroke.width, DROP_OUTLINE_WIDTH);
        assert!(stroke.width > HAIRLINE, "a drop target has to out-read a separator");
    }

    /// A selected row is a grey a control's step above the panel, never the
    /// accent the drop and browse-target outlines wear.
    #[test]
    fn a_selected_row_is_one_control_step_lighter_than_the_panel() {
        assert_eq!(SELECTED_ROW.r() - PANEL.r(), CONTROL.r() - PANEL.r());
        assert_eq!(SELECTED_ROW.r(), SELECTED_ROW.g());
        assert_eq!(SELECTED_ROW.g(), SELECTED_ROW.b());
        assert_ne!(SELECTED_ROW, ACCENT);
    }

    /// The chrome is built out of one control height: a lane row is one of
    /// them, and the top bar is one inside its padding. That is what lines
    /// the two bands of the window up, so it is a sum rather than a number
    /// anyone has to keep in step by hand.
    // The constants are the subject: these asserts are here so a change to
    // one of them fails the suite with its numbers, which a `const` block
    // (clippy's suggestion) could neither format nor report per test.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_chrome_is_built_from_the_control_height() {
        assert!(ROW_HEIGHT > CONTROL_HEIGHT, "a row is one control, a little taller");
        assert!(ROW_HEIGHT < 1.5 * CONTROL_HEIGHT, "and not much: two lines were too big");
        assert_eq!(ROW_PITCH, ROW_HEIGHT + ROW_GAP);
        assert_eq!(TOP_BAR_HEIGHT, CONTROL_HEIGHT + 2.0 * PANEL_PADDING);
        assert!(TOP_BAR_HEIGHT > CONTROL_HEIGHT, "the bar's padding has to fit around it");
        assert!(2.0 * CONTROL_PADDING.y < CONTROL_HEIGHT, "a control is all padding");
        assert!(ROW_GAP > 0.0, "two rows would touch");
        assert!(HEADER_FIELD_GAP < ITEM_SPACING, "a row's fields read as one lane's");
    }

    /// A lane row holds the fields it draws: each is wide enough for its own
    /// text once the control's padding is out of it, the colour dot has room
    /// for the dot, and the sample column — the one field given the width
    /// left over — is the widest of them, being the longest thing to read.
    // The constants are the subject: these asserts are here so a change to
    // one of them fails the suite with its numbers, which a `const` block
    // (clippy's suggestion) could neither format nor report per test.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn every_row_field_has_room_for_what_it_draws() {
        let fields = [
            ("grip", HEADER_GRIP_WIDTH),
            ("dot", HEADER_DOT_WIDTH),
            ("key", HEADER_KEY_WIDTH),
            ("name", HEADER_NAME_WIDTH),
            ("sample", HEADER_SAMPLE_MIN_WIDTH),
            ("volume", HEADER_VOLUME_WIDTH),
            ("pan", HEADER_PAN_WIDTH),
            ("choke", HEADER_CHOKE_WIDTH),
            ("toggle", HEADER_TOGGLE_WIDTH),
            ("audition", HEADER_AUDITION_WIDTH),
        ];
        assert!(fields.iter().all(|&(_, width)| width > 0.0), "every field is drawable");

        // Every field that draws text through a control: a field narrower
        // than its own padding would show none of it.
        for (name, width) in [
            ("key", HEADER_KEY_WIDTH),
            ("name", HEADER_NAME_WIDTH),
            ("sample", HEADER_SAMPLE_MIN_WIDTH),
            ("volume", HEADER_VOLUME_WIDTH),
            ("pan", HEADER_PAN_WIDTH),
            ("choke", HEADER_CHOKE_WIDTH),
            ("toggle", HEADER_TOGGLE_WIDTH),
        ] {
            assert!(width > 2.0 * CONTROL_PADDING.x, "the {name} field is all padding");
        }

        assert!(HEADER_DOT_WIDTH >= 2.0 * LANE_DOT_RADIUS, "the colour dot is clipped");
        assert!(HEADER_GRIP_WIDTH >= 4.0 * GRIP_DOT_RADIUS, "the grip's dots are clipped");
        // The audition button's triangle is drawn inside the box rather than
        // over the hairline around it.
        assert!(PLAY_GLYPH.x < HEADER_AUDITION_WIDTH - 2.0 * HAIRLINE, "the play glyph is clipped");
        assert!(PLAY_GLYPH.y < ROW_HEIGHT - 2.0 * HAIRLINE, "the play glyph is clipped");
        let widest = fields.iter().map(|&(_, width)| width).fold(0.0f32, f32::max);
        assert_eq!(widest, HEADER_SAMPLE_MIN_WIDTH, "the file name is the longest text");
        // Both corners are rounded, and the hand-drawn one less than the
        // controls', so a dot's box inside a row does not read as a button.
        assert!(CELL_CORNER.nw > 0 && CELL_CORNER.nw < CONTROL_CORNER.nw);
    }

    /// The top bar's three controls: each has room for its own label once
    /// the control's padding is out of it. That the three of them *fit* the
    /// narrowest window is checked in `editor::top_bar` instead, over a real
    /// layout pass, since only egui knows how wide a label comes out.
    // The constants are the subject: these asserts are here so a change to
    // one of them fails the suite with its numbers.
    #[allow(clippy::assertions_on_constants)]
    #[test]
    fn the_top_bars_controls_have_room_for_their_labels() {
        for (name, width) in [
            ("volume", TOP_BAR_VOLUME_WIDTH),
            ("switch", TOP_BAR_SWITCH_WIDTH),
            ("add", TOP_BAR_ADD_WIDTH),
        ] {
            assert!(width > 2.0 * CONTROL_PADDING.x, "the {name} control is all padding");
        }
        assert!(TOP_BAR_SWITCH_WIDTH > TOP_BAR_ADD_WIDTH, "the switch has the longest label");
    }

    /// The style a window opens with is this module's, not egui's default
    /// dark theme — the one check that needs a context, and it needs no
    /// display to make one.
    #[test]
    fn the_context_takes_this_palette() {
        let ctx = egui::Context::default();
        apply(&ctx);

        let style = ctx.global_style();
        assert!(style.visuals.dark_mode);
        assert_eq!(style.visuals.panel_fill, PANEL);
        assert_eq!(style.visuals.widgets.inactive.bg_fill, CONTROL);
        assert_eq!(style.visuals.window_shadow, SHADOW);
        assert_eq!(style.visuals.popup_shadow, SHADOW);

        // The text hierarchy: a control says its own text at full
        // brightness, everything else is secondary, and no override is left
        // in place to collapse the two.
        assert_eq!(style.visuals.override_text_color, None);
        assert_eq!(style.visuals.widgets.inactive.fg_stroke.color, TEXT);
        assert_eq!(style.visuals.widgets.hovered.fg_stroke.color, TEXT);
        assert_eq!(style.visuals.selection.stroke.color, TEXT);
        assert_eq!(style.visuals.widgets.noninteractive.fg_stroke.color, TEXT_DIM);
        assert_eq!(style.visuals.text_color(), TEXT_DIM, "a plain label is secondary text");

        // No widget grows under the pointer: one that did would pull a lane
        // row's fields out of line with the next row's.
        for widget in [
            &style.visuals.widgets.noninteractive,
            &style.visuals.widgets.inactive,
            &style.visuals.widgets.hovered,
            &style.visuals.widgets.active,
            &style.visuals.widgets.open,
        ] {
            assert_eq!(widget.expansion, 0.0);
            assert_eq!(widget.corner_radius, CONTROL_CORNER);
        }

        // The spacing a menu or a dropdown lays itself out with.
        assert_eq!(style.spacing.item_spacing, egui::vec2(ITEM_SPACING, ITEM_SPACING));
        assert_eq!(style.spacing.button_padding, CONTROL_PADDING);
        assert_eq!(style.spacing.interact_size.y, CONTROL_HEIGHT);
        assert_eq!(style.spacing.menu_margin, egui::Margin::same(PANEL_PADDING as i8));

        // Light stays dark: a theme flip must not hand the editor egui's
        // stock palette.
        ctx.set_theme(egui::Theme::Light);
        assert_eq!(ctx.global_style().visuals.panel_fill, PANEL);
    }

    /// Every source file whose strings can reach the window: the editor's
    /// own and the pure modules that format text for it (readouts, change
    /// labels, origins, messages).
    const SOURCES: &[(&str, &str)] = &[
        ("browser.rs", include_str!("../browser.rs")),
        ("chop.rs", include_str!("../chop.rs")),
        ("engine.rs", include_str!("../engine.rs")),
        ("lib.rs", include_str!("../lib.rs")),
        ("loader.rs", include_str!("../loader.rs")),
        ("model.rs", include_str!("../model.rs")),
        ("preview.rs", include_str!("../preview.rs")),
        ("render.rs", include_str!("../render.rs")),
        ("sample.rs", include_str!("../sample.rs")),
        ("state.rs", include_str!("../state.rs")),
        ("stretch.rs", include_str!("../stretch.rs")),
        ("time.rs", include_str!("../time.rs")),
        ("voices.rs", include_str!("../voices.rs")),
        ("editor/activity.rs", include_str!("activity.rs")),
        ("editor/browser_panel.rs", include_str!("browser_panel.rs")),
        ("editor/chop_strip.rs", include_str!("chop_strip.rs")),
        ("editor/closing.rs", include_str!("closing.rs")),
        ("editor/dock.rs", include_str!("dock.rs")),
        ("editor/drops.rs", include_str!("drops.rs")),
        ("editor/focus.rs", include_str!("focus.rs")),
        ("editor/history.rs", include_str!("history.rs")),
        ("editor/keys.rs", include_str!("keys.rs")),
        ("editor/lane_panel.rs", include_str!("lane_panel.rs")),
        ("editor/lanes.rs", include_str!("lanes.rs")),
        ("editor/menu.rs", include_str!("menu.rs")),
        ("editor/mod.rs", include_str!("mod.rs")),
        ("editor/notes.rs", include_str!("notes.rs")),
        ("editor/selection.rs", include_str!("selection.rs")),
        ("editor/status.rs", include_str!("status.rs")),
        ("editor/theme.rs", include_str!("theme.rs")),
        ("editor/top_bar.rs", include_str!("top_bar.rs")),
    ];

    /// The characters outside ASCII that the string literals of `source`'s
    /// shipped code (everything before its `#[cfg(test)]`) hold, `\u{…}`
    /// escapes decoded. Comments, char literals and lifetimes are skipped:
    /// a small lexer, enough for this crate's sources.
    fn literal_chars(source: &str) -> Vec<char> {
        let code = source.split("#[cfg(test)]").next().unwrap_or(source);
        let chars: Vec<char> = code.chars().collect();
        let mut found = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            match (chars[i], chars.get(i + 1).copied()) {
                ('/', Some('/')) => {
                    while i < chars.len() && chars[i] != '\n' {
                        i += 1;
                    }
                }
                ('/', Some('*')) => {
                    while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                        i += 1;
                    }
                    i += 2;
                }
                // A char literal ('x', '\n', '\u{2212}'), not a lifetime.
                ('\'', Some(next)) if next == '\\' || chars.get(i + 2) == Some(&'\'') => {
                    i += 2;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                    i += 1;
                }
                ('"', _) => {
                    i += 1;
                    while i < chars.len() && chars[i] != '"' {
                        if chars[i] == '\\' {
                            if chars.get(i + 1) == Some(&'u') {
                                let close = chars[i..].iter().position(|&c| c == '}').unwrap();
                                let hex: String = chars[i + 3..i + close].iter().collect();
                                let code = u32::from_str_radix(&hex, 16).unwrap();
                                found.extend(char::from_u32(code).filter(|c| !c.is_ascii()));
                                i += close;
                            } else {
                                i += 1;
                            }
                        } else if !chars[i].is_ascii() {
                            found.push(chars[i]);
                        }
                        i += 1;
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        found
    }

    /// The lexer finds what it should and nothing it should not.
    #[test]
    fn literal_chars_reads_strings_only() {
        let source = "// a comment \u{2192} →\n\
                      fn f<'a>(x: &'a str) { let c = '→'; let d = '\\u{2192}'; }\n\
                      const A: &str = \"x × y\"; /* — */ const B: &str = \"\\u{2026}\\\"\";\n\
                      #[cfg(test)] const C: &str = \"≈\";";
        assert_eq!(literal_chars(source), ['×', '…']);
    }

    /// Every character a shipped string holds is in egui's bundled fonts —
    /// a missing one draws as a blank box (a lane's "→ 5" output marker did,
    /// 2026-11-16). The proportional family is the one every label uses.
    #[test]
    fn every_shipped_string_has_its_glyphs() {
        let ctx = egui::Context::default();
        apply(&ctx);
        ctx.run_ui(egui::RawInput::default(), |_| {}).drop_without_applying_deltas();
        let font = egui::FontId::proportional(14.0);
        let mut missing = Vec::new();
        for (file, source) in SOURCES {
            for c in literal_chars(source) {
                if !ctx.fonts_mut(|fonts| fonts.has_glyph(&font, c)) {
                    missing.push(format!("{file}: {c:?} (U+{:04X})", u32::from(c)));
                }
            }
        }
        missing.dedup();
        assert!(missing.is_empty(), "no glyph for: {missing:#?}");
    }
}
