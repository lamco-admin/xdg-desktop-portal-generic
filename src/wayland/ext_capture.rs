//! ext-image-copy-capture frame capture state and session management.
//!
//! This module manages the frame capture loop for `ext-image-copy-capture-v1`,
//! the preferred screen capture protocol. Unlike wlr-screencopy (which is
//! stateless per-frame), this protocol uses sessions with explicit buffer
//! constraint negotiation.
//!
//! # Lifecycle
//!
//! 1. Bind `ext_output_image_capture_source_manager_v1` + `ext_image_copy_capture_manager_v1`
//! 2. Create a source from a `wl_output`
//! 3. Create a session from the source → receive `buffer_size`, `shm_format`, `done`
//! 4. For each frame:
//!    a. `session.create_frame()` → new frame object
//!    b. `frame.attach_buffer(wl_buffer)` — using SHM buffer matching constraints
//!    c. `frame.damage_buffer(0, 0, width, height)` — full damage
//!    d. `frame.capture()` — tell compositor to fill the buffer
//!    e. Wait for `ready` or `failed` event
//!    f. On `ready`: read pixels, send to PipeWire, destroy frame, go to 4a
//!    g. On `failed`: handle error, destroy frame
//! 5. `session.destroy()` to stop
//!
//! The `stopped` event on the session means the compositor tore down the
//! session (e.g., output removed). We must recreate the session if needed.

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use wayland_client::{
    QueueHandle,
    protocol::{
        wl_output::WlOutput,
        wl_shm::{self, WlShm},
    },
};
use wayland_protocols::ext::{
    image_capture_source::v1::client::{
        ext_image_capture_source_v1::ExtImageCaptureSourceV1,
        ext_output_image_capture_source_manager_v1::ExtOutputImageCaptureSourceManagerV1,
    },
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::ExtImageCopyCaptureFrameV1,
        ext_image_copy_capture_manager_v1::{self, ExtImageCopyCaptureManagerV1},
        ext_image_copy_capture_session_v1::ExtImageCopyCaptureSessionV1,
    },
};

use super::{
    dispatch::WaylandState,
    screencopy::{BufferFormatInfo, ShmFrameBuffer},
};
use crate::{pipewire::PipeWireManager, wayland::ScreenshotData};

/// Counter for generating unique screenshot capture IDs in the ext protocol.
/// Uses a different high range from screencopy to avoid collision.
static EXT_SCREENSHOT_ID_COUNTER: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0xA000_0000);

/// Buffer constraints received from the session's constraint negotiation.
///
/// Collected from `buffer_size` and `shm_format` events before the session
/// `done` event.
#[derive(Debug, Clone, Default)]
pub struct BufferConstraints {
    /// Width in pixels (from `buffer_size` event).
    pub width: u32,
    /// Height in pixels (from `buffer_size` event).
    pub height: u32,
    /// Supported SHM formats (from `shm_format` events).
    pub shm_formats: Vec<u32>,
    /// Whether constraints are complete (received `done`).
    pub done: bool,
}

impl BufferConstraints {
    /// Pick the best SHM format from the offered formats.
    ///
    /// Prefers ARGB8888, then XRGB8888, then the first offered format.
    pub fn pick_format(&self) -> Option<BufferFormatInfo> {
        if self.shm_formats.is_empty() || self.width == 0 || self.height == 0 {
            return None;
        }

        // Prefer ARGB8888 (format 0) then XRGB8888 (format 1)
        let format_raw = if self
            .shm_formats
            .contains(&(wl_shm::Format::Argb8888 as u32))
        {
            wl_shm::Format::Argb8888 as u32
        } else if self
            .shm_formats
            .contains(&(wl_shm::Format::Xrgb8888 as u32))
        {
            wl_shm::Format::Xrgb8888 as u32
        } else {
            self.shm_formats[0]
        };

        // Calculate stride: 4 bytes per pixel for ARGB/XRGB formats.
        // For other formats, default to 4 bpp (most common SHM formats are 32-bit).
        let bytes_per_pixel = 4u32;
        let stride = self.width * bytes_per_pixel;

        Some(BufferFormatInfo {
            format: match format_raw {
                x if x == wl_shm::Format::Argb8888 as u32 => wl_shm::Format::Argb8888,
                x if x == wl_shm::Format::Xrgb8888 as u32 => wl_shm::Format::Xrgb8888,
                x if x == wl_shm::Format::Abgr8888 as u32 => wl_shm::Format::Abgr8888,
                x if x == wl_shm::Format::Xbgr8888 as u32 => wl_shm::Format::Xbgr8888,
                _ => wl_shm::Format::Argb8888, // fallback
            },
            format_raw,
            width: self.width,
            height: self.height,
            stride,
        })
    }
}

/// State for one active ext-image-copy-capture session.
pub struct ActiveExtCapture {
    /// The wl_output being captured.
    pub output: WlOutput,
    /// Output global name (for logging/matching).
    pub output_global_name: u32,
    /// PipeWire stream node ID to deliver frames to.
    pub node_id: u32,
    /// The capture source object.
    pub source: ExtImageCaptureSourceV1,
    /// The capture session object.
    pub session: ExtImageCopyCaptureSessionV1,
    /// Buffer constraints from session negotiation.
    pub constraints: BufferConstraints,
    /// The allocated SHM buffer (reused across frames).
    pub shm_buffer: Option<ShmFrameBuffer>,
    /// The currently pending frame (waiting for ready/failed).
    pub pending_frame: Option<ExtImageCopyCaptureFrameV1>,
    /// Whether the session has been stopped by the compositor.
    pub stopped: bool,
    /// If set, this is a one-shot screenshot capture.
    pub screenshot_reply:
        Option<tokio::sync::oneshot::Sender<std::result::Result<ScreenshotData, String>>>,
    /// When this capture session was created. Used for handshake timeout detection.
    pub created_at: Instant,
    /// Whether the handshake timeout has already been reported for this session.
    pub timeout_reported: bool,
    /// When the current frame capture was initiated (for latency measurement).
    pub capture_started: Option<Instant>,
}

/// Central ext-image-copy-capture state, stored in WaylandState.
#[derive(Default)]
pub struct ExtCaptureState {
    /// The capture manager global.
    pub manager: Option<ExtImageCopyCaptureManagerV1>,
    /// The output source manager global.
    pub source_manager: Option<ExtOutputImageCaptureSourceManagerV1>,
    /// Active captures, keyed by PipeWire node ID.
    pub captures: HashMap<u32, ActiveExtCapture>,
    /// Reference to the PipeWire manager for sending frame data.
    pub pipewire: Option<Arc<PipeWireManager>>,
    /// Reference to the wl_shm global for buffer allocation.
    pub shm: Option<WlShm>,
    /// Handshake timeout for constraint events (0 = no timeout).
    pub handshake_timeout: Duration,
    /// Total frames delivered (for periodic logging and health).
    pub frame_count: u64,
    /// Health event sender for capture metrics.
    pub health_tx: Option<crate::health::HealthSender>,
}

impl ExtCaptureState {
    /// Start capturing an output via the ext protocol.
    ///
    /// Creates a source from the output, then a session from the source.
    /// The session will emit buffer constraint events, then `done`, at which
    /// point we allocate a buffer and begin the frame capture loop.
    pub fn start_capture(
        &mut self,
        qh: &QueueHandle<WaylandState>,
        output: WlOutput,
        output_global_name: u32,
        node_id: u32,
        paint_cursors: bool,
    ) {
        let Some(source_manager) = &self.source_manager else {
            tracing::error!("Cannot start ext capture: source manager not bound");
            return;
        };
        let Some(manager) = &self.manager else {
            tracing::error!("Cannot start ext capture: capture manager not bound");
            return;
        };

        tracing::info!(
            node_id,
            output = output_global_name,
            paint_cursors,
            "Starting ext-image-copy-capture"
        );

        // Create an image capture source from the output.
        // User data is the node_id for routing events.
        let source = source_manager.create_source(&output, qh, node_id);

        // Create a capture session from the source.
        let options = if paint_cursors {
            ext_image_copy_capture_manager_v1::Options::PaintCursors
        } else {
            ext_image_copy_capture_manager_v1::Options::empty()
        };
        let session = manager.create_session(&source, options, qh, node_id);

        let capture = ActiveExtCapture {
            output,
            output_global_name,
            node_id,
            source,
            session,
            constraints: BufferConstraints::default(),
            shm_buffer: None,
            pending_frame: None,
            stopped: false,
            screenshot_reply: None,
            created_at: Instant::now(),
            timeout_reported: false,
            capture_started: None,
        };

        self.captures.insert(node_id, capture);
    }

    /// Start a one-shot screenshot capture via the ext protocol.
    pub fn start_screenshot(
        &mut self,
        qh: &QueueHandle<WaylandState>,
        output: WlOutput,
        output_global_name: u32,
        reply: tokio::sync::oneshot::Sender<std::result::Result<ScreenshotData, String>>,
    ) {
        let Some(source_manager) = &self.source_manager else {
            let _ = reply.send(Err("ext source manager not bound".to_string()));
            return;
        };
        let Some(manager) = &self.manager else {
            let _ = reply.send(Err("ext capture manager not bound".to_string()));
            return;
        };

        let screenshot_id =
            EXT_SCREENSHOT_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        tracing::info!(
            screenshot_id,
            output = output_global_name,
            "Starting ext one-shot screenshot capture"
        );

        let source = source_manager.create_source(&output, qh, screenshot_id);
        let options = ext_image_copy_capture_manager_v1::Options::PaintCursors;
        let session = manager.create_session(&source, options, qh, screenshot_id);

        let capture = ActiveExtCapture {
            output,
            output_global_name,
            node_id: screenshot_id,
            source,
            session,
            constraints: BufferConstraints::default(),
            shm_buffer: None,
            pending_frame: None,
            stopped: false,
            screenshot_reply: Some(reply),
            created_at: Instant::now(),
            timeout_reported: false,
            capture_started: None,
        };

        self.captures.insert(screenshot_id, capture);
    }

    /// Stop capturing an output.
    pub fn stop_capture(&mut self, node_id: u32) {
        if let Some(mut capture) = self.captures.remove(&node_id) {
            // Destroy pending frame if any
            if let Some(frame) = capture.pending_frame.take() {
                frame.destroy();
            }
            // Destroy session and source
            capture.session.destroy();
            capture.source.destroy();
            // SHM buffer is dropped automatically
            tracing::info!(node_id, "Stopped ext capture");
        }
    }

    /// Handle the `buffer_size` event from a session.
    pub fn on_session_buffer_size(&mut self, node_id: u32, width: u32, height: u32) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            tracing::debug!(node_id, width, height, "ext capture session buffer_size");
            capture.constraints.width = width;
            capture.constraints.height = height;
        }
    }

    /// Handle the `shm_format` event from a session.
    pub fn on_session_shm_format(&mut self, node_id: u32, format: u32) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            tracing::debug!(node_id, format, "ext capture session shm_format");
            capture.constraints.shm_formats.push(format);
        }
    }

    /// Handle the `done` event from a session.
    ///
    /// All buffer constraints have been sent. Allocate a SHM buffer
    /// matching the constraints and request the first frame.
    pub fn on_session_done(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            capture.constraints.done = true;
            tracing::debug!(
                node_id,
                width = capture.constraints.width,
                height = capture.constraints.height,
                formats = ?capture.constraints.shm_formats,
                "ext capture session constraints done"
            );
        }

        // Allocate buffer and request first frame
        self.allocate_and_capture(node_id, qh);
    }

    /// Handle the `stopped` event from a session.
    ///
    /// The compositor tore down the session (e.g., output removed).
    pub fn on_session_stopped(&mut self, node_id: u32) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            capture.stopped = true;
            tracing::warn!(node_id, "ext capture session stopped by compositor");

            // For screenshots, send error
            if let Some(reply) = capture.screenshot_reply.take() {
                let _ = reply.send(Err("Capture session stopped by compositor".to_string()));
            }
        }

        // Clean up the stopped session
        self.stop_capture(node_id);
    }

    /// Allocate SHM buffer from constraints and request a frame capture.
    fn allocate_and_capture(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        let Some(shm) = &self.shm else {
            tracing::error!("Cannot allocate ext capture buffer: wl_shm not bound");
            return;
        };
        let shm = shm.clone();

        let Some(capture) = self.captures.get_mut(&node_id) else {
            return;
        };

        if capture.stopped {
            return;
        }

        let Some(format) = capture.constraints.pick_format() else {
            tracing::error!(node_id, "No suitable SHM format in ext capture constraints");
            if let Some(reply) = capture.screenshot_reply.take() {
                let _ = reply.send(Err("No suitable SHM format".to_string()));
            }
            return;
        };

        // Reuse existing SHM buffer if format matches
        let needs_new_buffer = match &capture.shm_buffer {
            Some(buf) => {
                buf.format.format_raw != format.format_raw
                    || buf.format.width != format.width
                    || buf.format.height != format.height
                    || buf.format.stride != format.stride
            }
            None => true,
        };

        if needs_new_buffer {
            match ShmFrameBuffer::new(&shm, qh, &format) {
                Ok(buf) => {
                    tracing::debug!(
                        node_id,
                        width = format.width,
                        height = format.height,
                        stride = format.stride,
                        format = format.format_raw,
                        "Allocated new SHM buffer for ext capture"
                    );
                    capture.shm_buffer = Some(buf);
                }
                Err(e) => {
                    tracing::error!(node_id, error = %e, "Failed to allocate SHM buffer for ext capture");
                    if let Some(reply) = capture.screenshot_reply.take() {
                        let _ = reply.send(Err(format!("SHM allocation failed: {e}")));
                    }
                    return;
                }
            }
        }

        // Create a frame, attach buffer, mark full damage, and capture
        let frame = capture.session.create_frame(qh, node_id);

        if let Some(buf) = &capture.shm_buffer {
            frame.attach_buffer(&buf.buffer);
            frame.damage_buffer(0, 0, format.width as i32, format.height as i32);
            capture.capture_started = Some(Instant::now());
            frame.capture();
            tracing::trace!(node_id, "Sent ext frame capture request");
        }

        capture.pending_frame = Some(frame);
    }

    /// Handle the `ready` event from a capture frame.
    ///
    /// The compositor has finished writing pixel data.
    pub fn on_frame_ready(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        self.frame_count += 1;

        // Measure capture latency
        let capture_latency = self
            .captures
            .get(&node_id)
            .and_then(|c| c.capture_started)
            .map(|start| start.elapsed());

        let (data, width, height, stride, format_raw, is_screenshot) = {
            let Some(capture) = self.captures.get_mut(&node_id) else {
                return;
            };

            // Destroy the completed frame
            if let Some(frame) = capture.pending_frame.take() {
                frame.destroy();
            }

            let Some(buf) = &capture.shm_buffer else {
                tracing::error!(node_id, "ext frame ready but no SHM buffer");
                if let Some(reply) = capture.screenshot_reply.take() {
                    let _ = reply.send(Err("No SHM buffer available".to_string()));
                }
                return;
            };

            let data = buf.read_pixels();
            let width = buf.format.width;
            let height = buf.format.height;
            let stride = buf.format.stride;
            let format_raw = buf.format.format_raw;
            let is_screenshot = capture.screenshot_reply.is_some();

            (data, width, height, stride, format_raw, is_screenshot)
        };

        if is_screenshot {
            // One-shot screenshot: send data and clean up
            if let Some(mut capture) = self.captures.remove(&node_id) {
                if let Some(reply) = capture.screenshot_reply.take() {
                    let _ = reply.send(Ok(ScreenshotData {
                        data,
                        width,
                        height,
                        stride,
                        format_raw,
                    }));
                }
                // Clean up session + source
                capture.session.destroy();
                capture.source.destroy();
                tracing::info!(node_id, width, height, "ext screenshot capture complete");
            }
        } else {
            // Continuous capture: send to PipeWire and request next frame
            if let Some(pw) = &self.pipewire {
                pw.queue_buffer(node_id, data, width, height, stride, format_raw);
                tracing::trace!(node_id, width, height, "Queued ext frame to PipeWire");
            }

            // Emit health event for successful frame
            if let Some(health_tx) = &self.health_tx {
                let frame_size = (width * height * 4) as usize;
                let _ = health_tx.try_send(crate::health::PortalHealthEvent::FrameCaptured {
                    node_id,
                    capture_latency: capture_latency.unwrap_or(std::time::Duration::ZERO),
                    frame_size_bytes: frame_size,
                    frame_number: self.frame_count,
                    damage_region_count: 1,
                });
            }

            self.request_next_frame(node_id, qh);
        }
    }

    /// Handle the `failed` event from a capture frame.
    pub fn on_frame_failed(&mut self, node_id: u32, reason: u32, qh: &QueueHandle<WaylandState>) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            if let Some(frame) = capture.pending_frame.take() {
                frame.destroy();
            }

            // For screenshots, send error and remove
            if capture.screenshot_reply.is_some() {
                if let Some(mut capture) = self.captures.remove(&node_id) {
                    if let Some(reply) = capture.screenshot_reply.take() {
                        let _ =
                            reply.send(Err(format!("ext frame capture failed (reason: {reason})")));
                    }
                    capture.session.destroy();
                    capture.source.destroy();
                }
                tracing::warn!(node_id, reason, "ext screenshot capture failed");
                return;
            }

            tracing::warn!(node_id, reason, "ext frame capture failed, retrying");

            // Emit health event for failed frame
            if let Some(health_tx) = &self.health_tx {
                let reason_str = match reason {
                    0 => "unknown",
                    1 => "buffer_constraints",
                    2 => "stopped",
                    _ => "unrecognized",
                };
                let _ = health_tx.try_send(crate::health::PortalHealthEvent::FrameFailed {
                    node_id,
                    reason: format!("ext capture failed: {reason_str}"),
                });
            }
        }

        // Retry for continuous captures (unless stopped or buffer_constraints failure)
        // reason 1 = buffer_constraints means we need new constraints
        if reason == 1 {
            tracing::warn!(
                node_id,
                "ext frame failed: buffer constraints changed, need re-negotiation"
            );
            // The session should send new constraint events; wait for them
            if let Some(capture) = self.captures.get_mut(&node_id) {
                capture.constraints = BufferConstraints::default();
                capture.shm_buffer = None;
            }
        } else {
            self.request_next_frame(node_id, qh);
        }
    }

    /// Check for handshake timeouts on sessions waiting for constraint events.
    ///
    /// Called periodically from the Wayland event loop. Returns node IDs of
    /// sessions whose handshake has timed out (no `done` event within the
    /// configured timeout). The caller should stop these sessions and report
    /// the failure.
    pub fn check_handshake_timeouts(&mut self, timeout: Duration) -> Vec<u32> {
        if timeout.is_zero() {
            return vec![];
        }

        let mut timed_out = vec![];

        for capture in self.captures.values_mut() {
            // Only check sessions still waiting for constraints
            if capture.constraints.done || capture.stopped || capture.timeout_reported {
                continue;
            }

            let elapsed = capture.created_at.elapsed();
            if elapsed >= timeout {
                tracing::warn!(
                    node_id = capture.node_id,
                    output = capture.output_global_name,
                    elapsed_ms = elapsed.as_millis() as u64,
                    timeout_ms = timeout.as_millis() as u64,
                    "ext-capture handshake timed out: compositor did not send constraint events"
                );
                capture.timeout_reported = true;
                timed_out.push(capture.node_id);

                // For screenshots, send error immediately
                if let Some(reply) = capture.screenshot_reply.take() {
                    let _ = reply.send(Err(format!(
                        "ext-capture handshake timed out after {}ms",
                        elapsed.as_millis()
                    )));
                }
            }
        }

        timed_out
    }

    /// Request the next frame in the capture loop.
    fn request_next_frame(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        let Some(capture) = self.captures.get_mut(&node_id) else {
            return;
        };

        if capture.stopped {
            return;
        }

        // Create next frame and attach existing buffer
        let frame = capture.session.create_frame(qh, node_id);

        if let Some(buf) = &capture.shm_buffer {
            frame.attach_buffer(&buf.buffer);
            let format = &buf.format;
            frame.damage_buffer(0, 0, format.width as i32, format.height as i32);
            frame.capture();
        }

        capture.pending_frame = Some(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_constraints_default() {
        let c = BufferConstraints::default();
        assert_eq!(c.width, 0);
        assert_eq!(c.height, 0);
        assert!(c.shm_formats.is_empty());
        assert!(!c.done);
    }

    #[test]
    fn test_buffer_constraints_pick_format_empty() {
        let c = BufferConstraints::default();
        assert!(c.pick_format().is_none());
    }

    #[test]
    fn test_buffer_constraints_pick_format_prefers_argb() {
        let c = BufferConstraints {
            width: 1920,
            height: 1080,
            shm_formats: vec![
                wl_shm::Format::Xrgb8888 as u32,
                wl_shm::Format::Argb8888 as u32,
            ],
            done: true,
        };
        let f = c.pick_format().unwrap();
        assert_eq!(f.format_raw, wl_shm::Format::Argb8888 as u32);
        assert_eq!(f.width, 1920);
        assert_eq!(f.height, 1080);
        assert_eq!(f.stride, 1920 * 4);
    }

    #[test]
    fn test_buffer_constraints_pick_format_xrgb_fallback() {
        let c = BufferConstraints {
            width: 1920,
            height: 1080,
            shm_formats: vec![wl_shm::Format::Xrgb8888 as u32],
            done: true,
        };
        let f = c.pick_format().unwrap();
        assert_eq!(f.format_raw, wl_shm::Format::Xrgb8888 as u32);
    }

    #[test]
    fn test_buffer_constraints_pick_format_first_fallback() {
        let c = BufferConstraints {
            width: 800,
            height: 600,
            shm_formats: vec![wl_shm::Format::Abgr8888 as u32],
            done: true,
        };
        let f = c.pick_format().unwrap();
        assert_eq!(f.format_raw, wl_shm::Format::Abgr8888 as u32);
        assert_eq!(f.stride, 800 * 4);
    }

    #[test]
    fn test_buffer_constraints_zero_size() {
        let c = BufferConstraints {
            width: 0,
            height: 1080,
            shm_formats: vec![wl_shm::Format::Argb8888 as u32],
            done: true,
        };
        assert!(c.pick_format().is_none());
    }

    #[test]
    fn test_ext_capture_state_default() {
        let state = ExtCaptureState::default();
        assert!(state.manager.is_none());
        assert!(state.source_manager.is_none());
        assert!(state.captures.is_empty());
        assert!(state.pipewire.is_none());
        assert!(state.shm.is_none());
    }

    #[test]
    fn test_stop_capture_nonexistent() {
        let mut state = ExtCaptureState::default();
        // Should not panic
        state.stop_capture(42);
        assert!(state.captures.is_empty());
    }

    #[test]
    fn test_session_buffer_size() {
        let mut state = ExtCaptureState::default();
        // No capture exists, should not panic
        state.on_session_buffer_size(42, 1920, 1080);
    }

    #[test]
    fn test_session_shm_format() {
        let mut state = ExtCaptureState::default();
        // No capture exists, should not panic
        state.on_session_shm_format(42, wl_shm::Format::Argb8888 as u32);
    }

    #[test]
    fn test_ext_screenshot_id_counter() {
        let id1 = EXT_SCREENSHOT_ID_COUNTER.load(std::sync::atomic::Ordering::Relaxed);
        assert!(id1 >= 0xA000_0000);
    }
}
