//! Integration tests for the public API surface.
//!
//! These tests verify that the public types, traits, and factory functions
//! are accessible and behave correctly without requiring runtime services
//! (no Wayland compositor, PipeWire, or D-Bus).

use xdg_desktop_portal_generic::{
    error::PortalError,
    services::{
        capture::CaptureProtocol,
        clipboard::ClipboardProtocol,
        input::{AvailableProtocols, InputBackendConfig, InputProtocol, ProtocolDetector},
    },
    session::{PersistMode, SessionManager, SessionManagerConfig, SessionState},
    types::{ButtonState, CursorMode, DeviceTypes, KeyState, SourceType, StreamOutputMapping},
};

// --- Session Management ---

#[test]
fn session_create_start_close_lifecycle() {
    let mut manager = SessionManager::new();
    let handle = SessionManager::generate_session_handle();

    let session = manager
        .create_session(
            handle.clone(),
            ":1.1".to_string(),
            "org.test.app".to_string(),
            PersistMode::None,
        )
        .unwrap();

    // Verify initial state
    assert_eq!(session.state, SessionState::Init);

    // Select devices
    if let Some(s) = manager.get_session_mut(&handle) {
        s.select_devices(DeviceTypes::all()).unwrap();
    }

    // Start with empty streams
    if let Some(s) = manager.get_session_mut(&handle) {
        s.start(vec![]).unwrap();
    }

    let session = manager.get_session(&handle).unwrap();
    assert_eq!(session.state, SessionState::Started);

    // Close
    let closed = manager.close_session(&handle);
    assert!(closed.is_some());
    assert!(manager.get_session(&handle).is_none());
}

#[test]
fn session_limit_enforcement() {
    let config = SessionManagerConfig {
        max_sessions_per_app: 2,
    };
    let mut manager = SessionManager::with_config(config);

    let h1 = SessionManager::generate_session_handle();
    let h2 = SessionManager::generate_session_handle();
    let h3 = SessionManager::generate_session_handle();

    manager
        .create_session(
            h1,
            ":1.1".to_string(),
            "org.test.app".to_string(),
            PersistMode::None,
        )
        .unwrap();
    manager
        .create_session(
            h2,
            ":1.1".to_string(),
            "org.test.app".to_string(),
            PersistMode::None,
        )
        .unwrap();

    // Third session should be rejected
    let result = manager.create_session(
        h3,
        ":1.1".to_string(),
        "org.test.app".to_string(),
        PersistMode::None,
    );
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        PortalError::SessionLimitExceeded(_, 2)
    ));
}

#[test]
fn session_handles_are_unique() {
    let h1 = SessionManager::generate_session_handle();
    let h2 = SessionManager::generate_session_handle();
    assert_ne!(h1, h2);
}

#[test]
fn close_sender_sessions_cleans_up() {
    let mut manager = SessionManager::new();

    let h1 = SessionManager::generate_session_handle();
    let h2 = SessionManager::generate_session_handle();

    manager
        .create_session(
            h1.clone(),
            ":1.10".to_string(),
            "app1".to_string(),
            PersistMode::None,
        )
        .unwrap();
    manager
        .create_session(
            h2.clone(),
            ":1.20".to_string(),
            "app2".to_string(),
            PersistMode::None,
        )
        .unwrap();

    // Disconnect sender :1.10
    let closed = manager.close_sender_sessions(":1.10");
    assert_eq!(closed.len(), 1);

    // app1's session gone, app2's session still exists
    assert!(manager.get_session(&h1).is_none());
    assert!(manager.get_session(&h2).is_some());
}

// --- Input Protocol Selection ---

#[test]
fn protocol_selection_prefers_eis_when_available() {
    let config = InputBackendConfig::default();
    let protocols = AvailableProtocols {
        eis: true,
        wlr_virtual_input: true,
    };

    let selected = ProtocolDetector::select(&config, &protocols).unwrap();
    assert_eq!(selected, InputProtocol::Eis);
}

#[test]
fn protocol_selection_falls_back_to_wlr() {
    let config = InputBackendConfig::default();
    let protocols = AvailableProtocols {
        eis: false,
        wlr_virtual_input: true,
    };

    let selected = ProtocolDetector::select(&config, &protocols).unwrap();
    assert_eq!(selected, InputProtocol::WlrVirtualInput);
}

#[test]
fn protocol_selection_fails_when_nothing_available() {
    let config = InputBackendConfig {
        allow_fallback: false,
        ..InputBackendConfig::default()
    };
    let protocols = AvailableProtocols {
        eis: false,
        wlr_virtual_input: false,
    };

    assert!(ProtocolDetector::select(&config, &protocols).is_err());
}

// --- Type Roundtrips ---

#[test]
fn persist_mode_dbus_roundtrip() {
    assert_eq!(PersistMode::from_dbus(0), PersistMode::None);
    assert_eq!(PersistMode::from_dbus(1), PersistMode::Transient);
    assert_eq!(PersistMode::from_dbus(2), PersistMode::Persistent);
    assert_eq!(PersistMode::from_dbus(99), PersistMode::None); // unknown -> None

    assert_eq!(PersistMode::None.to_dbus(), 0);
    assert_eq!(PersistMode::Transient.to_dbus(), 1);
    assert_eq!(PersistMode::Persistent.to_dbus(), 2);
}

#[test]
fn source_type_bits_roundtrip() {
    let types = SourceType::from_bits(0x07); // all three
    assert_eq!(types.len(), 3);
    assert!(types.contains(&SourceType::Monitor));
    assert!(types.contains(&SourceType::Window));
    assert!(types.contains(&SourceType::Virtual));
}

#[test]
fn device_types_all_and_bits() {
    let all = DeviceTypes::all();
    assert!(all.keyboard);
    assert!(all.pointer);
    assert!(all.touchscreen);

    let bits = all.to_bits();
    assert_eq!(bits, 0x07);

    let restored = DeviceTypes::from_bits(bits);
    assert_eq!(restored, all);
}

#[test]
fn cursor_mode_bits() {
    assert_eq!(CursorMode::Hidden.to_bits(), 1);
    assert_eq!(CursorMode::Embedded.to_bits(), 2);
    assert_eq!(CursorMode::Metadata.to_bits(), 4);
}

#[test]
fn button_and_key_state_conversions() {
    assert_eq!(ButtonState::from_dbus(0), ButtonState::Released);
    assert_eq!(ButtonState::from_dbus(1), ButtonState::Pressed);

    assert_eq!(KeyState::from_dbus(0), KeyState::Released);
    assert_eq!(KeyState::from_dbus(1), KeyState::Pressed);
}

// --- Error Conversions ---

#[test]
fn portal_error_to_dbus_error_mapping() {
    let access_denied: zbus::fdo::Error = PortalError::PermissionDenied("test".to_string()).into();
    assert!(matches!(access_denied, zbus::fdo::Error::AccessDenied(_)));

    let invalid_args: zbus::fdo::Error = PortalError::InvalidArgument("bad arg".to_string()).into();
    assert!(matches!(invalid_args, zbus::fdo::Error::InvalidArgs(_)));

    let generic: zbus::fdo::Error = PortalError::SessionNotFound("gone".to_string()).into();
    assert!(matches!(generic, zbus::fdo::Error::Failed(_)));
}

// --- Protocol Display ---

#[test]
fn protocol_display_strings() {
    assert_eq!(InputProtocol::Eis.to_string(), "EIS");
    assert_eq!(
        InputProtocol::WlrVirtualInput.to_string(),
        "wlr-virtual-input"
    );
    assert_eq!(
        CaptureProtocol::ExtImageCopyCapture.to_string(),
        "ext-image-copy-capture-v1"
    );
    assert_eq!(
        CaptureProtocol::WlrScreencopy.to_string(),
        "wlr-screencopy-v1"
    );
    assert_eq!(
        ClipboardProtocol::ExtDataControl.to_string(),
        "ext-data-control-v1"
    );
    assert_eq!(
        ClipboardProtocol::WlrDataControl.to_string(),
        "wlr-data-control-v1"
    );
}

// --- Stream Output Mapping ---

#[test]
fn stream_output_mapping_creation() {
    let mapping = StreamOutputMapping {
        stream_node_id: 42,
        x: 1920,
        y: 0,
        width: 2560,
        height: 1440,
    };

    assert_eq!(mapping.stream_node_id, 42);
    assert_eq!(mapping.x, 1920);
    assert_eq!(mapping.width, 2560);
}
