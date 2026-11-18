// Local patch — offer upstream (chopdrop, 2026-11-01; see `vendor/UPSTREAMING.md`
// §Patch 1).
//
// Changed: the `text/uri-list` parsing at the bottom of this file — `parse_data`
// now delegates to a new public `parse_uri_list` helper (re-exported from the
// crate root), and `ParseError` lost the variants that went with the old rules.
//
// Why: Bitwig's browser offers XDND URIs shaped like
// `file:/home/user/Music/BD%20606.wav` — one slash after `file:`, percent-
// encoded. Stock baseview 0.4.1 accepted `file://…` only, made any unparsable
// line fatal for the whole drop, and canonicalized every path (so a path had to
// exist to parse), which discarded Bitwig's drops entirely.
//
// Now: `file:/path`, `file:///path` and `file://localhost/path` all parse, `\n`
// ends a line as well as `\r\n`, empty and `#` comment lines are ignored, an
// unparsable line is skipped instead of failing the drop, percent-decoding is
// per path rather than over the whole payload, and no path is canonicalized
// (unreadable files are the loader's business). Only a payload that yields no
// path at all is refused. The helper is `pub` so chopdrop's integration tests can
// reach it: a vendored crate's own `#[cfg(test)]` tests never run under
// `cargo test --workspace`.
//
// Second local patch (chopdrop, 2026-11-02; same file, §Patch 1b): drops onto an
// *embedded* window. A plugin editor is a child of the host's top-level window, and an
// XDND source looks for `XdndAware` on the top-level window under the pointer, never on its
// children — so Bitwig showed "no drop" over the editor and never sent it a message. And
// Bitwig ignores `XdndProxy`, so redirecting from the host's window does not work either
// (it froze Bitwig, which then waited on the host's window for an answer). Now:
//
// - `DropOverlay`: while a drag is on (XFixes reports a new `XdndSelection` owner) and the
//   pointer is over the editor, an invisible override-redirect top-level window of ours,
//   marked `XdndAware`, sits exactly over the editor. The source talks to it directly.
// - Messages arrive addressed to the overlay, so every reply names the window the source
//   addressed (`addressed`) rather than our own.
// - A `SelectionNotify` is no longer dropped when its timestamp differs from the
//   request's (some selection owners answer with `CurrentTime`), and a refused
//   conversion (`property = None`) now rejects the drop instead of hanging it.
// - A status or finish with no known action answers `XdndActionCopy` rather
//   than `None`, which sources read as a refusal.
// - A failed `ConvertSelection` on the first position now stays rejected (it
//   fell through and overwrote the rejection).
use super::xcb_connection::{Atoms, GetPropertyError};
use super::*;
use crate::dpi::PhysicalPosition;
use crate::handler::WindowHandler;
use crate::platform::x11::error::{CookieExt, ReplyExt};
use crate::warn;
use crate::{DropData, Event, MouseEvent};
use core::result::Result;
use keyboard_types::Modifiers;
use percent_encoding::percent_decode;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::{mem, path::PathBuf, str::Utf8Error};
use x11rb::connection::Connection;
use x11rb::cookie::VoidCookie;
use x11rb::errors::ConnectionError;
use x11rb::protocol::xproto::{Atom, ClientMessageEvent, SelectionNotifyEvent, Timestamp};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;
use x11rb::{
    protocol::xproto::{self, ConnectionExt},
    x11_utils::Serialize,
};
use DragNDropState::*;

/// The Drag-N-Drop session state of a `baseview` X11 window, for which it is the target.
///
/// For more information about what the heck is going on here, see the
/// [XDND (X Drag-n-Drop) specification](https://www.freedesktop.org/wiki/Specifications/XDND/).
pub(crate) enum DragNDropState {
    /// There is no active XDND session for this window.
    NoCurrentSession,
    /// At some point in this session's lifetime, we have decided we couldn't possibly handle the
    /// source's Drop data. Every request from this source window from now on will be rejected,
    /// until either a Leave or Drop event is received.
    PermanentlyRejected {
        /// The source window the rejected drag session originated from.
        source_window: xproto::Window,
    },
    /// We have registered a new session (after receiving an Enter event), and are now waiting
    /// for a position event.
    WaitingForPosition {
        /// The protocol version used in this session.
        protocol_version: u8,
        /// The source window the current drag session originates from.
        source_window: xproto::Window,
    },
    /// We have performed a request for data (via `XConvertSelection`), and are now waiting for a
    /// reply.
    ///
    /// More position events can still be received to further update the position data.
    WaitingForData {
        /// The protocol version used in this session.
        protocol_version: u8,
        /// The source window the current drag session originates from.
        source_window: xproto::Window,
        /// The window the source addresses its messages to: ours, or a proxied ancestor.
        addressed: xproto::Window,
        /// The current position of the pointer, from the last received position event.
        position: PhysicalPosition<i16>,
        /// The timestamp of the event we made the selection request from.
        ///
        /// This is either from the first position event, or from the drop event if it arrived first.
        ///
        /// In very old versions of the protocol (v0), this timestamp isn't provided. In that case,
        /// this will be `None`.
        requested_at: Option<Timestamp>,

        requested_action: Option<DndAction>,
        /// This will be true if we received a drop event *before* we managed to fetch the data.
        ///
        /// If this is true, this means we must complete the drop upon receiving the data, instead
        /// of just going to [`Ready`].
        dropped: bool,
    },
    /// We have completed our quest for the drop data. All fields are populated, and the
    /// [`WindowHandler`] has been notified about the drop session.
    ///
    /// We are now waiting for the user to either drop the file, or leave the window.
    ///
    /// More position events can still be received to further update the position data.
    Ready {
        /// The protocol version used in this session.
        protocol_version: u8,
        /// The source window the current drag session originates from.
        source_window: xproto::Window,
        position: PhysicalPosition<i16>,
        data: DropData,
        requested_action: Option<DndAction>,
    },
}

// Note: For all X11 operations on this type, only ConnectionErrors are considered fatal.
// Other errors (protocol errors, transfer errors) should be dealt with as gracefully as possible.
impl DragNDropState {
    pub fn handle_enter_event(
        &mut self, window: &WindowInner, handler: &dyn WindowHandler, event: &ClientMessageEvent,
    ) -> Result<(), ConnectionError> {
        let data = event.data.as_data32();

        let source_window = data[0] as xproto::Window;
        let [protocol_version, _, _, flags] = data[1].to_be_bytes();

        // Fetch the list of supported data types. It can be either stored inline in the event, or
        // in a separate property on the source window.
        const FLAG_HAS_MORE_TYPES: u8 = 1 << 0;
        let has_more_types = (FLAG_HAS_MORE_TYPES & flags) == FLAG_HAS_MORE_TYPES;

        let extra_types;
        let supported_types = if !has_more_types {
            &data[2..5]
        } else {
            let result = window.connection.get_property(
                source_window,
                window.connection.atoms.XdndTypeList,
                xproto::Atom::from(xproto::AtomEnum::ATOM),
            );

            match result {
                Err(GetPropertyError::ConnectionError(e)) => return Err(e),
                Ok(v) => {
                    extra_types = v;
                    &extra_types
                }
                Err(e) => {
                    warn!("XdndEnter: Failed to fetch XdndTypeList from source window {source_window}: {}", e);
                    &data[2..5]
                }
            }
        };

        // We only support the TextUriList type
        let data_type_supported = supported_types.contains(&window.connection.atoms.TextUriList);

        // If there was an active drag session that we informed the handler about, we need to
        // generate the matching DragLeft before cancelling the previous session.
        let interrupted_active_drag = matches!(self, Ready { .. });

        // Clear any previous state, and mark the new session as started if we can handle the drop.
        *self = if data_type_supported {
            WaitingForPosition { source_window, protocol_version }
        } else {
            // Permanently reject the drop if the data isn't supported.
            PermanentlyRejected { source_window }
        };

        // Do this at the end, in case the handler panics, so that it doesn't poison our internal state.
        if interrupted_active_drag {
            handler.on_event(Event::Mouse(MouseEvent::DragLeft));
        }

        Ok(())
    }

    pub fn handle_position_event(
        &mut self, window: &WindowInner, handler: &dyn WindowHandler, event: &ClientMessageEvent,
    ) -> Result<(), ConnectionError> {
        let event_data = event.data.as_data32();

        let event_source_window = event_data[0] as xproto::Window;
        let addressed = event.window;
        let (event_x, event_y) = decode_xy(event_data[2]);

        match self {
            // Someone sent us a position event without first sending an enter event.
            // Weird, but we'll still politely tell them we reject the drop.
            NoCurrentSession => Ok(send_status_rejected(event_source_window, addressed, window)?),

            // The current session's source window does not match the given event.
            // This means it can either be from a stale session, or a misbehaving app.
            // In any case, we ignore the event but still tell the source we reject the drop.
            WaitingForPosition { source_window, .. }
            | PermanentlyRejected { source_window, .. }
            | WaitingForData { source_window, .. }
            | Ready { source_window, .. }
                if *source_window != event_source_window =>
            {
                Ok(send_status_rejected(event_source_window, addressed, window)?)
            }

            // We decided to permanently reject this drop.
            // This means the WindowHandler can't do anything with the data, so we reject the drop.
            PermanentlyRejected { .. } => {
                Ok(send_status_rejected(event_source_window, addressed, window)?)
            }

            // This is the position event we were waiting for. Now we can request the selection data.
            // The code above already checks that source_window == event_source_window.
            WaitingForPosition { protocol_version, source_window } => {
                let protocol_version = *protocol_version;
                let source_window = *source_window;

                // In version 0, time isn't specified
                let timestamp = (protocol_version >= 1).then_some(event_data[3] as Timestamp);
                // In version <2, action isn't specified
                let requested_action = (protocol_version >= 2).then_some(event_data[4] as Atom);
                let requested_action = requested_action.map(|a| {
                    DndAction::from_atom(a, &window.connection.atoms).unwrap_or(DndAction::Private)
                });

                if let Err(e) = request_convert_selection(window, timestamp)?.check() {
                    warn!("XdndPosition: ConvertSelection request failed: {}", e);

                    // Permanently reject the drop if we couldn't convert selection
                    *self = PermanentlyRejected { source_window };
                    return send_status_rejected(event_source_window, addressed, window);
                }

                // We set our state before translating position data, in case that fails.
                *self = WaitingForData {
                    protocol_version,
                    requested_at: timestamp,
                    source_window: event_source_window,
                    addressed,
                    position: PhysicalPosition::new(0, 0),
                    requested_action,
                    dropped: false,
                };

                let WaitingForData { position, .. } = self else { unreachable!() };

                if let Some(result) = translate_root_coordinates(window, event_x, event_y)? {
                    *position = result;
                }

                Ok(())
            }

            // We are still waiting for the data. So we'll just update the position in the meantime.
            WaitingForData { position, .. } => {
                if let Some(result) = translate_root_coordinates(window, event_x, event_y)? {
                    *position = result;
                }

                Ok(())
            }

            // We have already received the data. We can update the position and notify the handler
            Ready { protocol_version, position, data, requested_action, .. } => {
                // In version <2, action isn't specified
                *requested_action = if *protocol_version < 2 {
                    None
                } else {
                    DndAction::from_atom(event_data[4] as Atom, &window.connection.atoms)
                };

                // Inform the source that we are still accepting the drop.
                send_status_event(event_source_window, addressed, window, *requested_action)?;

                if let Some(result) = translate_root_coordinates(window, event_x, event_y)? {
                    *position = result;
                }

                handler.on_event(Event::Mouse(MouseEvent::DragMoved {
                    position: position.cast(),
                    data: data.clone(),
                    // We don't get modifiers for drag n drop events.
                    modifiers: Modifiers::empty(),
                }));

                Ok(())
            }
        }
    }

    pub fn handle_leave_event(&mut self, handler: &dyn WindowHandler, event: &ClientMessageEvent) {
        let data = event.data.as_data32();
        let event_source_window = data[0] as xproto::Window;

        let current_source_window = match self {
            NoCurrentSession => return,
            WaitingForPosition { source_window, .. }
            | PermanentlyRejected { source_window, .. }
            | WaitingForData { source_window, .. }
            | Ready { source_window, .. } => *source_window,
        };

        // Only accept the leave event if it comes from the source window of the current drag session.
        if event_source_window != current_source_window {
            return;
        }

        // If there was an active drag session that we informed the handler about, we need to
        // generate the matching DragLeft before cancelling the previous session.
        let left_active_drag = matches!(self, Ready { .. });

        // Clear everything.
        *self = NoCurrentSession;

        // Do this at the end, in case the handler panics, so that it doesn't poison our internal state.
        if left_active_drag {
            handler.on_event(Event::Mouse(MouseEvent::DragLeft));
        }
    }

    pub fn handle_drop_event(
        &mut self, window: &WindowInner, handler: &dyn WindowHandler, event: &ClientMessageEvent,
    ) -> Result<(), ConnectionError> {
        let data = event.data.as_data32();

        let event_source_window = data[0] as xproto::Window;
        let addressed = event.window;

        match self {
            // Someone sent us a position event without first sending an enter event.
            // Weird, but we'll still politely tell them we reject the drop.
            NoCurrentSession => send_finished_rejected(event_source_window, addressed, window)?,

            // The current session's source window does not match the given event.
            // This means it can either be from a stale session, or a misbehaving app.
            // In any case, we ignore the event but still tell the source we reject the drop.
            WaitingForPosition { source_window, .. }
            | PermanentlyRejected { source_window, .. }
            | WaitingForData { source_window, .. }
            | Ready { source_window, .. }
                if *source_window != event_source_window =>
            {
                send_finished_rejected(event_source_window, addressed, window)?;
            }

            // We decided to permanently reject this drop.
            // This means the WindowHandler can't do anything with the data, so we reject the drop.
            PermanentlyRejected { .. } => {
                *self = NoCurrentSession;

                send_finished_rejected(event_source_window, addressed, window)?
            }

            // We received a drop event without any position event. That's very weird, but not
            // irrecoverable: the drop event provides enough data as it is.
            // The code above already checks that source_window == event_source_window.
            WaitingForPosition { protocol_version, source_window } => {
                // In version 0, time isn't specified
                let timestamp = (*protocol_version >= 1).then_some(data[2] as Timestamp);

                // We have the timestamp, we can use it to request to convert the selection,
                // even in this state.

                // If we fail to send the request when the drop has completed, we can't do anything.
                // Just cancel the drop.
                if let Err(e) = request_convert_selection(window, timestamp)?.check() {
                    warn!("XdndDrop: ConvertSelection request failed: {}", e);
                    *self = PermanentlyRejected { source_window: *source_window };

                    // Try to inform the source that we ended up rejecting the drop.
                    return send_finished_rejected(event_source_window, addressed, window);
                };

                *self = WaitingForData {
                    protocol_version: *protocol_version,
                    requested_at: timestamp,
                    source_window: event_source_window,
                    addressed,
                    // We don't have usable position data. Maybe we'll receive a position later,
                    // but otherwise this will have to do.
                    position: PhysicalPosition::new(0, 0),
                    requested_action: Some(DndAction::Private),
                    dropped: true,
                };
            }

            // We are still waiting to receive the data.
            // In that case, we'll wait to receive all of it before finalizing the drop.
            WaitingForData { dropped, requested_at, .. } => {
                // If we have a timestamp, that means this is version >= 1.
                if let Some(requested_at) = *requested_at {
                    let event_timestamp = data[2] as Timestamp;

                    // Just in case, check if this drop event isn't stale
                    if requested_at > event_timestamp {
                        return Ok(());
                    }
                }

                // Indicate to the selection_notified handler that the user has performed the drop.
                // Now it should complete the drop instead of just waiting for more events.
                *dropped = true;
            }

            // The normal case.
            Ready { .. } => {
                let Ready { data, position, requested_action, .. } =
                    mem::replace(self, NoCurrentSession)
                else {
                    unreachable!()
                };

                send_finished_event(event_source_window, addressed, window, requested_action)?
                    .check_warn();

                handler.on_event(Event::Mouse(MouseEvent::DragDropped {
                    position: position.cast(),
                    data,
                    // We don't get modifiers for drag n drop events.
                    modifiers: Modifiers::empty(),
                }));
            }
        };

        Ok(())
    }

    pub fn handle_selection_notify_event(
        &mut self, window: &WindowInner, handler: &dyn WindowHandler, event: &SelectionNotifyEvent,
    ) -> Result<(), ConnectionError> {
        // Ignore the event if we weren't actually waiting for a selection notify event
        let WaitingForData {
            source_window,
            addressed,
            position,
            dropped,
            protocol_version,
            requested_action,
            ..
        } = *self
        else {
            return Ok(());
        };

        // Ignore if this was meant for another window (?)
        if event.requestor != window.xcb_window.id().get() {
            return Ok(());
        }

        // Local patch (chopdrop): no timestamp check here. Only one conversion is ever
        // outstanding, and owners that answer with `CurrentTime` instead of the
        // request's time had their data dropped, leaving the drag hanging.

        // The owner refused the conversion (`property = None`), or set the data on
        // our window, which we fetch.
        let fetched = if event.property == x11rb::NONE { None } else { fetch_dnd_data(window)? };

        match fetched {
            None => {
                *self = PermanentlyRejected { source_window };

                if dropped {
                    send_finished_rejected(source_window, addressed, window)?;
                } else {
                    send_status_rejected(source_window, addressed, window)?;
                }
            }
            Some(data) => {
                // Inform the source that we are (still) accepting the drop.

                // Handle the case where the user already dropped, but we only received the data later.
                if dropped {
                    *self = NoCurrentSession;

                    let reply_result =
                        send_finished_event(source_window, addressed, window, requested_action);

                    // Now that we have actual drop data, we can inform the handler about the drag AND drop events.
                    handler.on_event(Event::Mouse(MouseEvent::DragEntered {
                        position: position.cast(),
                        data: data.clone(),
                        // We don't get modifiers for drag n drop events.
                        modifiers: Modifiers::empty(),
                    }));

                    handler.on_event(Event::Mouse(MouseEvent::DragDropped {
                        position: position.cast(),
                        data: data.clone(),
                        // We don't get modifiers for drag n drop events.
                        modifiers: Modifiers::empty(),
                    }));

                    reply_result?.check_warn();
                } else {
                    // Save the data, now that we finally have it!
                    *self = Ready {
                        data: data.clone(),
                        source_window,
                        position,
                        requested_action,
                        protocol_version,
                    };

                    let reply_result =
                        send_status_event(source_window, addressed, window, requested_action);

                    // Now that we have actual drop data, we can inform the handler about the drag event.
                    handler.on_event(Event::Mouse(MouseEvent::DragEntered {
                        position: position.cast(),
                        data,
                        // We don't get modifiers for drag n drop events.
                        modifiers: Modifiers::empty(),
                    }));

                    reply_result?;
                }
            }
        }

        Ok(())
    }
}

fn send_status_rejected(
    source_window: xproto::Window, addressed: xproto::Window, window: &WindowInner,
) -> Result<(), ConnectionError> {
    let conn = &window.connection;

    let event = ClientMessageEvent {
        response_type: xproto::CLIENT_MESSAGE_EVENT,
        window: source_window,
        format: 32,
        data: [addressed, 0, 0, 0, conn.atoms.None as _].into(),
        sequence: 0,
        type_: conn.atoms.XdndStatus,
    };

    conn.conn
        .send_event(false, source_window, xproto::EventMask::NO_EVENT, event.serialize())?
        .check_warn();

    Ok(())
}

/// The action we tell the source we perform. Local patch (chopdrop): an unknown or
/// unspecified action answers `Copy` (XDND's default) rather than `None`, which a
/// source reads as a refusal.
fn accepted_action(action: Option<DndAction>, atoms: &Atoms) -> Atom {
    action.unwrap_or(DndAction::Copy).to_atom(atoms)
}

fn send_status_event(
    source_window: xproto::Window, addressed: xproto::Window, window: &WindowInner,
    action: Option<DndAction>,
) -> Result<(), ConnectionError> {
    let conn = &window.connection;

    let action = accepted_action(action, &window.connection.atoms);

    let event = ClientMessageEvent {
        response_type: xproto::CLIENT_MESSAGE_EVENT,
        window: source_window,
        format: 32,
        data: [addressed, 1, 0, 0, action as _].into(),
        sequence: 0,
        type_: conn.atoms.XdndStatus,
    };

    conn.conn.send_event(false, source_window, xproto::EventMask::NO_EVENT, event.serialize())?;

    conn.conn.flush()?;
    Ok(())
}

pub fn send_finished_rejected(
    source_window: xproto::Window, addressed: xproto::Window, window: &WindowInner,
) -> Result<(), ConnectionError> {
    let conn = &window.connection;

    let event = ClientMessageEvent {
        response_type: xproto::CLIENT_MESSAGE_EVENT,
        window: source_window,
        format: 32,
        data: [addressed, 1, window.connection.atoms.None as _, 0, 0].into(),
        sequence: 0,
        type_: conn.atoms.XdndFinished as _,
    };

    conn.conn
        .send_event(false, source_window, xproto::EventMask::NO_EVENT, event.serialize())?
        .check_warn();

    Ok(())
}

fn send_finished_event(
    source_window: xproto::Window, addressed: xproto::Window, window: &WindowInner,
    action: Option<DndAction>,
) -> Result<VoidCookie<'_, XCBConnection>, ConnectionError> {
    let conn = &window.connection;
    let action = accepted_action(action, &window.connection.atoms);

    let event = ClientMessageEvent {
        response_type: xproto::CLIENT_MESSAGE_EVENT,
        window: source_window,
        format: 32,
        data: [addressed, 1, action as _, 0, 0].into(),
        sequence: 0,
        type_: conn.atoms.XdndFinished as _,
    };

    conn.conn.send_event(false, source_window, xproto::EventMask::NO_EVENT, event.serialize())
}

/// How often the pointer is looked at while a drag is in progress (see [`DropOverlay`]).
pub(crate) const OVERLAY_POLL: std::time::Duration = std::time::Duration::from_millis(15);

/// Local patch (chopdrop): makes an embedded window a drop target for sources that only talk
/// to top-level windows.
///
/// A plugin editor is a child of the host's top-level window, and an XDND source looks for
/// `XdndAware` on the top-level window under the pointer and sends its messages there. Some
/// sources (GTK, Qt) honour the spec's `XdndProxy` redirection, but Bitwig does not: it
/// addresses the host's window itself, and X11 delivers those messages only to the client
/// that created that window, never to us. Marking the host's window `XdndAware` therefore
/// only made Bitwig wait for an answer that nobody sends.
///
/// So while a drag is on, we put a top-level window of our own under the pointer: an
/// invisible (input-only), override-redirect window marked `XdndAware`, placed exactly
/// over our editor and stacked above everything. The source finds it, talks to it, and its
/// messages reach this connection, where the event loop hands them to [`DragNDropState`] as
/// if they had been sent to our own window. Positions arrive in root coordinates and the
/// overlay covers the editor exactly, so nothing else in the drag handling changes.
///
/// A drag is noticed through XFixes: every XDND source takes ownership of `XdndSelection`
/// when a drag starts. From then on the pointer is polled every [`OVERLAY_POLL`]: the
/// overlay is shown while a button is held, the pointer is over the editor and the editor's
/// own top-level window is the topmost window there (so it never covers another window
/// lying over the editor), and taken away as soon as any of that stops being true. An
/// input-only window draws nothing, so the editor's own drop highlight stays visible
/// through it; and the source holds the pointer grab during a drag, so the overlay never
/// takes a click from the editor.
///
/// Only an embedded window needs this: a top-level baseview window is found directly.
pub(crate) struct DropOverlay {
    /// The overlay window; `None` when the server has no XFixes, or creating it failed.
    overlay: Option<xproto::Window>,
    /// A drag is in progress and the pointer is being watched.
    watching: bool,
    /// The overlay is mapped.
    mapped: bool,
    /// The root window's child holding our window: the top-level window the source sees
    /// under the pointer when the editor is in view. Found when a drag starts.
    top_level: Option<xproto::Window>,
}

impl DropOverlay {
    /// Creates the (unmapped) overlay and asks the server to tell us when a drag starts.
    /// A server without XFixes leaves the overlay inert; drops then work only for sources
    /// that find our window some other way.
    pub fn new(window: &WindowInner) -> Result<Self, ConnectionError> {
        let mut this =
            DropOverlay { overlay: None, watching: false, mapped: false, top_level: None };

        let conn = &window.connection;
        let root = conn.default_screen().root;

        match conn.conn.xfixes_query_version(5, 0)?.reply() {
            Ok(_) => {}
            Err(e) => {
                warn!("XFixes unavailable, drops onto an embedded window will not work: {}", e);
                return Ok(this);
            }
        }
        if !conn
            .conn
            .xfixes_select_selection_input(
                root,
                conn.atoms.XdndSelection,
                xfixes::SelectionEventMask::SET_SELECTION_OWNER,
            )?
            .check_is_ok()
        {
            return Ok(this);
        }

        let overlay = conn.conn.generate_id().map_err(|e| match e {
            x11rb::errors::ReplyOrIdError::ConnectionError(e) => e,
            _ => ConnectionError::UnknownError,
        })?;
        let created = conn
            .conn
            .create_window(
                0,
                overlay,
                root,
                0,
                0,
                1,
                1,
                0,
                xproto::WindowClass::INPUT_ONLY,
                0,
                &xproto::CreateWindowAux::new().override_redirect(1),
            )?
            .check_is_ok();
        if !created {
            return Ok(this);
        }
        conn.conn
            .change_property32(
                xproto::PropMode::REPLACE,
                overlay,
                conn.atoms.XdndAware,
                xproto::AtomEnum::ATOM,
                &[5u32],
            )?
            .check_warn();
        // Some sources only accept a window the window manager manages; say it is one.
        conn.conn
            .change_property32(
                xproto::PropMode::REPLACE,
                overlay,
                conn.atoms.WM_STATE,
                conn.atoms.WM_STATE,
                &[1u32, 0],
            )?
            .check_warn();

        this.overlay = Some(overlay);
        Ok(this)
    }

    /// Whether `window` is the overlay, whose drag messages are ours.
    pub fn addresses(&self, window: xproto::Window) -> bool {
        self.overlay == Some(window)
    }

    /// Whether the pointer is being watched, so the event loop keeps polling.
    pub fn is_watching(&self) -> bool {
        self.watching
    }

    /// A source took `XdndSelection`: a drag has started. Returns whether polling should
    /// begin (it may already be running). Only an embedded window watches.
    pub fn drag_started(&mut self, window: &WindowInner) -> Result<bool, ConnectionError> {
        if self.overlay.is_none() || self.watching {
            return Ok(false);
        }
        let root = window.connection.default_screen().root;
        self.top_level = top_level_of(window, root)?;
        // A window that is itself a top-level is found by the source without help.
        if self.top_level.is_none() || self.top_level == Some(window.xcb_window.id().get()) {
            return Ok(false);
        }
        self.watching = true;
        Ok(true)
    }

    /// One look at the pointer while a drag is on: shows or hides the overlay.
    pub fn poll(&mut self, window: &WindowInner) -> Result<(), ConnectionError> {
        let (Some(overlay), Some(top_level)) = (self.overlay, self.top_level) else {
            self.watching = false;
            return Ok(());
        };
        let conn = &window.connection;
        let root = conn.default_screen().root;
        let own = window.xcb_window.id().get();

        let pointer = conn.conn.query_pointer(root)?.reply_or_warn();
        let buttons = xproto::KeyButMask::BUTTON1 | xproto::KeyButMask::BUTTON2
            | xproto::KeyButMask::BUTTON3;
        let held = pointer.as_ref().is_some_and(|p| u16::from(p.mask) & u16::from(buttons) != 0);
        if !held {
            // The drag is over; the drop, if it was ours, is handled by the messages.
            self.watching = false;
            return self.set_mapped(window, None);
        }
        let Some(pointer) = pointer else { return Ok(()) };

        let origin = conn.conn.translate_coordinates(own, root, 0, 0)?.reply_or_warn();
        let geometry = conn.conn.get_geometry(own)?.reply_or_warn();
        let (Some(origin), Some(geometry)) = (origin, geometry) else { return Ok(()) };
        let (x, y) = (i32::from(origin.dst_x), i32::from(origin.dst_y));
        let (w, h) = (i32::from(geometry.width), i32::from(geometry.height));
        let (px, py) = (i32::from(pointer.root_x), i32::from(pointer.root_y));

        let inside = px >= x && px < x + w && py >= y && py < y + h;
        let on_top = pointer.child == top_level || (self.mapped && pointer.child == overlay);
        let rect = (inside && on_top).then_some((x, y, geometry.width, geometry.height));
        self.set_mapped(window, rect)
    }

    /// Maps the overlay over `rect` (root coordinates), or unmaps it for `None`.
    fn set_mapped(
        &mut self, window: &WindowInner, rect: Option<(i32, i32, u16, u16)>,
    ) -> Result<(), ConnectionError> {
        let Some(overlay) = self.overlay else { return Ok(()) };
        let conn = &window.connection.conn;
        match rect {
            Some((x, y, w, h)) if !self.mapped => {
                let aux = xproto::ConfigureWindowAux::new()
                    .x(x)
                    .y(y)
                    .width(u32::from(w.max(1)))
                    .height(u32::from(h.max(1)))
                    .stack_mode(xproto::StackMode::ABOVE);
                conn.configure_window(overlay, &aux)?;
                conn.map_window(overlay)?;
                conn.flush()?;
                self.mapped = true;
            }
            None if self.mapped => {
                conn.unmap_window(overlay)?;
                conn.flush()?;
                self.mapped = false;
            }
            _ => {}
        }
        Ok(())
    }

    /// Destroys the overlay, when the window closes.
    pub fn destroy(&mut self, window: &WindowInner) -> Result<(), ConnectionError> {
        self.watching = false;
        self.mapped = false;
        if let Some(overlay) = self.overlay.take() {
            window.connection.conn.destroy_window(overlay)?;
            window.connection.conn.flush()?;
        }
        Ok(())
    }
}

/// The child of `root` that holds our window (our window itself when it is a top-level),
/// or `None` if the tree cannot be read.
fn top_level_of(
    window: &WindowInner, root: xproto::Window,
) -> Result<Option<xproto::Window>, ConnectionError> {
    let mut current = window.xcb_window.id().get();
    // Hosts nest an editor one or two windows deep; the bound only stops a broken tree.
    for _ in 0..16 {
        let Some(tree) = window.connection.conn.query_tree(current)?.reply_or_warn() else {
            return Ok(None);
        };
        if tree.parent == root || tree.parent == x11rb::NONE {
            return Ok(Some(current));
        }
        current = tree.parent;
    }
    Ok(None)
}

fn request_convert_selection(
    window: &WindowInner, timestamp: Option<Timestamp>,
) -> Result<VoidCookie<'_, XCBConnection>, ConnectionError> {
    window.connection.conn.convert_selection(
        window.xcb_window.id().get(),
        window.connection.atoms.XdndSelection,
        window.connection.atoms.TextUriList,
        window.connection.atoms.XdndSelection,
        timestamp.unwrap_or(x11rb::CURRENT_TIME),
    )
}

fn decode_xy(data: u32) -> (u16, u16) {
    ((data >> 16) as u16, data as u16)
}

fn translate_root_coordinates(
    window: &WindowInner, x: u16, y: u16,
) -> Result<Option<PhysicalPosition<i16>>, ConnectionError> {
    let root_id = window.connection.default_screen().root;
    let x = x.try_into().unwrap_or(i16::MAX);
    let y = y.try_into().unwrap_or(i16::MAX);

    if root_id == window.xcb_window.id().get() {
        return Ok(Some(PhysicalPosition::new(x, y)));
    }

    let reply = window
        .connection
        .conn
        .translate_coordinates(root_id, window.xcb_window.id().get(), x, y)?
        .reply_or_warn();

    let Some(reply) = reply else { return Ok(None) };

    Ok(Some(PhysicalPosition::new(reply.dst_x, reply.dst_y)))
}

fn fetch_dnd_data(window: &WindowInner) -> Result<Option<DropData>, ConnectionError> {
    let conn = &window.connection;

    let data: Vec<u8> = match conn.get_property(
        window.xcb_window.id().get(),
        conn.atoms.XdndSelection,
        conn.atoms.TextUriList,
    ) {
        Ok(data) => data,
        Err(GetPropertyError::ConnectionError(e)) => return Err(e),
        Err(e) => {
            warn!("{}", e);
            return Ok(None);
        }
    };

    match parse_data(&data) {
        Ok(path_list) => Ok(Some(DropData::Files(path_list))),
        Err(e) => {
            warn!("{}", e);
            Ok(None)
        }
    }
}

// Parsing a `text/uri-list` drop payload.
//
// See: https://edeproject.org/spec/file-uri-spec.txt
// TL;DR: format is "file://<hostname>/<path>", hostname is optional and can be "localhost",
// and "file:/<path>" (no authority at all) is equally valid.
fn parse_data(data: &[u8]) -> Result<Vec<PathBuf>, ParseError> {
    if data.is_empty() {
        return Err(ParseError::EmptyData);
    }

    let data = std::str::from_utf8(data).map_err(ParseError::InvalidUtf8)?;

    let path_list = parse_uri_list(data);
    if path_list.is_empty() {
        return Err(ParseError::NoUsablePaths);
    }

    Ok(path_list)
}

/// Parses a `text/uri-list` drop payload into the local file paths it names.
///
/// Lines are separated by `\r\n` (as the uri-list spec says) or by a bare `\n`
/// (as several drag sources actually send); empty lines and comments (`#…`) are
/// ignored. A line that does not parse is skipped rather than fatal, so one bad
/// entry never costs the drop the paths that did parse.
///
/// Accepted forms are `file:/path`, `file:///path` and `file://localhost/path`;
/// `%XX` escapes anywhere in the path are decoded.
pub fn parse_uri_list(data: &str) -> Vec<PathBuf> {
    let mut path_list = Vec::new();

    for line in data.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        match parse_file_uri(line) {
            Some(path) => path_list.push(path),
            None => warn!("Skipping unusable Drag-n-Drop URI: {}", line),
        }
    }

    path_list
}

/// Turns a single `file:` URI into a path, or returns `None` if it names
/// something that cannot be opened locally: another scheme, a remote host, or
/// no path at all.
fn parse_file_uri(uri: &str) -> Option<PathBuf> {
    // We only support the file: protocol.
    let rest = uri.strip_prefix("file:")?;

    let path = match rest.strip_prefix("//") {
        // An authority is present: "file://<hostname>/<path>". It has to be
        // empty ("file:///path") or "localhost", since a remote host's files
        // are not ours to open.
        Some(authority) => {
            let (hostname, path) = match authority.find('/') {
                Some(slash) => authority.split_at(slash),
                // "file://<hostname>" names no path at all.
                None => return None,
            };

            if !(hostname.is_empty() || hostname.eq_ignore_ascii_case("localhost")) {
                return None;
            }

            path
        }
        // No authority: "file:/path", which is what Bitwig's browser sends.
        None if rest.starts_with('/') => rest,
        None => return None,
    };

    // Percent-decoding is per path, after the split: decoding the whole payload
    // first would let an encoded `%0A` forge a line end. The decoded bytes go
    // into the path as they are — a path is bytes here and need not be UTF-8.
    let decoded = percent_decode(path.as_bytes()).collect::<Vec<u8>>();
    Some(PathBuf::from(OsString::from_vec(decoded)))
}

#[derive(Debug)]
pub enum ParseError {
    EmptyData,
    InvalidUtf8(Utf8Error),
    NoUsablePaths,
}

impl Display for ParseError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Failed to parse Drag-n-Drop data: ")?;

        match self {
            ParseError::EmptyData => f.write_str("data is empty"),
            ParseError::InvalidUtf8(e) => e.fmt(f),
            ParseError::NoUsablePaths => f.write_str("no usable file URI in data"),
        }
    }
}

impl Error for ParseError {}

#[derive(Debug, Copy, Clone)]
pub(crate) enum DndAction {
    Copy,
    Move,
    Link,
    Ask,
    Private,
}

impl DndAction {
    pub fn from_atom(atom: Atom, atoms: &Atoms) -> Option<Self> {
        if atom == atoms.XdndActionCopy {
            Some(DndAction::Copy)
        } else if atom == atoms.XdndActionMove {
            Some(DndAction::Move)
        } else if atom == atoms.XdndActionLink {
            Some(DndAction::Link)
        } else if atom == atoms.XdndActionAsk {
            Some(DndAction::Ask)
        } else if atom == atoms.XdndActionPrivate {
            Some(DndAction::Private)
        } else {
            None
        }
    }

    pub fn to_atom(self, atoms: &Atoms) -> Atom {
        match self {
            DndAction::Copy => atoms.XdndActionCopy,
            DndAction::Move => atoms.XdndActionMove,
            DndAction::Link => atoms.XdndActionLink,
            DndAction::Ask => atoms.XdndActionAsk,
            DndAction::Private => atoms.XdndActionPrivate,
        }
    }
}
