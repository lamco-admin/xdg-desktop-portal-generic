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

use std::{collections::HashMap, os::unix::io::OwnedFd, sync::Arc};

use reis::request::EisRequest;

/// One past `KEY_MAX` (0x2ff) from Linux's `input-event-codes.h` -- the same
/// bound libei's C server enforces on button codes and keycodes (see
/// `upstream/reis/EI-EIS-DEEP-DIVE-2026-06-16.md` section 1, step 3 of its
/// 4-step validation guard). reis 0.7's `EisRequestConverter` does not apply
/// this itself, so it's enforced here at the point untrusted wire values
/// become our internal `InputEvent`.
const EVDEV_KEY_CNT: u32 = 768;

/// Reject a button code or keycode outside the evdev range, logging why.
/// `kind` is only used for the log message ("button" / "keycode").
fn validate_evdev_code(code: u32, kind: &str) -> Option<u32> {
    if code >= EVDEV_KEY_CNT {
        tracing::warn!(kind, code, "EIS code out of evdev range -- dropped");
        return None;
    }
    Some(code)
}

/// Convert one `ei_scroll.scroll_discrete` request into one event per nonzero
/// axis. A client can report both axes in the same request (simultaneous
/// diagonal scroll on modern trackpads); returning at most one event would
/// silently drop whichever axis wasn't picked.
fn scroll_discrete_events(discrete_dx: i32, discrete_dy: i32, time_usec: u64) -> Vec<InputEvent> {
    let mut events = Vec::with_capacity(2);
    if discrete_dy != 0 {
        events.push(InputEvent::Pointer(PointerEvent::ScrollDiscrete {
            axis: ScrollAxis::Vertical,
            steps: discrete_dy,
            time_usec,
        }));
    }
    if discrete_dx != 0 {
        events.push(InputEvent::Pointer(PointerEvent::ScrollDiscrete {
            axis: ScrollAxis::Horizontal,
            steps: discrete_dx,
            time_usec,
        }));
    }
    events
}

/// Convert one `char` to the XKB keysym `WlrInputBackend::keysym_to_keycode`
/// should resolve it against, for decomposing an `ei_text.utf8` string into
/// a sequence of keypresses.
///
/// Printable ASCII (0x20-0x7E) maps to its own codepoint -- the same value
/// the base "us" XKB layout's own keys carry, so `keysym_to_keycode`'s fast
/// path (a plain lookup against that layout) resolves it without touching
/// the dynamic keysym pool. Everything else uses the Unicode-keysym
/// convention (`0x0100_0000 + codepoint`), the same one
/// `lamco-rdp-server-dev`'s own Unicode-keyboard fallback uses and the one
/// already verified round-tripping through `splice_dynamic_keysyms` (see
/// `test_splice_dynamic_keysyms_recompiles_and_resolves`, which uses exactly
/// this convention for a Latin-1 codepoint, not just the higher planes).
fn char_to_keysym(c: char) -> u32 {
    let codepoint = c as u32;
    if (0x20..=0x7E).contains(&codepoint) {
        codepoint
    } else {
        0x0100_0000 + codepoint
    }
}

use super::{
    InputBackend, InputProtocol, WlrConfig, eis_backend::EisSession, wlr_backend::WlrInputBackend,
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
    /// Events staged since the session's last EIS `frame()`, forwarded to
    /// `wlr` as one atomic batch when the frame boundary arrives. See
    /// [`InputBackend::inject_event_batch`] for why this matters.
    pending_events: HashMap<String, Vec<InputEvent>>,
    /// Shared wlr backend for all sessions' output.
    wlr: WlrInputBackend,
    /// Health event sender for input metrics.
    health_tx: Option<crate::health::HealthSender>,
    /// Shared Wayland state, used to read the cached compositor keymap for
    /// new receiver-context sessions with keyboard capability.
    shared_wayland_state: Option<Arc<std::sync::Mutex<crate::wayland::SharedWaylandState>>>,
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
            pending_events: HashMap::new(),
            wlr,
            health_tx: None,
            shared_wayland_state: None,
        })
    }

    /// Stream value signalling "no specific per-output target" on a converted
    /// [`PointerEvent::MotionAbsolute`], used for events arriving over EIS (the wire
    /// protocol carries no stream/region selector of its own -- see
    /// [`Self::eis_request_to_input_event`]). Any real PipeWire node ID works as the
    /// "has a mapping" case in `WlrInputBackend::inject_event`'s `stream_mappings`
    /// lookup, including small values, so this needs to be a value no real node ID
    /// will ever collide with rather than e.g. `0`.
    const NO_TARGET_STREAM: u32 = u32::MAX;

    /// Convert a high-level `EisRequest` to zero or more `InputEvent`s.
    ///
    /// `layout_extent` is the layout's total pixel size (`WlrInputBackend::layout_extent`),
    /// needed to interpret `PointerMotionAbsolute`'s coordinates against the same space
    /// its EIS regions were advertised in.
    ///
    /// Returns an empty `Vec` for protocol-level events that don't map to
    /// input (Bind, Frame, DeviceStart/StopEmulating, Disconnect) and for
    /// requests that fail value-range validation (out-of-range button code
    /// or keycode -- logged and dropped rather than forwarded, matching
    /// libei's `VALUE`-class rejection). Returns two events for a
    /// `ScrollDiscrete` request that carries both axes at once (simultaneous
    /// diagonal scroll) -- a single event would silently drop one axis.
    fn eis_request_to_input_event(
        request: &EisRequest,
        layout_extent: (u32, u32),
    ) -> Vec<InputEvent> {
        match request {
            EisRequest::PointerMotion(m) => vec![InputEvent::Pointer(PointerEvent::Motion {
                dx: f64::from(m.dx),
                dy: f64::from(m.dy),
                time_usec: m.time,
            })],

            EisRequest::PointerMotionAbsolute(m) => {
                // The EIS wire protocol carries no per-request stream/region
                // selector (confirmed against reis's generated `PointerAbsolute`
                // request: only dx_absolute/dy_absolute). `dx_absolute`/
                // `dy_absolute` are coordinates within the device's own advertised
                // regions (see `EisSession::transition_to_active`, which advertises
                // one region per output via `WlrInputBackend::pointer_regions`, all
                // shifted into the same layout-origin space `layout_extent` covers).
                // So a request is interpreted as a point in the *whole* layout, not
                // resolved to any single output's region: NO_TARGET_STREAM steers
                // `inject_event`'s `stream_mappings` lookup to its "no mapping"
                // branch, which projects the point directly against `layout_extent`.
                let (x_extent, y_extent) = layout_extent;
                vec![InputEvent::Pointer(PointerEvent::MotionAbsolute {
                    x: f64::from(m.dx_absolute),
                    y: f64::from(m.dy_absolute),
                    x_extent,
                    y_extent,
                    stream: Self::NO_TARGET_STREAM,
                    time_usec: m.time,
                })]
            }

            EisRequest::Button(b) => {
                let Some(button) = validate_evdev_code(b.button, "button") else {
                    return vec![];
                };
                let state = match b.state {
                    reis::eis::button::ButtonState::Press => ButtonState::Pressed,
                    reis::eis::button::ButtonState::Released => ButtonState::Released,
                };
                vec![InputEvent::Pointer(PointerEvent::Button {
                    button,
                    state,
                    time_usec: b.time,
                })]
            }

            EisRequest::ScrollDelta(s) => vec![InputEvent::Pointer(PointerEvent::Scroll {
                dx: f64::from(s.dx),
                dy: f64::from(s.dy),
                time_usec: s.time,
            })],

            EisRequest::ScrollDiscrete(s) => {
                scroll_discrete_events(s.discrete_dx, s.discrete_dy, s.time)
            }

            EisRequest::ScrollStop(s) => {
                vec![InputEvent::Pointer(PointerEvent::ScrollStop {
                    time_usec: s.time,
                })]
            }

            EisRequest::KeyboardKey(k) => {
                let Some(keycode) = validate_evdev_code(k.key, "keycode") else {
                    return vec![];
                };
                let state = match k.state {
                    reis::eis::keyboard::KeyState::Press => KeyState::Pressed,
                    reis::eis::keyboard::KeyState::Released => KeyState::Released,
                };
                vec![InputEvent::Keyboard(KeyboardEvent {
                    keycode,
                    state,
                    time_usec: k.time,
                })]
            }

            EisRequest::TouchDown(t) => vec![InputEvent::Touch(TouchEvent::Down {
                id: t.touch_id as i32,
                x: f64::from(t.x),
                y: f64::from(t.y),
                stream: 0,
                time_usec: t.time,
            })],

            EisRequest::TouchMotion(t) => vec![InputEvent::Touch(TouchEvent::Motion {
                id: t.touch_id as i32,
                x: f64::from(t.x),
                y: f64::from(t.y),
                stream: 0,
                time_usec: t.time,
            })],

            EisRequest::TouchUp(t) => vec![InputEvent::Touch(TouchEvent::Up {
                id: t.touch_id as i32,
                time_usec: t.time,
            })],

            // Protocol-level events don't produce an InputEvent.
            // Frame is handled by the caller (frame-boundary batching);
            // DeviceStart/StopEmulating carry health data harvested in
            // process_events(); DeviceClosed teardown and Ready/resumed
            // gating are also handled there. RequestDevice is sender-context
            // lifecycle-only. TextKeysym and TextUtf8 are also handled in
            // process_events (resolving a keysym may need &mut self.wlr,
            // unavailable to this pure function) -- reaching either here
            // means it was already staged or logged there, so returning no
            // additional event is correct, not a gap. See
            // EI-TEXT-SCOPING-2026-09-07.md in lamco-admin for both.
            EisRequest::Disconnect
            | EisRequest::Bind(_)
            | EisRequest::Frame(_)
            | EisRequest::DeviceStartEmulating(_)
            | EisRequest::DeviceStopEmulating(_)
            | EisRequest::ScrollCancel(_)
            | EisRequest::TouchCancel(_)
            | EisRequest::DeviceClosed(_)
            | EisRequest::RequestDevice(_)
            | EisRequest::Ready(_)
            | EisRequest::TextKeysym(_)
            | EisRequest::TextUtf8(_) => vec![],
        }
    }
}

/// Decompose an `ei_text.utf8` string into a press+release keypress
/// sequence, staged into `pending_events` like any other input event -- the
/// underlying virtual keyboard protocol only understands scancodes, not
/// "type this text".
///
/// A free function taking `wlr`/`pending_events` as disjoint borrows
/// (rather than an `&mut self` method) because its only caller holds a
/// `&mut EisSession` borrowed from `self.sessions` at the same time --
/// borrowing all of `self` here would conflict with that.
///
/// `TextUtf8` carries one timestamp for the whole string, not one per
/// character, so each synthesized press/release edge gets a
/// 1ms-incrementing timestamp from that base -- enough to keep them
/// distinct and monotonically increasing downstream, not a claim about real
/// per-key timing (there is none to reconstruct).
fn inject_text_utf8(
    wlr: &mut WlrInputBackend,
    pending_events: &mut HashMap<String, Vec<InputEvent>>,
    session_id: &str,
    text: &str,
    base_time_usec: u64,
) {
    let mut time_usec = base_time_usec;
    for c in text.chars() {
        let keysym = char_to_keysym(c);
        let Some(keycode) = wlr.keysym_to_keycode(keysym) else {
            tracing::warn!(
                session_id = %session_id,
                ch = %c,
                keysym,
                "No keycode available for ei_text.utf8 character -- dropped"
            );
            continue;
        };
        let events = pending_events.entry(session_id.to_string()).or_default();
        events.push(InputEvent::Keyboard(KeyboardEvent {
            keycode,
            state: KeyState::Pressed,
            time_usec,
        }));
        time_usec += 1000;
        events.push(InputEvent::Keyboard(KeyboardEvent {
            keycode,
            state: KeyState::Released,
            time_usec,
        }));
        time_usec += 1000;
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
        let (eis_session, client_fd) = EisSession::new(
            devices,
            self.shared_wayland_state.clone(),
            self.wlr.pointer_regions(),
        )?;

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

        // Drop any events staged since the last frame -- they were never
        // atomically committed, so discarding rather than flushing them
        // unframed is correct.
        if let Some(dropped) = self.pending_events.remove(session_id) {
            if !dropped.is_empty() {
                tracing::warn!(
                    session_id = %session_id,
                    count = dropped.len(),
                    "Dropping unframed pending EIS events on session teardown"
                );
            }
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

    #[expect(
        clippy::too_many_lines,
        reason = "one sequential per-session loop -- health-signal harvesting, the \
                  receiver-context forwarding guard, and disconnect cleanup are all facets of \
                  the same event-draining pass, not separable concerns"
    )]
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

                                // The client's frame boundary: everything staged
                                // since the last one must land on the compositor
                                // as one atomic unit, not as separate flushes.
                                if let Some(batch) = self.pending_events.remove(session_id) {
                                    if !batch.is_empty() {
                                        if let Err(e) =
                                            self.wlr.inject_event_batch(session_id, &batch)
                                        {
                                            tracing::warn!(
                                                session_id = %session_id,
                                                error = %e,
                                                count = batch.len(),
                                                "Failed to forward EIS frame batch to wlr"
                                            );
                                        }
                                        for event in batch {
                                            all_events.push((session_id.clone(), event));
                                        }
                                    }
                                }
                            }
                            // A v3+ device withholds `resumed` (hence all input)
                            // until the client acknowledges `ei_device.done` with
                            // `ready()` -- see eis_backend.rs's version-gated
                            // `transition_to_active`. Applies uniformly to both
                            // sender and receiver-context devices since it's a
                            // device lifecycle gate, not a content-direction one.
                            EisRequest::Ready(ready) => {
                                tracing::debug!(
                                    session_id = %session_id,
                                    "EIS device ready() received; resuming"
                                );
                                ready.device.resumed();
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
                            // reis 0.7: a client `Release` now surfaces as DeviceClosed. The
                            // server must call Device::remove() to finish teardown and emit the
                            // protocol `destroyed` events (in 0.6.1 this was a silent no-op).
                            EisRequest::DeviceClosed(closed) => {
                                tracing::debug!(
                                    session_id = %session_id,
                                    "EIS client released a device; completing teardown"
                                );
                                closed.device.remove();
                            }
                            // ei_text.keysym injection request (sender-context client
                            // asking us to emulate a composed/Unicode keysym rather
                            // than a raw ei_keyboard.key). Handled here rather than in
                            // eis_request_to_input_event because resolving a keysym may
                            // need to dynamically extend the wlr keymap, which needs
                            // &mut self.wlr -- see EI-TEXT-SCOPING-2026-09-07.md in
                            // lamco-admin for why this exists and what it fixes.
                            EisRequest::TextKeysym(text_keysym) => {
                                if session.is_receiver() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Unexpected ei_text.keysym from receiver-context EIS \
                                         session -- dropped"
                                    );
                                    continue;
                                }
                                match self.wlr.keysym_to_keycode(text_keysym.keysym) {
                                    Some(keycode) => {
                                        let state = match text_keysym.state {
                                            reis::eis::keyboard::KeyState::Press => {
                                                KeyState::Pressed
                                            }
                                            reis::eis::keyboard::KeyState::Released => {
                                                KeyState::Released
                                            }
                                        };
                                        self.pending_events
                                            .entry(session_id.clone())
                                            .or_default()
                                            .push(InputEvent::Keyboard(KeyboardEvent {
                                                keycode,
                                                state,
                                                time_usec: text_keysym.time,
                                            }));
                                    }
                                    None => {
                                        tracing::warn!(
                                            session_id = %session_id,
                                            keysym = text_keysym.keysym,
                                            "No keycode available for ei_text.keysym -- dropped"
                                        );
                                    }
                                }
                            }
                            // ei_text.utf8 injection request: decomposes the whole
                            // string into a press+release keypress sequence via
                            // inject_text_utf8. Handled here rather than in
                            // eis_request_to_input_event for the same reason as
                            // TextKeysym -- resolving each character's keysym may
                            // need &mut self.wlr.
                            EisRequest::TextUtf8(text_utf8) => {
                                if session.is_receiver() {
                                    tracing::warn!(
                                        session_id = %session_id,
                                        "Unexpected ei_text.utf8 from receiver-context EIS \
                                         session -- dropped"
                                    );
                                    continue;
                                }
                                inject_text_utf8(
                                    &mut self.wlr,
                                    &mut self.pending_events,
                                    session_id,
                                    &text_utf8.text,
                                    text_utf8.time,
                                );
                            }
                            _ => {}
                        }

                        let converted =
                            Self::eis_request_to_input_event(request, self.wlr.layout_extent());
                        if converted.is_empty() {
                            continue;
                        }

                        if session.is_receiver() {
                            // A receiver-context session (InputCapture) is never
                            // supposed to send content requests -- it's the one
                            // receiving, not emulating. Log and drop rather than
                            // forwarding a malformed/unexpected event to the
                            // real compositor.
                            tracing::warn!(
                                session_id = %session_id,
                                "Unexpected content event from receiver-context EIS session -- dropped"
                            );
                            continue;
                        }

                        // Stage rather than forward immediately -- committed
                        // atomically as one batch when this session's
                        // EisRequest::Frame arrives (see the Frame arm above).
                        self.pending_events
                            .entry(session_id.clone())
                            .or_default()
                            .extend(converted);
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

    fn keysym_to_keycode(&mut self, keysym: u32) -> Option<u32> {
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

    fn start_input_capture(&mut self, session_id: &str) -> Result<()> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?
            .start_emulating()
    }

    fn forward_captured_pointer_motion(
        &mut self,
        session_id: &str,
        dx: f64,
        dy: f64,
        time_usec: u64,
    ) -> Result<()> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?
            .send_pointer_motion(dx, dy, time_usec)
    }

    fn stop_input_capture(&mut self, session_id: &str) -> Result<()> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?
            .stop_emulating()
    }

    fn forward_captured_key(
        &mut self,
        session_id: &str,
        keycode: u32,
        pressed: bool,
        time_usec: u64,
    ) -> Result<()> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?
            .send_key(keycode, pressed, time_usec)
    }

    fn forward_captured_modifiers(
        &mut self,
        session_id: &str,
        depressed: u32,
        latched: u32,
        locked: u32,
        group: u32,
    ) -> Result<()> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?
            .send_modifiers(depressed, latched, locked, group)
    }

    fn forward_captured_text(&mut self, session_id: &str, text: &str) -> Result<()> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?
            .send_text_utf8(text)
    }

    fn set_shared_wayland_state(
        &mut self,
        state: Arc<std::sync::Mutex<crate::wayland::SharedWaylandState>>,
    ) {
        self.shared_wayland_state = Some(state);
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
        let events =
            EisBridgeBackend::eis_request_to_input_event(&EisRequest::Disconnect, (1920, 1080));
        assert!(
            events.is_empty(),
            "Disconnect should not produce an InputEvent"
        );
    }

    #[test]
    fn test_char_to_keysym_ascii_maps_to_its_own_codepoint() {
        // Matches the base "us" XKB layout's own keysyms, so
        // keysym_to_keycode's fast path resolves these without touching
        // the dynamic pool.
        assert_eq!(char_to_keysym('a'), 0x61);
        assert_eq!(char_to_keysym('A'), 0x41);
        assert_eq!(char_to_keysym('0'), 0x30);
        assert_eq!(char_to_keysym(' '), 0x20);
        assert_eq!(char_to_keysym('~'), 0x7E);
    }

    #[test]
    fn test_char_to_keysym_latin1_supplement_uses_unicode_convention() {
        // é (U+00E9) has no key on a "us" layout -- must go through the
        // dynamic pool via the Unicode-keysym convention, the same value
        // already verified round-tripping through splice_dynamic_keysyms
        // in wlr_backend.rs's own tests.
        assert_eq!(char_to_keysym('é'), 0x0100_00E9);
    }

    #[test]
    fn test_char_to_keysym_cjk_uses_unicode_convention() {
        assert_eq!(char_to_keysym('世'), 0x0100_0000 + 0x4E16);
    }

    #[test]
    fn test_char_to_keysym_boundary_just_outside_ascii_range() {
        // 0x1F and 0x7F are just outside the printable-ASCII window this
        // function special-cases -- must fall through to the Unicode
        // convention, not be treated as direct keysyms.
        assert_eq!(char_to_keysym('\u{1F}'), 0x0100_0000 + 0x1F);
        assert_eq!(char_to_keysym('\u{7F}'), 0x0100_0000 + 0x7F);
    }

    #[test]
    fn test_validate_evdev_code_accepts_in_range() {
        assert_eq!(validate_evdev_code(0, "button"), Some(0));
        assert_eq!(validate_evdev_code(767, "keycode"), Some(767));
    }

    #[test]
    fn test_validate_evdev_code_rejects_out_of_range() {
        assert_eq!(validate_evdev_code(768, "button"), None);
        assert_eq!(validate_evdev_code(u32::MAX, "keycode"), None);
    }

    #[test]
    fn test_scroll_discrete_vertical_only() {
        let events = scroll_discrete_events(0, 3, 1000);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis: ScrollAxis::Vertical,
                steps: 3,
                ..
            })
        ));
    }

    #[test]
    fn test_scroll_discrete_horizontal_only() {
        let events = scroll_discrete_events(-2, 0, 1000);
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis: ScrollAxis::Horizontal,
                steps: -2,
                ..
            })
        ));
    }

    #[test]
    fn test_scroll_discrete_diagonal_emits_both_axes() {
        let events = scroll_discrete_events(-2, 3, 1000);
        assert_eq!(
            events.len(),
            2,
            "simultaneous diagonal scroll must not drop an axis"
        );
        assert!(matches!(
            events[0],
            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis: ScrollAxis::Vertical,
                steps: 3,
                ..
            })
        ));
        assert!(matches!(
            events[1],
            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis: ScrollAxis::Horizontal,
                steps: -2,
                ..
            })
        ));
    }

    #[test]
    fn test_scroll_discrete_zero_emits_nothing() {
        let events = scroll_discrete_events(0, 0, 1000);
        assert!(events.is_empty());
    }
}
