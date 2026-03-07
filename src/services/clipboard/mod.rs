//! Clipboard backend abstraction.
//!
//! Provides a [`ClipboardBackend`] trait with two implementations:
//! - [`ExtClipboardBackend`]: Uses `ext-data-control-v1` (preferred, staging)
//! - [`WlrClipboardBackend`]: Uses `zwlr-data-control-manager-v1` (fallback)
//!
//! Both backends communicate with the Wayland event loop thread via a
//! command channel (`ClipboardCommand`) and shared state
//! (`SharedClipboardState`). The backend is selected at startup based on
//! available protocols detected from the Wayland compositor's global registry.

mod ext_backend;
mod wlr_backend;

use std::sync::{mpsc, Arc, Mutex};

pub use ext_backend::ExtClipboardBackend;
pub use wlr_backend::WlrClipboardBackend;

use crate::{
    error::Result,
    types::ClipboardData,
    wayland::{AvailableProtocols, ClipboardCommand, SharedClipboardState},
};

/// Clipboard protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ClipboardProtocol {
    /// ext-data-control-v1 (staging standard).
    ExtDataControl,
    /// zwlr-data-control-manager-v1 (wlroots).
    WlrDataControl,
}

impl std::fmt::Display for ClipboardProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipboardProtocol::ExtDataControl => write!(f, "ext-data-control-v1"),
            ClipboardProtocol::WlrDataControl => write!(f, "wlr-data-control-v1"),
        }
    }
}

/// Clipboard protocol selection preferences.
///
/// Mirrors [`super::capture::CapturePreference`] pattern.
#[derive(Debug, Clone)]
pub struct ClipboardPreference {
    /// Preferred protocol. `None` = auto-detect (ext preferred over wlr).
    pub preferred: Option<ClipboardProtocol>,

    /// Allow fallback to alternative protocol.
    pub allow_fallback: bool,
}

impl Default for ClipboardPreference {
    fn default() -> Self {
        Self {
            preferred: None,
            allow_fallback: true,
        }
    }
}

impl ClipboardPreference {
    /// Create preferences from environment variables.
    ///
    /// Reads:
    /// - `XDP_GENERIC_CLIPBOARD_PROTOCOL`: "ext" or "wlr"
    /// - `XDP_GENERIC_CLIPBOARD_NO_FALLBACK`: "1" to disable fallback
    pub fn from_env() -> Self {
        let mut prefs = Self::default();

        if let Ok(protocol) = std::env::var("XDP_GENERIC_CLIPBOARD_PROTOCOL") {
            match protocol.to_lowercase().as_str() {
                "ext" | "ext-data-control" => {
                    prefs.preferred = Some(ClipboardProtocol::ExtDataControl);
                }
                "wlr" | "wlr-data-control" => {
                    prefs.preferred = Some(ClipboardProtocol::WlrDataControl);
                }
                _ => tracing::warn!("Unknown clipboard protocol: {}", protocol),
            }
        }

        if std::env::var("XDP_GENERIC_CLIPBOARD_NO_FALLBACK").is_ok() {
            prefs.allow_fallback = false;
        }

        prefs
    }
}

/// Abstraction over clipboard Wayland protocols.
///
/// This trait provides a unified interface for clipboard access,
/// regardless of which Wayland protocol is used underneath.
pub trait ClipboardBackend: Send + Sync {
    /// Get the clipboard protocol this backend implements.
    fn protocol_type(&self) -> ClipboardProtocol;

    /// Get current clipboard content.
    ///
    /// Returns available MIME types and any cached data.
    fn get_clipboard(&self) -> Result<ClipboardData>;

    /// Set clipboard content.
    ///
    /// Takes ownership of the selection with the given MIME types and data.
    fn set_clipboard(&mut self, data: ClipboardData) -> Result<()>;

    /// Register callback for clipboard selection changes.
    ///
    /// Called when the compositor's clipboard content changes.
    fn on_selection_changed(&mut self, callback: Box<dyn Fn(Vec<String>) + Send + Sync>);

    /// Read clipboard data for a specific MIME type.
    ///
    /// Returns the data bytes, or None if the MIME type is not available.
    fn read_selection(&self, mime_type: &str) -> Result<Option<Vec<u8>>>;

    /// Notify the backend that a clipboard write operation has completed.
    ///
    /// Called after the client finishes writing data through a
    /// `SelectionWrite` pipe. The `serial` matches the value from the
    /// corresponding `SelectionTransfer` signal, and `success` indicates
    /// whether the write completed successfully.
    fn write_done(&mut self, serial: u32, success: bool) -> Result<()>;
}

/// Create a clipboard backend based on preferences and available protocols.
///
/// Selection algorithm:
/// 1. If a preferred protocol is specified, try it first
/// 2. If preferred is unavailable and fallback is allowed, try the alternative
/// 3. If no preference, auto-detect (ext preferred over wlr)
/// 4. Returns None if no clipboard protocol is available or usable
///
/// The `clipboard_tx` is the command sender to the Wayland event loop, and
/// `shared_clipboard` provides cross-thread access to the current selection.
pub fn create_clipboard_backend(
    protocols: &AvailableProtocols,
    prefs: &ClipboardPreference,
    clipboard_tx: mpsc::Sender<ClipboardCommand>,
    shared_clipboard: Arc<Mutex<SharedClipboardState>>,
) -> Option<Box<dyn ClipboardBackend>> {
    let ext_available = protocols.ext_data_control;
    let wlr_available = protocols.wlr_data_control;

    let selected = if let Some(preferred) = prefs.preferred {
        let preferred_available = match preferred {
            ClipboardProtocol::ExtDataControl => ext_available,
            ClipboardProtocol::WlrDataControl => wlr_available,
        };

        if preferred_available {
            Some(preferred)
        } else if prefs.allow_fallback {
            let fallback = match preferred {
                ClipboardProtocol::ExtDataControl => ClipboardProtocol::WlrDataControl,
                ClipboardProtocol::WlrDataControl => ClipboardProtocol::ExtDataControl,
            };
            let fallback_available = match fallback {
                ClipboardProtocol::ExtDataControl => ext_available,
                ClipboardProtocol::WlrDataControl => wlr_available,
            };

            if fallback_available {
                tracing::info!(
                    "Preferred clipboard protocol {} unavailable, using {}",
                    preferred,
                    fallback
                );
                Some(fallback)
            } else {
                None
            }
        } else {
            tracing::warn!(
                "Preferred clipboard protocol {} unavailable and fallback disabled",
                preferred
            );
            None
        }
    } else {
        // Auto-detect: ext preferred over wlr
        if ext_available {
            Some(ClipboardProtocol::ExtDataControl)
        } else if wlr_available {
            Some(ClipboardProtocol::WlrDataControl)
        } else {
            None
        }
    };

    match selected {
        Some(ClipboardProtocol::ExtDataControl) => {
            tracing::info!("Using ext-data-control-v1 for clipboard");
            Some(Box::new(ExtClipboardBackend::new(
                clipboard_tx,
                shared_clipboard,
            )))
        }
        Some(ClipboardProtocol::WlrDataControl) => {
            tracing::info!("Using wlr-data-control-v1 for clipboard");
            Some(Box::new(WlrClipboardBackend::new(
                clipboard_tx,
                shared_clipboard,
            )))
        }
        None => {
            tracing::warn!("No clipboard protocols available");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clipboard_protocol_display() {
        assert_eq!(
            ClipboardProtocol::ExtDataControl.to_string(),
            "ext-data-control-v1"
        );
        assert_eq!(
            ClipboardProtocol::WlrDataControl.to_string(),
            "wlr-data-control-v1"
        );
    }

    #[test]
    fn test_create_clipboard_backend_ext() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let prefs = ClipboardPreference::default();
        let protocols = AvailableProtocols {
            ext_data_control: true,
            wlr_data_control: true,
            ..Default::default()
        };
        let backend = create_clipboard_backend(&protocols, &prefs, tx, shared).unwrap();
        assert_eq!(backend.protocol_type(), ClipboardProtocol::ExtDataControl);
    }

    #[test]
    fn test_create_clipboard_backend_wlr_fallback() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let prefs = ClipboardPreference::default();
        let protocols = AvailableProtocols {
            ext_data_control: false,
            wlr_data_control: true,
            ..Default::default()
        };
        let backend = create_clipboard_backend(&protocols, &prefs, tx, shared).unwrap();
        assert_eq!(backend.protocol_type(), ClipboardProtocol::WlrDataControl);
    }

    #[test]
    fn test_create_clipboard_backend_none() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let prefs = ClipboardPreference::default();
        let protocols = AvailableProtocols::default();
        assert!(create_clipboard_backend(&protocols, &prefs, tx, shared).is_none());
    }

    #[test]
    fn test_explicit_wlr_preference() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let prefs = ClipboardPreference {
            preferred: Some(ClipboardProtocol::WlrDataControl),
            allow_fallback: true,
        };
        let protocols = AvailableProtocols {
            ext_data_control: true,
            wlr_data_control: true,
            ..Default::default()
        };
        let backend = create_clipboard_backend(&protocols, &prefs, tx, shared).unwrap();
        assert_eq!(backend.protocol_type(), ClipboardProtocol::WlrDataControl);
    }

    #[test]
    fn test_preferred_unavailable_with_fallback() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let prefs = ClipboardPreference {
            preferred: Some(ClipboardProtocol::ExtDataControl),
            allow_fallback: true,
        };
        let protocols = AvailableProtocols {
            ext_data_control: false,
            wlr_data_control: true,
            ..Default::default()
        };
        let backend = create_clipboard_backend(&protocols, &prefs, tx, shared).unwrap();
        assert_eq!(backend.protocol_type(), ClipboardProtocol::WlrDataControl);
    }

    #[test]
    fn test_preferred_unavailable_no_fallback() {
        let (tx, _rx) = mpsc::channel();
        let shared = Arc::new(Mutex::new(SharedClipboardState::default()));
        let prefs = ClipboardPreference {
            preferred: Some(ClipboardProtocol::ExtDataControl),
            allow_fallback: false,
        };
        let protocols = AvailableProtocols {
            ext_data_control: false,
            wlr_data_control: true,
            ..Default::default()
        };
        assert!(create_clipboard_backend(&protocols, &prefs, tx, shared).is_none());
    }
}
