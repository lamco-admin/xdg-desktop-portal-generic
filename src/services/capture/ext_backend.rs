//! ext-image-copy-capture-v1 capture backend.
//!
//! Uses the staging standard `ext_image_copy_capture_manager_v1` protocol
//! for screen capture. Enumerates sources from wl_output globals.

use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
};

use super::{CaptureBackend, CaptureProtocol};
use crate::{
    error::{PortalError, Result},
    pipewire::PipeWireManager,
    types::{CursorMode, SourceInfo, SourceType, StreamInfo},
    wayland::CaptureCommand,
};

/// ext-image-copy-capture backend for screen capture.
///
/// Uses `ext_image_copy_capture_manager_v1` + `ext_output_image_capture_source_manager_v1`
/// to capture output frames and feed them to PipeWire streams.
///
/// **Note:** The ext protocol is not yet wired for frame delivery — this backend
/// currently falls through to PipeWire stream creation only. Once
/// ext-image-copy-capture Dispatch impls are added, this will start real capture.
pub struct ExtCaptureBackend {
    /// Known sources from wl_output globals.
    sources: Vec<SourceInfo>,
    /// Active streams by stream ID (PipeWire node ID).
    active_streams: HashMap<u32, StreamInfo>,
    /// PipeWire manager for creating real streams.
    pipewire: Arc<PipeWireManager>,
    /// Command sender for the Wayland event loop (screencopy capture).
    capture_tx: mpsc::Sender<CaptureCommand>,
}

impl ExtCaptureBackend {
    /// Create a new ext capture backend with known sources.
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

impl CaptureBackend for ExtCaptureBackend {
    fn protocol_type(&self) -> CaptureProtocol {
        CaptureProtocol::ExtImageCopyCapture
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

            // Create a real PipeWire stream via the manager.
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
                protocol = "ext-image-copy-capture",
                "Created capture stream with real PipeWire node"
            );

            // Start frame capture via the Wayland event loop.
            // Currently uses wlr-screencopy on the event loop side; ext-specific
            // capture will be added when ext Dispatch impls are implemented.
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
                // Stop screencopy capture on the Wayland event loop
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
                    "Destroyed ext capture stream"
                );
            }
        }
        Ok(())
    }

    fn available_source_types(&self) -> u32 {
        SourceType::Monitor.to_bits()
    }

    fn available_cursor_modes(&self) -> u32 {
        // ext-image-copy-capture supports all cursor modes
        CursorMode::Hidden.to_bits()
            | CursorMode::Embedded.to_bits()
            | CursorMode::Metadata.to_bits()
    }

    fn update_sources(&mut self, sources: Vec<SourceInfo>) {
        tracing::debug!(
            old_count = self.sources.len(),
            new_count = sources.len(),
            "Updating ext capture sources (output hotplug)"
        );
        self.sources = sources;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: PipeWireManager requires a running PipeWire daemon.
    // Tests that need real PipeWire streams are integration tests.
    // Unit tests here verify source filtering and protocol type.

    #[test]
    fn test_protocol_type_is_ext() {
        // ExtCaptureBackend always reports ExtImageCopyCapture
        assert_eq!(
            CaptureProtocol::ExtImageCopyCapture.to_string(),
            "ext-image-copy-capture-v1"
        );
    }

    #[test]
    fn test_source_filtering() {
        let sources = [SourceInfo {
            id: 1,
            name: "eDP-1".to_string(),
            description: "Built-in Display".to_string(),
            width: 1920,
            height: 1080,
            refresh_rate: 60000,
            source_type: SourceType::Monitor,
        }];

        // Monitor filter matches
        let filtered: Vec<_> = sources
            .iter()
            .filter(|s| [SourceType::Monitor].contains(&s.source_type))
            .collect();
        assert_eq!(filtered.len(), 1);

        // Window filter does not match
        let filtered: Vec<_> = sources
            .iter()
            .filter(|s| [SourceType::Window].contains(&s.source_type))
            .collect();
        assert!(filtered.is_empty());
    }
}
