//! wlr-screencopy-unstable-v1 capture backend.
//!
//! Uses the wlroots `zwlr_screencopy_manager_v1` protocol for screen capture.
//! This is the fallback when ext-image-copy-capture is not available.

use std::{
    collections::HashMap,
    sync::{mpsc, Arc},
};

use super::{CaptureBackend, CaptureProtocol};
use crate::{
    error::{PortalError, Result},
    pipewire::PipeWireManager,
    types::{CursorMode, SourceInfo, SourceType, StreamInfo},
    wayland::CaptureCommand,
};

/// wlr-screencopy backend for screen capture.
///
/// Uses `zwlr_screencopy_manager_v1` to capture output frames and feed
/// them to PipeWire streams. Frame capture is driven by the Wayland event
/// loop thread — this backend sends [`CaptureCommand`]s to start/stop
/// the capture loop.
pub struct WlrCaptureBackend {
    /// Known sources from wl_output globals.
    sources: Vec<SourceInfo>,
    /// Active streams by PipeWire node ID.
    active_streams: HashMap<u32, StreamInfo>,
    /// PipeWire manager for creating real streams.
    pipewire: Arc<PipeWireManager>,
    /// Command sender for the Wayland event loop (screencopy capture).
    capture_tx: mpsc::Sender<CaptureCommand>,
}

impl WlrCaptureBackend {
    /// Create a new wlr screencopy backend with known sources.
    pub fn new(
        sources: Vec<SourceInfo>,
        pipewire: Arc<PipeWireManager>,
        capture_tx: mpsc::Sender<CaptureCommand>,
    ) -> Self {
        Self {
            sources,
            active_streams: HashMap::new(),
            pipewire,
            capture_tx,
        }
    }
}

impl CaptureBackend for WlrCaptureBackend {
    fn protocol_type(&self) -> CaptureProtocol {
        CaptureProtocol::WlrScreencopy
    }

    fn get_sources(&self, source_types: &[SourceType]) -> Result<Vec<SourceInfo>> {
        if source_types.is_empty() {
            return Ok(self.sources.clone());
        }

        Ok(self
            .sources
            .iter()
            .filter(|s| source_types.contains(&s.source_type))
            .cloned()
            .collect())
    }

    fn create_capture_session(
        &mut self,
        sources: &[SourceInfo],
        cursor_mode: CursorMode,
    ) -> Result<Vec<StreamInfo>> {
        let mut streams = Vec::new();

        for source in sources {
            // Verify source exists
            if !self.sources.iter().any(|s| s.id == source.id) {
                return Err(PortalError::SourceNotFound(source.id));
            }

            // Create a real PipeWire stream via the manager
            let config = crate::pipewire::StreamConfig {
                source_id: source.id,
                width: source.width,
                height: source.height,
                framerate: 30,
            };

            let node_id = {
                let pw = Arc::clone(&self.pipewire);
                let rt = tokio::runtime::Handle::try_current();
                match rt {
                    Ok(handle) => handle.block_on(pw.create_stream(config)).map_err(|e| {
                        PortalError::PipeWire(format!("Failed to create stream: {e}"))
                    })?,
                    Err(_) => {
                        return Err(PortalError::PipeWire(
                            "Cannot create PipeWire stream outside async context".to_string(),
                        ));
                    }
                }
            };

            let stream = StreamInfo {
                node_id,
                source_id: source.id,
                position: (0, 0),
                size: (source.width, source.height),
                source_type: source.source_type,
                mapping_id: Some(format!("output:{}", source.name)),
                properties: HashMap::new(),
            };

            tracing::info!(
                node_id = node_id,
                source_id = source.id,
                source_name = %source.name,
                protocol = "wlr-screencopy",
                "Created capture stream with real PipeWire node"
            );

            // Tell the Wayland event loop to start screencopy capture for this output.
            // The event loop will request frames from the compositor and deliver
            // pixel data to PipeWire via queue_buffer().
            if let Err(e) = self.capture_tx.send(CaptureCommand::StartCapture {
                output_global_name: source.id,
                node_id,
                width: source.width,
                height: source.height,
                cursor_mode,
            }) {
                tracing::error!(
                    node_id,
                    error = %e,
                    "Failed to send StartCapture command to event loop"
                );
            }

            self.active_streams.insert(node_id, stream.clone());
            streams.push(stream);
        }

        Ok(streams)
    }

    fn destroy_capture_session(&mut self, stream_ids: &[u32]) -> Result<()> {
        for id in stream_ids {
            if let Some(stream) = self.active_streams.remove(id) {
                // Tell the Wayland event loop to stop screencopy capture
                let _ = self
                    .capture_tx
                    .send(CaptureCommand::StopCapture { node_id: *id });

                // Destroy the PipeWire stream
                let pw = Arc::clone(&self.pipewire);
                let node_id = *id;
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let _ = handle.block_on(pw.destroy_stream(node_id));
                }

                tracing::info!(
                    node_id = id,
                    source_id = stream.source_id,
                    "Destroyed wlr screencopy stream"
                );
            }
        }
        Ok(())
    }

    fn available_source_types(&self) -> u32 {
        SourceType::Monitor.to_bits()
    }

    fn available_cursor_modes(&self) -> u32 {
        // wlr-screencopy supports hidden and embedded (via with_cursor flag)
        CursorMode::Hidden.to_bits() | CursorMode::Embedded.to_bits()
    }

    fn update_sources(&mut self, sources: Vec<SourceInfo>) {
        tracing::debug!(
            old_count = self.sources.len(),
            new_count = sources.len(),
            "Updating wlr capture sources (output hotplug)"
        );
        self.sources = sources;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_type_is_wlr() {
        assert_eq!(
            CaptureProtocol::WlrScreencopy.to_string(),
            "wlr-screencopy-v1"
        );
    }

    #[test]
    fn test_cursor_modes_limited() {
        // wlr-screencopy does not support Metadata cursor mode
        let modes = CursorMode::Hidden.to_bits() | CursorMode::Embedded.to_bits();
        assert_eq!(modes & CursorMode::Metadata.to_bits(), 0);
        assert_ne!(modes & CursorMode::Embedded.to_bits(), 0);
    }
}
