//! EIS (Emulated Input Server) input backend.
//!
//! This backend uses the `reis` crate to implement the EIS protocol,
//! which is the emerging freedesktop standard for input emulation.
//!
//! # How It Works
//!
//! 1. Portal creates a Unix socket pair
//! 2. Portal keeps one end, passes the other to the client via D-Bus
//! 3. Client sends input events over the socket using libei
//! 4. Portal reads events via `process_events()` and returns them
//!
//! The caller is responsible for forwarding the returned events to the
//! appropriate output (e.g., wlr virtual input protocols).

use std::{
    collections::HashMap,
    os::unix::{
        io::{AsRawFd, FromRawFd, OwnedFd},
        net::UnixStream,
    },
};

use reis::{eis, PendingRequestResult};

use super::{EisConfig, InputBackend, InputProtocol};
use crate::{
    error::{PortalError, Result},
    types::{
        ButtonState, DeviceTypes, InputEvent, KeyState, KeyboardEvent, PointerEvent, ScrollAxis,
        TouchEvent,
    },
};

/// EIS-based input backend.
///
/// Implements the [`InputBackend`] trait using the EIS (Emulated Input Server) protocol.
/// Clients connect to a Unix socket and send input events, which are parsed and returned
/// via [`InputBackend::process_events()`].
pub struct EisInputBackend {
    /// Active EIS contexts by session ID.
    contexts: HashMap<String, EisSessionContext>,
}

/// An EIS context for a single session.
struct EisSessionContext {
    /// The underlying EIS context.
    context: eis::Context,
    /// Whether the handshake is complete.
    handshake_complete: bool,
}

impl EisInputBackend {
    /// Create a new EIS input backend.
    pub fn new(_config: &EisConfig) -> Result<Self> {
        tracing::info!("Initializing EIS input backend");

        Ok(Self {
            contexts: HashMap::new(),
        })
    }

    /// Process events for a specific session.
    fn process_session_events(&mut self, session_id: &str) -> Result<Vec<InputEvent>> {
        let context = self
            .contexts
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?;

        let mut events = Vec::new();

        // Read any pending data
        match context.context.read() {
            Ok(0) => return Ok(events),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(events),
            Err(e) => {
                return Err(PortalError::EisCreationFailed(format!(
                    "Failed to read from EIS socket: {e}"
                )));
            }
        }

        // Process pending requests
        while let Some(result) = context.context.pending_request() {
            match result {
                PendingRequestResult::Request(request) => {
                    if let Some(event) =
                        Self::handle_request(&mut context.handshake_complete, request)
                    {
                        events.push(event);
                    }
                }
                PendingRequestResult::ParseError(e) => {
                    tracing::warn!("EIS parse error: {:?}", e);
                }
                PendingRequestResult::InvalidObject(id) => {
                    tracing::warn!("EIS invalid object ID: {}", id);
                }
            }
        }

        // Flush any outgoing messages
        let _ = context.context.flush();

        Ok(events)
    }

    /// Handle an EIS protocol request, returning an input event if applicable.
    fn handle_request(handshake_complete: &mut bool, request: eis::Request) -> Option<InputEvent> {
        use eis::Request;

        match request {
            Request::Handshake(_, handshake_req) => {
                Self::handle_handshake(handshake_complete, handshake_req);
                None
            }
            Request::Connection(_, ref conn_req) => {
                Self::handle_connection_request(conn_req);
                None
            }
            Request::Seat(_, ref seat_req) => {
                Self::handle_seat_request(seat_req);
                None
            }
            Request::Device(_, ref device_req) => {
                Self::handle_device_request(device_req);
                None
            }
            Request::Keyboard(_, ref kb_req) => Self::handle_keyboard_request(kb_req),
            Request::Pointer(_, ref ptr_req) => Self::handle_pointer_request(ptr_req),
            Request::PointerAbsolute(_, ref ptr_req) => {
                Self::handle_pointer_absolute_request(ptr_req)
            }
            Request::Scroll(_, ref scroll_req) => Self::handle_scroll_request(scroll_req),
            Request::Button(_, ref btn_req) => Self::handle_button_request(btn_req),
            Request::Touchscreen(_, ref touch_req) => Self::handle_touch_request(touch_req),
            Request::Callback(_, _) | Request::Pingpong(_, _) => None,
            _ => {
                tracing::trace!("Unhandled EIS request type");
                None
            }
        }
    }

    fn handle_handshake(handshake_complete: &mut bool, request: eis::handshake::Request) {
        use eis::handshake::Request;

        match request {
            Request::HandshakeVersion { version } => {
                tracing::debug!(version = version, "EIS handshake version");
            }
            Request::ContextType { .. } => {
                tracing::debug!("EIS context type received");
            }
            Request::Name { name } => {
                tracing::debug!(name = %name, "EIS client name");
            }
            Request::InterfaceVersion { .. } => {
                tracing::trace!("EIS interface version negotiation");
            }
            Request::Finish => {
                tracing::debug!("EIS handshake finished");
                *handshake_complete = true;
            }
            _ => {}
        }
    }

    fn handle_connection_request(request: &eis::connection::Request) {
        use eis::connection::Request;

        match request {
            Request::Disconnect => {
                tracing::debug!("EIS client disconnected");
            }
            Request::Sync { .. } => {
                tracing::trace!("EIS sync request");
            }
            _ => {}
        }
    }

    fn handle_seat_request(request: &eis::seat::Request) {
        use eis::seat::Request;

        match request {
            Request::Bind { capabilities } => {
                tracing::debug!(capabilities = capabilities, "EIS seat bind request");
            }
            Request::Release => {
                tracing::debug!("EIS seat release");
            }
            _ => {}
        }
    }

    fn handle_device_request(request: &eis::device::Request) {
        use eis::device::Request;

        match request {
            Request::Release => {
                tracing::debug!("EIS device release");
            }
            Request::StartEmulating {
                last_serial,
                sequence,
            } => {
                tracing::debug!(
                    last_serial = last_serial,
                    sequence = sequence,
                    "EIS start emulating"
                );
            }
            Request::StopEmulating { last_serial } => {
                tracing::debug!(last_serial = last_serial, "EIS stop emulating");
            }
            Request::Frame {
                last_serial,
                timestamp,
            } => {
                tracing::trace!(
                    last_serial = last_serial,
                    timestamp = timestamp,
                    "EIS frame"
                );
            }
            _ => {}
        }
    }

    fn handle_keyboard_request(request: &eis::keyboard::Request) -> Option<InputEvent> {
        use eis::keyboard::Request;
        use reis::eis::keyboard::KeyState as EisKeyState;

        match request {
            Request::Key { key, state } => {
                let key_state = match state {
                    EisKeyState::Press => KeyState::Pressed,
                    EisKeyState::Released => KeyState::Released,
                };

                let event = InputEvent::Keyboard(KeyboardEvent {
                    keycode: *key,
                    state: key_state,
                    time_usec: current_time_usec(),
                });

                tracing::trace!(key = key, state = ?key_state, "EIS keyboard event");
                Some(event)
            }
            Request::Release => {
                tracing::debug!("EIS keyboard release");
                None
            }
            _ => None,
        }
    }

    fn handle_pointer_request(request: &eis::pointer::Request) -> Option<InputEvent> {
        use eis::pointer::Request;

        match request {
            Request::MotionRelative { x, y } => {
                let event = InputEvent::Pointer(PointerEvent::Motion {
                    dx: f64::from(*x),
                    dy: f64::from(*y),
                    time_usec: current_time_usec(),
                });

                tracing::trace!(dx = x, dy = y, "EIS pointer motion");
                Some(event)
            }
            Request::Release => {
                tracing::debug!("EIS pointer release");
                None
            }
            _ => None,
        }
    }

    fn handle_pointer_absolute_request(
        request: &eis::pointer_absolute::Request,
    ) -> Option<InputEvent> {
        use eis::pointer_absolute::Request;

        match request {
            Request::MotionAbsolute { x, y } => {
                let event = InputEvent::Pointer(PointerEvent::MotionAbsolute {
                    x: f64::from(*x),
                    y: f64::from(*y),
                    stream: 0,
                    time_usec: current_time_usec(),
                });

                tracing::trace!(x = x, y = y, "EIS absolute pointer motion");
                Some(event)
            }
            Request::Release => {
                tracing::debug!("EIS pointer absolute release");
                None
            }
            _ => None,
        }
    }

    fn handle_scroll_request(request: &eis::scroll::Request) -> Option<InputEvent> {
        use eis::scroll::Request;

        match request {
            Request::Scroll { x, y } => {
                let event = InputEvent::Pointer(PointerEvent::Scroll {
                    dx: f64::from(*x),
                    dy: f64::from(*y),
                    time_usec: current_time_usec(),
                });

                tracing::trace!(x = x, y = y, "EIS scroll");
                Some(event)
            }
            Request::ScrollDiscrete { x, y } => {
                // For discrete scroll, prefer vertical if both are set
                let event = if *y != 0 {
                    InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                        axis: ScrollAxis::Vertical,
                        steps: *y,
                        time_usec: current_time_usec(),
                    })
                } else {
                    InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                        axis: ScrollAxis::Horizontal,
                        steps: *x,
                        time_usec: current_time_usec(),
                    })
                };

                tracing::trace!(x = x, y = y, "EIS discrete scroll");
                Some(event)
            }
            Request::ScrollStop { .. } => {
                tracing::trace!("EIS scroll stop");
                None
            }
            Request::Release => {
                tracing::debug!("EIS scroll release");
                None
            }
            _ => None,
        }
    }

    fn handle_button_request(request: &eis::button::Request) -> Option<InputEvent> {
        use eis::button::Request;
        use reis::eis::button::ButtonState as EisButtonState;

        match request {
            Request::Button { button, state } => {
                let button_state = match state {
                    EisButtonState::Press => ButtonState::Pressed,
                    EisButtonState::Released => ButtonState::Released,
                };

                let event = InputEvent::Pointer(PointerEvent::Button {
                    button: *button,
                    state: button_state,
                    time_usec: current_time_usec(),
                });

                tracing::trace!(button = button, state = ?button_state, "EIS button");
                Some(event)
            }
            Request::Release => {
                tracing::debug!("EIS button release");
                None
            }
            _ => None,
        }
    }

    fn handle_touch_request(request: &eis::touchscreen::Request) -> Option<InputEvent> {
        use eis::touchscreen::Request;

        match request {
            Request::Down { touchid, x, y } => {
                let event = InputEvent::Touch(TouchEvent::Down {
                    id: *touchid as i32,
                    x: f64::from(*x),
                    y: f64::from(*y),
                    stream: 0,
                    time_usec: current_time_usec(),
                });

                tracing::trace!(id = touchid, x = x, y = y, "EIS touch down");
                Some(event)
            }
            Request::Motion { touchid, x, y } => {
                let event = InputEvent::Touch(TouchEvent::Motion {
                    id: *touchid as i32,
                    x: f64::from(*x),
                    y: f64::from(*y),
                    stream: 0,
                    time_usec: current_time_usec(),
                });

                tracing::trace!(id = touchid, x = x, y = y, "EIS touch motion");
                Some(event)
            }
            Request::Up { touchid } => {
                let event = InputEvent::Touch(TouchEvent::Up {
                    id: *touchid as i32,
                    time_usec: current_time_usec(),
                });

                tracing::trace!(id = touchid, "EIS touch up");
                Some(event)
            }
            Request::Release => {
                tracing::debug!("EIS touchscreen release");
                None
            }
            _ => None,
        }
    }
}

impl InputBackend for EisInputBackend {
    fn protocol_type(&self) -> InputProtocol {
        InputProtocol::Eis
    }

    fn create_context(
        &mut self,
        session_id: &str,
        devices: DeviceTypes,
    ) -> Result<Option<OwnedFd>> {
        tracing::debug!(
            session_id = %session_id,
            device_types = ?devices,
            "Creating EIS context"
        );

        if self.contexts.contains_key(session_id) {
            return Err(PortalError::InvalidSession(format!(
                "EIS context already exists for session {session_id}"
            )));
        }

        // Create a Unix socket pair
        let (server_socket, client_socket) = UnixStream::pair().map_err(|e| {
            PortalError::EisCreationFailed(format!("Failed to create socket pair: {e}"))
        })?;

        // Set non-blocking for async operation
        server_socket.set_nonblocking(true).map_err(|e| {
            PortalError::EisCreationFailed(format!("Failed to set non-blocking: {e}"))
        })?;

        // Create the EIS context from the server socket
        let eis_context = eis::Context::new(server_socket).map_err(|e| {
            PortalError::EisCreationFailed(format!("Failed to create EIS context: {e}"))
        })?;

        // Convert client socket to OwnedFd
        // SAFETY: client_socket is a valid Unix socket from UnixStream::pair()
        #[expect(unsafe_code, reason = "OwnedFd::from_raw_fd requires unsafe FFI")]
        let client_fd = unsafe { OwnedFd::from_raw_fd(client_socket.as_raw_fd()) };
        std::mem::forget(client_socket); // Prevent double-close

        let context = EisSessionContext {
            context: eis_context,
            handshake_complete: false,
        };

        self.contexts.insert(session_id.to_string(), context);

        tracing::info!(session_id = %session_id, "EIS context created");

        Ok(Some(client_fd))
    }

    fn destroy_context(&mut self, session_id: &str) -> Result<()> {
        if self.contexts.remove(session_id).is_some() {
            tracing::info!(session_id = %session_id, "EIS context destroyed");
        }
        Ok(())
    }

    fn inject_event(&mut self, _session_id: &str, _event: InputEvent) -> Result<()> {
        // For EIS, events come from the client via socket, not from D-Bus calls
        tracing::trace!("inject_event called on EIS backend (no-op, events come from socket)");
        Ok(())
    }

    fn process_events(&mut self) -> Result<Vec<(String, InputEvent)>> {
        let mut all_events = Vec::new();

        let session_ids: Vec<String> = self.contexts.keys().cloned().collect();
        for session_id in session_ids {
            match self.process_session_events(&session_id) {
                Ok(events) => {
                    for event in events {
                        all_events.push((session_id.clone(), event));
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %e,
                        "Error processing EIS events"
                    );
                }
            }
        }

        Ok(all_events)
    }

    fn has_context(&self, session_id: &str) -> bool {
        self.contexts.contains_key(session_id)
    }

    fn context_count(&self) -> usize {
        self.contexts.len()
    }

    fn keysym_to_keycode(&self, _keysym: u32) -> Option<u32> {
        // EIS sends keycodes directly from the client — keysym conversion
        // is not needed since the client handles keymap negotiation.
        None
    }
}

/// Get current monotonic time in microseconds.
///
/// EIS protocol timestamps must use `CLOCK_MONOTONIC`, not wall clock time.
/// Wall clock time (`SystemTime`) can jump on NTP sync or suspend/resume,
/// which would cause input event timing anomalies.
fn current_time_usec() -> u64 {
    let ts = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)
        .expect("CLOCK_MONOTONIC should always be available");
    ts.tv_sec() as u64 * 1_000_000 + ts.tv_nsec() as u64 / 1_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_eis_backend_create_context() {
        let mut backend = EisInputBackend::new(&EisConfig::default()).unwrap();

        let fd = backend
            .create_context("session-1", DeviceTypes::all())
            .unwrap();

        assert!(fd.is_some());
        assert!(backend.has_context("session-1"));
        assert_eq!(backend.context_count(), 1);
    }

    #[test]
    fn test_eis_backend_destroy_context() {
        let mut backend = EisInputBackend::new(&EisConfig::default()).unwrap();

        backend
            .create_context("session-1", DeviceTypes::all())
            .unwrap();
        assert!(backend.has_context("session-1"));

        backend.destroy_context("session-1").unwrap();
        assert!(!backend.has_context("session-1"));
        assert_eq!(backend.context_count(), 0);
    }

    #[test]
    fn test_eis_backend_duplicate_context_fails() {
        let mut backend = EisInputBackend::new(&EisConfig::default()).unwrap();

        backend
            .create_context("session-1", DeviceTypes::all())
            .unwrap();

        let result = backend.create_context("session-1", DeviceTypes::all());
        assert!(result.is_err());
    }

    #[test]
    fn test_eis_backend_protocol_type() {
        let backend = EisInputBackend::new(&EisConfig::default()).unwrap();

        assert_eq!(backend.protocol_type(), InputProtocol::Eis);
    }

    #[test]
    fn test_current_time_usec() {
        let time = current_time_usec();
        assert!(time > 0);
    }
}
