//! EIS (Emulated Input Server) input backend.
//!
//! This backend uses the `reis` crate's high-level `request` module to implement
//! a full EIS server. Clients connect via a Unix socket pair and send input events
//! using the libei protocol. The server handles handshake, device setup, and event
//! conversion.
//!
//! # How It Works
//!
//! 1. Portal creates a Unix socket pair via [`EisSession::new`]
//! 2. Client end is returned to caller (passed via D-Bus `ConnectToEIS`)
//! 3. Client performs EIS handshake (version, interfaces, context type)
//! 4. Server completes handshake, creates seat + device with requested capabilities
//! 5. Client sends input events; server converts them via `EisRequestConverter`
//! 6. Caller retrieves high-level `EisRequest` events from [`EisSession::process`]
//!
//! # Status
//!
//! **0.2.0:** Full EIS server using reis 0.6 high-level API. Proper handshake with
//! interface negotiation, automatic frame batching, touch tracking, and serial
//! management. Used by [`super::eis_bridge::EisBridgeBackend`] for forwarding
//! events to wlr virtual input protocols.

use std::os::unix::{
    io::{AsRawFd, FromRawFd, OwnedFd},
    net::UnixStream,
};

use enumflags2::BitFlags;
use reis::{
    eis,
    handshake::{EisHandshakeResp, EisHandshaker},
    request::{Device, DeviceCapability, EisRequest, EisRequestConverter, Seat},
    PendingRequestResult,
};

use crate::{
    error::{PortalError, Result},
    types::DeviceTypes,
};

/// Per-session EIS state.
///
/// Each session gets its own Unix socket pair, handshake state machine,
/// and request converter. The session progresses through phases:
/// `AwaitingHandshake` -> `Active` -> dropped.
pub struct EisSession {
    /// The raw EIS context wrapping the server-side socket.
    context: eis::Context,
    /// Session phase.
    phase: SessionPhase,
}

enum SessionPhase {
    /// Handshake in progress. Client hasn't sent Finish yet.
    AwaitingHandshake {
        handshaker: EisHandshaker,
        capabilities: BitFlags<DeviceCapability>,
    },
    /// Handshake complete, actively processing events.
    Active {
        converter: EisRequestConverter,
        #[expect(dead_code, reason = "seat must stay alive while session is active")]
        seat: Seat,
        #[expect(
            dead_code,
            reason = "device must stay alive for EIS protocol lifecycle"
        )]
        device: Device,
    },
}

impl EisSession {
    /// Create a new EIS session, returning the session and the client-side socket fd.
    ///
    /// The caller should pass the returned `OwnedFd` to the client via D-Bus
    /// (the `ConnectToEIS` method).
    pub fn new(device_types: DeviceTypes) -> Result<(Self, OwnedFd)> {
        let (server_socket, client_socket) = UnixStream::pair().map_err(|e| {
            PortalError::EisCreationFailed(format!("Failed to create socket pair: {e}"))
        })?;

        let eis_context = eis::Context::new(server_socket).map_err(|e| {
            PortalError::EisCreationFailed(format!("Failed to create EIS context: {e}"))
        })?;

        // Convert client socket to OwnedFd without double-close
        #[expect(unsafe_code, reason = "OwnedFd::from_raw_fd requires unsafe FFI")]
        let client_fd = unsafe { OwnedFd::from_raw_fd(client_socket.as_raw_fd()) };
        std::mem::forget(client_socket);

        let capabilities = device_types_to_capabilities(device_types);

        // Start the server-side handshake. This sends handshake_version(1)
        // to the client immediately.
        let handshaker = EisHandshaker::new(&eis_context, 1);

        let session = Self {
            context: eis_context,
            phase: SessionPhase::AwaitingHandshake {
                handshaker,
                capabilities,
            },
        };

        Ok((session, client_fd))
    }

    /// Process pending data on the EIS socket.
    ///
    /// Returns a list of high-level `EisRequest` events ready for consumption.
    /// The caller is responsible for converting these to `InputEvent` and
    /// forwarding them (e.g., to a wlr backend).
    ///
    /// During the handshake phase, this drives the handshake state machine.
    /// Once the handshake completes, a seat and device are created automatically.
    pub fn process(&mut self) -> Result<Vec<EisRequest>> {
        // Read any pending data from the socket
        match self.context.read() {
            Ok(0) => return Ok(vec![]),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(vec![]),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                tracing::debug!("EIS client disconnected (EOF)");
                return Ok(vec![EisRequest::Disconnect]);
            }
            Err(e) => {
                return Err(PortalError::EisCreationFailed(format!(
                    "EIS socket read error: {e}"
                )));
            }
        }

        match &mut self.phase {
            SessionPhase::AwaitingHandshake { .. } => self.process_handshake(),
            SessionPhase::Active { converter, .. } => {
                Ok(Self::process_active(&self.context, converter))
            }
        }
    }

    /// Drive the handshake state machine.
    fn process_handshake(&mut self) -> Result<Vec<EisRequest>> {
        // We need to temporarily take ownership to transition phases
        let SessionPhase::AwaitingHandshake {
            handshaker,
            capabilities,
        } = &mut self.phase
        else {
            unreachable!("called process_handshake in non-handshake phase");
        };

        while let Some(result) = self.context.pending_request() {
            let request = match result {
                PendingRequestResult::Request(r) => r,
                PendingRequestResult::ParseError(e) => {
                    tracing::warn!("EIS handshake parse error: {e:?}");
                    continue;
                }
                PendingRequestResult::InvalidObject(id) => {
                    tracing::warn!("EIS handshake invalid object: {id}");
                    continue;
                }
            };

            match handshaker.handle_request(request) {
                Ok(Some(resp)) => {
                    // Handshake complete -- transition to active phase
                    let caps = *capabilities;
                    self.transition_to_active(resp, caps);
                    return Ok(vec![]);
                }
                Ok(None) => {
                    // Handshake still in progress
                }
                Err(e) => {
                    tracing::warn!("EIS handshake error: {e}");
                    return Err(PortalError::EisCreationFailed(format!(
                        "Handshake failed: {e}"
                    )));
                }
            }
        }

        let _ = self.context.flush();
        Ok(vec![])
    }

    /// Transition from handshake to active phase.
    ///
    /// Creates the request converter, adds a seat with the requested capabilities,
    /// adds a device on that seat, and resumes it to signal readiness.
    fn transition_to_active(
        &mut self,
        resp: EisHandshakeResp,
        capabilities: BitFlags<DeviceCapability>,
    ) {
        tracing::info!(
            client_name = ?resp.name,
            context_type = ?resp.context_type,
            interfaces = ?resp.negotiated_interfaces.keys().collect::<Vec<_>>(),
            "EIS handshake complete"
        );

        let converter = EisRequestConverter::new(&self.context, resp, 1);
        let connection = converter.handle().clone();

        // Add a seat with the requested capabilities
        let seat = connection.add_seat(Some("portal-input"), capabilities);

        // Add a device on the seat with all requested capabilities
        let device = seat.add_device(
            Some("portal-device"),
            eis::device::DeviceType::Virtual,
            capabilities,
            |_device| {
                // Could set keymap here for keyboard devices via
                // device.device().keyboard().keymap(...) if needed.
                // For now, the client handles its own keymap.
            },
        );

        // Signal to the client that the device is ready for emulation
        device.resumed();

        // Flush the seat/device/resumed events to the client
        let _ = self.context.flush();

        tracing::debug!("EIS session active, device resumed");

        self.phase = SessionPhase::Active {
            converter,
            seat,
            device,
        };
    }

    /// Process events in the active phase.
    fn process_active(
        context: &eis::Context,
        converter: &mut EisRequestConverter,
    ) -> Vec<EisRequest> {
        let mut events = Vec::new();

        while let Some(result) = context.pending_request() {
            let request = match result {
                PendingRequestResult::Request(r) => r,
                PendingRequestResult::ParseError(e) => {
                    tracing::warn!("EIS parse error: {e:?}");
                    continue;
                }
                PendingRequestResult::InvalidObject(id) => {
                    tracing::warn!("EIS invalid object: {id}");
                    continue;
                }
            };

            if let Err(e) = converter.handle_request(request) {
                tracing::warn!("EIS request handling error: {e}");
                continue;
            }

            // Drain any converted high-level requests
            while let Some(eis_request) = converter.next_request() {
                events.push(eis_request);
            }
        }

        let _ = context.flush();
        events
    }

    /// Check if the handshake is complete and the session is active.
    pub fn is_active(&self) -> bool {
        matches!(self.phase, SessionPhase::Active { .. })
    }
}

/// Convert our `DeviceTypes` to reis `DeviceCapability` bitflags.
fn device_types_to_capabilities(devices: DeviceTypes) -> BitFlags<DeviceCapability> {
    let mut caps = BitFlags::empty();
    if devices.pointer {
        caps |= DeviceCapability::Pointer;
        caps |= DeviceCapability::PointerAbsolute;
        caps |= DeviceCapability::Button;
        caps |= DeviceCapability::Scroll;
    }
    if devices.keyboard {
        caps |= DeviceCapability::Keyboard;
    }
    if devices.touchscreen {
        caps |= DeviceCapability::Touch;
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_types_to_capabilities() {
        let all = DeviceTypes::all();
        let caps = device_types_to_capabilities(all);
        assert!(caps.contains(DeviceCapability::Pointer));
        assert!(caps.contains(DeviceCapability::PointerAbsolute));
        assert!(caps.contains(DeviceCapability::Keyboard));
        assert!(caps.contains(DeviceCapability::Touch));
        assert!(caps.contains(DeviceCapability::Button));
        assert!(caps.contains(DeviceCapability::Scroll));
    }

    #[test]
    fn test_device_types_keyboard_only() {
        let kb = DeviceTypes {
            keyboard: true,
            pointer: false,
            touchscreen: false,
        };
        let caps = device_types_to_capabilities(kb);
        assert!(caps.contains(DeviceCapability::Keyboard));
        assert!(!caps.contains(DeviceCapability::Pointer));
        assert!(!caps.contains(DeviceCapability::Touch));
    }

    #[test]
    fn test_eis_session_creation() {
        let (session, fd) = EisSession::new(DeviceTypes::all()).unwrap();
        assert!(!session.is_active());
        // fd should be valid
        assert!(fd.as_raw_fd() >= 0);
    }
}
