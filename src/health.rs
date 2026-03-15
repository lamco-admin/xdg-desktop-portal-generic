//! Portal health monitoring and metrics.
//!
//! Provides structured health events and metrics from portal subsystems
//! (capture, input, clipboard) for consumption by downstream applications.
//!
//! # Architecture
//!
//! Each subsystem reports events via `tokio::sync::mpsc` channel:
//!
//! ```text
//! Wayland capture ─┐
//! Input backend  ───┤──→ mpsc::Sender<PortalHealthEvent> ──→ Consumer
//! Clipboard      ───┘
//! ```
//!
//! The consumer (e.g., lamco-rdp-server) bridges these events into its
//! own health monitoring infrastructure.

use std::time::{Duration, Instant};

/// Health events produced by portal subsystems.
///
/// Each variant carries enough context for the consumer to update its
/// health model without querying back into the portal.
#[derive(Debug, Clone)]
pub enum PortalHealthEvent {
    // --- Capture ---
    /// Frame captured successfully with timing data.
    FrameCaptured {
        /// PipeWire node ID of the stream.
        node_id: u32,
        /// Time from capture request to ready event.
        capture_latency: Duration,
        /// Raw pixel data size in bytes.
        frame_size_bytes: usize,
        /// Monotonic frame counter for this stream.
        frame_number: u64,
        /// Number of damage regions in this frame.
        damage_region_count: u32,
    },

    /// Frame capture failed.
    FrameFailed {
        /// PipeWire node ID of the stream.
        node_id: u32,
        /// Human-readable failure reason.
        reason: String,
    },

    /// Capture backend state changed.
    CaptureStateChanged {
        /// Which protocol is active.
        protocol: CaptureProtocolType,
        /// New state.
        state: CaptureState,
    },

    // --- Input ---
    /// Batch of input events processed.
    ///
    /// Batched to avoid per-keystroke overhead. Emitted periodically
    /// (e.g., every 100 events or every second).
    InputBatch {
        /// Events successfully forwarded to compositor.
        events_forwarded: u64,
        /// Events that failed to forward.
        events_failed: u64,
        /// Which input protocol is in use.
        protocol: InputProtocolType,
    },

    /// Input backend disconnected or failed.
    InputDisconnected {
        /// Reason for disconnection.
        reason: String,
        /// Whether this is recoverable.
        recoverable: bool,
    },

    // --- Clipboard ---
    /// Clipboard selection changed on the Wayland side.
    ClipboardSelectionChanged {
        /// Number of MIME formats in the new selection.
        format_count: usize,
    },

    /// Clipboard data transfer completed or failed.
    ClipboardTransferResult {
        /// Whether the transfer succeeded.
        success: bool,
        /// Bytes transferred (0 on failure).
        bytes: usize,
    },

    // --- EIS Protocol ---
    /// EIS Frame event received with serial and timestamp.
    ///
    /// Frame events batch input events with a monotonic serial number
    /// and CLOCK_MONOTONIC microsecond timestamp. Serial gaps indicate
    /// lost events; inter-frame timing indicates client input rate.
    EisFrameReceived {
        /// Last serial number from the EIS client.
        last_serial: u32,
        /// Client-side CLOCK_MONOTONIC timestamp in microseconds.
        time_usec: u64,
    },

    /// EIS device emulation state changed.
    ///
    /// StartEmulating = client begins sending input events.
    /// StopEmulating = client stops sending input events.
    EisDeviceStateChanged {
        /// Whether the device is now emulating.
        emulating: bool,
        /// Serial number at the state change.
        serial: u32,
        /// Sequence number (nonzero for StartEmulating only).
        sequence: u32,
    },

    // --- Session ---
    /// Portal session state changed.
    SessionStateChanged {
        /// New session state.
        state: PortalSessionState,
    },
}

/// Capture protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureProtocolType {
    /// wlr-screencopy-unstable-v1
    WlrScreencopy,
    /// ext-image-copy-capture-v1
    ExtImageCopyCapture,
}

/// Capture backend state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureState {
    /// Actively capturing frames.
    Active,
    /// Paused (no damage, or stream paused).
    Paused,
    /// Failed (needs restart).
    Failed,
}

/// Input protocol in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputProtocolType {
    /// wlr-virtual-pointer + zwp-virtual-keyboard
    WlrVirtual,
    /// EIS bridge (via reis crate)
    Eis,
}

/// Portal session state for health reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalSessionState {
    /// Session created, not yet started.
    Init,
    /// Session active.
    Started,
    /// Session closed.
    Closed,
}

/// Aggregate capture metrics for periodic reporting.
///
/// Updated on every frame; snapshot can be taken at any time.
#[derive(Debug, Clone)]
pub struct CaptureMetrics {
    /// Total frames successfully captured.
    pub frames_captured: u64,
    /// Total frames where capture failed.
    pub frames_failed: u64,
    /// Most recent capture latency.
    pub last_capture_latency: Duration,
    /// Exponential moving average capture latency.
    pub avg_capture_latency: Duration,
    /// Timestamp of the most recent frame.
    pub last_frame_time: Instant,
    /// Capture protocol in use.
    pub protocol: CaptureProtocolType,
}

impl CaptureMetrics {
    /// Create new metrics for a capture protocol.
    pub fn new(protocol: CaptureProtocolType) -> Self {
        Self {
            frames_captured: 0,
            frames_failed: 0,
            last_capture_latency: Duration::ZERO,
            avg_capture_latency: Duration::ZERO,
            last_frame_time: Instant::now(),
            protocol,
        }
    }

    /// Record a successful frame capture.
    pub fn record_frame(&mut self, latency: Duration) {
        self.frames_captured += 1;
        self.last_capture_latency = latency;
        self.last_frame_time = Instant::now();

        // Exponential moving average (alpha = 0.1)
        if self.frames_captured == 1 {
            self.avg_capture_latency = latency;
        } else {
            let alpha = 0.1_f64;
            let avg_us = self.avg_capture_latency.as_micros() as f64;
            let new_us = latency.as_micros() as f64;
            let updated = avg_us * (1.0 - alpha) + new_us * alpha;
            self.avg_capture_latency = Duration::from_micros(updated as u64);
        }
    }

    /// Record a failed frame capture.
    pub fn record_failure(&mut self) {
        self.frames_failed += 1;
    }

    /// Current effective FPS based on recent frame timing.
    pub fn effective_fps(&self) -> f64 {
        if self.frames_captured < 2 {
            return 0.0;
        }
        let elapsed = self.last_frame_time.elapsed();
        if elapsed.as_secs() > 5 {
            return 0.0; // stale
        }
        // This is approximate; a proper implementation would use a ring buffer
        self.frames_captured as f64 / self.last_frame_time.elapsed().as_secs_f64().max(0.001)
    }
}

/// Type alias for the health event sender.
pub type HealthSender = tokio::sync::mpsc::Sender<PortalHealthEvent>;

/// Type alias for the health event receiver.
pub type HealthReceiver = tokio::sync::mpsc::Receiver<PortalHealthEvent>;

/// Create a health event channel with sensible buffer size.
///
/// Returns (sender, receiver). The sender is cloned and passed to each
/// subsystem. The receiver is consumed by the portal backend's consumer.
pub fn health_channel() -> (HealthSender, HealthReceiver) {
    tokio::sync::mpsc::channel(256)
}
