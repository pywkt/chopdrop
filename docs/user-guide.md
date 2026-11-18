# chopdrop user guide

chopdrop is a sample player built as a plain list. Every row — a **lane** —
holds one sample and answers to one note. You drag samples onto it from
Bitwig's browser (or pick them in chopdrop's own browser, with previews), give
the lanes names, and play them from [bitstep](https://github.com/pywkt/bitstep),
from a MIDI keyboard, or from notes in a clip.

A new instance starts with sixteen empty lanes, "Lane 1" to "Lane 16", on
the notes C1 to D#2. The first eight, C1 to G1 in the first eight lane
colours, are exactly what a new bitstep starts with, so bitstep's lanes play
chopdrop's lanes one to one without you setting anything up. The other eight
carry on up the scale for anything else you play.

Note names follow Bitwig's own: **C3 is 60** and **C1 is 36**.

## The window

Across the top:

- **Master** — the volume everything passes through, −60 to +12 dB. Drag
  along the rail to set it, or click anywhere on it to jump straight there;
  **right-click** it to type a value instead (`-6`, `-6 dB` and `3db` all
  work — Enter takes it, Escape leaves the volume alone). This one is a
  plugin parameter called *Volume*, so you can automate it or hook a
  modulator to it from Bitwig, and the rail follows along when you do.
- **bitstep Sample pad** — a switch for the whole instance, on in a new
  chopdrop.
  [What it does](#the-bitstep-sample-pad-switch).
- **+ lane** — adds a lane at the end of the list, named "Lane *n*", on the
  next note up from the last lane, in the next colour of the palette. It
  greys out when the list is full (128 lanes).

A size figure appears in the top bar once a project is carrying a lot of
sample data; see [Saving](#saving-and-reopening-a-project). The same spot is
where chopdrop tells you a drop of several files did not all fit; the message
goes away by itself after a few seconds.

Under the top bar, across the whole window, is the
[chop editor](#the-chop-editor): a strip for opening a song or loop and
cutting pieces of it onto lanes.

Below that is the lane list. It scrolls when there are more lanes than fit,
and the window can be resized.

On the right of the lane list a **side panel** can open, with two tabs along
its top: **lane settings**, which shows the settings of the lanes you select
(see [The lane panel](#the-lane-panel)), and **browse**, chopdrop's
[sample browser](#browsing-for-samples). The **×** beside the tabs closes the
panel. It starts closed. Clicking a lane's name selects the lane and opens
the panel on **lane settings** (or switches to it, if the browser is
showing). Clicking a lane's sample selects the lane too: on a lane that
already has a sample it also shows **lane settings**, and on an empty lane it
opens **browse** to find one. **browse** always browses for the selected
lane. The window grows wider to make room for the panel and gives the width
back when it closes.

With *every* lane deleted, the list shows one line in the middle of the
window: "Drag samples here from Bitwig's browser". Dropping a file there
gives you a lane again.

## Loading samples

Drag a sample out of Bitwig's browser (or out of a file manager) and onto the
chopdrop window. While the file is in the air, the place it will land is
outlined:

- **Onto a lane's row** — the sample goes into that lane. The lane keeps its
  name, note, colour, volume and pan: only the sound changes.
- **Onto the space below the last lane** — a new lane is added, named after
  the file with its extension removed, on the next note up and in the next
  colour.
- **Onto the chop editor** — the file opens there, ready to cut pieces from;
  no lane changes. If you drop several files, only the first one opens.

Drop **several files at once** onto a row or below the list and the first
one lands wherever you dropped them; the rest are added as new lanes at the
end, each named after its own file. If that would take the list past 128 lanes, the files that fit are
loaded and the top bar says how many did not — delete a lane and drop those
again, or put them on a second chopdrop.

A lane says "loading…" while the file is being read, then shows the file
name. Decoding happens in the background, so a long file never freezes the
window or interrupts playback.

**What can be loaded:** WAV, AIFF, FLAC, MP3 and OGG/Vorbis, mono or stereo,
at any sample rate — chopdrop plays a file at its own pitch whatever rate the
project runs at. Files with more than two channels are folded down to
stereo. The limit is 64 MiB per file, and about 25 minutes of audio.

**What can't:** anything else gets a short message on the lane in red, and
nothing else happens — the lane keeps whatever it had. The messages are:

| On the lane | What it means |
|---|---|
| `can't read file` | The file isn't there any more, or can't be opened. |
| `file too big (max 64 MiB)` | Pick a smaller file, or trim it first. |
| `Bitwig clips can't be loaded — drop a .wav instead` | Bitwig's factory drum packages hold `.bwclip` clips next to plain audio files. A clip is Bitwig's own closed format. |
| `unsupported audio file` | Not one of the formats above. |
| `no audio in file` | The file decoded to nothing. |
| `audio too long` | Over about 25 minutes. |

The message stays until that lane loads something successfully.

## Browsing for samples

You can also pick samples without leaving chopdrop. Click **"drop a sample
here"** on an empty lane, choose **browse…** from any lane's right-click
menu, or select a lane and click the side panel's **browse** tab. The side
panel opens on the right of the window, which grows wider to make room for
it, showing the browser, and the lane it will load into is outlined in blue.
(Clicking the sample name of a lane that already has one shows that lane's
settings instead, with the browser ready behind the **browse** tab.)

The browser is the side panel's **browse** tab. Switching to the **lane
settings** tab and back keeps the browser exactly where you left it — the same folder,
the same file highlighted.

- **Where it opens:** in the folder the lane's sample came from, or, for an
  empty lane, wherever you last loaded from (your Music folder the first
  time).
- **The buttons along the top** jump to a few places: **lane** (this lane's
  sample folder), **packages** (Bitwig's installed sound packages),
  **library** (your Bitwig Studio folder), **music** and **home**. Only the
  places that exist on your computer are shown. **recent**, beside the **×**,
  lists the last folders you loaded from.
- **up** goes to the folder above. The box beside the folder name filters the
  list: type part of a name.
- The list shows folders (with a folder icon) and the audio files chopdrop can
  load (with a small waveform). Everything else is hidden.

**Previewing.** With **auto-preview** ticked (it is, to begin with), a file
plays as soon as you select it, by clicking it or moving to it with the arrow
keys. **play** plays the highlighted file again, and turns into **stop** while
the browser's preview sounds. Untick auto-preview to browse in silence and preview only
with **play**. Previews play through chopdrop's output, at the master volume,
so Bitwig must be running audio to hear them.

**Loading.** Double-click a file, press **Enter**, or click **load**: the file
goes into the outlined lane, exactly as if you had dropped it there. The
browser stays open, so to fill several empty lanes, click the next one's
"drop a sample here" to point the browser at it and carry on. Double-clicking a folder (or Enter on
it) opens it.

**Opening in the chop editor.** To cut pieces from a file rather than load
it whole, click **chop** in the browser's bottom row, or right-click the
file and choose **open in chop editor**. The file opens in the chop editor
at the top of the window, just as if you had dropped it there; no lane
changes.

**Keys while the browser is open:** **↑** / **↓** move through the list,
**Page Up** / **Page Down** jump ten at a time, **Enter** opens or loads,
**Backspace** goes up a folder, and **Escape** clears the filter, then closes
the side panel. Closing it (Escape, or either **×** at the top) stops the
browser's preview and gives the window its width back; so do switching to the
lane tab and changing folder. None of these stop the chop editor's preview,
so a loop playing there keeps sounding while you browse.

chopdrop remembers the auto-preview setting, the last folder and the recent
folders for next time, in every project.

## The chop editor

The strip under the top bar is for cutting pieces out of a longer file (a
song, a loop, a field recording) and putting them on lanes. With nothing
open it says "drop a song or loop here, or open one from the browser".

### Opening a file

Drop a file onto the strip, or pick one in the
[browser](#browsing-for-samples) and click **chop**. While it opens the strip
says "loading…", and if the file can't be opened it shows why in red with
the file's path under it. Files up to 512 MiB (and about 25 minutes of
audio) open here, in the same formats a lane loads.

Once a file is open the strip shows it three ways: a thin overview of the
whole file along the top (the part you're looking at is the lighter band,
your selection the blue one), the waveform itself below it, and a time ruler
under that whose marks get finer as you zoom in.

The header row carries the file's name (hover it for the full path) and the
buttons **play** (it says **stop** while the strip is playing), **loop**,
**snap**, **next**, **add lane** and **×**.
**play**, **loop** and **next** grey out until a file is open; **snap** is
always there. **×** closes the file: the strip
forgets it and your selection (or the error, if the file couldn't be opened)
and goes back to its empty message.

### Moving around and selecting

- **Zoom:** hold Ctrl and roll the wheel over the strip to zoom around the
  mouse pointer: down zooms in (as far as one sample per pixel), up zooms
  out.
- **Scroll:** roll the wheel without Ctrl to move through the file: down
  goes earlier, up later. Sideways scrolling (a touchpad or a tilting
  wheel) moves through it too, with or without Ctrl held: left goes
  earlier, right later.
- **Select:** with nothing selected, drag across the waveform. The
  selection runs from where you pressed to where you let go, in either
  direction.
- **Adjust an edge:** press within a few pixels of it and drag. Or click
  anywhere else on the waveform: the nearer edge jumps to where you clicked
  and you can keep dragging it from there, while the other edge stays put.
  Inside the selection that trims it (the start if you click in its left
  half, the end in its right half); outside it, it stretches the selection
  out to the click. So you can trim or extend while zoomed in without
  finding the edge first. Hold Shift to place an edge exactly where the
  mouse is, ignoring snap.
- **Start again:** double-click anywhere on the waveform to drop the
  selection and start a new one there: keep the button down after the
  second click and drag. **Escape** drops the selection too.
- **Undo:** **Ctrl+Z** puts the selection back the way it was before your
  last click, drag or key, one step at a time, in the same
  [undo](#undo-and-redo) as everything else (**Ctrl+Shift+Z** redoes).
- **The overview:** click it to centre the view there, drag its blue band to
  move the whole selection along the file (its length stays the same), or
  drag anywhere else on it to scroll the view.

**snap** makes the edges of your selection land on the nearest point where
the waveform crosses zero, so cut pieces start and end without a click. It
is lit while on, starts on, and works with or without a file open.

Under the ruler a line reads out your selection: where it starts and ends,
how long it is in seconds, and the nearest note length with how far off it
is, for example "0:30.120 – 0:32.093 · 1.973 s · ≈ 1 bar, 1.4 % short at 120
BPM" ("short" means the piece is a little shorter than that note length). It
counts at the project's tempo, and follows it when the tempo changes.

**next** moves the selection on by its own length, so the next piece of the
same size starts where this one ended (the view follows it). It does nothing
when that piece would run past the end of the file.

### Listening

**play** plays your selection (with nothing selected, from the left edge of
the view to the end of the file); click it again while it sounds to stop.
**loop** (lit while on) makes play repeat the selection until you stop it.
Turning loop on or off while a preview sounds takes effect at the end of the
current pass, and moving the selection's edges while it plays changes what
the next pass plays.

With nothing selected, a click on the waveform plays from that point to the
end of the file, or stops whatever the strip is playing. While the strip
plays, a thin line marks where you are. Closing the file, opening another,
or closing the chopdrop window stops it. The preview plays through chopdrop's
main output (Out 1).

### Cutting pieces onto lanes

**add lane** cuts your selection out as a new lane at the bottom of the
list, named after the file and numbered ("Eye 1", "Eye 2", …), on the next
note up and in the next colour. The piece's edges are faded by a hair so it
starts and ends without a click. It is greyed out until you have a selection
(and while the list is full), and one undo takes the lane away again.

You can also drag the selection itself out of the strip, three ways: grab
the blue **drag to a lane** bar along the bottom of the selection; or press
inside the selection and drag down off the waveform (the edge you pressed
near jumps back to where it was, and the whole selection comes with you);
or hold **Ctrl**, press inside it and drag. Let go over a lane to replace that
lane's sample with the piece (the lane keeps its name, note, colour and
settings), or below the list (or anywhere else outside the strip) to add it
as a new lane, just like **add lane**. A blue outline shows where it will
land; letting go back over the strip does nothing, and one undo reverses
either.

A cut lane remembers where it came from. Resting the pointer on its sample
name says so ("from Eye.wav, 0:30.120–0:32.090"), and its right-click menu
can [reopen that file](#the-right-click-menu) with the piece selected. The
lane plays its own copy of the piece, so it keeps working if the file is
moved or deleted.

### Keys

**A** plays the selection, or stops it, from anywhere in the window (unless
you are typing in a box). The rest of the keys work once you click anywhere
in the strip to give it the keyboard (clicking elsewhere in the window takes
it away again):

| Key | What it does |
|---|---|
| **←** / **→** | Move the selection's active edge 10 ms earlier or later (the keys don't snap) |
| **Shift+←** / **Shift+→** | The same in 1 ms steps |
| **↑** / **↓** | Make the start / the end the active edge (it is drawn thicker) |
| **Z** | Snap on or off |
| **P** | Play the selection, or stop it (the same as **A**) |
| **L** | Loop on or off |
| **N** | The same as **next** |
| **Enter** | The same as **add lane** |
| **Escape** | Stop the strip's preview, or clear the selection when nothing is playing |
| **Ctrl+Z** | Undo, including changes to the selection (works anywhere in the window) |

Space still starts and stops Bitwig's transport.

### Folding and resizing

The small triangle at the left of the header folds the strip down to just
that row, and unfolds it again. Drag the strip's lower edge to make it
taller or shorter (it never goes below a little under 100 pixels). Folded or
not, and how tall it is, is saved with the project.

## A lane, field by field

Reading a row from left to right:

1. **The grip** — drag it up or down to move the lane in the list. This is
   display order only: a lane keeps the note it answers to, so moving rows
   around never changes what plays.
2. **The colour dot** — the lane's colour, which doubles as its
   [activity light](#audition-and-the-activity-light). Change the colour from
   the [right-click menu](#the-right-click-menu).
3. **The play button** — the triangle right after the dot. See
   [Audition](#audition-and-the-activity-light).
4. **The note** — which note plays this lane, written the way Bitwig writes
   it (`C1`, `F#1`). Several lanes may share a note; they all sound together.
5. **The name** — click to [select the lane](#selecting-several-lanes);
   double-click to type a new one, up to 64 characters, then press Enter.
   Escape leaves it as it was, and an empty name is refused.
6. **The sample** — the file's name, or "drop a sample here" when the lane is
   empty, or whatever went wrong with the last load. Click it to
   [browse for a sample](#browsing-for-samples).
7. **Volume** — −60 to +12 dB for this lane alone.
8. **Pan** — −100% (hard left) to +100% (hard right).
9. **The choke group** — 0 for none, or 1 to 16. See
   [Choke groups](#choke-groups).
10. **One-shot / gate** — the button showing `1` or `G` at the end of the
    row. See [One-shot and gate](#one-shot-and-gate).

The note, volume, pan and choke group all work the same three ways: **drag**
across the field to move the value, **roll the mouse wheel** over it to step
it one at a time, or **click** it and type. One notch of the wheel is one
semitone, one decibel, one percent of pan, or one choke group. Typing into
the note field takes names like `c1`, `F#1` or `Gb2`; anything else leaves
the note alone.

Whatever a field says is what the lane is: there is nothing to apply or
confirm. Every change takes effect on the next audio block, and a drag
through a whole octave counts as a single [undo](#undo-and-redo) step.

## Selecting several lanes

Click a lane's **name**, or an empty part of its row, to select it: the row
turns a lighter grey, and the side panel shows its settings on the **lane
settings** tab — opening if it was closed, or switching over from the
browser, which keeps its place for when you go back to it.

To select more than one:

- **Ctrl+click** a row adds that lane to the selection, or takes it out
  again.
- **Shift+click** a row selects every lane from the last one you clicked to
  this one, in the order the list shows them (not by note).
- A plain click selects just that lane again.

The selection is what the [lane panel](#the-lane-panel) and the menu's
[give each its own output](#the-right-click-menu) work on. The fields in a
row — note, volume, pan, choke, one-shot/gate, play — always change their
own lane only, selected or not. Selecting lanes is not an undoable change
and does not alter the project. A deleted lane drops out of the selection.

## The lane panel

The **lane settings** tab of the side panel shows the selected lanes'
settings. Its header is the lane's name, or "3 lanes" (say) when several are
selected; with none selected it says "click a lane name to select it". A setting the
selected lanes don't all share shows a dash (for the checkbox, a dash inside
it).

- **attack** — a fade-in at the start of every hit, 0 to 2000 ms (0 on a
  new lane: the hit starts at full level).
- **release** — how long a gate lane fades out after its note ends, 0 to
  5000 ms (300 ms on a new lane).
- **cut itself** — on for a new lane; see [Cut itself](#cut-itself).
  Clicking a dashed checkbox turns it on for every selected lane.
- **output** — which of chopdrop's 16 stereo outputs the lane plays through,
  Out 1 (the main output, on a new lane) to Out 16. Pick one from the
  dropdown and every selected lane moves to it. A lane on any output but
  Out 1 shows a small grey marker such as "out 5" after its sample name.

  **Add Missing Chains:** in Bitwig, Out 2–16 stay silent until you click
  **Add Missing Chains** on the chopdrop device, which gives each output its
  own chain in the device's mixer; resting the pointer on the dropdown
  reminds you. A host that gives chopdrop only its main output plays every
  lane through Out 1.

Drag the attack or release field (right or up for longer), turn the wheel
over it (10 ms a notch), or click it and type a number of milliseconds
(`250` or `250 ms`). Every change applies to all the selected lanes at once,
and one drag, notch, typed value, click or pick is one
[undo](#undo-and-redo) step however many lanes it changed. Dragging a field
that shows a dash moves every selected lane on from the first one's value.

With a single lane selected that was cut in the chop editor, a grey line at
the bottom of the panel says which file and which stretch of it the piece
came from ("from Eye.wav, 0:30.120–0:32.090").

### Following the tempo

The rest of the panel decides whether a lane follows the project's tempo.

- **time** — how the lane's length follows the project tempo: **Off** (the
  default: the sample plays as cut), **Repitch** (it plays faster or slower
  to last the chosen length, so its pitch moves too, like tape) or
  **Stretch** (it is time-stretched to last the chosen length and keeps its
  pitch). Click one; with several lanes selected that disagree, none is
  lit.
- **length** — what a Repitch or Stretch lane lasts: **as cut** (the
  default) or a note length from 1/16 to 8 bars, dotted or triplet. Each
  entry says how much it would change the sample's length at the current
  tempo ("1 bar −1.4 %"), and the closest one is marked "≈". A Repitch or
  Stretch lane with a note length shows it as a small grey marker such as
  "1 bar" after its sample name (before the output marker, if it has one).
- **fine** — a trim on top of the length, −10 % to +10 % in 0.1 % steps
  (wheel: 0.1 % a notch).
- **pitch** — −24 to +24 semitones, to the cent (wheel: 1 semitone a
  notch). In Off it repitches the sample tape-style. In Repitch the field
  is greyed out and shows the pitch the speed change makes instead (resting
  the pointer on it says so). In Stretch it shifts the pitch on its own,
  without changing the length.

Fine and pitch work like attack and release: drag, turn the wheel, or click
and type (`1.5`, `-2 %`, `+7 st`).

### What Stretch rebuilds

A Stretch lane is stretched in the background, a moment after you change
its time, length, fine, pitch or sample (and when a project opens or the
tempo changes). Meanwhile the panel shows a grey "rendering…" line and the
lane keeps playing in time, its pitch catching up when the stretch is
ready. If stretching fails, the line reads "render failed — click to retry"
(resting the pointer on it says why); click it to try again. The lane's row
says the same in place of its file name — "rendering…" in grey, or "render
failed" in red with the reason when you rest the pointer on it — so you
can see it without selecting the lane. To stretch a lane again by hand, use
**re-render** in its [right-click menu](#the-right-click-menu).

The stretched audio is never saved with the project: it is made again from
the lane's sample whenever the project opens. A Repitch lane needs no
stretching; it simply plays faster or slower, straight away.

## The right-click menu

Right-click anywhere on a lane's row:

- **rename** — opens the name box, the same as double-clicking the name.
- **colour** — a row of the ten lane colours; click one. The colour the lane
  is wearing has a ring around it.
- **duplicate** — puts a copy of the lane directly below it, with the same
  sample, note and settings. When the list is full (128 lanes) it is greyed
  out, and resting the pointer on it says so.
- **delete** — asks twice. The first click arms it and the item changes to
  "delete — click again"; the second click removes the lane. Clicking
  elsewhere, or opening another lane's menu, disarms it. A deleted lane comes
  back with Ctrl+Z.
- **browse…** — opens the [sample browser](#browsing-for-samples) for this
  lane, the same as clicking its sample.
- **reload from file** — reads the lane's own file again, from the path it
  was loaded from. Use it after you have edited a sample in an audio editor:
  the project plays its own copy of the file (see
  [Saving](#saving-and-reopening-a-project)), so the new take only arrives
  when you ask for it. Resting the pointer on this item shows the full path.
  It is greyed out on a lane cut in the chop editor: a chop has no file of
  its own, and reading its source again would replace the chop with the
  whole file — use **open source in editor** instead.
- **clear sample** — takes the sound off the lane and leaves the lane, with
  its name, note and colour.
- **open source in editor** — only on a lane cut in the chop editor: opens
  the file it was cut from in the chop editor (unfolding the strip if it was
  folded) and, once the file has loaded, selects exactly the piece the lane
  plays and zooms the view to it. Resting the pointer on it says where the
  piece came from. If the file has moved or gone, the strip says so; the
  lane keeps playing its own copy.
- **re-render** — only on a lane in Stretch with a sample: stretches it
  again from its sample. Stretching happens by itself whenever something
  changes; this is for a stretch that failed or sounds wrong. It is not an
  undo step.
- **give each its own output** — moves every selected lane (or just this
  one, if it is not selected) to the lowest output from Out 2 to Out 16 that
  no other lane uses, one lane per output; a lane already alone on one of
  those keeps it. One Ctrl+Z puts them all back. When the free outputs run
  out, the rest stay where they were and the top bar says how many for a
  few seconds ("2 lanes kept their output: no free outputs"). Remember
  **Add Missing Chains** in Bitwig to hear Out 2–16.

## Playing it

chopdrop answers to notes from anything in front of it on the track: bitstep,
a clip, a keyboard. A note-on starts every lane sitting on that note, at the
sample's own pitch, with that lane's volume and pan, and timed to the exact
position of the note inside the audio block.

It also follows the **per-note expressions** bitstep sends, so a step's own
settings come through: **velocity**, **pitch** (±8 octaves), **pan** and
**volume**, each applied per note rather than per lane. Expressions that
arrive while a note is sounding are followed as it plays.

Up to **64 voices** sound at once. Past that, the oldest one still ringing is
faded out — quickly, but never cut, so you hear a tail rather than a click.

## Choke groups

A lane's choke group is 0 (no group) or 1 to 16. When a lane in a group is
struck, everything still sounding from the *other* lanes in the same group is
silenced with a very short fade. The classic use is a hi-hat: put the open
and the closed hat on the same group, and the closed hat cuts the open one
off the moment it is hit.

Matching is by the group number alone, not by the lane, so a lane in a group
also cuts off its *own* earlier hits: put one lane on its own in a group and
it only ever sounds one note at a time, which is how you keep a long 808 or a
re-triggered loop from piling up on itself. Lanes in group 0 — the default —
choke nothing and are never choked.

A choke is always a short fade, never a hard stop, so you hear a hat closing
rather than a dropout.

## Cut itself

A new lane **cuts itself**: hitting it again fades out its own earlier hit
with the same short fade a choke uses, so a lane sounds one note at a time
without needing a choke group. Auditioning the lane does the same. It works
alongside a choke group — the lane still cuts, and is cut by, the other lanes
in its group. Lanes in projects saved by an earlier version of chopdrop keep
the old behaviour and let their hits overlap.

## One-shot and gate

The square showing `1` or `G` decides what happens when the note ends:

- **`1` — one-shot** (the default): the sample plays all the way through,
  however briefly the note was held. Right for drum hits.
- **`G` — gate**: the sound ends when the note ends, fading out over the
  lane's release time (300 ms on a new lane; at 0 ms it is a 4 ms fade, so
  there is never a click). Right for loops, pads and anything you want to be
  able to cut short from the keyboard or from a step's length. A choke still
  cuts a gate lane with the short 4 ms fade, whatever its release. Lanes in
  projects saved by an earlier version of chopdrop open with a 0 ms release,
  so their gate notes end with the same short fade they always had.

Click the square to switch between them. Auditioning a gate lane from its
own play button plays it to the end: there is no note to end it.

## Audition and the activity light

Click the triangle near the start of a row, right after its colour dot, to
hear that lane on its own, at the lane's own note and at full velocity — so
it sounds exactly as it will when a pattern plays it, tuning included. While it plays the triangle turns into
a square: click it to stop. Playing it again always starts from the
beginning, and starting one lane stops any other lane you were auditioning.
On an empty lane the triangle is dim and does nothing.

**S** does the same for the selected lane, from anywhere in the window
(unless you are typing in a box): press it to play, and again to stop. It
is handy after changing a lane's time or pitch settings.

The lane's colour dot brightens every time the lane is struck and fades back
over about a sixth of a second. With bitstep in front of chopdrop, that is the
quickest way to see which lane a step is hitting.

## The "bitstep Sample pad" switch

This is on in a new chopdrop, and with bitstep's default lanes that is what
you want: each sample plays at its own pitch. Here is the whole story.
bitstep has a per-lane
*Sample pad* setting, on for new lanes, which retunes each note it sends by
C3 minus the lane's note — it exists so that a Drum Machine pad with key
tracking on (root C3) plays its sample at the pitch it was recorded at. A
chopdrop lane already plays its sample at its own pitch, so those extra
semitones are heard as a transposition: a bitstep lane on C1 with *Sample
pad* on would play its chopdrop lane two octaves high. Turning chopdrop's
switch on makes chopdrop pitch notes the way a key-tracking Drum Machine pad
does — by the note's distance from C3 — which cancels bitstep's *Sample pad*
exactly, for every lane note, with nothing to change in bitstep.

So: **with chopdrop's switch on, keep *Sample pad* on for every bitstep lane
that plays chopdrop.** With the switch on, a bitstep lane with *Sample pad*
off plays its chopdrop lane transposed (a C1 lane two octaves down), and a
plain MIDI keyboard or a clip's notes key-track the whole kit from C3. If you
turn *Sample pad* off in bitstep, turn this switch off too.

A lane's play button always plays its sample at its own pitch, whichever way
the switch is: the switch only changes how incoming notes are pitched.

The switch belongs to the whole instance, not to a lane, and it is saved with
the project: a project saved with it off opens with it off. Flipping it never
bends a note that is already sounding.

## Saving and reopening a project

Saving a Bitwig project stores, for every lane: its name, note, colour,
volume, pan, choke group, one-shot/gate, the settings in the
[lane panel](#the-lane-panel), its place in the list, the path the
sample was loaded from, **and a copy of the sample file itself**. A Stretch
lane's stretched audio is not saved; it is
[made again](#what-stretch-rebuilds) when the project opens. The switch
and the Master volume are saved too.

That copy is what the lane plays when the project is reopened. It means a
project still sounds exactly as you left it after the sample files have been
moved, renamed, edited or deleted — and that a project you send to someone
else takes its sounds with it. The path is kept for reference, and
["reload from file"](#the-right-click-menu) is how you pull a changed file
back in.

The files make projects bigger, so chopdrop compresses what it saves and
shows a figure in the top bar once an instance is carrying more than 128 MiB
of samples. Nothing is stopped at that point — every save still saves every
sample — but a host may refuse to save a project very much larger than that,
so it is worth seeing. Clearing or replacing samples brings the figure back
down, and it disappears below the threshold.

If a lane shows a red `!` beside its file name, rest the pointer on it: there
is something to know about that lane's sample, such as the project keeping
only its path. A saved sample that cannot be decoded when the project opens
leaves the lane empty with a message rather than failing the load.

The [chop editor](#the-chop-editor) is saved too: which file it had open, where
you were looking and how far zoomed in, your selection, snap, and whether
the strip was folded and how tall. Unlike a lane's sample, the chop editor's
file is **not** copied into the project — only its path is kept, and the
file is opened again from there when the project opens. If it has been
moved or deleted, the strip says "source not found:" and the file's name;
the lanes, and any pieces you already cut onto them, load and play as
normal.

Opening a project is not an edit: there is nothing to undo straight after it.

## Undo and redo

**Ctrl+Z** undoes and **Ctrl+Shift+Z** redoes the last thing you changed in
chopdrop — a rename, a reorder, a colour, a value, a sample you dropped, a
lane you added or deleted, or the chop editor's selection. One gesture is one
step, so a drag through an
octave or a row carried the length of the list comes back in one press. The
history holds the last 100 steps, and it is chopdrop's own: Bitwig's undo is
separate. Selection steps belong to the file they were made on: once you
open another file in the chop editor, undo passes over the old file's
selections and goes on to your earlier lane edits.

While you are typing in a lane's name box, or a value into the master
volume's box, Ctrl+Z undoes the typing instead, which is what you want with a
half-typed name on screen.

## Keyboard

chopdrop's window hands the keys it doesn't use back to Bitwig, so you can
work with the plugin window focused:

- **Space**, **F1**–**F12** and anything held with **Ctrl** go to Bitwig —
  the transport keeps starting and stopping with Space, as it does everywhere
  else.
- **Ctrl+Z** and **Ctrl+Shift+Z** stay in chopdrop, as its
  [undo and redo](#undo-and-redo).
- **Enter**, **Escape**, **Tab**, the arrow keys and **Delete** belong to
  whatever you are editing in the window. While the
  [browser](#browsing-for-samples) has the keyboard, the arrow keys, Page
  Up/Down, Enter, Backspace and Escape move through it. It gets the keyboard
  when you open it from a lane or click inside it, and gives it up when you
  click anywhere else in the window (click back into it to carry on).
- **A** plays or stops the [chop editor](#the-chop-editor)'s selection and
  **S** plays or stops the selected lane, wherever the keyboard is.
- While the chop editor has the keyboard (click inside it), the arrow keys,
  **Z**, **P**, **L**, **N**, **Enter** and **Escape** work its selection and
  preview.
- While a name box or a value is being typed into, every key is the window's.

The window is not pinned above Bitwig's: Alt+Tab moves between them like any
other pair of windows.

## Limits

| | |
|---|---|
| Lanes | 128 |
| Voices sounding at once | 64 |
| Sample file size | 64 MiB |
| Chop editor file size | 512 MiB |
| Sample length | about 25 minutes |
| Lane name | 64 characters |
| Choke groups | 16 |
| Lane volume | −60 to +12 dB |
| Undo steps | 100 |
| Outputs | 16 stereo pairs (Out 1 is the main output) |

## Tips

- Keep chopdrop's lane order and names matched to bitstep's and the two
  windows read as one instrument side by side. chopdrop can't follow bitstep's
  lanes automatically — plugins have no way to talk to each other — so the
  order is yours to keep.
- Put two lanes on one note to layer samples; give each its own volume and
  pan.
- `+ lane` and a drop below the list both walk up the notes one semitone at a
  time, the same way bitstep's lanes do, so a kit built either way lines up.
