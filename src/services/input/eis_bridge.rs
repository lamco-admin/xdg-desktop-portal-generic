//! EIS bridge backend: EIS server on one side, wlr virtual input on the other.
//!
//! This backend accepts EIS connections from clients (via `ConnectToEIS`), parses
//! input events using the reis high-level API, and forwards them to the compositor
//! through wlr virtual keyboard/pointer protocols.
//!
//! # Architecture
//!
//! ```text
//! Client (libei) --[EIS socket]--> EisBridgeBackend --[wlr virtual input]--> Compositor
//! ```
//!
//! Each session maintains both an [`EisSession`] (for the client connection) and
//! a wlr virtual device set (for compositor injection). The bridge reads events
//! from EIS, converts them to [`InputEvent`], and forwards them via the shared
//! [`WlrInputBackend`].
//!
//! # When Is This Used?
//!
//! On wlroots compositors (Sway, Hyprland, etc.) that support wlr virtual input
//! but don't have native EIS support. The bridge lets portal clients use the
//! standard `ConnectToEIS` path while the portal handles the protocol translation.
//!
//! When Smithay PR #1388 lands, Smithay-based compositors will accept EIS natively,
//! and this bridge won't be needed for those compositors.

use std::{collections::HashMap, os::unix::io::OwnedFd};

use reis::request::EisRequest;

use super::{
    eis_backend::EisSession, wlr_backend::WlrInputBackend, InputBackend, InputProtocol, WlrConfig,
};
use crate::{
    error::{PortalError, Result},
    types::{
        ButtonState, DeviceTypes, InputEvent, KeyState, KeyboardEvent, PointerEvent, ScrollAxis,
        StreamOutputMapping, TouchEvent,
    },
};

/// EIS-to-wlr bridge backend.
///
/// Implements [`InputBackend`] by combining an EIS server (accepting client
/// input events) with a wlr virtual input backend (injecting events into
/// the compositor).
pub struct EisBridgeBackend {
    /// Per-session EIS state.
    sessions: HashMap<String, EisSession>,
    /// Shared wlr backend for all sessions' output.
    wlr: WlrInputBackend,
    /// Health event sender for input metrics.
    health_tx: Option<crate::health::HealthSender>,
}

impl EisBridgeBackend {
    /// Create a new EIS bridge backend.
    ///
    /// Initializes the wlr virtual input backend for compositor injection.
    /// EIS sessions are created per-session via [`InputBackend::create_context`].
    pub fn new(wlr_config: &WlrConfig) -> Result<Self> {
        tracing::info!("Initializing EIS bridge backend (EIS -> wlr virtual input)");

        let wlr = WlrInputBackend::new(wlr_config)?;

        Ok(Self {
            sessions: HashMap::new(),
            wlr,
            health_tx: None,
        })
    }

    /// Convert a high-level `EisRequest` to our `InputEvent`.
    ///
    /// Returns `None` for protocol-level events that don't map to input
    /// (Bind, Frame, DeviceStart/StopEmulating, Disconnect).
    fn eis_request_to_input_event(request: &EisRequest) -> Option<InputEvent> {
        match request {
            EisRequest::PointerMotion(m) => Some(InputEvent::Pointer(PointerEvent::Motion {
                dx: f64::from(m.dx),
                dy: f64::from(m.dy),
                time_usec: m.time,
            })),

            EisRequest::PointerMotionAbsolute(m) => {
                Some(InputEvent::Pointer(PointerEvent::MotionAbsolute {
                    x: f64::from(m.dx_absolute),
                    y: f64::from(m.dy_absolute),
                    stream: 0,
                    time_usec: m.time,
                }))
            }

            EisRequest::Button(b) => {
                let state = match b.state {
                    reis::eis::button::ButtonState::Press => ButtonState::Pressed,
                    reis::eis::button::ButtonState::Released => ButtonState::Released,
                };
                Some(InputEvent::Pointer(PointerEvent::Button {
                    button: b.button,
                    state,
                    time_usec: b.time,
                }))
            }

            EisRequest::ScrollDelta(s) => Some(InputEvent::Pointer(PointerEvent::Scroll {
                dx: f64::from(s.dx),
                dy: f64::from(s.dy),
                time_usec: s.time,
            })),

            EisRequest::ScrollDiscrete(s) => {
                // Prefer vertical if both are set
                if s.discrete_dy != 0 {
                    Some(InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                        axis: ScrollAxis::Vertical,
                        steps: s.discrete_dy,
                        time_usec: s.time,
                    }))
                } else {
                    Some(InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                        axis: ScrollAxis::Horizontal,
                        steps: s.discrete_dx,
                        time_usec: s.time,
                    }))
                }
            }

            EisRequest::ScrollStop(s) => {
                let _ = s;
                Some(InputEvent::Pointer(PointerEvent::ScrollStop {
                    time_usec: s.time,
                }))
            }

            EisRequest::KeyboardKey(k) => {
                let state = match k.state {
                    reis::eis::keyboard::KeyState::Press => KeyState::Pressed,
                    reis::eis::keyboard::KeyState::Released => KeyState::Released,
                };
                Some(InputEvent::Keyboard(KeyboardEvent {
                    keycode: k.key,
                    state,
                    time_usec: k.time,
                }))
            }

            EisRequest::TouchDown(t) => Some(InputEvent::Touch(TouchEvent::Down {
                id: t.touch_id as i32,
                x: f64::from(t.x),
                y: f64::from(t.y),
                stream: 0,
                time_usec: t.time,
            })),

            EisRequest::TouchMotion(t) => Some(InputEvent::Touch(TouchEvent::Motion {
                id: t.touch_id as i32,
                x: f64::from(t.x),
                y: f64::from(t.y),
                stream: 0,
                time_usec: t.time,
            })),

            EisRequest::TouchUp(t) => Some(InputEvent::Touch(TouchEvent::Up {
                id: t.touch_id as i32,
                time_usec: t.time,
            })),

            // Protocol-level events don't produce InputEvent.
            // Frame, DeviceStart/StopEmulating carry health data harvested in process_events().
            EisRequest::Disconnect
            | EisRequest::Bind(_)
            | EisRequest::Frame(_)
            | EisRequest::DeviceStartEmulating(_)
            | EisRequest::DeviceStopEmulating(_)
            | EisRequest::ScrollCancel(_)
            | EisRequest::TouchCancel(_) => None,
        }
    }
}

impl InputBackend for EisBridgeBackend {
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
            "Creating EIS bridge context"
        );

        if self.sessions.contains_key(session_id) {
            return Err(PortalError::InvalidSession(format!(
                "EIS bridge context already exists for session {session_id}"
            )));
        }

        // Create the EIS session (server-side socket + handshake)
        let (eis_session, client_fd) = EisSession::new(devices)?;

        // Create wlr virtual devices for forwarding
        self.wlr.create_context(session_id, devices)?;

        self.sessions.insert(session_id.to_string(), eis_session);

        tracing::info!(
            session_id = %session_id,
            "EIS bridge context created (EIS server + wlr virtual devices)"
        );

        Ok(Some(client_fd))
    }

    fn destroy_context(&mut self, session_id: &str) -> Result<()> {
        if self.sessions.remove(session_id).is_some() {
            tracing::info!(session_id = %session_id, "EIS bridge context destroyed");
        }

        // Clean up wlr virtual devices
        self.wlr.destroy_context(session_id)?;

        Ok(())
    }

    fn inject_event(&mut self, session_id: &str, event: InputEvent) -> Result<()> {
        // D-Bus Notify* methods bypass EIS and go directly to wlr.
        // This supports the dual-path model: clients can use either
        // ConnectToEIS or Notify* methods.
        self.wlr.inject_event(session_id, event)
    }

    fn process_events(&mut self) -> Result<Vec<(String, InputEvent)>> {
        let mut all_events = Vec::new();
        let mut disconnected = Vec::new();

        let session_ids: Vec<String> = self.sessions.keys().cloned().collect();

        for session_id in &session_ids {
            let Some(session) = self.sessions.get_mut(session_id) else {
                continue;
            };

            match session.process() {
                Ok(eis_requests) => {
                    for request in &eis_requests {
                        // Harvest health signals from protocol events
                        match request {
                            EisRequest::Disconnect => {
                                tracing::info!(
                                    session_id = %session_id,
                                    "EIS client disconnected"
                                );
                                if let Some(ref health_tx) = self.health_tx {
                                    let _ = health_tx.try_send(
                                        crate::health::PortalHealthEvent::InputDisconnected {
                                            reason: "EIS client disconnected".to_string(),
                                            recoverable: true,
                                        },
                                    );
                                }
                                disconnected.push(session_id.clone());
                                continue;
                            }
                            EisRequest::Frame(frame) => {
                                if let Some(ref health_tx) = self.health_tx {
                                    let _ = health_tx.try_send(
                                        crate::health::PortalHealthEvent::EisFrameReceived {
                                            last_serial: frame.last_serial,
                                            time_usec: frame.time,
                                        },
                                    );
                                }
                            }
                            EisRequest::DeviceStartEmulating(evt) => {
                                if let Some(ref health_tx) = self.health_tx {
                                    let _ = health_tx.try_send(
                                        crate::health::PortalHealthEvent::EisDeviceStateChanged {
                                            emulating: true,
                                            serial: evt.last_serial,
                                            sequence: evt.sequence,
                                        },
                                    );
                                }
                            }
                            EisRequest::DeviceStopEmulating(evt) => {
                                if let Some(ref health_tx) = self.health_tx {
                                    let _ = health_tx.try_send(
                                        crate::health::PortalHealthEvent::EisDeviceStateChanged {
                                            emulating: false,
                                            serial: evt.last_serial,
                                            sequence: 0,
                                        },
                                    );
                                }
                            }
                            _ => {}
                        }

                        if let Some(event) = Self::eis_request_to_input_event(request) {
                            // Forward the event to the compositor via wlr
                            if let Err(e) = self.wlr.inject_event(session_id, event.clone()) {
                                tracing::warn!(
                                    session_id = %session_id,
                                    error = %e,
                                    "Failed to forward EIS event to wlr"
                                );
                            }

                            all_events.push((session_id.clone(), event));
                        }
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

        // Clean up disconnected sessions
        for session_id in disconnected {
            if let Err(e) = self.destroy_context(&session_id) {
                tracing::warn!(
                    session_id = %session_id,
                    error = %e,
                    "Error cleaning up disconnected EIS session"
                );
            }
        }

        Ok(all_events)
    }

    fn has_context(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    fn context_count(&self) -> usize {
        self.sessions.len()
    }

    fn keysym_to_keycode(&self, keysym: u32) -> Option<u32> {
        // Delegate to wlr backend's XKB keymap
        self.wlr.keysym_to_keycode(keysym)
    }

    fn set_health_sender(&mut self, tx: crate::health::HealthSender) {
        self.health_tx = Some(tx.clone());
        self.wlr.set_health_sender(tx);
    }

    fn set_stream_mappings(&mut self, mappings: Vec<StreamOutputMapping>) {
        self.wlr.set_stream_mappings(mappings);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: EisRequest variant structs contain reis::request::Device which
    // requires a real EIS context to construct. Conversion tests that need
    // Device fields are covered by integration tests with actual socket pairs.
    // Unit tests here focus on the non-Device paths.

    #[test]
    fn test_eis_request_to_input_event_disconnect_returns_none() {
        let event = EisBridgeBackend::eis_request_to_input_event(&EisRequest::Disconnect);
        assert!(
            event.is_none(),
            "Disconnect should not produce an InputEvent"
        );
    }
}
