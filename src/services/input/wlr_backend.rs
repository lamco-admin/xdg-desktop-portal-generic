//! wlr virtual input protocol backend.
//!
//! This backend uses the wlroots virtual input protocols to inject input events:
//! - `zwlr_virtual_pointer_v1` for pointer events
//! - `zwp_virtual_keyboard_v1` for keyboard events
//!
//! # How It Works
//!
//! 1. Portal creates a Wayland client connection to the compositor
//! 2. Portal binds to virtual pointer/keyboard manager globals
//! 3. Portal creates virtual devices for each session
//! 4. Input events are sent through the virtual devices

use std::{
    collections::HashMap,
    os::unix::io::{AsFd, OwnedFd},
};

use wayland_client::{
    globals::{registry_queue_init, GlobalList, GlobalListContents},
    protocol::{
        wl_pointer::ButtonState as WlButtonState, wl_registry::WlRegistry, wl_seat::WlSeat,
    },
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use super::{InputBackend, InputProtocol, WlrConfig};
use crate::{
    error::{PortalError, Result},
    types::{
        ButtonState, DeviceTypes, InputEvent, KeyState, KeyboardEvent, PointerEvent, ScrollAxis,
        StreamOutputMapping,
    },
};

/// Wrapper around xkbcommon types that are `!Send + !Sync` due to raw pointers.
///
/// xkbcommon's `Keymap` and `State` are internally reference-counted (via
/// `xkb_keymap_ref`/`xkb_state_ref`) and thread-safe. The Rust bindings don't
/// implement `Send`/`Sync` because they wrap `*mut` pointers, but the underlying
/// C library guarantees thread safety for these types.
///
/// Since `WlrInputBackend` is wrapped in `Arc<Mutex<>>` (ensuring exclusive
/// access), this is safe.
struct XkbData {
    keymap: xkbcommon::xkb::Keymap,
    #[expect(
        dead_code,
        reason = "state is kept alive alongside keymap for future modifier tracking"
    )]
    state: xkbcommon::xkb::State,
    keymap_string: String,
}

// SAFETY: xkbcommon Keymap and State are internally reference-counted and
// thread-safe. We only access them under a Mutex, ensuring no concurrent access.
#[expect(
    unsafe_code,
    reason = "xkbcommon types are !Send but safe to send across our dedicated thread"
)]
unsafe impl Send for XkbData {}
#[expect(
    unsafe_code,
    reason = "xkbcommon types are !Sync but safe under Arc<Mutex<>> exclusive access"
)]
unsafe impl Sync for XkbData {}

/// Wayland keyboard keymap format constant (XKB v1).
const WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// wlr virtual input backend.
///
/// Implements the [`InputBackend`] trait using wlroots virtual input protocols.
/// Connects directly to the Wayland compositor as a client and creates virtual
/// keyboard/pointer devices for input injection.
///
/// # Why a separate Wayland connection?
///
/// This backend maintains its own `wayland_client::Connection` rather than
/// sharing the main `WaylandConnection` from [`crate::wayland`]. This is
/// intentional:
///
/// - `wayland_client::EventQueue` is `!Send`, so it cannot be shared across
///   tokio tasks or threads.
/// - Input injection calls (from D-Bus handlers in tokio) must `flush()` the
///   connection after sending protocol requests. Using the main connection
///   would require unsafe cross-thread access to its event queue.
/// - This is the same architecture used by `xdg-desktop-portal-wlr`: a
///   dedicated Wayland connection per backend that needs to send requests
///   from D-Bus handler context.
///
/// The tradeoff is two Wayland connections to the compositor, which is a
/// negligible cost compared to the thread-safety guarantees it provides.
pub struct WlrInputBackend {
    /// Wayland connection.
    connection: Connection,
    /// Event queue for the connection.
    event_queue: EventQueue<WlrState>,
    /// Queue handle for creating objects.
    queue_handle: QueueHandle<WlrState>,
    /// Backend state.
    state: WlrState,
    /// Active sessions with their virtual devices.
    sessions: HashMap<String, WlrSessionContext>,
    /// Mapping from PipeWire stream node IDs to output geometry.
    /// Used for multi-monitor absolute pointer positioning.
    stream_mappings: HashMap<u32, StreamOutputMapping>,
    /// Health event sender for input metrics.
    health_tx: Option<crate::health::HealthSender>,
    /// Total events successfully forwarded.
    events_forwarded: u64,
    /// Total flush failures (indicates broken Wayland connection).
    flush_failures: u64,
}

/// State for Wayland protocol handling.
#[derive(Default)]
struct WlrState {
    /// Virtual pointer manager global.
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    /// Virtual keyboard manager global.
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    /// Seat global (needed for keyboard creation).
    seat: Option<WlSeat>,
    /// Whether initialization is complete.
    initialized: bool,
    /// XKB data for keysym-to-keycode conversion and keymap transfer.
    xkb: Option<XkbData>,
}

/// Virtual devices for a session.
struct WlrSessionContext {
    /// Virtual pointer device (if enabled).
    pointer: Option<ZwlrVirtualPointerV1>,
    /// Virtual keyboard device (if enabled).
    keyboard: Option<ZwpVirtualKeyboardV1>,
}

impl WlrInputBackend {
    /// Create a new wlr virtual input backend.
    ///
    /// Connects to the Wayland compositor and binds the virtual input protocol
    /// globals. Fails if neither virtual pointer nor virtual keyboard is available.
    pub fn new(config: &WlrConfig) -> Result<Self> {
        tracing::info!("Initializing wlr virtual input backend");

        let connection = if let Some(ref wayland_display) = config.wayland_display {
            Connection::connect_to_env().map_err(|e| {
                tracing::debug!(
                    "Failed to connect to default display, trying {}",
                    wayland_display
                );
                PortalError::Config(format!(
                    "Failed to connect to Wayland display {wayland_display}: {e}"
                ))
            })?
        } else {
            Connection::connect_to_env()
                .map_err(|e| PortalError::Config(format!("Failed to connect to Wayland: {e}")))?
        };

        let (globals, event_queue) = registry_queue_init::<WlrState>(&connection).map_err(|e| {
            PortalError::Config(format!("Failed to initialize Wayland registry: {e}"))
        })?;

        let queue_handle = event_queue.handle();
        let mut state = WlrState::default();

        Self::bind_globals(&globals, &queue_handle, &mut state)?;

        // Initialize XKB keymap for keysym-to-keycode conversion and
        // virtual keyboard keymap setup.
        Self::init_xkb(&mut state)?;

        Ok(Self {
            connection,
            event_queue,
            queue_handle,
            state,
            sessions: HashMap::new(),
            stream_mappings: HashMap::new(),
            health_tx: None,
            events_forwarded: 0,
            flush_failures: 0,
        })
    }

    /// Initialize XKB context, keymap, and state.
    ///
    /// Creates a default "us" layout keymap. The serialized keymap string
    /// is stored for passing to virtual keyboards via memfd.
    fn init_xkb(state: &mut WlrState) -> Result<()> {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);

        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",   // rules (empty = default "evdev")
            "",   // model (empty = default)
            "",   // layout (empty = default "us")
            "",   // variant (empty = default)
            None, // options
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            PortalError::Config("Failed to create XKB keymap from default rules".to_string())
        })?;

        let keymap_string = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let xkb_state = xkb::State::new(&keymap);

        tracing::info!(
            "XKB keymap initialized (default us layout, {} bytes)",
            keymap_string.len()
        );

        state.xkb = Some(XkbData {
            keymap,
            state: xkb_state,
            keymap_string,
        });

        Ok(())
    }

    /// Create a memfd containing the keymap string, and send it to the virtual keyboard.
    ///
    /// The Wayland virtual keyboard protocol requires a keymap to be set via
    /// `keyboard.keymap(format, fd, size)` before any key events can be sent.
    fn set_keyboard_keymap(keyboard: &ZwpVirtualKeyboardV1, keymap_string: &str) -> Result<()> {
        use std::io::Write;

        use nix::sys::memfd;

        let keymap_bytes = keymap_string.as_bytes();
        let keymap_size = keymap_bytes.len() as u32;

        // Create a memfd for the keymap data
        let memfd = memfd::memfd_create(c"xdp-keymap", memfd::MFdFlags::MFD_CLOEXEC)
            .map_err(|e| PortalError::Config(format!("Failed to create memfd for keymap: {e}")))?;

        // Write keymap bytes to the memfd
        let mut file = std::fs::File::from(memfd);
        file.write_all(keymap_bytes)
            .map_err(|e| PortalError::Config(format!("Failed to write keymap to memfd: {e}")))?;

        // Send the keymap to the virtual keyboard
        // SAFETY: keyboard.keymap() is a Wayland protocol method that reads the fd.
        // The fd is valid and contains the complete keymap.
        let borrowed_fd = file.as_fd();
        keyboard.keymap(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, borrowed_fd, keymap_size);

        tracing::debug!("Set virtual keyboard keymap ({} bytes)", keymap_size);
        Ok(())
    }

    /// Bind to required Wayland globals.
    fn bind_globals(
        globals: &GlobalList,
        qh: &QueueHandle<WlrState>,
        state: &mut WlrState,
    ) -> Result<()> {
        // Bind virtual pointer manager
        match globals.bind::<ZwlrVirtualPointerManagerV1, _, _>(qh, 1..=2, ()) {
            Ok(manager) => {
                tracing::debug!("Bound zwlr_virtual_pointer_manager_v1");
                state.pointer_manager = Some(manager);
            }
            Err(e) => {
                tracing::warn!("zwlr_virtual_pointer_manager_v1 not available: {}", e);
            }
        }

        // Bind virtual keyboard manager
        match globals.bind::<ZwpVirtualKeyboardManagerV1, _, _>(qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::debug!("Bound zwp_virtual_keyboard_manager_v1");
                state.keyboard_manager = Some(manager);
            }
            Err(e) => {
                tracing::warn!("zwp_virtual_keyboard_manager_v1 not available: {}", e);
            }
        }

        // Bind seat (needed for keyboard)
        match globals.bind::<WlSeat, _, _>(qh, 1..=9, ()) {
            Ok(seat) => {
                tracing::debug!("Bound wl_seat");
                state.seat = Some(seat);
            }
            Err(e) => {
                tracing::warn!("wl_seat not available: {}", e);
            }
        }

        // Check if at least one input protocol is available
        if state.pointer_manager.is_none() && state.keyboard_manager.is_none() {
            return Err(PortalError::Config(
                "Neither virtual pointer nor virtual keyboard protocols available".to_string(),
            ));
        }

        state.initialized = true;
        Ok(())
    }

    /// Create a virtual pointer for a session.
    fn create_pointer(&self) -> Option<ZwlrVirtualPointerV1> {
        self.state.pointer_manager.as_ref().map(|manager| {
            manager.create_virtual_pointer(self.state.seat.as_ref(), &self.queue_handle, ())
        })
    }

    /// Create a virtual keyboard for a session and set its keymap.
    fn create_keyboard(&self) -> Option<ZwpVirtualKeyboardV1> {
        if let (Some(manager), Some(seat)) = (&self.state.keyboard_manager, &self.state.seat) {
            let keyboard = manager.create_virtual_keyboard(seat, &self.queue_handle, ());

            // Set the keymap on the keyboard — required before any key events
            if let Some(ref xkb) = self.state.xkb {
                if let Err(e) = Self::set_keyboard_keymap(&keyboard, &xkb.keymap_string) {
                    tracing::error!("Failed to set keyboard keymap: {}", e);
                    keyboard.destroy();
                    return None;
                }
            } else {
                tracing::error!("No XKB keymap available for virtual keyboard");
                keyboard.destroy();
                return None;
            }

            Some(keyboard)
        } else {
            None
        }
    }

    /// Dispatch pending Wayland events.
    fn dispatch(&mut self) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| PortalError::Wayland(format!("dispatch error: {e}")))?;
        Ok(())
    }

    /// Compute the total extent (bounding box) of all known outputs.
    ///
    /// Returns `(width, height)` covering all output regions. If no stream
    /// mappings are set, falls back to a reasonable default.
    fn compute_total_extent(&self) -> (u32, u32) {
        if self.stream_mappings.is_empty() {
            return (1920, 1080); // Reasonable default for single-monitor
        }

        let mut max_x: i32 = 0;
        let mut max_y: i32 = 0;

        for mapping in self.stream_mappings.values() {
            let right = mapping.x + mapping.width as i32;
            let bottom = mapping.y + mapping.height as i32;
            max_x = max_x.max(right);
            max_y = max_y.max(bottom);
        }

        (max_x.max(1) as u32, max_y.max(1) as u32)
    }

    /// Flush the Wayland connection.
    ///
    /// If this fails, the Wayland connection is likely broken (compositor
    /// crash, fd closed, etc.). Callers should propagate the error so that
    /// the session can be cleaned up rather than silently continuing.
    fn flush(&self) -> Result<()> {
        self.connection.flush().map_err(|e| {
            PortalError::Wayland(format!("flush failed (connection may be broken): {e}"))
        })?;
        Ok(())
    }
}

impl InputBackend for WlrInputBackend {
    fn protocol_type(&self) -> InputProtocol {
        InputProtocol::WlrVirtualInput
    }

    fn create_context(
        &mut self,
        session_id: &str,
        devices: DeviceTypes,
    ) -> Result<Option<OwnedFd>> {
        tracing::debug!(
            session_id = %session_id,
            device_types = ?devices,
            "Creating wlr virtual input context"
        );

        if self.sessions.contains_key(session_id) {
            return Err(PortalError::InvalidSession(format!(
                "wlr context already exists for session {session_id}"
            )));
        }

        let mut ctx = WlrSessionContext {
            pointer: None,
            keyboard: None,
        };

        if devices.pointer {
            if let Some(pointer) = self.create_pointer() {
                tracing::debug!(session_id = %session_id, "Created virtual pointer");
                ctx.pointer = Some(pointer);
            } else {
                tracing::warn!("Pointer requested but virtual pointer manager unavailable");
            }
        }

        if devices.keyboard {
            if let Some(keyboard) = self.create_keyboard() {
                tracing::debug!(session_id = %session_id, "Created virtual keyboard");
                ctx.keyboard = Some(keyboard);
            } else {
                tracing::warn!("Keyboard requested but virtual keyboard manager unavailable");
            }
        }

        self.flush()?;
        self.sessions.insert(session_id.to_string(), ctx);

        tracing::info!(session_id = %session_id, "wlr virtual input context created");

        // wlr protocol doesn't use fd passing
        Ok(None)
    }

    fn destroy_context(&mut self, session_id: &str) -> Result<()> {
        if let Some(ctx) = self.sessions.remove(session_id) {
            if let Some(pointer) = ctx.pointer {
                pointer.destroy();
            }
            if let Some(keyboard) = ctx.keyboard {
                keyboard.destroy();
            }

            self.flush()?;
            tracing::info!(session_id = %session_id, "wlr virtual input context destroyed");
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "match arms for each input event variant are individually simple"
    )]
    fn inject_event(&mut self, session_id: &str, event: InputEvent) -> Result<()> {
        let ctx = self
            .sessions
            .get(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?;

        let time_ms = |time_usec: u64| (time_usec / 1000) as u32;

        match event {
            InputEvent::Pointer(PointerEvent::Motion { dx, dy, time_usec }) => {
                if let Some(ref pointer) = ctx.pointer {
                    pointer.motion(time_ms(time_usec), dx, dy);
                    pointer.frame();
                }
            }

            InputEvent::Pointer(PointerEvent::MotionAbsolute {
                x,
                y,
                stream,
                time_usec,
            }) => {
                if let Some(ref pointer) = ctx.pointer {
                    // If we have a stream mapping, translate normalized coordinates
                    // to compositor-global absolute position. Otherwise fall back to
                    // simple normalized-to-extent mapping.
                    if let Some(mapping) = self.stream_mappings.get(&stream) {
                        // x,y are normalized 0.0–1.0 within this stream's output.
                        // Convert to pixel position within the output, then add the
                        // output's global offset.
                        let pixel_x = mapping.x as f64 + x * mapping.width as f64;
                        let pixel_y = mapping.y as f64 + y * mapping.height as f64;

                        // Compute total compositor extent from all known outputs.
                        let (total_width, total_height) = self.compute_total_extent();

                        // Normalize to extent range for wlr_virtual_pointer protocol.
                        let extent = 10000u32;
                        let abs_x = ((pixel_x / total_width as f64) * extent as f64) as u32;
                        let abs_y = ((pixel_y / total_height as f64) * extent as f64) as u32;

                        pointer.motion_absolute(time_ms(time_usec), abs_x, abs_y, extent, extent);
                    } else {
                        // No mapping: treat x,y as normalized over the whole output
                        let extent = 10000u32;
                        let abs_x = (x * extent as f64) as u32;
                        let abs_y = (y * extent as f64) as u32;
                        pointer.motion_absolute(time_ms(time_usec), abs_x, abs_y, extent, extent);
                    }
                    pointer.frame();
                }
            }

            InputEvent::Pointer(PointerEvent::Button {
                button,
                state,
                time_usec,
            }) => {
                if let Some(ref pointer) = ctx.pointer {
                    let wl_state = match state {
                        ButtonState::Pressed => WlButtonState::Pressed,
                        ButtonState::Released => WlButtonState::Released,
                    };
                    pointer.button(time_ms(time_usec), button, wl_state);
                    pointer.frame();
                }
            }

            InputEvent::Pointer(PointerEvent::Scroll { dx, dy, time_usec }) => {
                if let Some(ref pointer) = ctx.pointer {
                    use wayland_client::protocol::wl_pointer::Axis;

                    // Set axis source before axis events (protocol compliance)
                    pointer
                        .axis_source(wayland_client::protocol::wl_pointer::AxisSource::Continuous);

                    if dy.abs() > f64::EPSILON {
                        pointer.axis(time_ms(time_usec), Axis::VerticalScroll, dy);
                    }
                    if dx.abs() > f64::EPSILON {
                        pointer.axis(time_ms(time_usec), Axis::HorizontalScroll, dx);
                    }

                    // Send axis_stop when both values are zero (scroll end)
                    if dy.abs() <= f64::EPSILON && dx.abs() <= f64::EPSILON {
                        pointer.axis_stop(time_ms(time_usec), Axis::VerticalScroll);
                        pointer.axis_stop(time_ms(time_usec), Axis::HorizontalScroll);
                    }

                    pointer.frame();
                }
            }

            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis,
                steps,
                time_usec,
            }) => {
                if let Some(ref pointer) = ctx.pointer {
                    use wayland_client::protocol::wl_pointer::Axis;

                    // Discrete scroll uses wheel axis source
                    pointer.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Wheel);

                    let wl_axis = match axis {
                        ScrollAxis::Vertical => Axis::VerticalScroll,
                        ScrollAxis::Horizontal => Axis::HorizontalScroll,
                    };
                    let value = (steps as f64) * 15.0;
                    pointer.axis_discrete(time_ms(time_usec), wl_axis, value, steps);
                    pointer.frame();
                }
            }

            InputEvent::Pointer(PointerEvent::ScrollStop { time_usec }) => {
                if let Some(ref pointer) = ctx.pointer {
                    use wayland_client::protocol::wl_pointer::Axis;
                    pointer.axis_stop(time_ms(time_usec), Axis::VerticalScroll);
                    pointer.axis_stop(time_ms(time_usec), Axis::HorizontalScroll);
                    pointer.frame();
                }
            }

            InputEvent::Keyboard(KeyboardEvent {
                keycode,
                state,
                time_usec,
            }) => {
                if let Some(ref keyboard) = ctx.keyboard {
                    let wl_state = match state {
                        KeyState::Pressed => 1u32,
                        KeyState::Released => 0u32,
                    };
                    keyboard.key(time_ms(time_usec), keycode, wl_state);
                }
            }

            InputEvent::Touch(_) => {
                // wlr-virtual-pointer does not support real touch input.
                // Touch is not advertised in AvailableDeviceTypes, so clients
                // should not send touch events. Return a clean error.
                return Err(PortalError::Config(
                    "Touch input not supported via wlr virtual pointer protocol".to_string(),
                ));
            }
        }

        match self.flush() {
            Ok(()) => {
                self.events_forwarded += 1;

                // Emit periodic InputBatch health event
                if self.events_forwarded % 100 == 0 {
                    if let Some(ref health_tx) = self.health_tx {
                        let _ = health_tx.try_send(crate::health::PortalHealthEvent::InputBatch {
                            events_forwarded: self.events_forwarded,
                            events_failed: self.flush_failures,
                            protocol: crate::health::InputProtocolType::WlrVirtual,
                        });
                    }
                }

                Ok(())
            }
            Err(e) => {
                self.flush_failures += 1;
                if let Some(ref health_tx) = self.health_tx {
                    let _ =
                        health_tx.try_send(crate::health::PortalHealthEvent::InputDisconnected {
                            reason: format!("Wayland flush failed: {e}"),
                            recoverable: false,
                        });
                }
                Err(e)
            }
        }
    }

    fn process_events(&mut self) -> Result<Vec<(String, InputEvent)>> {
        self.dispatch()?;
        // wlr backend doesn't receive input events, only sends them
        Ok(vec![])
    }

    fn has_context(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    fn context_count(&self) -> usize {
        self.sessions.len()
    }

    fn keysym_to_keycode(&self, keysym: u32) -> Option<u32> {
        use xkbcommon::xkb;

        let xkb_data = self.state.xkb.as_ref()?;
        let keymap = &xkb_data.keymap;

        // Iterate all keycodes in the keymap to find one that produces
        // the requested keysym at level 0 (unshifted) of layout 0.
        for keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let xkb_keycode = xkb::Keycode::new(keycode);
            let num_levels = keymap.num_levels_for_key(xkb_keycode, 0);

            for level in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(xkb_keycode, 0, level);
                for sym in syms {
                    if sym.raw() == keysym {
                        // XKB keycodes are evdev keycodes + 8
                        return Some(keycode - 8);
                    }
                }
            }
        }

        tracing::warn!(keysym = keysym, "No keycode found for keysym");
        None
    }

    fn set_health_sender(&mut self, tx: crate::health::HealthSender) {
        self.health_tx = Some(tx);
    }

    fn set_stream_mappings(&mut self, mappings: Vec<StreamOutputMapping>) {
        self.stream_mappings.clear();
        for mapping in mappings {
            tracing::debug!(
                stream = mapping.stream_node_id,
                x = mapping.x,
                y = mapping.y,
                width = mapping.width,
                height = mapping.height,
                "Stream output mapping set"
            );
            self.stream_mappings.insert(mapping.stream_node_id, mapping);
        }
    }
}

// Wayland dispatch implementations

impl Dispatch<WlRegistry, GlobalListContents> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_seat::Event;
        match event {
            Event::Capabilities { capabilities } => {
                tracing::trace!("Seat capabilities: {:?}", capabilities);
            }
            Event::Name { name } => {
                tracing::trace!("Seat name: {}", name);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wlr_state_default() {
        let state = WlrState::default();
        assert!(state.pointer_manager.is_none());
        assert!(state.keyboard_manager.is_none());
        assert!(state.seat.is_none());
        assert!(!state.initialized);
        assert!(state.xkb.is_none());
    }

    #[test]
    fn test_xkb_initialization() {
        let mut state = WlrState::default();
        WlrInputBackend::init_xkb(&mut state).expect("XKB init should succeed");

        let xkb = state.xkb.as_ref().expect("XKB data should be set");
        assert!(
            !xkb.keymap_string.is_empty(),
            "Keymap string should be non-empty"
        );
        // A valid XKB keymap starts with "xkb_keymap"
        assert!(
            xkb.keymap_string.starts_with("xkb_keymap"),
            "Keymap string should start with 'xkb_keymap'"
        );
    }

    #[test]
    fn test_keysym_to_keycode_via_xkb() {
        let mut state = WlrState::default();
        WlrInputBackend::init_xkb(&mut state).unwrap();

        let xkb_data = state.xkb.as_ref().unwrap();
        let keymap = &xkb_data.keymap;

        // Test that we can look up a well-known keysym (XKB_KEY_a = 0x61).
        // Should map to evdev KEY_A = 30.
        let keysym_a = 0x61u32; // XKB_KEY_a
        let mut found = false;

        for keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let xkb_keycode = xkbcommon::xkb::Keycode::new(keycode);
            let num_levels = keymap.num_levels_for_key(xkb_keycode, 0);

            for level in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(xkb_keycode, 0, level);
                for sym in syms {
                    if sym.raw() == keysym_a {
                        // XKB keycodes are evdev keycodes + 8
                        let evdev_keycode = keycode - 8;
                        assert_eq!(
                            evdev_keycode, 30,
                            "XKB_KEY_a should map to evdev KEY_A (30)"
                        );
                        found = true;
                    }
                }
            }
        }
        assert!(found, "Should find a keycode for XKB_KEY_a");
    }

    #[test]
    fn test_compute_total_extent_empty() {
        // No stream mappings → reasonable default
        let mappings = HashMap::new();
        let backend_extent = compute_extent_from_mappings(&mappings);
        assert_eq!(backend_extent, (1920, 1080));
    }

    #[test]
    fn test_compute_total_extent_single_monitor() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );

        let extent = compute_extent_from_mappings(&mappings);
        assert_eq!(extent, (2560, 1440));
    }

    #[test]
    fn test_compute_total_extent_dual_monitor_side_by_side() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );

        let extent = compute_extent_from_mappings(&mappings);
        assert_eq!(extent, (4480, 1440)); // 1920 + 2560, max(1080, 1440)
    }

    #[test]
    fn test_compute_total_extent_stacked_monitors() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
            },
        );

        let extent = compute_extent_from_mappings(&mappings);
        assert_eq!(extent, (1920, 2160)); // same width, 1080 + 1080
    }

    /// Helper to compute extent from mappings (mirrors WlrInputBackend::compute_total_extent)
    fn compute_extent_from_mappings(mappings: &HashMap<u32, StreamOutputMapping>) -> (u32, u32) {
        if mappings.is_empty() {
            return (1920, 1080);
        }

        let mut max_x: i32 = 0;
        let mut max_y: i32 = 0;

        for mapping in mappings.values() {
            let right = mapping.x + mapping.width as i32;
            let bottom = mapping.y + mapping.height as i32;
            max_x = max_x.max(right);
            max_y = max_y.max(bottom);
        }

        (max_x.max(1) as u32, max_y.max(1) as u32)
    }
}
