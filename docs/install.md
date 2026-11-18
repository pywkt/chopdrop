# Installing chopdrop

chopdrop is a CLAP plugin. Right now it is **Linux only** (x86_64) and there
are no prebuilt downloads, so you build it from source. It takes a few
minutes the first time.

## What you need

- **Bitwig Studio** with CLAP support (Bitwig added CLAP in version 4.3; any
  current version works).
- **Rust**, installed through [rustup](https://rustup.rs). You don't need to
  pick a version: the repository's `rust-toolchain.toml` pins one, and rustup
  downloads it automatically the first time you build.
- **A C/C++ toolchain, clang, and the X11/OpenGL development libraries**
  (clang is needed to build the time-stretching library):

  ```sh
  # Arch / Manjaro
  sudo pacman -S --needed base-devel clang libx11 libxcb xcb-util-wm libxcursor libxkbcommon libglvnd

  # Debian / Ubuntu
  sudo apt install build-essential clang libclang-dev pkg-config libgl-dev \
    libx11-xcb-dev libxcb1-dev libxcb-dri2-0-dev libxcb-icccm4-dev libxcursor-dev \
    libxkbcommon-dev libxcb-shape0-dev libxcb-xfixes0-dev
  ```

No audio libraries are needed: chopdrop decodes WAV, AIFF, FLAC, MP3 and
OGG/Vorbis itself.

## 1. Build

Get the source code (on GitHub, **Code → Download ZIP**, or `git clone` the
repository), then from its root folder run:

```sh
cargo xtask bundle chopdrop --release
```

The finished plugin is `target/bundled/chopdrop.clap` (a single file on Linux).

> **Using the Flatpak version of Bitwig, or a rolling-release distro like
> Arch?** A plugin only loads if it was built against a C library (glibc) no
> newer than the one Bitwig runs with. The Flatpak brings its own, older
> glibc, so a plugin built on an up-to-date system can fail to load. Build it
> in an Ubuntu 24.04 container instead (needs Docker; you don't need Rust,
> clang or any of the packages above installed for this: the container
> brings its own, including the clang and libclang the time-stretching
> library is built with). The result works with the Flatpak and with most
> Linux systems:
>
> ```sh
> ./scripts/build-in-docker.sh
> ```
>
> (Run it with `sudo` if your user isn't in the `docker` group.) The first run
> takes a while to set up the container. The plugin lands in the same place,
> `target/bundled/chopdrop.clap`.
>
> If you have already built chopdrop on your own system, delete the `target`
> folder before the first container build: both builds keep their files
> there, and leftovers from the one on your system can get mixed into the
> container's plugin.

## 2. Install

Copy it into your user CLAP folder:

```sh
mkdir -p ~/.clap
cp target/bundled/chopdrop.clap ~/.clap/
```

To install it for every user on the machine instead, copy it to
`/usr/lib/clap/` (needs `sudo`).

> **Developing chopdrop?** Symlink instead of copying, so every rebuild is
> picked up without reinstalling:
> `ln -s "$PWD/target/bundled/chopdrop.clap" ~/.clap/`

Copy or replace the file with Bitwig closed. Bitwig only scans `~/.clap` when
a project is open, and it remembers a plugin that failed to load.

## 3. Load it in Bitwig

1. Open **Settings → Locations → Plug-in Locations** and check that `~/.clap`
   is scanned (it is by default).
2. Click **Rescan**, or restart Bitwig.
3. In the device browser, search for **chopdrop**. It is an **instrument**, so
   it goes at the end of a track's chain. To play it from
   [bitstep](https://github.com/pywkt/bitstep), put the two on one track in
   that order: **bitstep → chopdrop**.
4. Click the device's plugin-window button to open the lane list.

If it installed correctly:

- chopdrop is listed with the instruments, not with the note effects.
- On the track it shows a note input and a stereo audio output. It has 15
  more stereo outputs for lanes you send elsewhere; Bitwig gives them their
  own chains when you click **Add Missing Chains** on the device (see
  [outputs](user-guide.md#the-lane-panel)).
- Its window opens with sixteen lanes — "Lane 1" on C1 through "Lane 16" on D#2 —
  each saying "drop a sample here", with readable text.

Continue with the [user guide](user-guide.md).

## Updating

Pull the new source, rebuild (step 1), copy the file again (step 2), then
rescan in Bitwig or restart it. Bitwig caches plugin information, so a rescan
is the safest way to make sure it loads the new build.

Projects saved with an older version open unchanged, with their samples: every
project carries its own copy of each sample file (see
[Saving](user-guide.md#saving-and-reopening-a-project)).

## Uninstalling

```sh
rm ~/.clap/chopdrop.clap
```

Projects that used chopdrop will open with the device marked as missing. Their
samples are inside the project files, so reinstalling brings the sounds back.

## Troubleshooting

| Problem | What to try |
|---|---|
| chopdrop doesn't appear in the browser | Check the file is at `~/.clap/chopdrop.clap`, then rescan. Then look for `chopdrop` in Bitwig's log, `~/.BitwigStudio/log/BitwigStudio.log`: `grep -i chopdrop ~/.BitwigStudio/log/*.log` |
| The log says ``version `GLIBC_2.xx' not found`` | The plugin was built on a system with a newer C library than Bitwig uses. Build it in the container (see the note under [step 1](#1-build)), copy it again and rescan. |
| It still doesn't load after a fixed build | Bitwig remembers the failure. With Bitwig closed, move `~/.BitwigStudio/cache/clap-metadata-13` aside, then start Bitwig and rescan. |
| The container build stops with a linking error, or the plugin it builds still won't load | Builds on your own system and builds in the container write to the same `target` folder, so parts of an earlier build can end up in the container's plugin. Delete the `target` folder, then run `./scripts/build-in-docker.sh` again. |
| The build fails with a missing `xcb`, `X11` or `GL` library | Install the system packages listed under [What you need](#what-you-need). |
| The build stops in `signalsmith-stretch` saying it is `Unable to find libclang`, or `'stddef.h' file not found` | The time-stretching library needs clang and libclang to build. Install `clang` (Arch) or `clang libclang-dev` (Debian/Ubuntu) as listed under [What you need](#what-you-need), or build in the container (see the note under [step 1](#1-build)), which has them. |
| The editor window opens but shows no text | The plugin was built without egui's bundled fonts. Build with the command in step 1 from an unmodified checkout. |
| Bitwig still runs the old version after an update | Rescan plug-ins or restart Bitwig. |
| No sound | Check a lane has a sample in it, that its note matches what you are playing, and that its volume and the Master volume are up. Click the lane's play button to hear it on its own. |
| Dragging from Bitwig's browser does nothing | Bitwig's factory drum packages hold `.bwclip` clips as well as plain files. A clip can't be loaded and the lane says so; drag a `.wav` instead. |

## Checking a build (developers)

To see the newest glibc a build needs (it must be no newer than the one
Bitwig runs with; the Flatpak's runtime 25.08 has 2.42):

```sh
objdump -T target/bundled/chopdrop.clap | grep -o 'GLIBC_[0-9.]*' | sort -V | tail -1
```

To run the CLAP validator against a build:

```sh
clap-validator validate target/bundled/chopdrop.clap
```

It should exit 0. Use `validate <path>` rather than `list plugins`, because
clap-validator 0.4.1's scanner doesn't see flat `.clap` files on Linux.
