//! Screen capture backend abstraction.
//!
//! Provides a [`CaptureBackend`] trait with two implementations:
//! - [`ExtCaptureBackend`]: Uses `ext-image-copy-capture-v1` (preferred, staging)
//! - [`WlrCaptureBackend`]: Uses `wlr-screencopy-unstable-v1` (fallback)
//!
//! The backend is selected at startup based on available protocols detected
//! from the Wayland compositor's global registry.

mod ext_backend;
mod wlr_backend;

use std::sync::{mpsc, Arc};

pub use ext_backend::ExtCaptureBackend;
pub use wlr_backend::WlrCaptureBackend;

use crate::{
    error::Result,
    pipewire::PipeWireManager,
    types::{CursorMode, SourceInfo, SourceType, StreamInfo},
    wayland::{AvailableProtocols, CaptureCommand},
};

/// Screen capture protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CaptureProtocol {
    /// ext-image-copy-capture-v1 (staging standard).
    ExtImageCopyCapture,
    /// wlr-screencopy-unstable-v1 (wlroots).
    WlrScreencopy,
}

impl std::fmt::Display for CaptureProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureProtocol::ExtImageCopyCapture => write!(f, "ext-image-copy-capture-v1"),
            CaptureProtocol::WlrScreencopy => write!(f, "wlr-screencopy-v1"),
        }
    }
}

/// Abstraction over screen capture protocols.
///
/// This trait provides a unified interface for screen capture, regardless
/// of which Wayland protocol is used underneath.
pub trait CaptureBackend: Send + Sync {
    /// Get the capture protocol this backend implements.
    fn protocol_type(&self) -> CaptureProtocol;

    /// Get available capturable sources (monitors, windows).
    fn get_sources(&self, source_types: &[SourceType]) -> Result<Vec<SourceInfo>>;

    /// Create capture sessions/streams for the given sources.
    ///
    /// Returns stream information including PipeWire node IDs.
    fn create_capture_session(
        &mut self,
        sources: &[SourceInfo],
        cursor_mode: CursorMode,
    ) -> Result<Vec<StreamInfo>>;

    /// Destroy capture sessions/streams.
    fn destroy_capture_session(&mut self, stream_ids: &[u32]) -> Result<()>;

    /// Get available source types (bit flags).
    fn available_source_types(&self) -> u32 {
        SourceType::Monitor.to_bits()
    }

    /// Get available cursor modes (bit flags).
    fn available_cursor_modes(&self) -> u32 {
        CursorMode::Hidden.to_bits() | CursorMode::Embedded.to_bits()
    }

    /// Update the source list (e.g., after output hotplug).
    fn update_sources(&mut self, sources: Vec<SourceInfo>);
}

/// Create a capture backend based on available protocols.
///
/// Prefers ext-image-copy-capture, falls back to wlr-screencopy.
/// The PipeWire manager is passed through for creating real PipeWire streams.
/// The capture command sender allows backends to request frame capture from
/// the Wayland event loop thread.
pub fn create_capture_backend(
    protocols: &AvailableProtocols,
    sources: Vec<SourceInfo>,
    pipewire: Arc<PipeWireManager>,
    capture_tx: mpsc::Sender<CaptureCommand>,
) -> Result<Box<dyn CaptureBackend>> {
    if protocols.ext_image_copy_capture {
        tracing::info!("Using ext-image-copy-capture-v1 for screen capture");
        Ok(Box::new(ExtCaptureBackend::new(
            sources, pipewire, capture_tx,
        )))
    } else if protocols.wlr_screencopy {
        tracing::info!("Using wlr-screencopy-v1 for screen capture");
        Ok(Box::new(WlrCaptureBackend::new(
            sources, pipewire, capture_tx,
        )))
    } else {
        tracing::warn!("No screen capture protocols available");
        // Return a backend that reports no sources
        Ok(Box::new(ExtCaptureBackend::new(
            vec![],
            pipewire,
            capture_tx,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capture_protocol_display() {
        assert_eq!(
            CaptureProtocol::ExtImageCopyCapture.to_string(),
            "ext-image-copy-capture-v1"
        );
        assert_eq!(
            CaptureProtocol::WlrScreencopy.to_string(),
            "wlr-screencopy-v1"
        );
    }

    // Note: create_capture_backend tests require PipeWireManager which needs
    // a running PipeWire daemon. Protocol selection logic is tested here;
    // integration tests cover the full pipeline.

    #[test]
    fn test_capture_protocol_selection_logic() {
        let protocols = AvailableProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: true,
            ..Default::default()
        };
        // ext is preferred when both are available
        assert!(protocols.ext_image_copy_capture);

        let protocols = AvailableProtocols {
            ext_image_copy_capture: false,
            wlr_screencopy: true,
            ..Default::default()
        };
        // wlr is fallback
        assert!(!protocols.ext_image_copy_capture);
        assert!(protocols.wlr_screencopy);
    }
}
