//! Session state types and state machine.

use std::time::SystemTime;

use zbus::zvariant::ObjectPath;

use crate::{
    error::{PortalError, Result},
    types::{DeviceTypes, PointerBarrier, SourceInfo, StreamInfo},
};

/// Restore data for persistent sessions.
///
/// Stored by ScreenCast/RemoteDesktop when `persist_mode != None`, and passed
/// back on subsequent `SelectSources` calls to restore the previous selection.
/// Serialized on D-Bus as `(suv)`: (vendor, version, data).
#[derive(Debug, Clone)]
pub struct RestoreData {
    /// Vendor identifier (always `"generic"` for this backend).
    pub vendor: String,
    /// Data format version.
    pub version: u32,
    /// Previously selected output names.
    pub output_names: Vec<String>,
}

/// Session persistence mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum PersistMode {
    /// Session is not persistent.
    #[default]
    None,
    /// Persist until explicitly revoked.
    Persistent,
    /// Persist for this application instance only.
    Transient,
}

impl PersistMode {
    /// Parse from D-Bus value.
    pub fn from_dbus(value: u32) -> Self {
        match value {
            1 => PersistMode::Transient,
            2 => PersistMode::Persistent,
            _ => PersistMode::None,
        }
    }

    /// Convert to D-Bus value.
    pub fn to_dbus(self) -> u32 {
        match self {
            PersistMode::None => 0,
            PersistMode::Transient => 1,
            PersistMode::Persistent => 2,
        }
    }
}

/// Session state in the state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum SessionState {
    /// Session created but not started.
    #[default]
    Init,
    /// Session is active.
    Started,
    /// Session is closed.
    Closed,
}

impl std::fmt::Display for SessionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionState::Init => write!(f, "Init"),
            SessionState::Started => write!(f, "Started"),
            SessionState::Closed => write!(f, "Closed"),
        }
    }
}

/// A portal session.
///
/// Sessions track the state and resources for a RemoteDesktop/ScreenCast/Clipboard
/// portal connection.
#[derive(Debug)]
pub struct Session {
    /// Unique session ID (D-Bus object path).
    pub id: ObjectPath<'static>,
    /// D-Bus sender that created this session.
    pub sender: String,
    /// Application ID.
    pub app_id: String,
    /// Current session state.
    pub state: SessionState,

    // RemoteDesktop
    /// Selected device types for input.
    pub device_types: DeviceTypes,
    /// Whether devices have been selected.
    pub devices_selected: bool,
    /// Whether this session uses EIS for input.
    pub uses_eis: bool,
    /// EIS context ID if connected.
    pub eis_context_id: Option<u32>,

    // ScreenCast
    /// Selected sources for capture.
    pub sources: Vec<SourceInfo>,
    /// Whether sources have been selected.
    pub sources_selected: bool,
    /// Active streams.
    pub streams: Vec<StreamInfo>,

    // Clipboard
    /// Whether clipboard is enabled for this session.
    pub clipboard_enabled: bool,
    /// Whether clipboard has been requested.
    pub clipboard_requested: bool,

    // InputCapture
    /// Whether this session was created via `InputCapture.CreateSession`/
    /// `CreateSession2`, as opposed to RemoteDesktop or ScreenCast. Scopes
    /// `GetZones`/`SetPointerBarriers`/`Enable`/`Disable`/`Release`/
    /// `ConnectToEIS` and filters which sessions receive `ZonesChanged`.
    pub is_input_capture_session: bool,
    /// Toggled by `Enable()`/`Disable()`. Independent of `state` — a
    /// `Started` session can be enabled/disabled repeatedly without
    /// restarting.
    pub input_capture_enabled: bool,
    /// Whether a barrier has actually fired (pointer locked, capture in
    /// progress).
    pub input_capture_active: bool,
    /// Barriers from the most recent successful `SetPointerBarriers` call.
    pub input_capture_barriers: Vec<PointerBarrier>,
    /// Monotonic counter for `activation_id` values handed out by
    /// [`Self::begin_input_capture_activation`].
    pub input_capture_next_activation_id: u32,
    /// The current activation's id, per the spec's `Activated`/
    /// `Deactivated`/`Release` `activation_id` contract. `None` when no
    /// activation is in progress.
    pub input_capture_activation_id: Option<u32>,

    // Persistence
    /// Persistence mode for this session.
    pub persist_mode: PersistMode,
    /// Restore token for persistent sessions.
    pub restore_token: Option<String>,
    /// Restore data from a previous session: output names that were previously selected.
    pub restore_data: Option<RestoreData>,

    // Metadata
    /// When the session was created.
    pub created_at: SystemTime,
    /// When the session was started (if started).
    pub started_at: Option<SystemTime>,
    /// When the last activity occurred (updated on any D-Bus call for this session).
    pub last_activity: SystemTime,
}

impl Session {
    /// Create a new session.
    pub fn new(id: ObjectPath<'static>, sender: String, app_id: String) -> Self {
        Self {
            id,
            sender,
            app_id,
            state: SessionState::Init,

            device_types: DeviceTypes::default(),
            devices_selected: false,
            uses_eis: false,
            eis_context_id: None,

            sources: Vec::new(),
            sources_selected: false,
            streams: Vec::new(),

            clipboard_enabled: false,
            clipboard_requested: false,

            is_input_capture_session: false,
            input_capture_enabled: false,
            input_capture_active: false,
            input_capture_barriers: Vec::new(),
            input_capture_next_activation_id: 0,
            input_capture_activation_id: None,

            persist_mode: PersistMode::None,
            restore_token: None,
            restore_data: None,

            created_at: SystemTime::now(),
            started_at: None,
            last_activity: SystemTime::now(),
        }
    }

    /// Record that activity occurred on this session.
    pub fn touch(&mut self) {
        self.last_activity = SystemTime::now();
    }

    /// Check if devices can be selected in current state.
    pub fn can_select_devices(&self) -> bool {
        self.state == SessionState::Init
    }

    /// Check if sources can be selected in current state.
    pub fn can_select_sources(&self) -> bool {
        self.state == SessionState::Init
    }

    /// Check if clipboard can be requested in current state.
    pub fn can_request_clipboard(&self) -> bool {
        // Clipboard can be requested before or after Start
        self.state != SessionState::Closed
    }

    /// Check if the session can be started.
    ///
    /// Requires that at least one of devices or sources has been selected,
    /// preventing empty sessions from starting.
    pub fn can_start(&self) -> bool {
        self.state == SessionState::Init && (self.devices_selected || self.sources_selected)
    }

    /// Check if the session has been started (is actively streaming).
    pub fn is_started(&self) -> bool {
        self.state == SessionState::Started
    }

    /// Check if EIS can be connected.
    pub fn can_connect_to_eis(&self) -> bool {
        self.state == SessionState::Started && self.devices_selected && !self.uses_eis
    }

    /// Transition to a new state.
    pub fn transition_to(&mut self, new_state: SessionState) -> Result<()> {
        let valid = match (self.state, new_state) {
            // Init -> Started (via Start), or Any -> Closed
            (SessionState::Init, SessionState::Started) | (_, SessionState::Closed) => true,
            // Same state is a no-op
            (s1, s2) if s1 == s2 => true,
            // Everything else is invalid
            _ => false,
        };

        if valid {
            tracing::debug!(
                session_id = %self.id,
                from = %self.state,
                to = %new_state,
                "Session state transition"
            );
            self.state = new_state;
            if new_state == SessionState::Started {
                self.started_at = Some(SystemTime::now());
            }
            Ok(())
        } else {
            Err(PortalError::InvalidState {
                expected: format!("valid transition from {}", self.state),
                actual: format!("{} -> {}", self.state, new_state),
            })
        }
    }

    /// Select devices for input.
    pub fn select_devices(&mut self, devices: DeviceTypes) -> Result<()> {
        if !self.can_select_devices() {
            return Err(PortalError::InvalidState {
                expected: "Init".to_string(),
                actual: self.state.to_string(),
            });
        }
        self.device_types = devices;
        self.devices_selected = true;
        tracing::debug!(
            session_id = %self.id,
            keyboard = devices.keyboard,
            pointer = devices.pointer,
            touchscreen = devices.touchscreen,
            "Devices selected"
        );
        Ok(())
    }

    /// Select sources for capture.
    pub fn select_sources(&mut self, sources: Vec<SourceInfo>) -> Result<()> {
        if !self.can_select_sources() {
            return Err(PortalError::InvalidState {
                expected: "Init".to_string(),
                actual: self.state.to_string(),
            });
        }
        tracing::debug!(
            session_id = %self.id,
            source_count = sources.len(),
            "Sources selected"
        );
        self.sources = sources;
        self.sources_selected = true;
        Ok(())
    }

    /// Request clipboard access.
    pub fn request_clipboard(&mut self) -> Result<()> {
        if !self.can_request_clipboard() {
            return Err(PortalError::InvalidState {
                expected: "not Closed".to_string(),
                actual: self.state.to_string(),
            });
        }
        self.clipboard_requested = true;
        self.clipboard_enabled = true;
        tracing::debug!(session_id = %self.id, "Clipboard enabled");
        Ok(())
    }

    /// Start the session.
    pub fn start(&mut self, streams: Vec<StreamInfo>) -> Result<()> {
        if !self.can_start() {
            return Err(PortalError::InvalidState {
                expected: "Init".to_string(),
                actual: self.state.to_string(),
            });
        }
        self.streams = streams;
        self.transition_to(SessionState::Started)?;
        tracing::info!(
            session_id = %self.id,
            app_id = %self.app_id,
            stream_count = self.streams.len(),
            "Session started"
        );
        Ok(())
    }

    /// Mark that EIS is connected.
    pub fn connect_to_eis(&mut self, context_id: u32) -> Result<()> {
        if !self.can_connect_to_eis() {
            return Err(PortalError::InvalidState {
                expected: "Started with devices selected".to_string(),
                actual: format!(
                    "state={}, devices_selected={}",
                    self.state, self.devices_selected
                ),
            });
        }
        self.uses_eis = true;
        self.eis_context_id = Some(context_id);
        tracing::debug!(
            session_id = %self.id,
            context_id = context_id,
            "EIS connected"
        );
        Ok(())
    }

    /// Enable or disable InputCapture for this session.
    ///
    /// Requires the session to be `Started`. Disabling while a barrier is
    /// active clears `input_capture_active` (the caller is responsible for
    /// emitting `Deactivated` before `Disabled` — this method only updates
    /// state).
    pub fn set_input_capture_enabled(&mut self, enabled: bool) -> Result<()> {
        if self.state != SessionState::Started {
            return Err(PortalError::InvalidState {
                expected: "Started".to_string(),
                actual: self.state.to_string(),
            });
        }
        self.input_capture_enabled = enabled;
        if !enabled {
            self.input_capture_active = false;
            // Prevents a stale id from producing a duplicate Deactivated
            // later -- end_input_capture_activation() already no-ops
            // safely on None.
            self.input_capture_activation_id = None;
        }
        tracing::debug!(
            session_id = %self.id,
            enabled = enabled,
            "InputCapture enabled state changed"
        );
        Ok(())
    }

    /// Begin a new InputCapture activation: bumps the counter, marks the
    /// session active, and returns the new `activation_id` for the
    /// `Activated` signal's `options`.
    pub fn begin_input_capture_activation(&mut self) -> u32 {
        let id = self.input_capture_next_activation_id;
        self.input_capture_next_activation_id =
            self.input_capture_next_activation_id.wrapping_add(1);
        self.input_capture_activation_id = Some(id);
        self.input_capture_active = true;
        id
    }

    /// End the current InputCapture activation, if any. Returns the
    /// `activation_id` that was active (for the `Deactivated` signal's
    /// `options`), or `None` if there wasn't one -- e.g. `Disable()`
    /// already ended it, so the caller should skip emitting a duplicate
    /// signal.
    pub fn end_input_capture_activation(&mut self) -> Option<u32> {
        self.input_capture_active = false;
        self.input_capture_activation_id.take()
    }

    /// Replace this session's pointer barriers with a newly-accepted set.
    pub fn set_pointer_barriers(&mut self, barriers: Vec<PointerBarrier>) {
        tracing::debug!(
            session_id = %self.id,
            barrier_count = barriers.len(),
            "Pointer barriers updated"
        );
        self.input_capture_barriers = barriers;
    }

    /// Close the session.
    pub fn close(&mut self) {
        if self.state != SessionState::Closed {
            tracing::info!(
                session_id = %self.id,
                app_id = %self.app_id,
                "Session closed"
            );
            self.state = SessionState::Closed;
        }
    }

    /// Get stream IDs for cleanup.
    pub fn stream_ids(&self) -> Vec<u32> {
        self.streams.iter().map(|s| s.node_id).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_session() -> Session {
        Session::new(
            ObjectPath::try_from("/org/freedesktop/portal/generic/session/test").unwrap(),
            ":1.123".to_string(),
            "com.example.app".to_string(),
        )
    }

    #[test]
    fn test_session_creation() {
        let session = test_session();
        assert_eq!(session.state, SessionState::Init);
        assert!(!session.devices_selected);
        assert!(!session.sources_selected);
        assert!(!session.clipboard_enabled);
    }

    #[test]
    fn test_session_state_transitions() {
        let mut session = test_session();

        // Cannot start without selecting devices or sources
        assert!(!session.can_start());
        assert!(session.start(vec![]).is_err());

        // Select devices, then start
        session.select_devices(DeviceTypes::all()).unwrap();
        assert!(session.can_start());
        assert!(session.start(vec![]).is_ok());
        assert_eq!(session.state, SessionState::Started);

        // Invalid: Started -> Started (no-op is ok)
        assert!(session.transition_to(SessionState::Started).is_ok());

        // Valid: Started -> Closed
        session.close();
        assert_eq!(session.state, SessionState::Closed);

        // Invalid: Closed -> Started
        assert!(session.transition_to(SessionState::Started).is_err());
    }

    #[test]
    fn test_cannot_start_without_selection() {
        let mut session = test_session();

        // No devices or sources selected → can't start
        assert!(!session.can_start());
        assert!(session.start(vec![]).is_err());

        // Select sources → can start
        let mut session2 = test_session();
        session2
            .select_sources(vec![crate::types::SourceInfo {
                id: 1,
                name: "Test".to_string(),
                description: "Test output".to_string(),
                width: 1920,
                height: 1080,
                refresh_rate: 60000,
                source_type: crate::types::SourceType::Monitor,
            }])
            .unwrap();
        assert!(session2.can_start());
    }

    #[test]
    fn test_device_selection() {
        let mut session = test_session();

        let devices = DeviceTypes {
            keyboard: true,
            pointer: true,
            touchscreen: false,
        };

        assert!(session.can_select_devices());
        assert!(session.select_devices(devices).is_ok());
        assert!(session.devices_selected);
        assert!(session.device_types.keyboard);
        assert!(session.device_types.pointer);
        assert!(!session.device_types.touchscreen);

        // Can't select devices after start
        session.start(vec![]).unwrap();
        assert!(!session.can_select_devices());
    }

    #[test]
    fn test_eis_connection() {
        let mut session = test_session();

        // Can't connect before start
        assert!(!session.can_connect_to_eis());

        // Select devices and start
        session.select_devices(DeviceTypes::all()).unwrap();
        session.start(vec![]).unwrap();

        // Now can connect
        assert!(session.can_connect_to_eis());
        assert!(session.connect_to_eis(42).is_ok());
        assert!(session.uses_eis);
        assert_eq!(session.eis_context_id, Some(42));

        // Can't connect twice
        assert!(!session.can_connect_to_eis());
    }

    #[test]
    fn test_clipboard_request() {
        let mut session = test_session();

        // Can request before start
        assert!(session.can_request_clipboard());
        assert!(session.request_clipboard().is_ok());
        assert!(session.clipboard_enabled);

        // Can request after start too
        let mut session2 = test_session();
        session2.select_devices(DeviceTypes::all()).unwrap();
        session2.start(vec![]).unwrap();
        assert!(session2.can_request_clipboard());

        // Can't request after close
        session2.close();
        assert!(!session2.can_request_clipboard());
    }

    #[test]
    fn test_input_capture_session_marker() {
        let mut session = test_session();
        assert!(!session.is_input_capture_session);
        session.is_input_capture_session = true;
        assert!(session.is_input_capture_session);
    }

    #[test]
    fn test_input_capture_enable_requires_started() {
        let mut session = test_session();

        // Can't enable before Start
        assert!(session.set_input_capture_enabled(true).is_err());
        assert!(!session.input_capture_enabled);

        session.select_devices(DeviceTypes::all()).unwrap();
        session.start(vec![]).unwrap();

        assert!(session.set_input_capture_enabled(true).is_ok());
        assert!(session.input_capture_enabled);
    }

    #[test]
    fn test_input_capture_disable_clears_active() {
        let mut session = test_session();
        session.select_devices(DeviceTypes::all()).unwrap();
        session.start(vec![]).unwrap();

        session.set_input_capture_enabled(true).unwrap();
        session.input_capture_active = true;

        session.set_input_capture_enabled(false).unwrap();
        assert!(!session.input_capture_enabled);
        assert!(!session.input_capture_active);
    }

    #[test]
    fn test_input_capture_activation_id_sequencing() {
        let mut session = test_session();
        assert_eq!(session.input_capture_activation_id, None);

        let first = session.begin_input_capture_activation();
        assert_eq!(first, 0);
        assert!(session.input_capture_active);
        assert_eq!(session.input_capture_activation_id, Some(0));

        let ended = session.end_input_capture_activation();
        assert_eq!(ended, Some(0));
        assert!(!session.input_capture_active);
        assert_eq!(session.input_capture_activation_id, None);

        // Counter keeps advancing across repeated activations.
        let second = session.begin_input_capture_activation();
        assert_eq!(second, 1);
    }

    #[test]
    fn test_input_capture_end_activation_without_begin_is_none() {
        let mut session = test_session();
        // No activation in progress -- e.g. Disable() already ended it.
        assert_eq!(session.end_input_capture_activation(), None);
    }

    #[test]
    fn test_input_capture_disable_clears_activation_id() {
        let mut session = test_session();
        session.select_devices(DeviceTypes::all()).unwrap();
        session.start(vec![]).unwrap();

        session.set_input_capture_enabled(true).unwrap();
        session.begin_input_capture_activation();
        assert!(session.input_capture_activation_id.is_some());

        session.set_input_capture_enabled(false).unwrap();
        assert_eq!(session.input_capture_activation_id, None);
        // A late end_input_capture_activation() (e.g. from a stray
        // Deactivated event) must not fire a duplicate signal.
        assert_eq!(session.end_input_capture_activation(), None);
    }

    #[test]
    fn test_pointer_barriers_replace() {
        let mut session = test_session();
        assert!(session.input_capture_barriers.is_empty());

        let barrier = crate::types::PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 0,
            x2: 1920,
            y2: 0,
        };
        session.set_pointer_barriers(vec![barrier]);
        assert_eq!(session.input_capture_barriers.len(), 1);
        assert_eq!(session.input_capture_barriers[0].barrier_id, 1);

        // Replacing with an empty set clears it
        session.set_pointer_barriers(vec![]);
        assert!(session.input_capture_barriers.is_empty());
    }

    #[test]
    fn test_persist_mode() {
        assert_eq!(PersistMode::from_dbus(0), PersistMode::None);
        assert_eq!(PersistMode::from_dbus(1), PersistMode::Transient);
        assert_eq!(PersistMode::from_dbus(2), PersistMode::Persistent);

        assert_eq!(PersistMode::None.to_dbus(), 0);
        assert_eq!(PersistMode::Transient.to_dbus(), 1);
        assert_eq!(PersistMode::Persistent.to_dbus(), 2);
    }
}
