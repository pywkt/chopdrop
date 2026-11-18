//! Note names, Bitwig's way: **60 is C3**, and so 36 is C1.
//!
//! A note number is what the model stores, what a project saves and what the
//! host sends; a note *name* is the only form of it the window shows, because
//! a lane answers to the pad Bitwig's note editor calls C1 and nobody reads
//! 36 as that. The convention is Bitwig's own and not the one most MIDI
//! documentation uses — which names 60 "C4", an octave above this one — and
//! that choice is the whole point of the module: the figure in a chopdrop row
//! has to be the figure in the clip the user is looking at beside it, since
//! lining the two up by hand is exactly the mistake a drum rack makes easy.
//!
//! Pure: no egui and no nice-plug, so the names are unit-tested on their own
//! (the purity rule, by the spirit of it — this is a GUI module that happens
//! to need none of the GUI). Both directions live here —
//! [`note_name`] for every note the window shows, [`parse_note_name`] for
//! the one a user types into a field — and the round trip between them is
//! the module's contract: a name chopdrop printed always parses back to the
//! note it was printed from, which is what lets the field be a text box at
//! all. The field itself, with the drag and the wheel on it, is
//! `editor/lanes.rs`.

/// The twelve notes of an octave, in order from C, as the window spells
/// them.
///
/// Sharps only. Flats are a thing a user may type ([`parse_note_name`]) and
/// never a thing chopdrop prints: one spelling per note, so a field that is
/// dragged through an octave reads the same way going up as coming down.
pub const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

/// The octave note 0 sits in: −2.
///
/// The one number that encodes the convention. Octaves step every twelve
/// notes from here, so note 60 lands in octave 60 / 12 − 2 = 3 and is named
/// C3, which is what Bitwig calls it; the same arithmetic puts the lowest
/// note of all at C−2 and the highest at G8.
pub const LOWEST_OCTAVE: i32 = -2;

/// The name of a note, as a lane row shows it: `C1`, `F#1`, `C-2`.
///
/// Total over every `u8`, though only 0–127 can reach it — the writer API
/// clamps a note to the MIDI range on the way in
/// ([`crate::model::Model::set_note`]), and a `u8` cannot be negative. A
/// value above 127 would come out as the name its arithmetic gives, which is
/// a name out of MIDI's range rather than a panic or a blank.
pub fn note_name(note: u8) -> String {
    let octave = note as i32 / NOTE_NAMES.len() as i32 + LOWEST_OCTAVE;
    format!("{}{octave}", NOTE_NAMES[note as usize % NOTE_NAMES.len()])
}

/// The highest note there is: 127, which this convention names G8.
///
/// The top of MIDI's range and so the top of a note field's own range
/// (`editor/lanes.rs`) — the same 127 the model's own `clamp_note` holds a
/// written note down to, kept here as well because a name is parsed into a
/// note *before* any writer sees it and a field that offered G#8 would be
/// offering a note no lane can hold.
pub const HIGHEST_NOTE: u8 = 127;

/// The note a user typed into a field, or `None` for a name no lane could
/// have.
///
/// Case-insensitive, and flats are taken although none is ever printed:
/// `A#1`, `Bb1` and `bB1` are all the one note 46, which is the point — a
/// user who thinks in flats types what they think and reads it back in the
/// window's one spelling. Outer whitespace is ignored, since a name pasted
/// into the box tends to come with some. The octave is the rest of the name
/// and may be negative (`C-2` is note 0).
///
/// Nothing is clamped: a name whose arithmetic lands outside MIDI's range —
/// `G#8`, a semitone above [`HIGHEST_NOTE`] — is refused along with the
/// garbage, because a refusal puts the lane's own note back in the field,
/// where a clamp would quietly set a note the user never asked for.
pub fn parse_note_name(name: &str) -> Option<u8> {
    let mut letters = name.trim().chars();

    let semitone: i32 = match letters.next()?.to_ascii_uppercase() {
        'C' => 0,
        'D' => 2,
        'E' => 4,
        'F' => 5,
        'G' => 7,
        'A' => 9,
        'B' => 11,
        _ => return None,
    };

    // The note name is taken first and the accidental only out of what is
    // left, which is what makes `Bb` two different letters doing two
    // different jobs: the `B` of a note and the `b` of a flat.
    let rest = letters.as_str();
    let (accidental, octave) = if let Some(octave) = rest.strip_prefix('#') {
        (1, octave)
    } else if let Some(octave) = rest.strip_prefix(['b', 'B']) {
        (-1, octave)
    } else {
        (0, rest)
    };

    let octave: i32 = octave.parse().ok()?;
    // Checked, because that octave came off a keyboard and nothing has
    // bounded it yet: `C999999999` is a refusal, not an overflow.
    let note = octave
        .checked_sub(LOWEST_OCTAVE)?
        .checked_mul(NOTE_NAMES.len() as i32)?
        .checked_add(semitone + accidental)?;

    (0..=i32::from(HIGHEST_NOTE)).contains(&note).then_some(note as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two notes Bitwig's convention is known by, and the two ends of
    /// the range. If this test ever has to change, something has moved the
    /// plugin off Bitwig's convention and every lane row in every saved
    /// project now reads an octave out.
    #[test]
    fn sixty_is_c3_and_thirty_six_is_c1() {
        assert_eq!(note_name(60), "C3", "Bitwig's middle C");
        assert_eq!(note_name(36), "C1", "the first default lane's note");
        assert_eq!(note_name(0), "C-2", "the lowest note there is");
        assert_eq!(note_name(127), "G8", "the highest note there is");
    }

    /// The lanes a fresh instance opens with, named. Consecutive semitones
    /// from C1 — the first eight are the pads bitstep sends to by default,
    /// which is why these names are the ones a user sees first.
    #[test]
    fn the_default_lanes_read_as_c1_upwards() {
        let names: Vec<String> = (crate::model::DEFAULT_FIRST_NOTE
            ..crate::model::DEFAULT_FIRST_NOTE + crate::model::DEFAULT_LANE_COUNT as u8)
            .map(note_name)
            .collect();

        assert_eq!(
            names,
            [
                "C1", "C#1", "D1", "D#1", "E1", "F1", "F#1", "G1", "G#1", "A1", "A#1", "B1", "C2",
                "C#2", "D2", "D#2",
            ]
        );
    }

    /// A ninth lane added to those defaults lands on 44 (the next
    /// semitone up), and 44 is a sharp in the next-but-one position — the
    /// one case where the name has both a sharp and an octave the row has
    /// not shown yet.
    #[test]
    fn a_sharp_keeps_its_octave() {
        assert_eq!(note_name(44), "G#1");
        assert_eq!(note_name(61), "C#3");
        assert_eq!(note_name(70), "A#3");
    }

    /// Octaves turn over at C and nowhere else: B2 is the note below C3, not
    /// B3. The off-by-one every note-naming scheme invites.
    #[test]
    fn the_octave_turns_over_at_c() {
        assert_eq!(note_name(59), "B2");
        assert_eq!(note_name(60), "C3");
        assert_eq!(note_name(71), "B3");
        assert_eq!(note_name(72), "C4");
    }

    /// Every note in range has a name of its own, so no two lanes with
    /// different notes can read as the same pad — the property that makes
    /// the field worth showing as a name at all. And every name is a note
    /// name followed by an octave, which is the shape
    /// [`parse_note_name`] reads back.
    #[test]
    fn all_one_hundred_and_twenty_eight_notes_are_named_once() {
        let mut names: Vec<String> = (0..=127u8).map(note_name).collect();
        let count = names.len();

        for name in &names {
            let (letters, octave) =
                name.split_at(name.find(|c: char| c == '-' || c.is_ascii_digit()).unwrap());
            assert!(NOTE_NAMES.contains(&letters), "{name} does not start with a note name");
            let octave: i32 = octave.parse().unwrap_or_else(|_| panic!("{name} has no octave"));
            assert!((LOWEST_OCTAVE..=8).contains(&octave), "{name} is out of octave range");
        }

        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "two notes share a name");
    }

    /// The module's contract: every name the window prints parses back to
    /// the note it was printed from. The field a user types into shows a
    /// name to start with, so a round trip that lost a note somewhere would
    /// mean a field that refuses what it is already displaying.
    #[test]
    fn every_printed_name_parses_back_to_its_own_note() {
        for note in 0..=HIGHEST_NOTE {
            assert_eq!(parse_note_name(&note_name(note)), Some(note), "note {note}");
        }
    }

    /// What a user may type and chopdrop never prints: lower case, flats,
    /// and a name with whitespace around it. Each one is the note the
    /// window would spell some other way.
    #[test]
    fn a_typed_name_may_be_lower_case_flat_or_padded() {
        assert_eq!(parse_note_name("c3"), Some(60), "Bitwig's middle C, typed in a hurry");
        assert_eq!(parse_note_name("f#1"), Some(42));
        assert_eq!(parse_note_name("F#1"), Some(42));
        assert_eq!(parse_note_name("Gb1"), Some(42), "the same black key, spelled flat");
        assert_eq!(parse_note_name("gB1"), Some(42), "the flat is case-insensitive too");

        // The one ambiguous letter: a `B` that is a note and a `b` that is a
        // flat, in either case and in one name.
        assert_eq!(parse_note_name("B1"), Some(47));
        assert_eq!(parse_note_name("Bb1"), Some(46));
        assert_eq!(parse_note_name("bb1"), Some(46));
        assert_eq!(parse_note_name("bB1"), Some(46));
        assert_eq!(parse_note_name("A#1"), Some(46), "which is this note's own spelling");

        assert_eq!(parse_note_name("  C1  "), Some(36));
        assert_eq!(parse_note_name("\tC1\n"), Some(36));
    }

    /// The two ends of the range, and the negative octaves the convention
    /// puts the bottom of MIDI in.
    #[test]
    fn the_ends_of_the_range_parse_and_a_semitone_past_them_does_not() {
        assert_eq!(parse_note_name("C-2"), Some(0), "the lowest note there is");
        assert_eq!(parse_note_name("G8"), Some(HIGHEST_NOTE), "the highest");
        assert_eq!(parse_note_name("c-1"), Some(12));

        assert_eq!(parse_note_name("Cb-2"), None, "a semitone below note 0");
        assert_eq!(parse_note_name("G#8"), None, "a semitone above note 127");
        assert_eq!(parse_note_name("A8"), None);
        assert_eq!(parse_note_name("C9"), None, "a whole octave above the top");
        assert_eq!(parse_note_name("C-3"), None, "and one below the bottom");
    }

    /// Garbage is refused rather than guessed at, which is what leaves the
    /// lane's note alone and the field showing it. Nothing here may panic:
    /// every one of these is something a user can type into the box.
    #[test]
    fn a_name_that_is_not_a_note_is_refused() {
        for garbage in [
            "",
            " ",
            "H3",
            "X#1",
            "C",
            "C#",
            "Cb",
            "3",
            "60",
            "-1",
            "C3x",
            "Cx3",
            "C 3",
            "C3.5",
            "C--2",
            "##3",
            "#3",
            "C3C3",
            "♯C3",
        ] {
            assert_eq!(parse_note_name(garbage), None, "{garbage:?} is not a note name");
        }
    }

    /// An octave no arithmetic can hold is a refusal and not an overflow:
    /// the octave is parsed off a keyboard, so the multiply that turns it
    /// into a note has to survive whatever was typed.
    #[test]
    fn an_absurd_octave_does_not_overflow() {
        for absurd in ["C2147483647", "C-2147483648", "C999999999", "C-999999999", "G#9999"] {
            assert_eq!(parse_note_name(absurd), None, "{absurd:?}");
        }
    }

    /// The note a name may parse to and the note a writer will keep are the
    /// one range: were the field's own ceiling to drift above the model's,
    /// a typed `G#8` would be taken and then silently clamped to G8.
    #[test]
    fn the_fields_ceiling_is_the_models_own() {
        assert_eq!(HIGHEST_NOTE, crate::model::clamp_note(u8::MAX));
        assert_eq!(parse_note_name(&note_name(HIGHEST_NOTE)), Some(HIGHEST_NOTE));
    }
}
