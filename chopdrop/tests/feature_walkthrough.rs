//! The user-journey walkthrough, pinned here as data.
//!
//! The walkthrough is a **manual** protocol — its journeys end in a window on
//! a screen, a kit out of a pair of speakers and a note source in front of
//! the plugin, none of which a test run has — so it is written down the
//! way bitstep writes its own (its `feature_walkthrough.rs`). [`WALKTHROUGH`]
//! is the checklist the user walks: twenty-three journeys in a fixed order
//! (the v1 thirteen, then the v2 ones), each with the steps to take, what
//! must be true at the end of them, and the part only eyes and ears can
//! decide. To walk it beside Bitwig:
//!
//! ```text
//! cargo test -p chopdrop --test feature_walkthrough -- --nocapture the_protocol
//! ```
//!
//! Each journey also carries the half of itself that **is** reachable from
//! here, as a function driving it against the real code — writer API,
//! loader worker, drop router, key dispatcher, persist path and engine,
//! headless. That is what makes this a test and not a document: a journey
//! whose container half has rotted fails the suite. The drivers are
//! deliberately **thin**, since every contract under them is tested where
//! it lives (`voices.rs`, `engine.rs`, `model.rs`,
//! `tests/state_round_trip.rs`); each asserts the one thing the user is
//! about to check with their own senses, so a failure names the journey and
//! the module tests say why.
//!
//! A gap the user finds walking this is noted in a line naming the journey's
//! [`Journey::id`]. The file reads in four parts: [`Journey`] and the
//! [`WALKTHROUGH`] table, the shared fixtures and helpers, the drivers in the
//! table's order — one per journey, named after it — and the four `#[test]`s
//! over all of it at the end, where
//! [`every_journey_still_describes_the_plugin`] pairs each journey with its
//! driver.

use std::path::{Path, PathBuf};
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use egui::{pos2, vec2, Event, Key, Modifiers, Rect};

use chopdrop::chop::{self, ChopOrigin, View};
use chopdrop::editor::activity::{self, Lights};
use chopdrop::editor::chop_strip::{self, StripOutput};
use chopdrop::editor::drops::{self, LaneRect, Landing};
use chopdrop::editor::focus::KeyOwner;
use chopdrop::editor::selection::Selection;
use chopdrop::editor::{history, lane_panel, lanes, menu, status, theme, top_bar};
use chopdrop::engine::{
    Engine, EngineEvent, NoteTarget, Outputs, TimedEvent, OUTPUTS, PAD_ROOT_KEY,
};
use chopdrop::loader::{Loader, Source};
use chopdrop::model::{
    lock_model, LaneMode, Model, PublishedCell, SharedModel, CHOKE_NONE, DEFAULT_LANE_COUNT,
};
use chopdrop::preview::{PreviewChannel, Previewer, Region};
use chopdrop::sample::{encode_wav24, SampleData};
use chopdrop::state::{Snapshot, LANE_PALETTE};
use chopdrop::time;
use chopdrop::voices::step_for;

// ---- The protocol ------------------------------------------------------

/// One journey of the walkthrough: what the user does, what must come of
/// it, and what this file can already say about it.
struct Journey {
    /// A stable handle, for a note about the journey to name.
    id: &'static str,
    /// What the journey is, in a line.
    title: &'static str,
    /// What the user does, in order.
    steps: &'static [&'static str],
    /// What must be true at the end — what a pass is decided on.
    expect: &'static [&'static str],
    /// The part no test here can reach: what the user is looking at,
    /// listening to or driving the plugin from — why this is manual at all.
    eyes: &'static str,
    /// The claim the journey's driver makes headless, in a line.
    pinned: &'static str,
}

/// The journeys of the walkthrough, v1's and then v2's, in their fixed
/// order — which is also a sensible order to walk them
/// in: a kit is loaded, played, shaped, saved and reopened, then spread over
/// the outputs.
const WALKTHROUGH: &[Journey] = &[
    Journey {
        id: "drop-on-row",
        title: "Drop a sample from Bitwig's browser onto a lane row",
        steps: &[
            "Open a fresh chopdrop: sixteen empty lanes, Lane 1 on C1, each saying \
             \"drop a sample here\".",
            "Find a one-shot in Bitwig's browser and drag it over the window.",
            "Hold it over Lane 2's row, then let go.",
        ],
        expect: &[
            "While the file is in the air, Lane 2's row — and only that row — is outlined.",
            "Lane 2 shows \"loading…\" and then the file's name.",
            "Lane 2 keeps its own name, note, colour, volume and pan: only the sample changed.",
            "No lane was added, and the other fifteen are still empty.",
        ],
        eyes: "the outline that follows the pointer, and Bitwig's browser as the drag source",
        pinned: "a drop routed to a row loads that row's sample and adds no lane",
    },
    Journey {
        id: "drop-below-list",
        title: "Drop a sample into the space below the last lane",
        steps: &[
            "Drag a sample over the window again, this time to the empty space under Lane 16.",
            "Let go.",
        ],
        expect: &[
            "The space below the rows is outlined while the file is in the air, not any row.",
            "A seventeenth lane appears at the end, named after the file without its extension.",
            "It is on the next note up from Lane 16, in the next palette colour, and it plays.",
            "The sixteen lanes above it are untouched.",
        ],
        eyes: "where the outline is drawn, and that the new row scrolls into view",
        pinned: "a drop below the rows adds a lane named after the file and loads it",
    },
    Journey {
        id: "drop-many",
        title: "Drop several samples at once",
        steps: &[
            "Select three samples in Bitwig's browser and drag them over Lane 1 together.",
            "Let go.",
        ],
        expect: &[
            "The first file lands on Lane 1, replacing whatever was there.",
            "The other two add lanes at the end, each named after its own file.",
            "All three rows show their file names, and all three play.",
            "A drop that would take the list past 128 lanes loads the files that fit, and the \
             top bar says for a few seconds how many did not.",
        ],
        eyes: "the order the files land in, which is the order the browser handed them over",
        pinned: "the first path takes the row and every path after it adds a lane",
    },
    Journey {
        id: "audition",
        title: "Play a lane from its own row",
        steps: &[
            "Click the big play button at the right-hand end of a loaded lane's row (pick a \
             long sample), and click it again while it plays.",
            "Click that lane's sample name, then press S, and S again while it plays.",
            "Click the play triangle on a lane with nothing in it.",
        ],
        expect: &[
            "Each row is one line, with the play button right after the colour dot, wider \
             than the mode toggle and easy to hit.",
            "The loaded lane sounds at its own note — not at the file's recorded pitch — and \
             its triangle turns into a square while it plays. The second click stops it and \
             the triangle comes back; it also comes back on its own when the sample ends.",
            "Its dot lights up and fades out.",
            "The sample click selects the lane and shows its settings in the side panel, not \
             the browser. S plays it from the start, and S again stops it.",
            "The empty lane's triangle is dim, says so on hover, and does nothing.",
        ],
        eyes: "that the click is heard, and that the dim triangle reads as unavailable",
        pinned: "an auditioned lane starts one voice at the lane's note and counts the hit",
    },
    Journey {
        id: "play-from-bitstep",
        title: "Play the kit from bitstep and watch the lights",
        steps: &[
            "Put bitstep in front of chopdrop on the same track and build a pattern on the \
             notes the lanes answer to.",
            "Put two lanes on one note.",
            "Start the transport and take your hands off the mouse.",
            "Drag a lane's volume and pan while the pattern runs.",
        ],
        expect: &[
            "Every hit sounds the lane on that note, in time, not quantised to the buffer.",
            "Two lanes on one note both sound: lanes layer.",
            "The dots light on the beat and fade over about a sixth of a second, with the \
             window untouched.",
            "Volume and pan moved mid-pattern are heard from the next block, with no click.",
        ],
        eyes: "the timing against Bitwig's metronome, and that an idle window still redraws",
        pinned: "a note-on at an offset starts every lane on that key there, and counts the hits \
                 the lights read",
    },
    Journey {
        id: "choke-pair",
        title: "An open and a closed hat in one choke group",
        steps: &[
            "Load an open hat on one lane and a closed hat on another.",
            "Set both lanes' choke fields to the same group — 1, say.",
            "Play the open hat, then the closed hat while the open one is still ringing.",
            "Set one of them back to 0 and play the pair again.",
        ],
        expect: &[
            "The closed hat cuts the open one off, from the moment it is struck.",
            "The cut is a short fade, not a click.",
            "With one of them out of the group, the two ring together.",
        ],
        eyes: "that the cut sounds like a hi-hat closing rather than like a dropout",
        pinned: "a note-on in a choke group fades the group's other voice and sounds itself",
    },
    Journey {
        id: "gate-vs-one-shot",
        title: "Gate against one-shot",
        steps: &[
            "Load the same long sample on two lanes.",
            "Leave one lane on 1 (one-shot) and click the other to G (gate).",
            "In the gate lane's panel, set its release to 0 ms.",
            "Play a short note on each from a keyboard or from bitstep.",
        ],
        expect: &[
            "The one-shot lane plays the whole sample however short the note is.",
            "The gate lane stops when the note does, with a short fade rather than a click.",
            "Set the release back to 300 ms (a new lane's default) and play again: the gate \
             lane now fades out over about a third of a second after the note ends.",
            "A gate lane auditioned from its own play button plays to the end: there is no \
             note to end it.",
        ],
        eyes: "the two tails, which is the whole difference between the modes",
        pinned: "a note-off fades a gate lane's voice and leaves a one-shot's playing",
    },
    Journey {
        id: "rename-reorder-recolour",
        title: "Rename, reorder and recolour a lane",
        steps: &[
            "Double-click a lane's name, type a new one and press Enter.",
            "Drag the grip at the left of a row up past the row above it.",
            "Right-click the row, pick \"colour\" and click a swatch.",
            "Press Ctrl+Z three times.",
        ],
        expect: &[
            "The name sticks, and an empty name is refused rather than taken.",
            "The row lands where it was dropped and the list redraws in the new order.",
            "The lane's dot and row take the new colour.",
            "Each gesture is one undo step: three presses put all three back.",
        ],
        eyes: "that the dragged row follows the pointer and the swatches read as a palette",
        pinned: "rename, reorder and recolour are three committed gestures and three undo steps",
    },
    Journey {
        id: "save-close-reopen",
        title: "Save, close, move the sample files, reopen",
        steps: &[
            "Load two lanes from files, move their settings off the defaults and turn the \
             \"bitstep Sample pad\" switch on.",
            "Save the Bitwig project and close it.",
            "Move or delete the sample files the lanes were loaded from.",
            "Reopen the project — with the plugin window closed — and play the kit.",
        ],
        expect: &[
            "Both lanes still sound: the project carries the sample files inside it.",
            "Names, notes, colours, volumes, pans, choke groups, modes and order all came back.",
            "The switch came back on.",
            "Nothing in the window says anything went wrong, and Ctrl+Z undoes nothing: \
             opening a project is not an edit.",
        ],
        eyes: "that Bitwig's own save and reload are what carried the state, and the project \
               file's size",
        pinned: "a saved project reopened with its files gone restores the lanes and renders \
                 the same audio",
    },
    Journey {
        id: "reload-from-file",
        title: "Reload a lane from its file",
        steps: &[
            "Load a lane from a file, then edit that file in an audio editor and save it \
             over itself.",
            "Right-click the lane and pick \"reload from file\".",
        ],
        expect: &[
            "The lane plays the new audio, from the same path it already had.",
            "Hovering the menu's top line shows that path.",
            "A lane with nothing in it offers no reload.",
            "A lane cut in the chop editor shows reload greyed out; hovering it points to \
             \"open source in editor\".",
            "A file that has gone leaves a message on the lane and keeps the old sample off it.",
        ],
        eyes: "that the new take is what is heard, and the path in the menu's hover",
        pinned: "reloading a lane's own path re-reads whatever is at it now",
    },
    Journey {
        id: "delete-undo",
        title: "Delete a lane and undo it",
        steps: &[
            "Right-click a loaded lane, click \"delete\", then click \"delete — click again\".",
            "Press Ctrl+Z.",
            "Press Ctrl+Shift+Z.",
            "Start renaming a lane, type something and press Ctrl+Z without leaving the box.",
        ],
        expect: &[
            "The first click arms the delete and the second one takes the lane.",
            "Ctrl+Z brings it back in its old place, with its sample and all its settings.",
            "Ctrl+Shift+Z takes it away again.",
            "Ctrl+Z inside the name box undoes the typing, not the lanes.",
        ],
        eyes: "the two-click confirmation, and that a note sounding from the deleted lane rings \
               on to its end",
        pinned: "a deleted lane's undo step restores the lane, its sample and its settings",
    },
    Journey {
        id: "sample-pad-switch",
        title: "The switch on, with bitstep's Sample pad on",
        steps: &[
            "Load one lane and put it on a note well away from C3.",
            "Click its play button, then turn chopdrop's \"bitstep Sample pad\" switch (on in \
             a new chopdrop) off and click it again.",
            "Play it with both switches off: chopdrop's and bitstep's own Sample pad.",
            "Turn chopdrop's switch on and play it again from a plain keyboard.",
            "Turn bitstep's Sample pad on as well and play the pattern.",
        ],
        expect: &[
            "The play button plays the file at its own pitch with the switch on and off.",
            "Both off: the lane plays its file at the file's own pitch, whatever note it is on.",
            "chopdrop's switch on, plain keyboard: the kit keytracks from C3, like a Drum \
             Machine pad.",
            "Both on: every lane is back at its file's own pitch — the two cancel.",
            "Flipping the switch while a note rings does not bend that note.",
        ],
        eyes: "the pitch, which is the only thing the switch changes",
        pinned: "the switch keytracks from C3 and cancels bitstep's own `60 − key` tuning",
    },
    Journey {
        id: "browse-and-preview",
        title: "Browse for a sample, preview it and load it",
        steps: &[
            "Click an empty lane's \"drop a sample here\".",
            "Click \"packages\" or \"music\", open a folder of drums, and arrow down through it.",
            "Untick auto-preview, select another file and click play.",
            "Press Enter on a file, then click another lane's sample and load one there too.",
            "Press Escape.",
        ],
        expect: &[
            "The browser opens on the right, the window grows wider, and the lane is outlined.",
            "With auto-preview on, each file plays as it is selected; with it off, only play \
             plays it, and play turns into stop while it sounds.",
            "Enter loads the file into the outlined lane; the browser stays open, and clicking \
             another lane's sample moves the outline there.",
            "Escape closes the browser, stops any preview and gives the window its width back.",
            "Reopened later, the browser remembers the auto-preview setting and the folder.",
        ],
        eyes: "the preview heard through the track, and the host resizing the window",
        pinned: "a selected file previews through the engine and a load lands in the target lane",
    },
    Journey {
        id: "lane-panel-multi",
        title: "Change several lanes at once from the lane panel",
        steps: &[
            "Load three lanes and give the second one a different attack in the lane tab.",
            "Click Lane 1's name, then Shift+click Lane 3's name.",
            "Read the lane tab, then rest the pointer on its output dropdown.",
            "Pick \"Out 3\" from the dropdown and play the three lanes.",
            "Press Ctrl+Z once.",
        ],
        expect: &[
            "All three rows are highlighted and the lane tab's header says \"3 lanes\".",
            "The attack field shows a dash, since the three lanes disagree; output says Out 1.",
            "The dropdown's hover says Out 2\u{2013}16 are silent in Bitwig until you click Add \
             Missing Chains on the device.",
            "All three rows show \"out 3\" after their sample names, and Lane 4 does not.",
            "After Add Missing Chains, the three lanes play through the device's Out 3 chain \
             and no longer through the main one.",
            "One Ctrl+Z puts all three back on Out 1.",
        ],
        eyes: "the highlighted rows, the hover, and which chain's meter moves in Bitwig",
        pinned: "one panel edit over a selection moves every selected lane to the output, \
                 renders it there and undoes as one step",
    },
    Journey {
        id: "outputs-own-chain",
        title: "Give each lane its own output and its own effects",
        steps: &[
            "Load three lanes. Click Lane 1's name and Ctrl+click Lane 2's.",
            "Right-click Lane 1 and pick \"give each its own output\".",
            "Right-click Lane 3, which is not selected, and pick the same item.",
            "Click Add Missing Chains on the chopdrop device, and put an effect on Out 3's chain.",
            "Play all three lanes.",
            "Add lanes until there are more than sixteen, select them all and pick the item \
             again.",
        ],
        expect: &[
            "Lane 1 shows \"out 2\" and Lane 2 \"out 3\"; Lane 3 stays on Out 1 until \
             its own menu moves it alone, to Out 4.",
            "Each lane is heard through its own chain, and the effect on Out 3 changes Lane 2 \
             only.",
            "With every output from Out 2 to Out 16 taken, the top bar says for a few seconds, \
             in the warning colour, how many lanes kept their output.",
            "One Ctrl+Z undoes each pick as a whole.",
        ],
        eyes: "Bitwig's chains and the effect on one of them, which is the point of outputs",
        pinned: "the menu gives each selected lane the lowest free output, each renders only \
                 there, and a full house is counted for the bar",
    },
    Journey {
        id: "outputs-old-project",
        title: "Open a project saved before chopdrop had 16 outputs",
        steps: &[
            "Open a Bitwig project saved with the earlier, single-output chopdrop.",
            "Play the kit as it was.",
            "Look at the chopdrop device's outputs; if Bitwig shows only one, reload the device.",
        ],
        expect: &[
            "The project opens with every lane, sample and setting it was saved with.",
            "Every lane plays through Out 1 and sounds as it did, chokes and gate tails \
             included; no lane cuts itself off on a repeated hit.",
            "The device offers sixteen outputs, possibly only after a reload; write down \
             which it was.",
        ],
        eyes: "how Bitwig treats a device whose port count grew between saving and opening",
        pinned: "a v1 project restores onto Out 1 with v1 behaviour and sounds the same whether \
                 the host gives it one pair or sixteen",
    },
    Journey {
        id: "chop-open-source",
        title: "Open a song in the chop editor",
        steps: &[
            "Find a song or a long loop in Bitwig's browser and drag it over the strip above \
             the lanes.",
            "Hold it over the strip, then let go.",
            "Rest the pointer on the file name in the strip's header.",
        ],
        expect: &[
            "While the file is in the air, the strip — and not a lane row — is outlined.",
            "The strip says \"loading…\" and then draws the whole song, fitted to its width, \
             with the overview along the top and a time ruler under it.",
            "The header names the file, and its hover shows the full path.",
            "No lane was added or changed, and nothing was selected yet.",
            "Ctrl+Z undoes nothing: opening a source is not an edit of the kit.",
        ],
        eyes: "the waveform drawn as the song really looks, and how long a long file takes",
        pinned: "a file let go over the strip opens as the source, fitted, with no lane touched \
                 and no undo step",
    },
    Journey {
        id: "chop-select-loop",
        title: "Pick a section by ear and loop it",
        steps: &[
            "Click on the strip's waveform to give it the keyboard, then drag across a phrase.",
            "Press P, and let the selection play to its end.",
            "Press L, then P again, and let it go round a few times.",
            "While it loops, press the right arrow a few times, then press L again.",
            "Zoom in on the selection's start with Ctrl+wheel, click inside the selection a \
             little right of its start, and drag. Then click inside it near its end.",
            "Click on the waveform a little outside the selection, on either side.",
            "Press Ctrl+Z three times.",
            "Double-click away from the selection, holding the button down on the second \
             click, and drag.",
            "Click a lane row to give the lanes the keyboard, then press A, and A again.",
        ],
        expect: &[
            "The drag leaves a highlighted selection whose edges sit on zero crossings; the \
             readout under the ruler gives its start, end and length.",
            "P plays exactly the selection once, through the track, with no click at either \
             end, and a hairline follows it.",
            "With loop lit, it repeats seamlessly until stopped; Space still starts and stops \
             Bitwig's transport.",
            "Each right arrow makes the loop 10 ms longer from the next pass on; with loop off \
             again, it plays to the new end and stops.",
            "The click moves the start to where you clicked and the drag carries it on; the \
             end stays put. The second click moves the end the same way.",
            "The nearer edge stretches out to the click; the selection grows rather than \
             starting again, and nothing plays.",
            "Each Ctrl+Z puts the selection back one click or drag at a time.",
            "The old selection disappears and a new one is drawn from the double-click.",
            "A plays the selection and A again stops it, wherever the keyboard was; the play \
             button reads stop while it sounds.",
        ],
        eyes: "whether the loop seam is musical and clickless, which only ears can say",
        pinned: "a drag selects, P plays exactly that region, L loops it through the engine, \
                 and an edge nudge reaches the live region",
    },
    Journey {
        id: "chop-to-lane",
        title: "Turn a selection into lanes",
        steps: &[
            "With a selection in a song called Eye, press Enter (or click add lane).",
            "Play the new lane's note from the keyboard or bitstep.",
            "Press N, then hold Ctrl, press inside the selection and drag it onto Lane 2's \
             row.",
            "Press Ctrl+Z three times.",
            "Drag the selection's \"drag to a lane\" bar onto Lane 3's row. Then press \
             inside the selection and drag straight down onto Lane 4's row.",
        ],
        expect: &[
            "Enter adds a lane at the end called \"Eye 1\", on the next note and colour, \
             playing exactly the selection.",
            "N moves the selection on by its own length.",
            "While the chop is carried, the row under the pointer is outlined; let go, Lane 2 \
             plays the new section and keeps its name, note, colour and settings.",
            "The first Ctrl+Z empties Lane 2 again, the second moves the selection back \
             where N found it, and the third removes \"Eye 1\".",
            "Both drags carry the chop like the Ctrl drag: Lanes 3 and 4 take it, and the \
             edge pressed near snaps back once the pointer leaves the waveform.",
        ],
        eyes: "the feel of the drag out of the strip, and that the chop sounds as it previewed",
        pinned: "Enter adds a chop lane named after the source, a chop Ctrl+dragged onto a \
                 row replaces its sample, and each is one undo step",
    },
    Journey {
        id: "chop-reopen-source",
        title: "Find where a chop came from, after reopening the project",
        steps: &[
            "Cut a chop onto a lane, close the strip's source with \u{00d7}, save the project \
             and close it.",
            "Reopen the project, rest the pointer on the chop lane's sample name, and click \
             the lane's name to see the lane tab.",
            "Right-click the chop lane and pick \"open source in editor\".",
            "Move the song out of its folder, reopen the project and pick the item again.",
        ],
        expect: &[
            "The chop lane plays after reopening, even before its source is opened.",
            "The hover and the lane tab both say where it was cut from, as \"from Eye.wav, \
             0:00.625\u{2013}0:00.875\".",
            "The menu offers the item only on chop lanes; picking it opens the song with the \
             chop's section selected and in view.",
            "With the song gone, the strip says \"source not found\" with its name, and the \
             chop lane still plays.",
        ],
        eyes: "the hover and the strip framing the section, in Bitwig's own project reload",
        pinned: "a saved chop lane plays from the project, names its origin, and reopens its \
                 source with the cut selected — or says the source is gone",
    },
    Journey {
        id: "tempo-while-stopped",
        title: "Change the tempo with the transport stopped",
        steps: &[
            "With Bitwig's transport stopped at 120 BPM, load a two-second loop onto a lane.",
            "Click the lane's name, set time to Repitch and length to 1 bar, and read the \
             pitch field.",
            "Still stopped, set Bitwig's tempo to 90 BPM and read the pitch field again.",
            "Click the lane's play triangle.",
            "Open the same loop in the strip, drag across the whole of it and read the readout.",
        ],
        expect: &[
            "At 120 BPM the loop is exactly a bar: the pitch field shows 0.00 st.",
            "At 90 BPM, without pressing play, the field shows \u{2212}4.98 st.",
            "The audition plays the loop slower and lower, lasting a bar at 90 BPM.",
            "The readout ends \"at 90 BPM\", not 120.",
            "If nothing follows the tempo until the transport starts, write down that Bitwig \
             sends no tempo while stopped: the last tempo it sent stays in use.",
        ],
        eyes: "whether Bitwig hands over its tempo while stopped, which only Bitwig can say",
        pinned: "a tempo the host sends while stopped reaches the panel, the audition and the \
                 readout, and a block without one keeps it",
    },
    Journey {
        id: "repitch-to-bar",
        title: "Fit a loop to one bar by repitching it",
        steps: &[
            "Set Bitwig's tempo to 125 BPM and load a two-second loop onto Lane 1.",
            "Click Lane 1's name, set time to Repitch and open the length dropdown.",
            "Pick 1 bar, then rest the pointer on the pitch field.",
            "Play the lane against Bitwig's metronome, then press Ctrl+Z once.",
        ],
        expect: &[
            "The dropdown marks the nearest length: \"\u{2248} 1 bar \u{2212}4.0 %\".",
            "Lane 1's row shows \"1 bar\" after its sample name.",
            "The pitch field is greyed out at +0.71 st, and its hover says the length sets it.",
            "The loop plays a little faster and higher, and ends exactly on the next bar line.",
            "One Ctrl+Z puts the length back to as cut, and the loop plays as it was.",
        ],
        eyes: "that the loop lands on the bar line by ear, against Bitwig's own metronome",
        pinned: "a Repitch lane set to 1 bar plays for exactly one bar at the host's tempo, \
                 shows the pitch it costs, and undoes in one step",
    },
    Journey {
        id: "stretch-to-bar",
        title: "Fit a loop to one bar by stretching it, keeping its pitch",
        steps: &[
            "Set Bitwig's tempo to 125 BPM and load a two-second loop onto Lane 1.",
            "Click Lane 1's name, set time to Stretch, pick 1 bar from the length dropdown and \
             watch the panel's bottom line.",
            "Play the lane against Bitwig's metronome.",
            "Set the pitch field to +7 st and play the lane again.",
            "Close the plugin window, set Bitwig's tempo to 100 BPM and play the lane twice, a \
             moment apart.",
        ],
        expect: &[
            "A grey \"rendering\u{2026}\" line shows for a moment after the pick, then goes.",
            "Lane 1's row shows \"1 bar\" after its sample name, and the pitch field stays \
             live at 0.00 st.",
            "The loop keeps its own pitch and ends exactly on the next bar line.",
            "At +7 st the loop is a fifth higher and still exactly a bar long.",
            "At 100 BPM the loop lasts a bar from the very first play, window closed; its \
             pitch is only right once the new stretch lands.",
        ],
        eyes: "how the stretch sounds, and that it lands on the bar line by ear",
        pinned: "a Stretch lane set to 1 bar renders in the background to exactly one bar, \
                 keeps its pitch or shifts it alone, and follows a tempo change headless",
    },
];

// ---- What the drivers are driven against -------------------------------

/// The host rate every block here renders at. Neither fixture's own (both
/// are 44 100 Hz), so every voice plays through the interpolator.
const RATE: f64 = 48_000.0;

/// One block, the size Bitwig's default buffer is.
const BLOCK: usize = 512;

/// The three checked-in fixtures (`chopdrop/tests/fixtures/README.md`): a
/// 400-frame mono one-shot, under half a block at the host's rate, and two
/// of 4 410 frames — stereo and mono, in two more formats — that are still
/// ringing blocks later. The hat pair, and the take dropped on a row.
const SHORT: &[u8] = include_bytes!("fixtures/tiny_mono.flac");
const LONG_STEREO: &[u8] = include_bytes!("fixtures/tiny_stereo.ogg");
const LONG_MONO: &[u8] = include_bytes!("fixtures/tiny_mono.mp3");

/// A held Ctrl, as X11 hands one to egui: both bits, which is what
/// `editor/keys.rs` reads (`editor/history.rs`'s own tests do the same).
const CTRL: Modifiers = Modifiers::CTRL.plus(Modifiers::COMMAND);

/// A loaded instance without a plugin: the lanes behind the writer mutex
/// and the worker that fills them, which is every thread a journey touches.
///
/// `Chopdrop` holds exactly this pair and keeps both private, and
/// [`Loader::finish`] — the only way to know a decode has landed without
/// polling — consumes the loader, so the drivers build the pair themselves
/// (`tests/state_round_trip.rs` explains the same split at more length).
struct Instance {
    /// The lanes every writer goes through.
    model: SharedModel,
    /// The worker a drop, a reload and a restore all queue onto.
    loader: Loader,
}

impl Instance {
    /// The default eight lanes, nothing loaded.
    fn fresh() -> Instance {
        Instance::over(Arc::new(Mutex::new(Model::new())))
    }

    /// A fresh worker over lanes that already exist.
    fn over(model: SharedModel) -> Instance {
        let loader = Loader::new(Arc::clone(&model));
        Instance { model, loader }
    }

    /// The writer guard a GUI gesture takes.
    fn write(&self) -> MutexGuard<'_, Model> {
        lock_model(&self.model)
    }

    /// The published lanes the audio thread reads.
    fn published(&self) -> Arc<PublishedCell> {
        Arc::clone(self.write().published())
    }

    /// Lane ids, in display order.
    fn ids(&self) -> Vec<u32> {
        self.write().lanes().iter().map(|lane| lane.id).collect()
    }

    /// Wait for every queued decode to land, then hand the instance back
    /// with a fresh worker — no sleep and no poll.
    fn settle(self) -> Instance {
        let Instance { model, loader } = self;
        loader.finish();
        Instance::over(model)
    }

    /// Load a file onto a lane and wait for it: what a drop on that row and
    /// the menu's "reload from file" both come down to.
    fn load(self, lane: u32, path: &Path) -> Instance {
        {
            let mut model = self.write();
            let source = Source::File(path.to_path_buf());
            assert!(self.loader.queue(&mut model, lane, source), "the worker took the job");
        }

        let instance = self.settle();
        {
            let model = instance.write();
            let loaded = model.lane(lane).expect("the lane is still there");
            assert!(loaded.sample.is_some(), "lane {lane} decoded nothing: {:?}", loaded.error);
            assert!(!loaded.loading, "and the row has stopped saying \"loading…\"");
        }
        instance
    }
}

/// Hands every [`ScratchFile`] a directory of its own: the drivers run in
/// one process and several write the same fixture to disk.
static SCRATCH_FILES: AtomicU64 = AtomicU64::new(0);

/// A file holding these bytes under a directory of its own in the temp
/// directory, both removed when the guard drops (`loader.rs`'s and
/// `stress.rs`'s tests do the same with a unique file *name*).
///
/// A directory and not a name prefix, because the name is part of what
/// these journeys check: a lane labels itself from the file's own name
/// (`editor/drops.rs`), so "BD 606.flac" must still be called that on disk.
struct ScratchFile {
    /// The directory holding it, removed with the file.
    dir: PathBuf,
    /// Where it is, which is what a drop carries and a lane remembers.
    path: PathBuf,
}

impl ScratchFile {
    fn new(name: &str, bytes: &[u8]) -> ScratchFile {
        let unique = SCRATCH_FILES.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir()
            .join(format!("chopdrop-walkthrough-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("the temp directory is writable");
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("the temp directory is writable");
        ScratchFile { dir, path }
    }
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A loaded kit: these files written to the temp directory and loaded onto
/// the first lanes, with the guards handed back — a journey that moves or
/// rewrites a file needs them, and dropping one deletes its file.
fn kit(files: &[(&str, &[u8])]) -> (Instance, Vec<u32>, Vec<ScratchFile>) {
    let scratch: Vec<ScratchFile> =
        files.iter().map(|(name, bytes)| ScratchFile::new(name, bytes)).collect();
    let mut instance = Instance::fresh();
    let ids = instance.ids();
    for (lane, file) in ids.iter().zip(&scratch) {
        instance = instance.load(*lane, &file.path);
    }
    (instance, ids, scratch)
}

/// A render loop at 64 frames a block, so a fade of a few milliseconds
/// ([`chopdrop::voices::FADE_MS`]) can be watched in flight: a default-sized
/// buffer starts *and* finishes one inside a single call, leaving nothing
/// to look at. A host at 64 frames is a host on a low latency setting.
struct ShortBlocks {
    /// The left channel of the block last rendered.
    left: [f32; 64],
    /// And the right, which no journey here reads: they ask about voices.
    right: [f32; 64],
}

impl ShortBlocks {
    fn new() -> ShortBlocks {
        ShortBlocks { left: [0.0; 64], right: [0.0; 64] }
    }

    /// One block, reporting its loudest frame.
    #[must_use]
    fn block(&mut self, engine: &mut Engine, cell: &PublishedCell, events: &[TimedEvent]) -> f32 {
        engine.run_block(
            cell,
            events,
            &mut Outputs::main(&mut self.left, &mut self.right),
            RATE,
            1.0,
        );
        peak(&self.left)
    }
}

/// The rows of a lane list as the editor drew them, which is all
/// [`drops::route`] needs: ids and rectangles. Under the top bar and at the
/// theme's own pitch, so a driver points where a user does.
fn drawn_list(ids: &[u32]) -> (Vec<LaneRect>, Rect) {
    let list = Rect::from_min_size(pos2(0.0, top_bar::BAR_HEIGHT), vec2(600.0, 400.0));
    let rows = ids
        .iter()
        .enumerate()
        .map(|(n, id)| LaneRect {
            id: *id,
            rect: Rect::from_min_size(
                pos2(list.left(), list.top() + n as f32 * theme::ROW_PITCH),
                vec2(list.width(), theme::ROW_HEIGHT),
            ),
        })
        .collect();
    (rows, list)
}

/// A note as a host sends one, matched by channel and key — and its
/// note-on and note-off at `timing` frames into a block.
fn target(key: u8) -> NoteTarget {
    NoteTarget { channel: 0, key, voice_id: None }
}

fn note_on(timing: u32, key: u8) -> TimedEvent {
    TimedEvent::new(timing, EngineEvent::NoteOn { note: target(key), velocity: 0.8 })
}

fn note_off(timing: u32, key: u8) -> TimedEvent {
    TimedEvent::new(timing, EngineEvent::NoteOff { note: target(key) })
}

/// A key press, as egui-baseview hands one to the editor.
fn press(key: Key, modifiers: Modifiers) -> Event {
    Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers }
}

/// The loudest frame of a block: how a driver says "this sounded".
fn peak(block: &[f32]) -> f32 {
    block.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()))
}

/// Whether this lane's voice is on its way out — cut by a choke group or
/// released by a note-off. Panics if the lane has no voice at all: every
/// caller has just struck it, and a voice already gone is a different
/// failure from one that is not fading.
fn fading(engine: &Engine, lane: u32) -> bool {
    engine
        .pool()
        .voices()
        .find(|voice| voice.lane_id == lane)
        .map(|voice| voice.is_fading())
        .unwrap_or_else(|| panic!("lane {lane} has no voice in the pool"))
}

/// Renders `blocks` blocks with the events `script` gives each, both
/// channels interleaved — so a comparison is of the stereo field, not of
/// one side of it.
fn render(
    cell: &PublishedCell,
    engine: &mut Engine,
    blocks: usize,
    script: impl Fn(usize) -> Vec<TimedEvent>,
) -> Vec<f32> {
    let mut out = Vec::with_capacity(blocks * BLOCK * 2);
    for block in 0..blocks {
        let mut left = [0.0f32; BLOCK];
        let mut right = [0.0f32; BLOCK];
        engine.run_block(
            cell,
            &script(block),
            &mut Outputs::main(&mut left, &mut right),
            RATE,
            1.0,
        );
        out.extend(left.iter().zip(&right).flat_map(|(left, right)| [*left, *right]));
    }
    out
}

/// One block with the host handing over the first `provided` of the
/// plugin's [`OUTPUTS`] stereo pairs — 1 for a host that kept the old
/// single-output device, all of them for one that took the new layout —
/// reporting each pair's loudest frame (0 for a pair not provided).
fn block_on_outputs(
    cell: &PublishedCell,
    engine: &mut Engine,
    events: &[TimedEvent],
    provided: usize,
) -> Vec<f32> {
    let mut buffers = vec![(vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]); OUTPUTS];
    let mut outputs = Outputs { pairs: [const { None }; OUTPUTS] };
    for (slot, (left, right)) in outputs.pairs.iter_mut().zip(&mut buffers).take(provided) {
        *slot = Some((left.as_mut_slice(), right.as_mut_slice()));
    }
    engine.run_block(cell, events, &mut outputs, RATE, 1.0);
    buffers.iter().map(|(left, right)| peak(left).max(peak(right))).collect()
}

/// The pairs a block was heard on, by index (0 = Out 1).
fn heard_on(peaks: &[f32]) -> Vec<usize> {
    peaks.iter().enumerate().filter(|(_, peak)| **peak > 0.0).map(|(n, _)| n).collect()
}

/// The script the save-and-reopen journey renders on both sides of the
/// round trip: a note-on per loaded lane, and the gate lane's note-off two
/// blocks later so its release is in the comparison too.
fn saved_kit_script(block: usize) -> Vec<TimedEvent> {
    match block {
        0 => vec![note_on(0, 36), note_on(128, 38)],
        2 => vec![note_off(0, 38)],
        _ => Vec::new(),
    }
}

/// The song the chop journeys cut from, at the host's rate so a frame of
/// it is a frame of output: two seconds of a 220 Hz sine, never silent for
/// more than a frame, as the 24-bit WAV the plugin itself writes.
const SONG_RATE: u32 = 48_000;
const SONG_FRAMES: u64 = 96_000;

fn song() -> Vec<u8> {
    let step = std::f32::consts::TAU * 220.0 / SONG_RATE as f32;
    let frames = (0..SONG_FRAMES).map(|n| 0.5 * (n as f32 * step).sin()).collect();
    encode_wav24(&SampleData { rate: SONG_RATE, channels: 1, frames })
}

impl Instance {
    /// Open a file as the chop editor's source and wait for it to land.
    fn open_song(self, path: &Path) -> Instance {
        {
            let mut model = self.write();
            assert!(self.loader.open_source(&mut model, path), "the worker took the job");
            assert_eq!(chop_strip::body(&model), chop_strip::Body::Loading);
        }
        let instance = self.settle();
        assert_eq!(chop_strip::body(&instance.write()), chop_strip::Body::Loaded);
        instance
    }
}

/// The window the chop journeys draw: the strip as `editor/mod.rs` lays it
/// out — across the top, owning the keyboard — and the lane rows under it,
/// with a chop let go over them landed as the window lands one.
const STRIP_WINDOW: egui::Vec2 = vec2(600.0, 800.0);

struct Strip {
    ctx: egui::Context,
    /// What the strip plays through; its channel is the engine's too.
    previewer: Previewer,
    /// The strip as last drawn.
    out: Option<StripOutput>,
    /// The rows as last drawn, under the strip.
    rows: Vec<LaneRect>,
    /// The lane the last frame's chop landed on.
    landed: Option<u32>,
}

impl Strip {
    fn new(model: &Model) -> Strip {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let handle = model.handle().clone();
        let previewer = Previewer::new(Arc::new(PreviewChannel::new(&handle)), handle);
        Strip { ctx, previewer, out: None, rows: Vec::new(), landed: None }
    }

    fn channel(&self) -> &Arc<PreviewChannel> {
        self.previewer.channel()
    }

    /// One frame of the window with these input events.
    fn frame(&mut self, model: &mut Model, events: Vec<Event>) -> StripOutput {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), STRIP_WINDOW)),
            events,
            ..egui::RawInput::default()
        };
        let previewer = &self.previewer;
        let (mut out, mut rows, mut landed) = (None, Vec::new(), None);
        let output = self.ctx.run_ui(input, |ui| {
            let strip = chop_strip::show(ui, model, previewer, KeyOwner::Chop);
            let list = ui.available_rect_before_wrap();
            rows = model
                .lanes()
                .iter()
                .enumerate()
                .map(|(n, lane)| LaneRect {
                    id: lane.id,
                    rect: Rect::from_min_size(
                        pos2(list.left(), list.top() + n as f32 * theme::ROW_PITCH),
                        vec2(list.width(), theme::ROW_HEIGHT),
                    ),
                })
                .collect();
            drops::outline_chop(ui, &rows, list, strip.rect);
            landed = drops::land_chop(ui, model, &rows, list, strip.rect);
            out = Some(strip);
        });
        output.drop_without_applying_deltas();
        (self.out, self.rows, self.landed) = (out, rows, landed);
        out.expect("the strip is drawn")
    }

    /// One key pressed, without modifiers.
    fn key(&mut self, model: &mut Model, key: Key) -> StripOutput {
        self.frame(model, vec![press(key, Modifiers::NONE)])
    }

    /// Where the main waveform view was last drawn.
    fn main_view(&self) -> Rect {
        let canvas = self.out.and_then(|out| out.canvas).expect("a source is open");
        chop_strip::canvas_layout(canvas).main
    }

    /// The x of a source frame in the main view.
    fn x_of(&self, model: &Model, frame: u64) -> f32 {
        self.main_view().min.x + model.chop().view.px_of(frame)
    }

    /// Press at `from`, move to `to` in steps, let go there; answers the
    /// release frame's output.
    fn drag(&mut self, model: &mut Model, from: egui::Pos2, to: egui::Pos2) -> StripOutput {
        self.drag_with(model, from, to, Modifiers::NONE)
    }

    /// [`drag`](Self::drag) with `modifiers` held throughout.
    fn drag_with(
        &mut self,
        model: &mut Model,
        from: egui::Pos2,
        to: egui::Pos2,
        modifiers: Modifiers,
    ) -> StripOutput {
        let button = |pos, pressed| Event::PointerButton {
            pos,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers,
        };
        self.frame(model, vec![Event::ModifiersChanged(modifiers)]);
        self.frame(model, vec![Event::PointerMoved(from)]);
        self.frame(model, vec![button(from, true)]);
        for step in 1..=5 {
            let at = from + (to - from) * (step as f32 / 5.0);
            self.frame(model, vec![Event::PointerMoved(at)]);
        }
        let out = self.frame(model, vec![button(to, false)]);
        self.frame(model, vec![Event::ModifiersChanged(Modifiers::NONE)]);
        out
    }
}

/// Renders 64-frame blocks through an engine with the strip's preview
/// voice, answering the loudest frame of the last `tail` of them.
fn preview_blocks(
    engine: &mut Engine,
    cell: &PublishedCell,
    blocks: &mut ShortBlocks,
    count: usize,
    tail: usize,
) -> f32 {
    let mut loudest = 0.0f32;
    for n in 0..count {
        let peak = blocks.block(engine, cell, &[]);
        if n + tail >= count {
            loudest = loudest.max(peak);
        }
    }
    loudest
}

/// The 64-frame blocks `frames` of preview take, rounded up.
fn blocks_for(frames: u64) -> usize {
    frames.div_ceil(64) as usize
}

// ---- The journeys, headless --------------------------------------------

/// `drop-on-row`: the row takes the sample, nothing else about it changes.
fn drop_on_a_row() {
    let file = ScratchFile::new("BD 606.flac", SHORT);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let (rows, list) = drawn_list(&ids);

    // The pointer is in the middle of the second row — and the lane the
    // drop writes is the lane the outline promised.
    let landing = drops::route(Some(rows[1].rect.center()), &rows, list, Rect::NOTHING);
    assert_eq!(landing, Landing::Row { id: ids[1], outline: rows[1].rect });

    {
        let mut model = instance.write();
        drops::apply(&mut model, &instance.loader, landing, slice::from_ref(&file.path));
    }
    let instance = instance.settle();

    let model = instance.write();
    assert_eq!(model.lanes().len(), ids.len(), "a drop on a row adds no lane");

    let lane = model.lane(ids[1]).expect("the row the drop landed on");
    let sample = lane.sample.as_ref().expect("the file decoded");
    assert_eq!(sample.file_name, "BD 606.flac", "the row shows the file it was given");
    assert_eq!(
        status::sample_status(lane),
        status::SampleStatus::Loaded { file_name: "BD 606.flac", note: None }
    );
    assert_eq!(lane.name, "Lane 2", "the row keeps its own name");
    assert_eq!(lane.note, 37, "and its own note");
    assert_eq!(lane.colour, LANE_PALETTE[1], "and its own colour");
    assert!(model.lane(ids[0]).expect("still there").sample.is_none(), "the row above is empty");
}

/// `drop-below-list`: the space below the rows adds a lane, named after the
/// file.
fn drop_below_the_list() {
    let file = ScratchFile::new("Rim 808.ogg", LONG_STEREO);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let (rows, list) = drawn_list(&ids);

    let below = pos2(list.center().x, rows[ids.len() - 1].rect.bottom() + theme::ROW_PITCH);
    let landing = drops::route(Some(below), &rows, list, Rect::NOTHING);
    assert!(matches!(landing, Landing::NewLanes { .. }), "{landing:?} is not the space below");

    {
        let mut model = instance.write();
        drops::apply(&mut model, &instance.loader, landing, slice::from_ref(&file.path));
    }
    let instance = instance.settle();

    let model = instance.write();
    assert_eq!(model.lanes().len(), ids.len() + 1, "one lane for the one file");

    let added = model.lanes().last().expect("the lane the drop added");
    assert_eq!(added.name, "Rim 808", "named after the file, without its extension");
    assert_eq!(added.note, 52, "the next note up from the sixteen defaults");
    assert_eq!(added.colour, LANE_PALETTE[6], "and the next colour along");
    let data = &added.sample.as_ref().expect("the file decoded").data;
    assert_eq!(data.channels, 2, "the stereo file, in two channels");
}

/// `drop-many`: the first file takes the row and the rest add lanes, in the
/// order the browser handed them over.
fn drop_several_at_once() {
    let kick = ScratchFile::new("kick.flac", SHORT);
    let snare = ScratchFile::new("snare.ogg", LONG_STEREO);
    let hat = ScratchFile::new("hat.mp3", LONG_MONO);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let (rows, list) = drawn_list(&ids);

    let landing = drops::route(Some(rows[0].rect.center()), &rows, list, Rect::NOTHING);
    let paths = [kick.path.clone(), snare.path.clone(), hat.path.clone()];
    {
        let mut model = instance.write();
        drops::apply(&mut model, &instance.loader, landing, &paths);
    }
    let instance = instance.settle();

    let model = instance.write();
    assert_eq!(model.lanes().len(), ids.len() + 2, "one row taken, two lanes added");

    let first = model.lane(ids[0]).expect("the row the drop was aimed at");
    assert_eq!(first.sample.as_ref().expect("the first file").file_name, "kick.flac");
    assert_eq!(first.name, "Lane 1", "the row it landed on kept its name");

    let added = &model.lanes()[ids.len()..];
    let names: Vec<&str> = added.iter().map(|lane| lane.name.as_str()).collect();
    assert_eq!(names, ["snare", "hat"], "in the order the browser handed them over");
    for lane in added {
        let data = &lane.sample.as_ref().expect("every file decoded").data;
        assert!(data.frame_count() > 0, "{} has no audio", lane.name);
    }
}

/// `audition`: one voice in that lane at the lane's own note, and the hit
/// counted for the dot.
fn audition_a_lane() {
    let (instance, ids, _files) = kit(&[("clap.ogg", LONG_STEREO)]);
    assert!(instance.write().set_note(ids[0], 48));

    let cell = instance.published();
    let mut engine = Engine::new();

    // The click: the row pushes its lane onto the ring the engine drains at
    // the top of the next block. The empty lane is pushed too — its own
    // button is dim and pushes nothing (`editor/lanes.rs`), and the engine
    // starts nothing for it either, which is the half pinned here.
    assert!(engine.auditions().push(ids[0]));
    assert!(engine.auditions().push(ids[1]));

    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    engine.run_block(&cell, &[], &mut Outputs::main(&mut left, &mut right), RATE, 1.0);

    assert_eq!(engine.auditions().pending(), 0, "the ring is drained at the top of the block");
    assert_eq!(engine.pool().active_count(), 1, "the loaded lane sounded, the empty one did not");

    let voice = engine.pool().voices().next().expect("the audition started a voice");
    assert_eq!(voice.lane_id, ids[0]);
    assert_eq!(voice.note_key, 48, "at the lane's own note");
    assert!(peak(&left) > 0.0, "and it is audible");

    let published = cell.get();
    let lane = published.lane(ids[0]).expect("a loaded lane is published");
    assert_eq!(lane.activity(), 1, "the hit the dot lights for");
}

/// `play-from-bitstep`: a note-on at an offset starts every lane on that
/// key *there*, and the lights follow the counters it bumps.
fn play_from_a_note_source() {
    let (instance, ids, _files) = kit(&[("kick.ogg", LONG_STEREO), ("layer.mp3", LONG_MONO)]);
    {
        let mut model = instance.write();
        // Two lanes on one note: they layer.
        assert!(model.set_note(ids[0], 36));
        assert!(model.set_note(ids[1], 36));
    }

    let cell = instance.published();
    let mut engine = Engine::new();
    let mut lights = Lights::default();

    // A window open and dark since before the first hit: a lane it sees
    // for the first time starts dark.
    let counters = |published: &chopdrop::model::PublishedLanes| -> Vec<(u32, u32)> {
        published.lanes().iter().map(|lane| (lane.id, lane.activity())).collect()
    };
    lights.observe(counters(&cell.get()).into_iter(), 0.0);
    assert!(!lights.fading(), "nothing has played yet");

    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    engine.run_block(
        &cell,
        &[note_on(64, 36)],
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        1.0,
    );

    assert_eq!(engine.pool().active_count(), 2, "one key, both lanes on it: they layer");
    assert!(engine.needs_a_frame(), "the audio thread asks the window for a frame");
    assert!(left[..64].iter().all(|sample| *sample == 0.0), "silent before the note's own frame");
    assert!(peak(&left[64..]) > 0.0, "and struck on it, not at the top of the block");

    let published = cell.get();
    lights.observe(counters(&published).into_iter(), 1.0);
    for id in [ids[0], ids[1]] {
        assert_eq!(lights.fade(id), 1.0, "lane {id}'s dot is lit by its own hit");
    }
    assert!(lights.fading(), "and the window asks for the frames the fade needs");

    // Past the fade, not exactly on its end: that is a float's width
    // either side of out ([`activity::fade_at`]).
    lights.observe(counters(&published).into_iter(), 1.0 + 2.0 * activity::FADE);
    assert_eq!(lights.fade(ids[0]), 0.0, "out again a fade later");
    assert!(!lights.fading(), "and the window goes back to sleep");
}

/// `choke-pair`: a note-on in a choke group fades the group's other voice
/// and sounds itself.
fn choke_a_pair() {
    let files = [("hat-open.ogg", LONG_STEREO), ("hat-closed.mp3", LONG_MONO)];
    let (instance, ids, _files) = kit(&files);
    {
        let mut model = instance.write();
        assert!(model.set_note(ids[0], 46));
        assert!(model.set_note(ids[1], 42));
        assert!(model.set_choke(ids[0], 1));
        assert!(model.set_choke(ids[1], 1));
        assert_eq!(model.lane(ids[2]).expect("an empty lane").choke, CHOKE_NONE);
    }

    let cell = instance.published();
    let mut engine = Engine::new();
    let mut blocks = ShortBlocks::new();

    assert!(blocks.block(&mut engine, &cell, &[note_on(0, 46)]) > 0.0, "the open hat rings");
    assert!(!fading(&engine, ids[0]), "with nothing cutting it");

    // The closed hat, struck while the open one is still ringing.
    assert!(blocks.block(&mut engine, &cell, &[note_on(0, 42)]) > 0.0);
    assert!(fading(&engine, ids[0]), "the open hat is being cut");
    assert!(!fading(&engine, ids[1]), "and the closed hat is what cut it");

    // Four more blocks: longer than the fade, nowhere near either sample's end.
    for _ in 0..4 {
        assert!(blocks.block(&mut engine, &cell, &[]) > 0.0, "the closed hat plays on");
    }
    assert_eq!(engine.pool().active_count(), 1, "the open hat has gone");
    assert_eq!(engine.pool().voices().next().expect("the closed hat").lane_id, ids[1]);

    // And the host is told the cut voice ended, so it can stop tracking it.
    let ended: Vec<u8> = engine.pool().terminated().iter().map(|voice| voice.note_key).collect();
    assert_eq!(ended, [46], "the open hat's note ended; the closed hat's has not");
}

/// `gate-vs-one-shot`: a note-off fades a gate lane's voice and leaves a
/// one-shot's playing.
fn gate_against_one_shot() {
    // The same audio on both lanes, so the tails differ by mode alone.
    let (instance, ids, _files) = kit(&[("pad.ogg", LONG_STEREO), ("pad.ogg", LONG_STEREO)]);
    {
        let mut model = instance.write();
        assert!(model.set_note(ids[0], 36));
        assert!(model.set_note(ids[1], 38));
        assert!(model.set_mode(ids[0], LaneMode::Gate));
        assert!(model.set_mode(ids[1], LaneMode::OneShot));
        // Release 0: the shortest gate fade, `FADE_MS`.
        assert!(model.set_release_ms(ids[0], 0.0));
    }

    let cell = instance.published();
    let mut engine = Engine::new();
    let mut blocks = ShortBlocks::new();

    // A short note on each: both keys down, and up again a block later —
    // well before either sample has run out.
    let on = [note_on(0, 36), note_on(0, 38)];
    let off = [note_off(0, 36), note_off(0, 38)];
    assert!(blocks.block(&mut engine, &cell, &on) > 0.0, "both lanes sound");
    assert!(blocks.block(&mut engine, &cell, &off) > 0.0);

    assert!(fading(&engine, ids[0]), "the gate lane's note ended with the key");
    assert!(!fading(&engine, ids[1]), "the one-shot ignored the note-off");

    for _ in 0..4 {
        assert!(blocks.block(&mut engine, &cell, &[]) > 0.0, "the one-shot plays on");
    }
    assert_eq!(engine.pool().active_count(), 1, "the gate lane's fade is over");

    let playing = engine.pool().voices().next().expect("the one-shot");
    assert_eq!(playing.lane_id, ids[1]);
    assert_eq!(playing.lane_mode, LaneMode::OneShot, "and it is the mode that kept it");
}

/// `rename-reorder-recolour`: three gestures, three commits, three undo
/// steps.
fn rename_reorder_recolour() {
    let instance = Instance::fresh();
    let ids = instance.ids();
    let mut model = instance.write();

    // Each gesture ends in the commit the editor makes for it, which is
    // what makes it one Ctrl+Z.
    assert!(model.rename(ids[1], "Snare 909"));
    assert!(model.commit());
    assert!(model.reorder(1, 0));
    assert!(model.commit());
    assert!(model.set_colour(ids[1], LANE_PALETTE[4]));
    assert!(model.commit());

    let moved = &model.lanes()[0];
    assert_eq!(moved.id, ids[1], "the dragged row is now the first");
    assert_eq!(moved.name, "Snare 909");
    assert_eq!(moved.colour, LANE_PALETTE[4]);
    assert_eq!(moved.note, 37, "and the audio follows the lane, not the row it is in");
    assert_eq!(model.undo_depth(), 3, "three gestures, three steps back");

    // Ctrl+Z three times, as `editor/history.rs` takes them: a step a press.
    let back = vec![press(Key::Z, CTRL), press(Key::Z, CTRL), press(Key::Z, CTRL)];
    assert_eq!(history::apply(&back, false, &mut model), 3);

    let lane = &model.lanes()[1];
    assert_eq!(lane.id, ids[1], "back where it started");
    assert_eq!(lane.name, "Lane 2", "under its own name");
    assert_eq!(lane.colour, LANE_PALETTE[1], "in its own colour");
    assert!(!model.can_undo(), "and the history is empty again");
}

/// `save-close-reopen`: the project carries the samples, so a kit whose
/// files have gone still sounds — note for note.
fn save_close_and_reopen() {
    let kick = ScratchFile::new("kick.flac", SHORT);
    let snare = ScratchFile::new("snare.ogg", LONG_STEREO);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let instance = instance.load(ids[0], &kick.path);
    let instance = instance.load(ids[1], &snare.path);
    {
        let mut model = instance.write();
        assert!(model.set_note(ids[0], 36));
        assert!(model.set_note(ids[1], 38));
        assert!(model.rename(ids[1], "Snare 909"));
        assert!(model.set_volume_db(ids[1], -6.0));
        assert!(model.set_pan(ids[1], -0.5));
        assert!(model.set_choke(ids[1], 3));
        assert!(model.set_mode(ids[1], LaneMode::Gate));
        assert!(model.reorder(1, 0));
        model.set_sample_pad_switch(true);
        // The kit as the project holds it, not as an edit of it.
        model.rebase();
    }

    let before = render(&instance.published(), &mut Engine::new(), 12, saved_kit_script);
    assert!(peak(&before) > 0.0, "the kit sounds to begin with");

    // Save: the host asks for the state (`lib.rs`'s `LanesField::map`) and
    // writes it into its project. The JSON and zstd around it are
    // nice-plug's; the blob is ours.
    let project = serde_json::to_string(&instance.write().snapshot()).expect("the state saves");
    assert!(project.len() > SHORT.len(), "with the sample files inside it");

    // Close the project, and move the samples out of the way.
    drop(instance);
    drop(kick);
    drop(snare);

    // Reopen: the host hands the blob back (`LanesField::set`), with no
    // window open anywhere.
    let reopened = Instance::fresh();
    let saved: Snapshot = serde_json::from_str(&project).expect("the state loads");
    {
        let mut model = reopened.write();
        let pending = model.load_snapshot(saved);
        assert_eq!(pending.len(), 2, "two samples to restore");
        for (lane, sample) in pending {
            assert!(reopened.loader.queue(&mut model, lane, Source::Restore(sample)));
        }
    }
    let reopened = reopened.settle();

    {
        let model = reopened.write();
        let saved = "the lanes came back, in the order they were saved";
        assert_eq!(model.lanes().len(), ids.len(), "{saved}");

        let restored = &model.lanes()[0];
        assert_eq!(restored.name, "Snare 909");
        assert_eq!(restored.note, 38);
        assert_eq!(restored.volume_db, -6.0);
        assert_eq!(restored.pan, -0.5);
        assert_eq!(restored.choke, 3);
        assert_eq!(restored.mode, LaneMode::Gate);
        assert!(restored.error.is_none(), "nothing went wrong: {:?}", restored.error);
        assert!(restored.sample.is_some(), "and its file is gone, so this came out of the project");
        assert!(model.sample_pad_switch(), "the switch came back on");
        assert!(!model.can_undo(), "opening a project is not an edit");
    }

    let after = render(&reopened.published(), &mut Engine::new(), 12, saved_kit_script);
    assert_eq!(after, before, "the reopened kit sounds the same, note for note");
}

/// `reload-from-file`: the menu's "reload from file" re-reads the lane's
/// own path, whatever is at it now.
fn reload_from_file() {
    let file = ScratchFile::new("tom.flac", SHORT);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let instance = instance.load(ids[0], &file.path);
    {
        let model = instance.write();
        let lane = model.lane(ids[0]).expect("the loaded lane");
        assert_eq!(lane.sample.as_ref().expect("the file").data.channels, 1, "the mono take");
        assert_eq!(
            menu::path_hover(lane),
            Some(file.path.to_str().expect("the path is text")),
            "the menu's top line shows where the lane's sample came from"
        );
        assert_eq!(menu::path_hover(model.lane(ids[1]).expect("an empty lane")), None);
    }

    // A new take saved over the same path — a different format, even: the
    // decoder goes by the bytes, not by the extension (`sample.rs`).
    std::fs::write(&file.path, LONG_STEREO).expect("the temp directory is writable");

    // "reload from file" hands the lane's own path back to the loader,
    // which is the whole of it (`editor/lanes.rs`).
    assert_eq!(menu::RELOAD_LABEL, "reload from file");
    let instance = instance.load(ids[0], &file.path);

    let model = instance.write();
    let lane = model.lane(ids[0]).expect("the lane is still there");
    let sample = lane.sample.as_ref().expect("the reload landed");
    assert_eq!(sample.data.channels, 2, "the new take, in two channels");
    assert_eq!(sample.path, file.path.to_string_lossy(), "from the path it already had");
}

/// `delete-undo`: the lane, its sample and its settings all come back.
fn delete_and_undo() {
    let file = ScratchFile::new("conga.ogg", LONG_STEREO);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let instance = instance.load(ids[1], &file.path);
    let cell = instance.published();
    let mut model = instance.write();
    assert!(model.set_volume_db(ids[1], -3.5));
    model.commit();
    assert!(cell.get().lane(ids[1]).is_some(), "the audio thread can see it");

    // The second of the menu's two clicks, as `editor/lanes.rs` applies
    // it: the writer call and one commit.
    assert_eq!(menu::DELETE_CONFIRM_LABEL, "delete — click again");
    assert!(model.delete_lane(ids[1]));
    assert!(model.commit());
    assert_eq!(model.lanes().len(), ids.len() - 1);
    assert!(model.lane(ids[1]).is_none(), "the lane has gone");
    assert!(cell.get().lane(ids[1]).is_none(), "and the audio thread has stopped seeing it");

    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    assert_eq!(model.lanes().len(), ids.len());
    let back = &model.lanes()[1];
    assert_eq!(back.id, ids[1], "back in its old place");
    assert_eq!(back.volume_db, -3.5, "with the volume it had");
    assert!(back.sample.is_some(), "and its sample with it");
    assert!(cell.get().lane(ids[1]).is_some(), "the audio thread has it again");

    assert_eq!(history::apply(&[press(Key::Z, CTRL | Modifiers::SHIFT)], false, &mut model), 1);
    assert_eq!(model.lanes().len(), ids.len() - 1, "Ctrl+Shift+Z takes it away again");

    // And while a name is being typed, the box has the shortcut.
    assert_eq!(history::apply(&[press(Key::Z, CTRL)], true, &mut model), 0);
    assert_eq!(model.lanes().len(), ids.len() - 1, "the rename box kept it");
}

/// `sample-pad-switch`: off is the file's own pitch, on keytracks from C3,
/// and on with bitstep's Sample pad on the two cancel.
fn the_switch_with_bitstep_driving() {
    let file = ScratchFile::new("bell.ogg", LONG_STEREO);
    let instance = Instance::fresh();
    let ids = instance.ids();
    let instance = instance.load(ids[0], &file.path);

    // An octave below C3, so the switch has something to do.
    let key = PAD_ROOT_KEY - 12;
    let rate = {
        let mut model = instance.write();
        assert!(model.set_note(ids[0], key));
        assert!(model.sample_pad_switch(), "a new instance starts with the switch on");
        model.set_sample_pad_switch(false);
        model.lane(ids[0]).expect("the lane").sample.as_ref().expect("its file").data.rate
    };

    let cell = instance.published();
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];

    // Both switches off: the file's own pitch, whatever note the lane is on.
    let mut engine = Engine::new();
    engine.run_block(
        &cell,
        &[note_on(0, key)],
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        1.0,
    );
    let own_pitch = engine.pool().voices().next().expect("the note started").step;
    assert_eq!(own_pitch, step_for(rate, RATE, 0.0), "the file's rate against the host's");

    // The switch on, a plain keyboard in front: the kit keytracks from C3.
    instance.write().set_sample_pad_switch(true);
    let mut engine = Engine::new();
    engine.run_block(
        &cell,
        &[note_on(0, key)],
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        1.0,
    );
    let voice = engine.pool().voices().next().expect("the note started");
    assert_eq!(voice.semis, -12.0, "an octave below C3 plays an octave down");
    assert!(voice.step < own_pitch, "which is a slower step through the file");

    // And bitstep's Sample pad on too: it sends `60 − key` with the note,
    // the switch adds `key − 60`, and the lane is back at its own pitch.
    let bitstep = TimedEvent::new(
        0,
        EngineEvent::PolyTuning {
            note: target(key),
            semis: f32::from(PAD_ROOT_KEY) - f32::from(key),
        },
    );
    let mut engine = Engine::new();
    engine.run_block(
        &cell,
        &[note_on(0, key), bitstep],
        &mut Outputs::main(&mut left, &mut right),
        RATE,
        1.0,
    );
    let voice = engine.pool().voices().next().expect("the note started");
    assert_eq!(voice.semis, 0.0, "the two cancel");
    assert_eq!(voice.step, own_pitch, "back at the file's own pitch");
}

/// `browse-and-preview`: the browser lists the file, a preview of it sounds
/// through the engine's own preview voice, and loading it is the drop path
/// onto the target lane.
fn browse_and_preview() {
    use chopdrop::browser::{Activation, Browser};
    use chopdrop::preview::PreviewStatus;
    use std::time::{Duration, Instant};

    let file = ScratchFile::new("tom.flac", SHORT);
    let folder = file.path.parent().expect("the scratch dir").to_path_buf();
    let mut browser = Browser::default();
    browser.open(&folder);
    browser.move_selection(1);
    assert_eq!(browser.selected().map(|entry| entry.path.clone()), Some(file.path.clone()));

    // Auto-preview: the selected file decodes on the previewer's thread and
    // the engine plays it, outside the lanes.
    let instance = Instance::fresh();
    let handle = instance.write().handle().clone();
    let channel = Arc::new(PreviewChannel::new(&handle));
    let previewer = Previewer::new(Arc::clone(&channel), handle);
    let mut engine = Engine::new().with_preview(Arc::clone(&channel));
    previewer.play(&file.path);
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(previewer.status(), PreviewStatus::Loading(_)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(previewer.status(), PreviewStatus::Playing(file.path.clone()));
    let mut blocks = ShortBlocks::new();
    let cell = instance.published();
    let heard = (0..4).map(|_| blocks.block(&mut engine, &cell, &[])).fold(0.0, f32::max);
    assert!(heard > 0.0, "the preview is heard");
    assert_eq!(engine.pool().active_count(), 0, "and it is not a lane's voice");

    // Enter: the browser hands the file over, and the window queues it on
    // the loader for the target lane exactly as a drop on that row does.
    let Activation::Load(path) = browser.activate() else { panic!("a file asks to be loaded") };
    let ids = instance.ids();
    let instance = instance.load(ids[3], &path);
    let model = instance.write();
    assert!(model.lane(ids[3]).expect("the target").sample.is_some());
}

/// `lane-panel-multi`: a selection made with a click and a Shift+click, one
/// panel edit over all of it, heard on the output it picked and undone in
/// one step.
fn lane_panel_on_several_lanes() {
    let files = [("kick.ogg", LONG_STEREO), ("snare.mp3", LONG_MONO), ("hat.ogg", LONG_STEREO)];
    let (instance, ids, _files) = kit(&files);
    let mut model = instance.write();
    assert!(model.set_attack_ms(ids[1], 20.0));
    model.commit();
    let depth = model.undo_depth();

    // Lane 1's name, then Shift+click on Lane 3's: the three of them.
    let mut selection = Selection::new();
    selection.click(ids[0]);
    selection.shift_click(ids[2], &ids);
    assert_eq!(selection.ids(), &ids[..3], "the display-order range from the anchor");

    let selected = lane_panel::selected_lanes(&model, selection.ids());
    let values = lane_panel::Values::of(&selected, model.tempo().get());
    assert_eq!(values.header, "3 lanes");
    assert_eq!(values.attack, lane_panel::MIXED, "the three disagree on attack");
    assert_eq!(values.output, "Out 1", "and all start on the main output");
    assert!(lane_panel::OUTPUT_HOVER.contains("Add Missing Chains"), "the hover says what to do");

    // "Out 3" picked from the dropdown: one edit, one ended gesture.
    let pick = lane_panel::Gesture { edit: Some(lane_panel::Edit::Output(2)), ended: true };
    lane_panel::apply(&mut model, selection.ids(), pick);
    assert_eq!(model.undo_depth(), depth + 1, "one pick, one undo step, however many lanes");
    for &id in &ids[..3] {
        let lane = model.lane(id).expect("a selected lane");
        assert_eq!(lane.output, 2, "{} moved to Out 3", lane.name);
        assert_eq!(lanes::row_marker(lane), "out 3", "and its row says so");
    }
    let unselected = model.lane(ids[3]).expect("Lane 4");
    assert_eq!(unselected.output, 0, "a lane outside the selection stays put");
    assert_eq!(lanes::row_marker(unselected), "");

    // Played: every selected lane is on Out 3's pair, none on the main one.
    let cell = Arc::clone(model.published());
    let notes = [note_on(0, 36), note_on(0, 37), note_on(0, 38)];
    let peaks = block_on_outputs(&cell, &mut Engine::new(), &notes, OUTPUTS);
    assert_eq!(heard_on(&peaks), [2], "the three lanes are heard on Out 3 only");

    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    for &id in &ids[..3] {
        assert_eq!(model.lane(id).expect("still there").output, 0, "one Ctrl+Z, all back");
    }
}

/// `outputs-own-chain`: "give each its own output" on a selection and on a
/// lone unselected lane, each lane heard on its own pair only, and a full
/// house counted for the bar.
fn give_each_its_own_output() {
    let files = [("kick.ogg", LONG_STEREO), ("snare.mp3", LONG_MONO), ("hat.ogg", LONG_STEREO)];
    let (instance, ids, _files) = kit(&files);
    let mut model = instance.write();

    let mut selection = Selection::new();
    selection.click(ids[0]);
    selection.ctrl_click(ids[1]);

    // The item on Lane 1, which is selected: the whole selection moves —
    // as `editor/lanes.rs` routes it — and is one undo step.
    assert_eq!(menu::OWN_OUTPUTS_LABEL, "give each its own output");
    assert!(selection.contains(ids[0]));
    assert_eq!(model.assign_own_outputs(selection.ids()), 0, "every lane found an output");
    assert!(model.commit());
    let marker = |model: &Model, id: u32| lanes::row_marker(model.lane(id).expect("a lane"));
    assert_eq!(marker(&model, ids[0]), "out 2");
    assert_eq!(marker(&model, ids[1]), "out 3");
    assert_eq!(marker(&model, ids[2]), "", "Lane 3 was not selected");

    // The item on Lane 3, not selected: it moves alone, to the next free one.
    assert!(!selection.contains(ids[2]));
    assert_eq!(model.assign_own_outputs(&[ids[2]]), 0);
    assert!(model.commit());
    assert_eq!(marker(&model, ids[2]), "out 4");

    // Each lane is heard on its own pair and nowhere else — so an effect on
    // Out 3's chain reaches Lane 2 only.
    let cell = Arc::clone(model.published());
    for (key, pair) in [(36, 1), (37, 2), (38, 3)] {
        let peaks = block_on_outputs(&cell, &mut Engine::new(), &[note_on(0, key)], OUTPUTS);
        assert_eq!(heard_on(&peaks), [pair], "the lane on key {key} is heard on its own pair");
    }

    // More lanes than Out 2–16 can hold: fifteen find one, the rest stay
    // where they were, and the bar has the count to say so.
    while model.lanes().len() < OUTPUTS + 1 {
        model.add_lane().expect("room for another lane");
    }
    let all: Vec<u32> = model.lanes().iter().map(|lane| lane.id).collect();
    let unplaced = model.assign_own_outputs(&all);
    assert!(model.commit());
    assert_eq!(unplaced, all.len() - (OUTPUTS - 1), "Out 2 to Out 16, one lane each");
    assert_eq!(
        status::outputs_kept(unplaced).as_deref(),
        Some("2 lanes kept their output: no free outputs")
    );

    // And each pick was one step back.
    assert_eq!(marker(&model, ids[3]), "out 5", "Lane 4 took the first free output");
    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    assert_eq!(marker(&model, ids[3]), "", "the full-house pick came off whole");
    assert_eq!(marker(&model, ids[2]), "out 4", "and the pick before it stayed");
}

/// The project chopdrop v1 saved, checked in and never edited
/// (`chopdrop/tests/fixtures/README.md`).
const V1_PROJECT: &str = include_str!("fixtures/v1_project.json");

/// `outputs-old-project`: a v1 project restores onto Out 1 with v1's
/// behaviour, and sounds the same whether the host hands over the one pair
/// the old device had or the sixteen the new one declares.
fn open_a_project_from_before_outputs() {
    let saved: Snapshot = serde_json::from_str(V1_PROJECT).expect("the v1 project parses");
    assert_eq!(saved.version, 1, "and it is a v1 file");

    let instance = Instance::fresh();
    {
        let mut model = instance.write();
        let pending = model.load_snapshot(saved);
        assert_eq!(pending.len(), 1, "the kick's embedded sample");
        for (lane, sample) in pending {
            assert!(instance.loader.queue(&mut model, lane, Source::Restore(sample)));
        }
    }
    let instance = instance.settle();

    let cell = {
        let model = instance.write();
        assert_eq!(model.lanes().len(), 2, "every lane it was saved with");
        for lane in model.lanes() {
            assert_eq!(lane.output, 0, "{} is on Out 1", lane.name);
            assert!(!lane.cut_itself, "{} does not cut itself, as in v1", lane.name);
            assert_eq!(lanes::row_marker(lane), "", "and no row grew a marker");
        }
        let kick = &model.lanes()[0];
        assert!(kick.sample.is_some(), "the kick came out of the project: {:?}", kick.error);
        assert!(!model.can_undo(), "opening a project is not an edit");
        Arc::clone(model.published())
    };

    // The kick, on the note it was saved on, with both port counts a host
    // might hand a device whose layout grew since the project was saved.
    let kick = [note_on(0, 36)];
    assert_eq!(OUTPUTS, 16, "the plugin now declares sixteen pairs");
    let sixteen = block_on_outputs(&cell, &mut Engine::new(), &kick, OUTPUTS);
    let one = block_on_outputs(&cell, &mut Engine::new(), &kick, 1);
    assert_eq!(heard_on(&sixteen), [0], "heard on Out 1 and nowhere else");
    assert_eq!(sixteen, one, "the same kit whether the host gives one pair or sixteen");
}

/// `chop-open-source`: a file let go over the strip opens as its source,
/// fitted to the view, with no lane touched and nothing to undo.
fn open_a_song_in_the_strip() {
    let file = ScratchFile::new("Eye.wav", &song());
    let instance = Instance::fresh();
    let ids = instance.ids();
    let (rows, list) = drawn_list(&ids);
    let strip = Rect::from_min_size(pos2(0.0, 0.0), vec2(list.width(), list.top()));

    // Over the strip, the strip is the target — not a row, not a new lane.
    let landing = drops::route(Some(strip.center()), &rows, list, strip);
    assert_eq!(landing, Landing::Strip { outline: strip });
    {
        let mut model = instance.write();
        drops::apply(&mut model, &instance.loader, landing, slice::from_ref(&file.path));
        assert_eq!(chop_strip::body(&model), chop_strip::Body::Loading, "\"loading…\" at once");
    }
    let instance = instance.settle();

    let mut model = instance.write();
    assert_eq!(chop_strip::body(&model), chop_strip::Body::Loaded);
    let editor = model.chop();
    assert_eq!(editor.total(), SONG_FRAMES, "the whole song");
    assert_eq!(editor.view, View::fit(SONG_FRAMES, editor.view_width_px), "fitted to the strip");
    assert_eq!(editor.selection, None, "and nothing selected yet");
    let path = editor.source_path.clone().expect("the path is kept");
    assert_eq!(chop_strip::file_name(&path), "Eye.wav", "the header names the file");

    let mut window = Strip::new(&model);
    let out = window.frame(&mut model, Vec::new());
    assert!(out.canvas.is_some(), "the strip draws the song");

    assert_eq!(model.lanes().len(), ids.len(), "no lane was added");
    assert!(model.lanes().iter().all(|lane| lane.sample.is_none() && !lane.loading));
    assert!(!model.can_undo(), "opening a source is not an edit of the kit");
}

/// `chop-select-loop`: a drag in the main view selects (snapped), `P` plays
/// exactly that region through the engine's preview voice, `L` loops it,
/// and a nudged edge reaches the loop at its next pass.
fn select_and_loop_by_ear() {
    let file = ScratchFile::new("Eye.wav", &song());
    let instance = Instance::fresh().open_song(&file.path);
    let mut model = instance.write();
    let mut strip = Strip::new(&model);
    strip.frame(&mut model, Vec::new());
    strip.frame(&mut model, Vec::new());

    // A drag across the main view, left to right: a selection, its edges
    // pulled onto zero crossings, the end the active edge.
    let main = strip.main_view();
    let (from, to) = (main.min.x + 100.0, main.min.x + 250.0);
    let out = strip.drag(&mut model, pos2(from, main.center().y), pos2(to, main.center().y));
    assert_eq!(out.click, None, "a drag is not a click");
    let selection = model.chop().selection.expect("the drag selected");
    let (view, window) = (model.chop().view, chop::snap_window_frames(SONG_RATE));
    assert!(selection.start.abs_diff(view.frame_at(from - main.min.x)) <= window);
    assert!(selection.end.abs_diff(view.frame_at(to - main.min.x)) <= window);
    assert_eq!(selection.active, chop::Edge::End);

    // P: exactly the selection, once.
    let cell = Arc::clone(model.published());
    let mut engine = Engine::new().with_preview(Arc::clone(strip.channel()));
    let mut blocks = ShortBlocks::new();
    strip.key(&mut model, Key::P);
    {
        let state = strip.channel().get();
        let region = Region { start: selection.start, end: selection.end };
        assert_eq!(state.region, Some(region), "P plays the selection");
        assert!(!state.looping, "once, with loop off");
    }
    let len = selection.len();
    assert!(preview_blocks(&mut engine, &cell, &mut blocks, 4, 4) > 0.0, "it is heard");
    preview_blocks(&mut engine, &cell, &mut blocks, blocks_for(len) + 4, 0);
    assert!(!strip.channel().is_playing(), "and stops at the selection's end");

    // L, then P: round and round.
    strip.key(&mut model, Key::L);
    assert!(model.chop().looping, "loop is lit");
    strip.key(&mut model, Key::P);
    assert!(strip.channel().get().looping, "the preview loops");
    let heard = preview_blocks(&mut engine, &cell, &mut blocks, 3 * blocks_for(len), 8);
    assert!(heard > 0.0 && strip.channel().is_playing(), "still sounding three passes on");

    // The right arrow moves the active edge, and the loop hears it.
    strip.frame(&mut model, vec![]);
    strip.key(&mut model, Key::ArrowRight);
    let nudged = model.chop().selection.expect("still selected");
    let nudge = chop::nudge_frames(chop::NUDGE_COARSE_MS, SONG_RATE) as u64;
    assert_eq!(nudged.end, selection.end + nudge, "10 ms longer");
    assert_eq!(strip.channel().live_region().end, nudged.end, "at the next pass");

    // Loop off again: to the end of this pass, then silence.
    strip.key(&mut model, Key::L);
    assert!(!model.chop().looping && !strip.channel().live_looping());
    preview_blocks(&mut engine, &cell, &mut blocks, blocks_for(nudged.len()) + 4, 0);
    assert!(!strip.channel().is_playing(), "the pass ends and so does the preview");
}

/// `chop-to-lane`: Enter adds the selection as a lane named after the
/// source, `N` moves the selection on, and the selection Ctrl+dragged onto a row
/// replaces that row's sample — each one undo step.
fn chop_onto_lanes() {
    let file = ScratchFile::new("Eye.wav", &song());
    let instance = Instance::fresh().open_song(&file.path);
    let ids = instance.ids();
    let mut model = instance.write();
    let mut strip = Strip::new(&model);
    strip.frame(&mut model, Vec::new());
    let selection = chop::Selection { start: 24_000, end: 36_000, active: chop::Edge::End };
    assert!(model.set_selection(Some(selection)));
    let depth = model.undo_depth();

    // Enter: a new lane at the end, the selection's frames on it.
    let added = strip.key(&mut model, Key::Enter).added.expect("Enter added a lane");
    assert_eq!(model.undo_depth(), depth + 1, "one undo step");
    assert_eq!(model.lanes().len(), ids.len() + 1);
    let lane = model.lane(added).expect("the new lane");
    assert_eq!(lane.name, "Eye 1", "named after the source");
    assert_eq!(lane.note, model.lanes()[ids.len() - 1].note + 1, "on the next note up");
    let next_colour = LANE_PALETTE[ids.len() % LANE_PALETTE.len()];
    assert_eq!(lane.colour, next_colour, "in the next colour along");
    let sample = lane.sample.as_ref().expect("the chop");
    let source_path = file.path.to_string_lossy().into_owned();
    let origin = ChopOrigin { source_path, start_frame: 24_000, end_frame: 36_000 };
    assert_eq!(sample.origin.as_ref(), Some(&origin), "it remembers where it came from");
    assert_eq!(sample.data.frame_count() as u64, selection.len(), "exactly the selection");
    let key = lane.note;
    let cell = Arc::clone(model.published());
    assert!(ShortBlocks::new().block(&mut Engine::new(), &cell, &[note_on(0, key)]) > 0.0);

    // N, then the selection carried onto Lane 2's row.
    strip.key(&mut model, Key::N);
    let next = model.chop().selection.expect("still selected");
    assert_eq!((next.start, next.end), (36_000, 48_000), "on by its own length");
    strip.frame(&mut model, Vec::new());
    let y = strip.main_view().center().y;
    let inside = pos2(strip.x_of(&model, (next.start + next.end) / 2), y);
    let row = strip.rows[1].rect.center();
    strip.drag_with(&mut model, inside, row, Modifiers::CTRL | Modifiers::COMMAND);
    assert_eq!(model.lanes().len(), ids.len() + 1, "a row takes the chop, no lane is added");
    let lane2 = model.lane(ids[1]).expect("Lane 2");
    assert_eq!((lane2.name.as_str(), lane2.note), ("Lane 2", 37), "it keeps its name and note");
    assert_eq!(lane2.colour, LANE_PALETTE[1], "and its colour");
    let replaced = lane2.sample.as_ref().expect("the chop landed on Lane 2");
    let moved = ChopOrigin { start_frame: 36_000, end_frame: 48_000, ..origin };
    assert_eq!(replaced.origin.as_ref(), Some(&moved));
    assert_eq!(model.undo_depth(), depth + 3, "Enter, N and the drop: a step each");

    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    assert!(model.lane(ids[1]).expect("Lane 2").sample.is_none(), "Lane 2 is empty again");
    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    let back = model.chop().selection.expect("still selected");
    assert_eq!((back.start, back.end), (24_000, 36_000), "N is undone: the selection is back");
    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    assert!(model.lane(added).is_none(), "and Eye 1 is gone");
    assert_eq!(model.lanes().len(), ids.len());
}

/// `chop-reopen-source`: a chop lane saved in a project plays from it
/// before its source is open, names its origin on hover and in the lane
/// panel, reopens the source with the cut selected — and with the source
/// gone, says so and still plays.
fn reopen_a_chops_source() {
    let file = ScratchFile::new("Eye.wav", &song());
    let instance = Instance::fresh().open_song(&file.path);
    let (start, end) = (30_000, 42_000);
    let project = {
        let mut model = instance.write();
        let selection = chop::Selection { start, end, active: chop::Edge::End };
        assert!(model.set_selection(Some(selection)));
        model.add_chop_lane(selection).expect("a chop lane");
        assert!(model.commit());
        model.close_source();
        serde_json::to_string(&model.snapshot()).expect("the state saves")
    };
    drop(instance);

    // Reopen: the chop is in the project, with no source open.
    let reopen = |project: &str| {
        let instance = Instance::fresh();
        {
            let mut model = instance.write();
            let saved: Snapshot = serde_json::from_str(project).expect("the state loads");
            let pending = model.load_snapshot(saved);
            assert_eq!(pending.len(), 1, "the chop's embedded sample");
            for (lane, sample) in pending {
                assert!(instance.loader.queue(&mut model, lane, Source::Restore(sample)));
            }
            assert!(!instance.loader.reopen_source(&mut model), "the source was closed");
        }
        instance.settle()
    };
    let chop_plays = |model: &Model, key: u8| {
        let cell = Arc::clone(model.published());
        ShortBlocks::new().block(&mut Engine::new(), &cell, &[note_on(0, key)]) > 0.0
    };
    let instance = reopen(&project);
    let (origin, key) = {
        let model = instance.write();
        assert_eq!(chop_strip::body(&model), chop_strip::Body::Empty);
        let lane = &model.lanes()[DEFAULT_LANE_COUNT as usize];
        assert_eq!(lane.name, "Eye 1");
        assert!(chop_plays(&model, lane.note), "the chop plays from the project");

        // The hover and the lane tab say where it came from.
        let sample = lane.sample.as_ref().expect("restored");
        let origin = sample.origin.clone().expect("the origin was saved");
        let text = "from Eye.wav, 0:00.625\u{2013}0:00.875";
        assert_eq!(sample.origin_text().as_deref(), Some(text), "the sample-name hover");
        let selected = lane_panel::selected_lanes(&model, &[lane.id]);
        let values = lane_panel::Values::of(&selected, model.tempo().get());
        assert_eq!(values.origin.as_deref(), Some(text), "the lane tab's line");

        assert!(menu::offers_open_source(lane), "the item is on the chop lane");
        assert!(!menu::offers_open_source(&model.lanes()[0]), "and not on Lane 1");
        (origin, lane.note)
    };

    // "open source in editor": the song, the cut selected and framed.
    {
        let mut model = instance.write();
        assert!(instance.loader.open_origin(&mut model, &origin));
    }
    let instance = instance.settle();
    {
        let model = instance.write();
        let editor = model.chop();
        assert_eq!(chop_strip::body(&model), chop_strip::Body::Loaded);
        let selection = editor.selection.expect("the cut is selected");
        assert_eq!((selection.start, selection.end), (start, end));
        let width = editor.view_width_px;
        assert_eq!(editor.view, View::around(start, end, SONG_FRAMES, width), "and in view");
    }

    // The song moved away: the strip says so, and the chop plays regardless.
    drop(file);
    let instance = reopen(&project);
    {
        let mut model = instance.write();
        assert!(instance.loader.open_origin(&mut model, &origin));
    }
    let instance = instance.settle();
    let model = instance.write();
    let missing = format!("{}Eye.wav", chop::SOURCE_MISSING_PREFIX);
    assert_eq!(model.chop().error.as_deref(), Some(missing.as_str()), "source not found");
    assert!(matches!(chop_strip::body(&model), chop_strip::Body::Failed { .. }));
    assert!(chop_plays(&model, key), "and the chop still plays");
}

/// A lane's whole panel drawn once, headless, for what only drawing says:
/// whether the pitch field takes input.
fn draw_panel(model: &mut Model, ids: &[u32]) -> lane_panel::PanelOutput {
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    let mut out = lane_panel::PanelOutput::default();
    for _ in 0..2 {
        let input = egui::RawInput {
            screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(320.0, 600.0))),
            ..egui::RawInput::default()
        };
        let output = ctx.run_ui(input, |ui| out = lane_panel::show(ui, model, ids));
        output.drop_without_applying_deltas();
    }
    out
}

/// The step of the one voice sounding in `engine`.
fn the_step(engine: &Engine) -> f64 {
    engine.pool().voices().next().expect("a voice is sounding").step
}

/// `tempo-while-stopped`: the host tempo goes into the instance's one cell
/// whether or not the transport runs (`process()` stores it every block), so
/// the panel, an audition and the strip's readout all follow it — and a block
/// that brings none keeps the last.
fn tempo_while_stopped() {
    let file = ScratchFile::new("loop.wav", &song());
    let instance = Instance::fresh().load(0, &file.path);
    let lane = instance.ids()[0];
    let mut model = instance.write();
    // The bare note-on below stands in for the audition this journey
    // describes; with the Sample pad switch on (a new instance's default) it
    // would also carry the switch's −24 semitones on C1, which is not what
    // this journey measures. The real audition ignores the switch.
    model.set_sample_pad_switch(false);
    let pick = |edit| lane_panel::Gesture { edit: Some(edit), ended: true };
    let repitch = lane_panel::Edit::TimeMode(time::TimeMode::Repitch);
    lane_panel::apply(&mut model, &[lane], pick(repitch));
    let bar = time::Length::Beats(time::NoteLength::Bar);
    lane_panel::apply(&mut model, &[lane], pick(lane_panel::Edit::Length(bar)));

    // The engine reads the model's cell, as the plugin builds it.
    let tempo = Arc::clone(model.tempo());
    let mut engine = Engine::new().with_tempo(Arc::clone(&tempo));
    assert!(Arc::ptr_eq(engine.tempo(), model.tempo()), "one cell, not two");
    let pitch = |model: &Model| {
        let selected = lane_panel::selected_lanes(model, &[lane]);
        lane_panel::Values::of(&selected, model.tempo().get()).pitch
    };
    assert_eq!(tempo.get(), time::TEMPO_DEFAULT, "120 before the host says anything");
    assert_eq!(pitch(&model), "0.00 st", "two seconds is a bar at 120");

    // Stopped, the host at 90: the tempo arrives with the block regardless.
    tempo.update(Some(90.0));
    assert_eq!(pitch(&model), "\u{2212}4.98 st", "the panel follows without a play");
    let cell = Arc::clone(model.published());
    let key = model.lane(lane).expect("the lane").note;
    let mut blocks = ShortBlocks::new();
    assert!(blocks.block(&mut engine, &cell, &[note_on(0, key)]) > 0.0, "the audition sounds");
    assert!((the_step(&engine) - 0.75).abs() < 1e-9, "a bar at 90 BPM is 3/4 speed");

    // A block without a tempo keeps the last one: nothing jumps back to 120.
    tempo.update(None);
    assert_eq!(tempo.get(), 90.0);
    let _ = blocks.block(&mut engine, &cell, &[]);
    assert!((the_step(&engine) - 0.75).abs() < 1e-9, "the sounding voice kept its speed");
    assert_eq!(pitch(&model), "\u{2212}4.98 st");
    drop(model);

    // The strip's readout is worked out at the same cell.
    let instance = instance.open_song(&file.path);
    let model = instance.write();
    let whole = chop::Selection { start: 0, end: SONG_FRAMES, active: chop::Edge::End };
    let line = chop::readout(&whole, SONG_RATE, model.tempo().get());
    assert!(line.ends_with("at 90 BPM"), "the readout is at the host's tempo: {line}");
}

/// `repitch-to-bar`: a two-second loop at 125 BPM, set to Repitch and 1 bar
/// from the panel, offers the bar as nearest, shows the pitch it costs with
/// the field disabled, plays for exactly one bar and undoes in one step.
fn repitch_to_a_bar() {
    let file = ScratchFile::new("loop.wav", &song());
    let instance = Instance::fresh().load(0, &file.path);
    let lane = instance.ids()[0];
    let mut model = instance.write();
    // Its bare note-ons play C1 untuned, as an audition or a bitstep lane
    // with Sample pad off would; the Sample pad switch (on in a new
    // instance) would add −24 semitones that this journey does not measure.
    model.set_sample_pad_switch(false);
    model.tempo().update(Some(125.0));
    let pick = |edit| lane_panel::Gesture { edit: Some(edit), ended: true };
    let repitch = lane_panel::Edit::TimeMode(time::TimeMode::Repitch);
    lane_panel::apply(&mut model, &[lane], pick(repitch));

    // The dropdown: 1 bar is the nearest, 4 % shorter than the loop.
    let natural = lane_panel::natural_secs(model.lane(lane).expect("the lane"));
    assert_eq!(natural, Some(2.0), "the loop is two seconds");
    let entries = lane_panel::length_entries(natural, model.tempo().get());
    let bar = time::Length::Beats(time::NoteLength::Bar);
    let label = entries.iter().find(|(length, _)| *length == bar).map(|(_, label)| label.as_str());
    assert_eq!(label, Some("\u{2248} 1 bar \u{2212}4.0 %"));

    let depth = model.undo_depth();
    lane_panel::apply(&mut model, &[lane], pick(lane_panel::Edit::Length(bar)));
    assert_eq!(model.undo_depth(), depth + 1, "one pick, one step");
    assert_eq!(lanes::row_marker(model.lane(lane).expect("the lane")), "1 bar");
    let selected = lane_panel::selected_lanes(&model, &[lane]);
    assert_eq!(lane_panel::Values::of(&selected, model.tempo().get()).pitch, "+0.71 st");
    assert!(!draw_panel(&mut model, &[lane]).pitch_enabled, "the length sets the pitch");
    assert!(lane_panel::PITCH_REPITCH_HOVER.contains("length"), "and the hover says so");

    // Played: it lasts one bar at 125 BPM, 1.92 s — 180 blocks to the frame.
    let blocks_heard = |model: &Model| {
        let cell = Arc::clone(model.published());
        let key = model.lane(lane).expect("the lane").note;
        let mut engine = Engine::new().with_tempo(Arc::clone(model.tempo()));
        let (mut left, mut right) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
        let mut blocks = 0;
        loop {
            let events = if blocks == 0 { vec![note_on(0, key)] } else { Vec::new() };
            let mut outputs = Outputs::main(&mut left, &mut right);
            engine.run_block(&cell, &events, &mut outputs, RATE, 1.0);
            blocks += 1;
            if engine.pool().active_count() == 0 || blocks > 400 {
                return blocks;
            }
        }
    };
    let bar_frames = 4.0 * 60.0 / 125.0 * RATE;
    assert_eq!(bar_frames, 92_160.0);
    assert_eq!(blocks_heard(&model), 180, "one bar at 125 BPM, ending on the bar line");

    // One Ctrl+Z: as cut again, the loop's own two seconds.
    assert_eq!(history::apply(&[press(Key::Z, CTRL)], false, &mut model), 1);
    assert_eq!(model.lane(lane).expect("the lane").length, time::Length::AsCut);
    assert_eq!(lanes::row_marker(model.lane(lane).expect("the lane")), "");
    let own = SONG_FRAMES.div_ceil(BLOCK as u64) as usize;
    assert_eq!(blocks_heard(&model), own, "as cut, it plays as it was");
}

/// Zero crossings per second of a mono sample: the sine's pitch, twice over.
fn crossings_per_sec(data: &SampleData) -> f64 {
    let crossings = data.frames.windows(2).filter(|pair| (pair[0] < 0.0) != (pair[1] < 0.0));
    crossings.count() as f64 * f64::from(data.rate) / data.frame_count() as f64
}

/// `stretch-to-bar`: a two-second loop at 125 BPM, set to Stretch and 1 bar
/// from the panel, says "rendering…" until the worker lands a render of
/// exactly one bar that keeps the loop's pitch, plays it for one bar at its
/// own speed, shifts the pitch without the length, and follows a tempo change
/// with no editor at all — speed-corrected at once, re-rendered after.
fn stretch_to_a_bar() {
    use chopdrop::render::RenderWorker;
    use std::time::Duration;

    let file = ScratchFile::new("loop.wav", &song());
    let instance = Instance::fresh().load(0, &file.path);
    let lane = instance.ids()[0];
    // Its bare note-ons play C1 untuned, as an audition or a bitstep lane
    // with Sample pad off would; the Sample pad switch (on in a new
    // instance) would add −24 semitones that this journey does not measure.
    instance.write().set_sample_pad_switch(false);
    let tempo = Arc::clone(instance.write().tempo());
    tempo.update(Some(125.0));
    // The worker as the plugin builds it — the model's own tempo cell — with
    // a debounce short enough not to slow the suite.
    let worker = || {
        RenderWorker::with_debounce(
            Arc::clone(&instance.model),
            Arc::clone(&tempo),
            Duration::from_millis(2),
        )
    };
    let pick = |edit| lane_panel::Gesture { edit: Some(edit), ended: true };
    let status =
        |model: &Model| lane_panel::render_status(&lane_panel::selected_lanes(model, &[lane]));
    let rendered = |model: &Model| {
        let render = model.lane(lane).expect("the lane").render.as_ref().expect("a render landed");
        SampleData::clone(&render.data)
    };
    // Played once from a note-on: the blocks it sounds for, its step, and
    // whether it played the render rather than the raw loop.
    let play = |model: &Model| {
        let cell = Arc::clone(model.published());
        let key = model.lane(lane).expect("the lane").note;
        let mut engine = Engine::new().with_tempo(Arc::clone(model.tempo()));
        let (mut left, mut right) = ([0.0f32; BLOCK], [0.0f32; BLOCK]);
        let (mut blocks, mut step, mut from_render) = (0, 0.0, false);
        loop {
            let events = if blocks == 0 { vec![note_on(0, key)] } else { Vec::new() };
            let mut outputs = Outputs::main(&mut left, &mut right);
            engine.run_block(&cell, &events, &mut outputs, RATE, 1.0);
            if blocks == 0 {
                let voice = engine.pool().voices().next().expect("the lane sounds");
                (step, from_render) = (voice.step, voice.rendered);
            }
            blocks += 1;
            if engine.pool().active_count() == 0 || blocks > 400 {
                return (blocks, step, from_render);
            }
        }
    };

    // Stretch, 1 bar: two picks, and the panel says a render is on its way.
    let worker_a = worker();
    {
        let mut model = instance.write();
        let stretch = lane_panel::Edit::TimeMode(time::TimeMode::Stretch);
        lane_panel::apply(&mut model, &[lane], pick(stretch));
        let bar = time::Length::Beats(time::NoteLength::Bar);
        lane_panel::apply(&mut model, &[lane], pick(lane_panel::Edit::Length(bar)));
        assert_eq!(status(&model), Some(lane_panel::RenderStatus::Rendering));
        assert_eq!(lane_panel::RenderStatus::Rendering.text(), "rendering\u{2026}");
        assert_eq!(lanes::row_marker(model.lane(lane).expect("the lane")), "1 bar");
        let selected = lane_panel::selected_lanes(&model, &[lane]);
        assert_eq!(lane_panel::Values::of(&selected, model.tempo().get()).pitch, "0.00 st");
        assert!(draw_panel(&mut model, &[lane]).pitch_enabled, "Stretch leaves pitch live");
    }
    worker_a.wake();
    assert!(worker_a.finish() >= 1, "the worker rendered");

    // Landed: one bar at 125 BPM to the frame, the loop's own pitch, the
    // line gone — and it plays at its own speed for 180 blocks.
    let bar_frames: usize = 92_160;
    let unstretched = {
        let model = instance.write();
        let loop_ = model.lane(lane).expect("the lane").sample.as_ref().expect("loaded");
        crossings_per_sec(&loop_.data)
    };
    {
        let model = instance.write();
        assert_eq!(status(&model), None, "\"rendering…\" has gone");
        let render = rendered(&model);
        assert_eq!(render.frame_count(), bar_frames, "exactly one bar at 125 BPM");
        let pitch = crossings_per_sec(&render) / unstretched;
        assert!((pitch - 1.0).abs() < 0.02, "the stretch keeps the pitch: {pitch}");
        let (blocks, step, from_render) = play(&model);
        assert!(from_render, "the note plays the render");
        assert!((step - 1.0).abs() < 1e-9, "at its own speed: {step}");
        assert_eq!(blocks, bar_frames.div_ceil(BLOCK), "ending on the bar line");
    }

    // +7 st: a fifth up, the same bar long.
    let worker_b = worker();
    {
        let mut model = instance.write();
        lane_panel::apply(&mut model, &[lane], pick(lane_panel::Edit::PitchSt(7.0)));
        assert_eq!(status(&model), Some(lane_panel::RenderStatus::Rendering));
    }
    worker_b.wake();
    assert!(worker_b.finish() >= 1, "the worker rendered again");
    {
        let model = instance.write();
        let render = rendered(&model);
        assert_eq!(render.frame_count(), bar_frames, "the pitch leaves the length alone");
        let fifth = 2f64.powf(7.0 / 12.0);
        let pitch = crossings_per_sec(&render) / unstretched;
        assert!((pitch - fifth).abs() < 0.03, "a fifth higher: {pitch}");
        assert_eq!(play(&model).0, bar_frames.div_ceil(BLOCK));
    }

    // 100 BPM, no editor: the first note is a bar long at once, the old
    // render sped down; the worker alone re-renders it to the new bar.
    tempo.update(Some(100.0));
    let slow_bar: usize = 115_200;
    {
        let model = instance.write();
        let (blocks, step, from_render) = play(&model);
        assert!(from_render, "still the +7 st render");
        assert!((step - 0.8).abs() < 1e-9, "speed-corrected to the new bar: {step}");
        assert_eq!(blocks, slow_bar.div_ceil(BLOCK), "a bar at 100 BPM before the re-render");
    }
    assert!(worker().finish() >= 1, "the tempo alone re-rendered the lane");
    let model = instance.write();
    assert_eq!(rendered(&model).frame_count(), slow_bar, "one bar at 100 BPM");
    let (blocks, step, _) = play(&model);
    assert!((step - 1.0).abs() < 1e-9, "and plays at its own speed again: {step}");
    assert_eq!(blocks, slow_bar.div_ceil(BLOCK));
}

// ---- The protocol as a test -------------------------------------------

/// The journeys the walkthrough is meant to cover, in their fixed order —
/// spelled out beside [`WALKTHROUGH`] so that one cannot be dropped from the
/// checklist without this file saying so.
const SPEC_ORDER: [&str; 23] = [
    "drop-on-row",
    "drop-below-list",
    "drop-many",
    "audition",
    "play-from-bitstep",
    "choke-pair",
    "gate-vs-one-shot",
    "rename-reorder-recolour",
    "save-close-reopen",
    "reload-from-file",
    "delete-undo",
    "sample-pad-switch",
    "browse-and-preview",
    "lane-panel-multi",
    "outputs-own-chain",
    "outputs-old-project",
    "chop-open-source",
    "chop-select-loop",
    "chop-to-lane",
    "chop-reopen-source",
    "tempo-while-stopped",
    "repitch-to-bar",
    "stretch-to-bar",
];

/// The checklist covers every journey it is meant to, and nothing else.
#[test]
fn the_protocol_is_the_list_the_spec_asks_for() {
    let ids: Vec<&str> = WALKTHROUGH.iter().map(|journey| journey.id).collect();

    assert_eq!(ids, SPEC_ORDER, "the checklist and its list of journeys have drifted");
}

/// Every journey is something a user can actually walk: a title, steps to
/// take, a result to judge it by, and words for both halves of it.
#[test]
fn every_journey_reads_as_a_checklist() {
    for journey in WALKTHROUGH {
        let id = journey.id;
        assert!(
            id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
            "{id} is not a kebab-case handle"
        );
        assert!(!journey.title.is_empty(), "{id} has no title");
        assert!(journey.steps.len() >= 2, "{id} is not a journey, it is a click");
        assert!(!journey.expect.is_empty(), "{id} says nothing a pass could be judged on");
        assert!(!journey.eyes.is_empty(), "{id} does not say what the user is for");
        assert!(!journey.pinned.is_empty(), "{id} does not say what it pins");

        for line in journey.steps.iter().chain(journey.expect) {
            assert_eq!(line.trim(), *line, "{id} has a line with loose whitespace: {line:?}");
            assert!(line.ends_with('.'), "{id} has a line that is not a sentence: {line:?}");
        }
    }
}

/// The container's half of every journey, driven against the real code.
///
/// One test and not one per journey, because the claim is about the protocol as a
/// whole: the checklist is only worth walking if every journey on it still
/// describes this plugin. The id is printed before its driver runs, so a
/// panic out of one names the journey it came from.
///
/// The pairing lives here rather than in [`WALKTHROUGH`] so that the table
/// stays what it is — the words the user walks — and is asserted against
/// [`SPEC_ORDER`] below, which is what stops a journey from losing its
/// driver, or a driver its journey, without the suite saying so.
#[test]
fn every_journey_still_describes_the_plugin() {
    let drivers: [(&str, fn()); SPEC_ORDER.len()] = [
        ("drop-on-row", drop_on_a_row),
        ("drop-below-list", drop_below_the_list),
        ("drop-many", drop_several_at_once),
        ("audition", audition_a_lane),
        ("play-from-bitstep", play_from_a_note_source),
        ("choke-pair", choke_a_pair),
        ("gate-vs-one-shot", gate_against_one_shot),
        ("rename-reorder-recolour", rename_reorder_recolour),
        ("save-close-reopen", save_close_and_reopen),
        ("reload-from-file", reload_from_file),
        ("delete-undo", delete_and_undo),
        ("sample-pad-switch", the_switch_with_bitstep_driving),
        ("browse-and-preview", browse_and_preview),
        ("lane-panel-multi", lane_panel_on_several_lanes),
        ("outputs-own-chain", give_each_its_own_output),
        ("outputs-old-project", open_a_project_from_before_outputs),
        ("chop-open-source", open_a_song_in_the_strip),
        ("chop-select-loop", select_and_loop_by_ear),
        ("chop-to-lane", chop_onto_lanes),
        ("chop-reopen-source", reopen_a_chops_source),
        ("tempo-while-stopped", tempo_while_stopped),
        ("repitch-to-bar", repitch_to_a_bar),
        ("stretch-to-bar", stretch_to_a_bar),
    ];

    let paired: Vec<&str> = drivers.iter().map(|(id, _)| *id).collect();
    assert_eq!(paired, SPEC_ORDER, "a journey without a driver, or a driver without one");

    for (id, check) in drivers {
        println!("{id}");
        check();
    }
}

/// The checklist, printed to walk beside Bitwig (the module header has the
/// command). A test rather than a binary or a document: it is the one place
/// the protocol lives, and `cargo test` is what the user already has.
#[test]
fn the_protocol() {
    println!("\nchopdrop user-journey walkthrough — {} journeys\n", WALKTHROUGH.len());
    println!("Walk them in order. Note anything that fails, naming the");
    println!("journey's id.\n");

    let mut printed = 0;
    for (n, journey) in WALKTHROUGH.iter().enumerate() {
        println!("{}. {} [{}]", n + 1, journey.title, journey.id);
        println!("   do:");
        for step in journey.steps {
            println!("     - {step}");
        }
        println!("   pass if:");
        for expect in journey.expect {
            println!("     - {expect}");
        }
        println!("   yours to judge: {}", journey.eyes);
        println!("   pinned here:    {}\n", journey.pinned);
        printed += 1;
    }

    // So that a protocol nobody can read is a failure and not quiet output.
    assert_eq!(printed, SPEC_ORDER.len(), "the whole checklist was printed");
}

