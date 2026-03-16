//! Unified input injection backend supporting multiple protocols.
//!
//! This module provides an abstraction over different input injection protocols:
//!
//! - **EIS (Emulated Input Server)**: The emerging freedesktop standard using `reis`
//! - **wlr Virtual Input**: wlroots virtual keyboard/pointer protocols
//!
//! # Architecture
//!
//! The [`InputBackend`] trait provides a unified interface. At startup, protocol
//! detection determines which backend to use based on compositor capabilities
//! and configuration.
//!
//! # Example
//!
//! ```ignore
//! use xdg_desktop_portal_generic::services::input::{
//!     create_input_backend, InputBackendConfig,
//! };
//!
//! let config = InputBackendConfig::default();
//! let backend = create_input_backend(&config, &wayland_protocols)?;
//! ```

mod detection;
mod eis_backend;
mod eis_bridge;
#[cfg(feature = "uinput")]
pub mod uinput_backend;
mod wlr_backend;

use std::{os::unix::io::OwnedFd, path::PathBuf};

pub use detection::{AvailableProtocols, ProtocolDetector};
pub use eis_backend::EisSession;
pub use eis_bridge::EisBridgeBackend;
pub use wlr_backend::WlrInputBackend;

use crate::{
    error::Result,
    types::{DeviceTypes, InputEvent},
};

/// The input injection protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum InputProtocol {
    /// EIS (Emulated Input Server) via the `reis` crate.
    ///
    /// The client receives a socket fd and sends events directly to the portal,
    /// which forwards them to the compositor. This is the emerging freedesktop
    /// standard for input emulation.
    Eis,

    /// wlroots virtual input protocols.
    ///
    /// The portal creates virtual keyboard/pointer devices via Wayland protocols
    /// and injects events through them. Widely supported by wlroots-based compositors.
    WlrVirtualInput,
}

impl std::fmt::Display for InputProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InputProtocol::Eis => write!(f, "EIS"),
            InputProtocol::WlrVirtualInput => write!(f, "wlr-virtual-input"),
        }
    }
}

/// Abstraction over input injection protocols.
///
/// This trait provides a unified interface for both EIS and wlr virtual input
/// protocols. Implementations handle protocol-specific details internally.
pub trait InputBackend: Send + Sync {
    /// Get the protocol type this backend implements.
    fn protocol_type(&self) -> InputProtocol;

    /// Create an input context for a session.
    ///
    /// For EIS: Creates a new EIS context and returns the socket fd to pass to client.
    /// For wlr: Creates virtual keyboard/pointer devices, returns `None`.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Unique session identifier
    /// * `devices` - Device types to enable (keyboard, pointer, touchscreen)
    ///
    /// # Returns
    ///
    /// * `Ok(Some(fd))` - EIS protocol: fd to pass to client via D-Bus
    /// * `Ok(None)` - wlr protocol: no fd needed, use `inject_event()` instead
    fn create_context(&mut self, session_id: &str, devices: DeviceTypes)
    -> Result<Option<OwnedFd>>;

    /// Destroy an input context for a session.
    ///
    /// Releases all resources associated with the session's input context.
    fn destroy_context(&mut self, session_id: &str) -> Result<()>;

    /// Inject an input event for a session.
    ///
    /// For EIS: Events come from the client via socket; this is typically a no-op
    ///          unless the compositor needs notification.
    /// For wlr: Events are dispatched through virtual devices to the compositor.
    ///
    /// # Arguments
    ///
    /// * `session_id` - Session to inject event for
    /// * `event` - The input event to inject
    fn inject_event(&mut self, session_id: &str, event: InputEvent) -> Result<()>;

    /// Process pending events (for event-loop integration).
    ///
    /// For EIS: Processes incoming client events from the socket.
    /// For wlr: Typically a no-op as events flow outward only.
    ///
    /// # Returns
    ///
    /// Vector of (session_id, event) pairs for events received from clients.
    fn process_events(&mut self) -> Result<Vec<(String, InputEvent)>>;

    /// Check if a session has an active input context.
    fn has_context(&self, session_id: &str) -> bool;

    /// Get the number of active contexts.
    fn context_count(&self) -> usize;

    /// Convert an XKB keysym to an evdev keycode.
    ///
    /// Used by `NotifyKeyboardKeysym` to translate keysyms into keycodes
    /// that can be sent through the virtual keyboard protocol.
    ///
    /// # Arguments
    ///
    /// * `keysym` - XKB keysym value (e.g., `XKB_KEY_a`, `XKB_KEY_Return`)
    ///
    /// # Returns
    ///
    /// * `Some(keycode)` - The evdev keycode (minus the XKB offset of 8)
    /// * `None` - No keycode produces this keysym in the current keymap
    fn keysym_to_keycode(&self, keysym: u32) -> Option<u32>;

    /// Set the mapping from PipeWire stream node IDs to output geometry.
    ///
    /// Used for multi-monitor absolute pointer positioning. When a
    /// `NotifyPointerMotionAbsolute` call specifies a stream ID, the input
    /// backend uses this mapping to translate normalized (0.0–1.0) coordinates
    /// into compositor-global absolute coordinates.
    ///
    /// # Arguments
    ///
    /// * `mappings` - Stream-to-output geometry mappings
    fn set_stream_mappings(&mut self, _mappings: Vec<crate::types::StreamOutputMapping>) {
        // Default no-op for backends that don't need multi-monitor support
    }

    /// Set the health event sender for input metrics reporting.
    ///
    /// When set, the backend emits [`crate::health::PortalHealthEvent`] variants
    /// for input operations (event counts, EIS frame serials, disconnects).
    fn set_health_sender(&mut self, _tx: crate::health::HealthSender) {
        // Default no-op for backends that don't implement health monitoring
    }
}

/// Configuration for input backend selection.
#[derive(Debug, Clone)]
pub struct InputBackendConfig {
    /// Preferred protocol (if available).
    pub preferred: InputProtocol,

    /// Allow fallback to alternative protocol if preferred is unavailable.
    pub allow_fallback: bool,

    /// EIS-specific configuration.
    pub eis: EisConfig,

    /// wlr-specific configuration.
    pub wlr: WlrConfig,
}

impl Default for InputBackendConfig {
    fn default() -> Self {
        Self {
            // EIS is the emerging standard, prefer it when available
            preferred: InputProtocol::Eis,
            allow_fallback: true,
            eis: EisConfig::default(),
            wlr: WlrConfig::default(),
        }
    }
}

impl InputBackendConfig {
    /// Create configuration from environment variables.
    ///
    /// Reads:
    /// - `XDP_GENERIC_INPUT_PROTOCOL`: "eis" or "wlr"
    /// - `XDP_GENERIC_INPUT_NO_FALLBACK`: "1" to disable fallback
    /// - `XDP_GENERIC_EIS_SOCKET`: Custom EIS socket path
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(protocol) = std::env::var("XDP_GENERIC_INPUT_PROTOCOL") {
            match protocol.to_lowercase().as_str() {
                "eis" => config.preferred = InputProtocol::Eis,
                "wlr" => config.preferred = InputProtocol::WlrVirtualInput,
                _ => tracing::warn!("Unknown input protocol: {}", protocol),
            }
        }

        if std::env::var("XDP_GENERIC_INPUT_NO_FALLBACK").is_ok() {
            config.allow_fallback = false;
        }

        if let Ok(socket) = std::env::var("XDP_GENERIC_EIS_SOCKET") {
            config.eis.socket_path = Some(PathBuf::from(socket));
        }

        config
    }
}

/// EIS-specific configuration.
#[derive(Debug, Clone, Default)]
pub struct EisConfig {
    /// Custom socket path for EIS listener.
    ///
    /// If `None`, uses automatic path in `XDG_RUNTIME_DIR`.
    pub socket_path: Option<PathBuf>,
}

/// wlr virtual input specific configuration.
#[derive(Debug, Clone, Default)]
pub struct WlrConfig {
    /// Wayland display to connect to.
    ///
    /// If `None`, uses `WAYLAND_DISPLAY` environment variable.
    pub wayland_display: Option<String>,
}

/// Create an input backend based on configuration and Wayland protocol availability.
///
/// This function:
/// 1. Detects available protocols from the Wayland connection
/// 2. Selects the best protocol based on config preferences
/// 3. Creates and returns the appropriate backend
///
/// # Arguments
///
/// * `config` - Backend configuration with protocol preferences
/// * `wayland_protocols` - Available protocols from the Wayland connection
///
/// # Returns
///
/// A boxed `InputBackend` implementation, or an error if no suitable protocol is available.
pub fn create_input_backend(
    config: &InputBackendConfig,
    wayland_protocols: &crate::wayland::AvailableProtocols,
) -> Result<Box<dyn InputBackend>> {
    let available = ProtocolDetector::detect(wayland_protocols);

    tracing::info!(
        "Available input protocols: EIS={}, wlr={}",
        available.eis,
        available.wlr_virtual_input
    );

    let protocol = ProtocolDetector::select(config, &available)?;
    tracing::info!("Selected input protocol: {}", protocol);

    match protocol {
        InputProtocol::Eis => {
            // EIS bridge mode: accept EIS connections from clients, forward
            // events to the compositor through wlr virtual input protocols.
            if !wayland_protocols.wlr_virtual_pointer && !wayland_protocols.zwp_virtual_keyboard {
                return Err(crate::error::PortalError::Config(
                    "EIS bridge mode requires wlr virtual input protocols".to_string(),
                ));
            }
            tracing::info!("Using EIS bridge backend (EIS server -> wlr virtual input)");
            let backend = EisBridgeBackend::new(&config.wlr)?;
            Ok(Box::new(backend))
        }
        InputProtocol::WlrVirtualInput => {
            let backend = WlrInputBackend::new(&config.wlr)?;
            Ok(Box::new(backend))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_protocol_display() {
        assert_eq!(InputProtocol::Eis.to_string(), "EIS");
        assert_eq!(
            InputProtocol::WlrVirtualInput.to_string(),
            "wlr-virtual-input"
        );
    }

    #[test]
    fn test_config_default() {
        let config = InputBackendConfig::default();
        assert_eq!(config.preferred, InputProtocol::Eis);
        assert!(config.allow_fallback);
    }
}
