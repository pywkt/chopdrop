# Offering the vendored patches upstream

The two crates under `vendor/` are patched copies of crates.io releases,
redirected with `[patch.crates-io]` in the workspace root `Cargo.toml`. Both
patches fix general drag-and-drop bugs — nothing in either is specific to this
plugin — so both should be offered upstream. Until they land, this project
carries two forked crates that have to be re-patched by hand on every version
bump, which is reason enough to get them merged.

This file is the write-up to file them from: for each patch, where it goes,
what it is based on, what stock does wrong, what the patch does, the evidence,
and the caveats an upstream maintainer will want flagged rather than
discovered.

Extract either diff against the commit that vendored the pristine sources
(`6705c38`, "Vendor baseview 0.4.1 and egui-baseview 0.9.0 unpatched"):

```sh
git diff 6705c38 HEAD -- vendor/baseview
git diff 6705c38 HEAD -- vendor/egui-baseview
```

Each changed file carries a comment block at the top saying what changed and
why; an upstream PR would keep the *why* in the code comments and drop the
"local patch" framing.

## Patch 1 — baseview: parse the `file:` URIs drag sources actually send

| | |
| --- | --- |
| Upstream | <https://github.com/RustAudio/baseview> |
| Vendored release | baseview 0.4.1 (crates.io) |
| Base commit | `8c6e6bab4b3c10f34b23dc12e878814349578777` (the release's `.cargo_vcs_info.json`) |
| Local commit | `9be813f` |
| Files | `src/lib.rs`, `src/platform/x11/mod.rs`, `src/platform/x11/drag_n_drop.rs` (+103/−33) |
| Scope | X11 only; no new dependencies (`percent-encoding` is already a Linux dependency) |

### What stock does wrong

`parse_data` in `src/platform/x11/drag_n_drop.rs` turns an XDND
`text/uri-list` payload into paths. As shipped it:

1. **Accepts `file://…` only.** A URI with no authority — `file:/path`, which
   is equally valid per the file-URI spec — is rejected as an unsupported
   protocol. Bitwig's browser sends exactly that:
   `file:/home/user/Music/Kits/BD%20606.wav`, one slash after `file:`.
2. **Treats one unparsable line as fatal for the whole drop.** Any line that
   does not match returns `Err`, so a single odd entry costs the drop every
   path that did parse.
3. **Canonicalizes every path**, so a path has to exist *and be resolvable* to
   parse at all; an `io::Error` there also fails the whole drop.
4. **Percent-decodes the entire payload before splitting it into lines**, so an
   encoded `%0A` inside a file name forges a line break and splits one path
   into two.
5. **Splits on `\r\n` only.** The uri-list spec does say `\r\n`, but several
   real drag sources send bare `\n`, and those payloads parse as one long
   garbage line.

Items 1–3 together mean Bitwig's drops are discarded outright: the plugin
window accepts the drag and then receives no paths.

### What the patch does

`parse_data` keeps its signature and delegates to a new `parse_uri_list(&str)
-> Vec<PathBuf>` plus a per-line `parse_file_uri`:

- Accepts `file:/path`, `file:///path` and `file://localhost/path`
  (host compared case-insensitively). A non-empty, non-`localhost` authority is
  refused — a remote host's files are not ours to open — as is `file://host`
  with no path, and any other scheme.
- Splits on `\n`, stripping a trailing `\r`, so both line endings work.
- Ignores empty lines and `#` comments, per the uri-list spec.
- Percent-decodes **per path, after the split**, so an encoded newline cannot
  forge a line end. The decoded bytes go into the `PathBuf` as bytes
  (`OsString::from_vec`) — a path need not be UTF-8.
- Skips an unparsable line with a `warn!` instead of failing, so a partially
  bad payload still delivers the paths that parsed.
- Canonicalizes nothing. Whether a path is readable or a supported file type is
  the host application's business, and it has a far better place to report it
  than a drop handler.
- Only a payload that yields no path at all is refused; `ParseError` loses the
  three variants that went with the old rules and gains `NoUsablePaths`.

### Evidence

`chopdrop/tests/drop_uri.rs` in this repo — 14 tests driving
`baseview::parse_uri_list` directly, covering `file:/`, `file:///`,
`file://localhost/`, percent-decoded spaces and other escapes, a literal space
left alone, several files in one payload with `\r\n` and with `\n`, order
preservation, empty and comment lines, a bad line among good ones, a non-`file:`
scheme, a remote host, degenerate input, and the encoded-newline case.

Behaviourally: this patch alone is not enough inside a plugin host. Patch 2
and patch 1b below are needed as well, because without 1b a drag source never
sends the embedded editor a message in the first place.

### Caveat for the upstream diff

`parse_uri_list` was made `pub` and re-exported from the crate root **only** so
that out-of-crate tests can reach it: a path-dependency crate's own
`#[cfg(test)]` tests never run under the consuming workspace's `cargo test`, so
there was no other way to test this from here. Upstream has in-crate tests
available and may well prefer the helper private — that part of the diff is a
packaging artefact of vendoring, not something being argued for.

## Patch 1b — baseview: drops onto an embedded (plugin) window

| | |
| --- | --- |
| Upstream | <https://github.com/RustAudio/baseview> |
| Files | `src/platform/x11/drag_n_drop.rs`, `event_loop.rs`, `xcb_connection.rs`, `Cargo.toml` |

### What stock does wrong

baseview puts `XdndAware` on its own window only. A plugin editor is a
*child* of the host's top-level window, and XDND drag sources look for
`XdndAware` on the top-level window under the pointer, never on its children.
So no source ever sends an embedded baseview window a single XDND message,
and dropping onto a plugin editor can't work. Found in Bitwig 6.1.4 on
2026-11-02, with patches 1 and 2 already applied.

The textbook answer, `XdndProxy` on the host's top-level window, doesn't
work with every source. Bitwig ignores it and keeps addressing the host's
window, whose owner (Bitwig's plugin-host process) never answers. Bitwig then
waits forever for an `XdndStatus`, and its UI freezes. So the host's window is
never touched.

Smaller problems in the same state machine, fixed alongside:

- `SelectionNotify` was ignored unless its `time` equalled the request's
  exactly. An owner that answers with `CurrentTime` left the session waiting
  for data forever, with no `XdndStatus` sent.
- A refused conversion (`property = None`) was filtered out in the event loop
  (it matched on the property, not the selection) and also hung the session.
- An accepting `XdndStatus`/`XdndFinished` with no known action said `None`,
  which sources read as a refusal. It now says `XdndActionCopy`.
- A failed `ConvertSelection` on the first position set `PermanentlyRejected`
  and then fell through and overwrote it with `WaitingForData`.

### What the patch does

`DropOverlay` gives the source a top-level window of ours to find:

- Every XDND source takes ownership of `XdndSelection` when a drag starts.
  XFixes reports that (`SelectSelectionInput`, `SET_SELECTION_OWNER`), and the
  event loop then polls the pointer every 15 ms while a button is held.
- While the pointer is over the editor and the editor's own top-level window
  is the topmost window there, an **input-only, override-redirect** window
  marked `XdndAware` is mapped exactly over the editor. The source finds it
  and talks to it, and the event loop treats its messages as its own. When
  the pointer leaves, another window is on top, or the button is released, it
  is unmapped. Input-only means it draws nothing, so the app's own drop
  highlight stays visible, and the source's pointer grab means it never takes
  a click.
- Positions arrive in root coordinates and the overlay covers the editor
  exactly, so the rest of the drag handling is unchanged.
- A top-level baseview window never watches; it is found directly.

### Evidence

Probes on the user's machine against the real Bitwig: a two-connection probe
showed Bitwig addresses the top-level window only, ignoring `XdndProxy`.
Overlay probes, both coloured and input-only, received Enter → Position →
data → Drop → Finished. Then an Xvfb harness ran the real editor, spawned the
way nice-plug's CLAP wrapper does, against a source that behaves like Bitwig
(top-level window under the pointer only, button held via XTEST). The drop
was delivered, the sample decoded into the lane under the pointer, and the
overlay was gone after release. Finally the user verified it in Bitwig.

### Caveat for the upstream diff

This is a workaround for sources that skip `XdndProxy`. Upstream might
prefer it opt-in, or combined with `XdndProxy` for sources that honour it.
It needs the XFixes extension; without it the overlay stays inert.

## Patch 2 — egui-baseview: forward drag events into egui

| | |
| --- | --- |
| Upstream | <https://codeberg.org/RustAudio/egui-baseview> (Codeberg, not GitHub) |
| Vendored release | egui-baseview 0.9.0 (crates.io) |
| Base commit | `1f645a35d25fd63784dcf2f91c6d72d467c6036b` (the release's `.cargo_vcs_info.json`) |
| Local commits | `9b5960b`, then `af57e15` (which added `push_drag_pointer`) |
| Files | `src/window.rs` only (+133/−5 for the two commits combined) |
| Scope | platform-independent; no new dependencies |

### What stock does wrong

In `EguiWindow::on_event`, every one of baseview's drag events —
`MouseEvent::DragEntered`, `DragMoved`, `DragLeft`, `DragDropped` — falls into
the `_ => {}` catch-all of the `MouseEvent` match. Consequently:

1. `RawInput::hovered_files` and `RawInput::dropped_files` are never filled, so
   an egui app built on this integration cannot see dropped files at all. Every
   egui drag-and-drop example is dead code here.
2. The handler never returns `EventStatus::AcceptDrop`. On platforms that honour
   that status the drag source refuses the drop before any data is transferred;
   X11 happens to deliver anyway, but the paths then go nowhere (point 1).

### What the patch does

- `DragEntered` / `DragMoved` carrying `DropData::Files`: set
  `RawInput::hovered_files` (one `egui::HoveredFile` per path) and return
  `EventStatus::AcceptDrop(DropEffect::Copy)`. Non-file drag data clears the
  hover and keeps the stock status.
- `DragLeft`: clear `hovered_files`.
- `DragDropped` carrying `DropData::Files`: append one dropped-file handle per
  path to `RawInput::dropped_files`, clear the hover, and accept with
  `DropEffect::Copy`.
- Each of those arms requests a redraw, so the app sees the drop on the next
  frame rather than whenever something else happens to cause one.
- The trailing `baseview::Event::Mouse(_)` fallback no longer overwrites the
  status those arms chose: it used to replace it with `Captured`/`Ignored`
  depending on egui's pointer interest, which threw the `AcceptDrop` away. Drag
  events now return the status the arm decided; all other mouse events keep the
  stock behaviour.
- **Every drag event's own `position` is pushed as an
  `egui::Event::PointerMoved`** (`push_drag_pointer`), converted to logical
  points exactly as the `CursorMoved` arm does. This is the non-obvious half:
  X11 sends the target window **no pointer motion events while a drag is
  held** — the drag source owns the pointer grab, and the pointer position
  exists only on the XDND messages baseview turns into these drag events.
  Without it, egui knows files are over the window but reports them hovering at
  a stale pointer position, or at none at all, and the app cannot tell which of
  its widgets a drop is aimed at. In this plugin that is the difference between
  "a file was dropped" and "a file was dropped on *that* lane".

### Evidence

Behaviour here is verified by hand, not by test: drag-and-drop needs a real X11
drag source, and this project's CI container has no display. On a Linux desktop
with Bitwig, at the two manual drag-and-drop checkpoints of this project's
build (2026-11-01 and later):

- dragging a sample from Bitwig's browser onto the plugin window delivers its
  path to the egui app;
- the same from a file manager works, including multi-file selections;
- the hover highlight tracks the row under the pointer *during* the drag, and a
  drop lands on the row it was released over — which is what the
  `PointerMoved` part of the patch buys.

The paths that reach egui come from patch 1; the two were verified together.

### Caveat for the upstream diff

The dropped-file side is shaped to **egui 0.37**, which takes dropped files as
`egui::DroppedFileHandle` (`Arc<dyn egui::DroppedFile>`) and so leaves the
concrete handle to the integration — hence the small `BaseviewDroppedFile`
struct implementing `path()` and `bytes()`. Older egui versions use a plain
`egui::DroppedFile` struct with `path`/`bytes` fields instead. That part needs
re-stating against whatever egui the upstream branch tracks; the drag-event
plumbing, the status fix and `push_drag_pointer` are version-independent.

## If a patch lands upstream

Drop the corresponding `vendor/` directory and its `[patch.crates-io]` entry in
the root `Cargo.toml`, bump the dependency to the release that contains the fix,
and re-run the manual drag-and-drop checks — `cargo test` covers patch 1's
parsing but nothing in the container can exercise a real drag. Keep
`chopdrop/tests/drop_uri.rs` only if the parser is still reachable from outside
baseview (see patch 1's caveat); otherwise delete it with the vendored copy.
