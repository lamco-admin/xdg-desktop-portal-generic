//! wlr-screencopy frame capture state and SHM buffer management.
//!
//! This module manages the frame capture loop for `zwlr_screencopy_manager_v1`.
//! Each active capture corresponds to one wl_output being captured into one
//! PipeWire stream. The lifecycle:
//!
//! 1. `StartCapture` command → request frame from compositor
//! 2. Frame `buffer` event → allocate SHM buffer matching the format
//! 3. `frame.copy(buffer)` → compositor writes pixels into SHM
//! 4. Frame `ready` event → read SHM, send to PipeWire, request next frame
//! 5. `StopCapture` command → stop requesting frames, clean up

use std::{
    collections::HashMap,
    os::unix::io::{AsFd, OwnedFd},
    sync::Arc,
};

use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_output::WlOutput,
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
    },
    QueueHandle,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use super::dispatch::WaylandState;
use crate::{pipewire::PipeWireManager, types::CursorMode, wayland::ScreenshotData};

/// Information about a buffer format offered by the compositor for a frame.
#[derive(Debug, Clone, Copy)]
pub struct BufferFormatInfo {
    /// wl_shm format enum.
    pub format: wl_shm::Format,
    /// wl_shm format as raw u32 (for PipeWire SPA format mapping).
    pub format_raw: u32,
    /// Buffer width in pixels.
    pub width: u32,
    /// Buffer height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
}

/// A shared-memory frame buffer for receiving screencopy data.
///
/// Wraps a memfd-backed `wl_buffer` that the compositor writes pixel data into.
/// The buffer is mmap'd for direct read access after the frame `ready` event.
pub struct ShmFrameBuffer {
    /// The Wayland buffer object.
    pub buffer: WlBuffer,
    /// The SHM pool (kept alive to prevent the buffer from being invalidated).
    _pool: WlShmPool,
    /// The mmap'd pointer for reading pixel data.
    mmap_ptr: *mut libc::c_void,
    /// Size of the mmap'd region in bytes.
    mmap_size: usize,
    /// The buffer format.
    pub format: BufferFormatInfo,
}

// SAFETY: The ShmFrameBuffer is only used on the Wayland event loop thread.
// The mmap pointer is valid for the lifetime of the memfd (which we keep alive
// via the wl_shm_pool's fd). We never share the pointer across threads.
#[expect(
    unsafe_code,
    reason = "ShmFrameBuffer contains a raw mmap pointer but is only used on the Wayland event loop thread"
)]
unsafe impl Send for ShmFrameBuffer {}

impl ShmFrameBuffer {
    /// Allocate a new SHM frame buffer.
    ///
    /// Creates a memfd, sizes it, mmaps it, and wraps it in wl_shm_pool + wl_buffer.
    pub fn new(
        shm: &WlShm,
        qh: &QueueHandle<WaylandState>,
        format: &BufferFormatInfo,
    ) -> Result<Self, String> {
        let size = (format.stride * format.height) as usize;
        if size == 0 {
            return Err("Buffer size is zero".to_string());
        }

        // Create anonymous memfd for the SHM pool
        let fd = Self::create_memfd(size)?;

        // mmap the memfd for reading pixel data after compositor writes
        // SAFETY: fd is a valid file descriptor we just created, size > 0, and offset 0
        // is within the file bounds. MAP_SHARED is required for wl_shm interop.
        #[expect(
            unsafe_code,
            reason = "mmap requires unsafe FFI call with validated fd and size"
        )]
        let mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                std::os::unix::io::AsRawFd::as_raw_fd(&fd),
                0,
            )
        };

        if mmap_ptr == libc::MAP_FAILED {
            return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
        }

        // Create wl_shm_pool from the memfd
        let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());

        // Create wl_buffer from the pool
        let buffer = pool.create_buffer(
            0, // offset
            format.width as i32,
            format.height as i32,
            format.stride as i32,
            format.format,
            qh,
            (),
        );

        Ok(Self {
            buffer,
            _pool: pool,
            mmap_ptr,
            mmap_size: size,
            format: *format,
        })
    }

    /// Create an anonymous memfd and set its size.
    fn create_memfd(size: usize) -> Result<OwnedFd, String> {
        use nix::sys::memfd::{memfd_create, MemFdCreateFlag};

        let name = c"xdp-screencopy";

        let fd = memfd_create(
            name,
            MemFdCreateFlag::MFD_CLOEXEC | MemFdCreateFlag::MFD_ALLOW_SEALING,
        )
        .map_err(|e| format!("memfd_create failed: {e}"))?;

        // Set the size
        nix::unistd::ftruncate(&fd, size as i64).map_err(|e| format!("ftruncate failed: {e}"))?;

        Ok(fd)
    }

    /// Read pixel data from the mmap'd buffer.
    ///
    /// Called after the frame `ready` event. Returns a copy of the pixel data.
    pub fn read_pixels(&self) -> Vec<u8> {
        let mut data = vec![0u8; self.mmap_size];
        // SAFETY: mmap_ptr is valid and points to mmap_size bytes of readable memory.
        // The compositor has finished writing (we only call this after `ready`).
        #[expect(
            unsafe_code,
            reason = "reading from validated mmap pointer after compositor ready event"
        )]
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.mmap_ptr as *const u8,
                data.as_mut_ptr(),
                self.mmap_size,
            );
        }
        data
    }
}

impl Drop for ShmFrameBuffer {
    fn drop(&mut self) {
        if self.mmap_ptr != libc::MAP_FAILED && !self.mmap_ptr.is_null() {
            // SAFETY: mmap_ptr was returned by a successful mmap() call with mmap_size.
            #[expect(
                unsafe_code,
                reason = "munmap requires unsafe FFI call to free mmap allocation"
            )]
            unsafe {
                libc::munmap(self.mmap_ptr, self.mmap_size);
            }
        }
        self.buffer.destroy();
    }
}

/// Counter for generating unique screenshot capture IDs.
/// Uses high range to avoid collision with PipeWire node IDs.
static SCREENSHOT_ID_COUNTER: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0x8000_0000);

/// State for one active screen capture (output → PipeWire stream).
pub struct ActiveCapture {
    /// The wl_output being captured.
    pub output: WlOutput,
    /// Output global name (for logging/matching).
    pub output_global_name: u32,
    /// PipeWire stream node ID to deliver frames to.
    pub node_id: u32,
    /// Whether to overlay the cursor in captured frames.
    pub cursor_overlay: bool,
    /// The allocated SHM buffer (reused across frames).
    pub shm_buffer: Option<ShmFrameBuffer>,
    /// The currently pending frame (waiting for ready/failed).
    pub pending_frame: Option<ZwlrScreencopyFrameV1>,
    /// Buffer format info from the most recent frame `buffer` event.
    pub pending_format: Option<BufferFormatInfo>,
    /// Whether we've received `buffer_done` (v3) for the current frame.
    pub buffer_done_received: bool,
    /// If set, this is a one-shot screenshot capture. The reply channel
    /// receives the pixel data when the frame is ready, and the capture
    /// is removed (no next-frame loop).
    pub screenshot_reply:
        Option<tokio::sync::oneshot::Sender<std::result::Result<ScreenshotData, String>>>,
}

/// Central screencopy state, stored in WaylandState.
#[derive(Default)]
pub struct ScreencopyState {
    /// The screencopy manager global.
    pub manager: Option<ZwlrScreencopyManagerV1>,
    /// Protocol version of the bound manager (1, 2, or 3).
    /// v3 adds `buffer_done` and `linux_dmabuf` events.
    pub manager_version: u32,
    /// Active captures, keyed by PipeWire node ID.
    pub captures: HashMap<u32, ActiveCapture>,
    /// Reference to the PipeWire manager for sending frame data.
    pub pipewire: Option<Arc<PipeWireManager>>,
    /// Reference to the wl_shm global for buffer allocation.
    pub shm: Option<WlShm>,
}

// ScreencopyState derives Default since all fields are Option/HashMap.

impl ScreencopyState {
    /// Start capturing an output.
    ///
    /// Sends the initial `capture_output` request to the compositor.
    pub fn start_capture(
        &mut self,
        qh: &QueueHandle<WaylandState>,
        output: WlOutput,
        output_global_name: u32,
        node_id: u32,
        cursor_mode: CursorMode,
    ) {
        let Some(manager) = &self.manager else {
            tracing::error!("Cannot start capture: screencopy manager not bound");
            return;
        };

        let cursor_overlay = match cursor_mode {
            CursorMode::Embedded => 1,
            _ => 0,
        };

        tracing::info!(
            node_id,
            output = output_global_name,
            cursor_overlay,
            "Starting screencopy capture"
        );

        // Request the first frame from the compositor.
        // The frame will be created with node_id as user data so we can
        // route events back to the right ActiveCapture.
        let frame = manager.capture_output(cursor_overlay, &output, qh, node_id);

        let capture = ActiveCapture {
            output,
            output_global_name,
            node_id,
            cursor_overlay: cursor_overlay != 0,
            shm_buffer: None,
            pending_frame: Some(frame),
            pending_format: None,
            buffer_done_received: false,
            screenshot_reply: None,
        };

        self.captures.insert(node_id, capture);
    }

    /// Start a one-shot screenshot capture of an output.
    ///
    /// Captures a single frame and sends the pixel data through the reply
    /// channel. The capture is removed after the frame is received.
    pub fn start_screenshot(
        &mut self,
        qh: &QueueHandle<WaylandState>,
        output: WlOutput,
        output_global_name: u32,
        reply: tokio::sync::oneshot::Sender<std::result::Result<ScreenshotData, String>>,
    ) {
        let Some(manager) = &self.manager else {
            let _ = reply.send(Err("Screencopy manager not bound".to_string()));
            return;
        };

        let screenshot_id =
            SCREENSHOT_ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        tracing::info!(
            screenshot_id,
            output = output_global_name,
            "Starting one-shot screenshot capture"
        );

        let frame = manager.capture_output(1, &output, qh, screenshot_id);

        let capture = ActiveCapture {
            output,
            output_global_name,
            node_id: screenshot_id,
            cursor_overlay: true,
            shm_buffer: None,
            pending_frame: Some(frame),
            pending_format: None,
            buffer_done_received: false,
            screenshot_reply: Some(reply),
        };

        self.captures.insert(screenshot_id, capture);
    }

    /// Stop capturing an output.
    pub fn stop_capture(&mut self, node_id: u32) {
        if let Some(mut capture) = self.captures.remove(&node_id) {
            // Destroy the pending frame if any
            if let Some(frame) = capture.pending_frame.take() {
                frame.destroy();
            }
            // SHM buffer is dropped automatically
            tracing::info!(node_id, "Stopped screencopy capture");
        }
    }

    /// Handle the `buffer` event from a screencopy frame.
    ///
    /// Records the offered SHM format. For v1/v2 (no `buffer_done` event),
    /// immediately allocates the buffer and calls `frame.copy()`.
    pub fn on_frame_buffer(
        &mut self,
        node_id: u32,
        format_info: BufferFormatInfo,
        qh: &QueueHandle<WaylandState>,
    ) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            // Store the first offered SHM format (we prefer the first one)
            if capture.pending_format.is_none() {
                tracing::debug!(
                    node_id,
                    format = format_info.format_raw,
                    width = format_info.width,
                    height = format_info.height,
                    stride = format_info.stride,
                    "Screencopy frame buffer format received"
                );
                capture.pending_format = Some(format_info);
            }
        }

        // For v1/v2, there is no buffer_done event — allocate and copy immediately
        // after the first buffer event.
        if self.manager_version < 3 {
            self.allocate_and_copy(node_id, qh);
        }
    }

    /// Handle the `buffer_done` event (v3+).
    ///
    /// All buffer format events have been sent. Allocate the SHM buffer
    /// and call `frame.copy()`.
    pub fn on_frame_buffer_done(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            capture.buffer_done_received = true;
            self.allocate_and_copy(node_id, qh);
        }
    }

    /// Allocate SHM buffer and initiate copy for a frame.
    ///
    /// Called after `buffer_done` (v3) or after the first `buffer` event
    /// when using v1/v2 protocol versions.
    fn allocate_and_copy(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        let Some(shm) = &self.shm else {
            tracing::error!("Cannot allocate screencopy buffer: wl_shm not bound");
            return;
        };
        let shm = shm.clone();

        let Some(capture) = self.captures.get_mut(&node_id) else {
            return;
        };

        let Some(format) = capture.pending_format else {
            tracing::error!(node_id, "No buffer format available for frame copy");
            return;
        };

        // Reuse existing SHM buffer if format matches, otherwise allocate new one
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
                        "Allocated new SHM frame buffer"
                    );
                    capture.shm_buffer = Some(buf);
                }
                Err(e) => {
                    tracing::error!(node_id, error = %e, "Failed to allocate SHM buffer");
                    return;
                }
            }
        }

        // Copy: tell the compositor to write into our buffer
        if let (Some(frame), Some(buf)) = (&capture.pending_frame, &capture.shm_buffer) {
            frame.copy(&buf.buffer);
            tracing::trace!(node_id, "Sent frame.copy() to compositor");
        }
    }

    /// Handle the `ready` event from a screencopy frame.
    ///
    /// The compositor has finished writing pixel data. Read from SHM,
    /// send to PipeWire (or reply channel for screenshots), and request
    /// the next frame (or remove the capture for screenshots).
    pub fn on_frame_ready(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        // Read pixel data from the SHM buffer
        let (data, width, height, stride, format_raw, is_screenshot) = {
            let Some(capture) = self.captures.get_mut(&node_id) else {
                return;
            };

            // Destroy the completed frame
            if let Some(frame) = capture.pending_frame.take() {
                frame.destroy();
            }

            let Some(buf) = &capture.shm_buffer else {
                tracing::error!(node_id, "Frame ready but no SHM buffer");
                // If this was a screenshot, send error through reply
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
            // One-shot screenshot: send data through reply channel and remove capture
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
                tracing::info!(node_id, width, height, "Screenshot capture complete");
            }
        } else {
            // Continuous capture: send to PipeWire and request next frame
            if let Some(pw) = &self.pipewire {
                pw.queue_buffer(node_id, data, width, height, stride, format_raw);
                tracing::trace!(node_id, width, height, "Queued frame to PipeWire");
            }

            self.request_next_frame(node_id, qh);
        }
    }

    /// Handle the `failed` event from a screencopy frame.
    ///
    /// The frame capture failed. For regular captures, retry. For screenshots,
    /// send an error through the reply channel.
    pub fn on_frame_failed(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        if let Some(capture) = self.captures.get_mut(&node_id) {
            if let Some(frame) = capture.pending_frame.take() {
                frame.destroy();
            }

            // For screenshots, send error and remove
            if capture.screenshot_reply.is_some() {
                if let Some(mut capture) = self.captures.remove(&node_id) {
                    if let Some(reply) = capture.screenshot_reply.take() {
                        let _ = reply.send(Err("Screencopy frame capture failed".to_string()));
                    }
                }
                tracing::warn!(node_id, "Screenshot capture failed");
                return;
            }

            tracing::warn!(node_id, "Screencopy frame failed, retrying");
        }

        // Request next frame (retry) for regular captures
        self.request_next_frame(node_id, qh);
    }

    /// Request the next frame from the compositor.
    fn request_next_frame(&mut self, node_id: u32, qh: &QueueHandle<WaylandState>) {
        let Some(manager) = &self.manager else { return };

        let Some(capture) = self.captures.get_mut(&node_id) else {
            return;
        };

        // Reset per-frame state
        capture.pending_format = None;
        capture.buffer_done_received = false;

        // Request next frame
        let cursor = i32::from(capture.cursor_overlay);
        let frame = manager.capture_output(cursor, &capture.output, qh, node_id);
        capture.pending_frame = Some(frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_format_info() {
        let info = BufferFormatInfo {
            format: wl_shm::Format::Argb8888,
            format_raw: 0,
            width: 1920,
            height: 1080,
            stride: 7680,
        };
        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert_eq!(info.stride, 7680);
    }

    #[test]
    fn test_screencopy_state_default() {
        let state = ScreencopyState::default();
        assert!(state.manager.is_none());
        assert!(state.captures.is_empty());
        assert!(state.pipewire.is_none());
        assert!(state.shm.is_none());
    }

    #[test]
    fn test_stop_capture_nonexistent() {
        let mut state = ScreencopyState::default();
        // Should not panic
        state.stop_capture(42);
        assert!(state.captures.is_empty());
    }
}
