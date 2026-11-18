# chopdrop

A sample player for [Bitwig Studio](https://www.bitwig.com/), laid out as a
plain list instead of a grid of pads.

Each row is a **lane**: one sample, one note, a name and a colour. Drag a
sample onto a lane from Bitwig's browser, or pick one in chopdrop's own
browser with previews. Play the lanes from a step sequencer, a MIDI keyboard
or notes in a clip.

chopdrop is a CLAP instrument for Linux. It was built as a companion to
[bitstep](https://github.com/pywkt/bitstep), a step sequencer for Bitwig, but
it works with any note source.

## Features

**Loading samples**

- **Drag and drop** from Bitwig's browser or a file manager. Drop onto a lane
  to replace its sample, or below the list to add new lanes, one per file.
- **A built-in browser:** click a lane's sample to open a panel beside the
  lanes. It has shortcuts to your music folder, your Bitwig library and
  Bitwig's installed sound packages, plus a filter and recent folders.
  Auto-preview plays each file as you select it.
- Reads **WAV, AIFF, FLAC, MP3 and OGG/Vorbis**.

**Chopping**

- **A chop editor** across the top of the window: open a song or loop, zoom
  into its waveform, select a piece and preview it (looped if you like).
  Selections snap to zero crossings so pieces start and end cleanly, and a
  readout gives the piece's length as the nearest note length at the
  project's tempo.
- **add lane** (or Enter) cuts the piece onto a new lane; drag the selection
  onto a lane to replace its sample instead. **next** moves on to the next
  piece of the same length, so a loop slices up in a few keystrokes.
- A cut lane remembers where it came from and can reopen its file in the
  editor with the piece selected.

**Lanes**

- Add, rename, reorder, recolour, duplicate and delete lanes, up to 128 of
  them.
- Each lane has its own note, volume (−60 to +12 dB) and pan.
- **Choke groups** (1–16): a hit silences the other lanes in its group, so a
  closed hi-hat cuts off an open one.
- **One-shot or gate:** one-shot plays the whole sample however short the
  note is. Gate stops the sound when the note ends, fading out over the
  lane's release time.
- **Select several lanes** (Ctrl+click, Shift+click) and change them
  together in the **lane panel** beside the list: attack, release, cut
  itself and output, all in one undo step.
- **Attack and release:** a fade-in on every hit, and a fade-out when a gate
  lane's note ends.
- **Cut itself:** a new hit on a lane fades out its previous one, so a lane
  never piles up on itself (on for new lanes).
- **Output per lane:** pick one of 16 stereo outputs for each lane, or let
  **give each its own output** spread a selection across them, so each lane
  gets its own effects chain in Bitwig (click **Add Missing Chains** on the
  device).
- **Follow the tempo:** set a lane to **Repitch** (plays faster or slower,
  like tape) or **Stretch** (time-stretched, keeping its pitch) and give it
  a length from 1/16 to 8 bars, with a fine trim and a pitch shift. Stretch
  lanes are rebuilt in the background when the tempo or their settings
  change, and stay in time while they are.
- **Audition** a lane from the play button on its row.
- **Activity lights** show which lanes are being hit.

**Playing**

- Notes in, 16 stereo outputs, up to 64 voices at once.
- Responds to velocity and per-note expressions (pitch, pan and volume).
- A master volume in the top bar.

**Projects**

- **Samples are saved inside the project.** Each lane keeps the file's path
  *and a copy of the audio*, so a project still sounds right after its
  samples have been moved, renamed or deleted. **Reload from file** picks up
  a sample you have edited.
- **Undo and redo** for every edit (Ctrl+Z / Ctrl+Shift+Z).

## Using it with bitstep

A new chopdrop starts with sixteen empty lanes on C1 to D#2. The first eight
are the notes and colours a new [bitstep](https://github.com/pywkt/bitstep)
starts with.
Put **bitstep → chopdrop** on one track and each bitstep lane plays the
chopdrop lane with the same name, with nothing to set up. The two share a
look, so their windows read as a pair side by side.

bitstep's per-step velocity, pitch, pan and volume come through as per-note
expressions. If you use bitstep's *Sample pad* option, turn on chopdrop's
**bitstep Sample pad** switch (the
[user guide](docs/user-guide.md#the-bitstep-sample-pad-switch) explains why).

## Requirements

- Linux (x86_64)
- Bitwig Studio with CLAP support (the regular or the Flatpak version)

chopdrop is at version 0.1.0. There are no prebuilt downloads yet, so you
build it from source, which takes a few minutes.

## Install

The [install guide](docs/install.md) covers it step by step. If you already
have [Rust](https://rustup.rs) and the build dependencies it lists:

```sh
cargo xtask bundle chopdrop --release
mkdir -p ~/.clap && cp target/bundled/chopdrop.clap ~/.clap/
```

Then rescan plugins in Bitwig and add **chopdrop** to a track.

Using the **Flatpak** version of Bitwig? Build with
`./scripts/build-in-docker.sh` instead
([here's why](docs/install.md#1-build)).

## Documentation

- [Install guide](docs/install.md): building, installing and troubleshooting.
- [User guide](docs/user-guide.md): loading and browsing samples, the chop
  editor and its keys, each lane control, selecting lanes and the lane panel,
  outputs, Repitch and Stretch, choke groups, gate and one-shot, the bitstep
  switch, how saving works, and the keyboard shortcuts.

## Building from source

```sh
cargo build --workspace                # build everything
cargo test --workspace                 # run the test suite
cargo xtask bundle chopdrop --release   # make target/bundled/chopdrop.clap
```

The Rust version is pinned in `rust-toolchain.toml`, and rustup installs it
automatically. chopdrop is built with
[nice-plug](https://crates.io/crates/nice-plug) (a fork of NIH-plug),
[egui](https://github.com/emilk/egui) for the interface and
[Symphonia](https://github.com/pdeljanov/Symphonia) for decoding and
[Signalsmith Stretch](https://github.com/Signalsmith-Audio/signalsmith-stretch)
for time-stretching. The patched
copies of baseview and egui-baseview under `vendor/` make drag and drop from
Bitwig work.

## More for Bitwig

Other Bitwig projects by the same author:

- [bitstep](https://github.com/pywkt/bitstep): a step sequencer that sends notes, with per-lane lengths and rates, 64 patterns and a song mode.
- [stuttter](https://github.com/pywkt/stuttter): a stutter, repeat and tape-stop effect built from step blocks.
- [Concord](https://github.com/pywkt/concord): play one key, hear a full chord, voiced and strummed.
- [knobbox](https://github.com/pywkt/knobbox): a DIY USB MIDI controller whose knobs follow the selected device.

## License

[ISC](LICENSE)
