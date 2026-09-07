//! Data types for the portal backend.
//!
//! This module contains shared data types used across the portal backend,
//! including source information, stream information, device types, input
//! events, clipboard data, and cursor modes.

use std::collections::HashMap;

/// Information about a capturable source (monitor or window).
#[derive(Debug, Clone)]
pub struct SourceInfo {
    /// Unique identifier for this source.
    pub id: u32,
    /// Human-readable name (e.g., "eDP-1").
    pub name: String,
    /// Human-readable description (e.g., "Built-in Display").
    pub description: String,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Refresh rate in millihertz.
    pub refresh_rate: u32,
    /// Position x, in the compositor's global (layout) coordinate space.
    pub x: i32,
    /// Position y, in the compositor's global (layout) coordinate space.
    pub y: i32,
    /// Source type.
    pub source_type: SourceType,
}

/// Type of capturable source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourceType {
    /// A display/monitor output.
    Monitor,
    /// An application window.
    Window,
    /// A virtual source (e.g., region selection).
    Virtual,
}

impl SourceType {
    /// Convert to D-Bus bit flags.
    pub fn to_bits(self) -> u32 {
        match self {
            SourceType::Monitor => 0x01,
            SourceType::Window => 0x02,
            SourceType::Virtual => 0x04,
        }
    }

    /// Parse from D-Bus bit flags.
    pub fn from_bits(bits: u32) -> Vec<Self> {
        let mut types = Vec::new();
        if bits & 0x01 != 0 {
            types.push(SourceType::Monitor);
        }
        if bits & 0x02 != 0 {
            types.push(SourceType::Window);
        }
        if bits & 0x04 != 0 {
            types.push(SourceType::Virtual);
        }
        types
    }
}

/// Information about a PipeWire stream.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StreamInfo {
    /// PipeWire node ID.
    pub node_id: u32,
    /// PipeWire `object.serial` of the stream node, when available.
    ///
    /// Emitted as the ScreenCast v6 `pipewire-serial` stream property so
    /// clients can re-follow the stream across output reconfiguration without
    /// relying on the (deprecated) node ID. `None` when the compositor's
    /// PipeWire is too old to assign a serial.
    pub serial: Option<u64>,
    /// Source ID this stream captures.
    pub source_id: u32,
    /// Stream position (x, y).
    pub position: (i32, i32),
    /// Stream size (width, height).
    pub size: (u32, u32),
    /// Source type (Monitor, Window, Virtual).
    pub source_type: SourceType,
    /// Mapping ID for persistent source identification across sessions.
    ///
    /// Format: `"output:<name>"` (e.g., `"output:eDP-1"`). Used by ScreenCast v5
    /// to let clients restore the same source selection without user interaction.
    pub mapping_id: Option<String>,
    /// Additional properties.
    pub properties: HashMap<String, String>,
}

/// Mapping from a PipeWire stream node ID to its output geometry.
///
/// Used for multi-monitor absolute pointer positioning. When a client sends
/// `NotifyPointerMotionAbsolute` with a stream ID, the input backend uses
/// this mapping to translate normalized (0.0–1.0) coordinates into
/// compositor-global absolute coordinates.
#[derive(Debug, Clone)]
pub struct StreamOutputMapping {
    /// PipeWire stream node ID.
    pub stream_node_id: u32,
    /// X position of this output in compositor-global coordinates.
    pub x: i32,
    /// Y position of this output in compositor-global coordinates.
    pub y: i32,
    /// Width of this output in pixels.
    pub width: u32,
    /// Height of this output in pixels.
    pub height: u32,
}

/// A single InputCapture zone: one output's geometry in compositor-global
/// coordinates.
///
/// Returned by `InputCapture.GetZones` as `a(uuii)` = (width, height, x, y).
/// Deliberately a separate type from [`SourceInfo`] (which has no position
/// fields) rather than a breaking addition to that published, non-exhaustive
/// struct — see [`StreamOutputMapping`] for the same "need geometry outside
/// the Wayland thread" precedent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InputCaptureZone {
    /// Zone width in pixels.
    pub width: u32,
    /// Zone height in pixels.
    pub height: u32,
    /// X position in compositor-global coordinates.
    pub x: i32,
    /// Y position in compositor-global coordinates.
    pub y: i32,
}

/// A pointer barrier submitted via `InputCapture.SetPointerBarriers`.
///
/// Per spec, a barrier must be axis-aligned: horizontal (`y1 == y2`) or
/// vertical (`x1 == x2`). Diagonal barriers are not supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PointerBarrier {
    /// Caller-assigned barrier identifier. Must be non-zero.
    pub barrier_id: u32,
    /// Start X coordinate.
    pub x1: i32,
    /// Start Y coordinate.
    pub y1: i32,
    /// End X coordinate.
    pub x2: i32,
    /// End Y coordinate.
    pub y2: i32,
}

impl PointerBarrier {
    /// Check structural validity: non-zero id, axis-aligned, non-degenerate.
    ///
    /// `horizontal ^ vertical` rejects both failure cases with one
    /// condition: a degenerate point (`x1==x2 && y1==y2`) has both true
    /// (XOR is false), and a diagonal has both false (XOR is false); a
    /// proper axis-aligned barrier has exactly one true (XOR is true).
    pub fn is_valid(&self) -> bool {
        let horizontal = self.y1 == self.y2;
        let vertical = self.x1 == self.x2;
        self.barrier_id != 0 && (horizontal ^ vertical)
    }
}

/// Device types for input injection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceTypes {
    /// Keyboard input.
    pub keyboard: bool,
    /// Pointer (mouse) input.
    pub pointer: bool,
    /// Touchscreen input.
    pub touchscreen: bool,
}

impl DeviceTypes {
    /// All device types enabled.
    pub fn all() -> Self {
        Self {
            keyboard: true,
            pointer: true,
            touchscreen: true,
        }
    }

    /// Parse from D-Bus bit flags.
    pub fn from_bits(bits: u32) -> Self {
        Self {
            keyboard: (bits & 0x01) != 0,
            pointer: (bits & 0x02) != 0,
            touchscreen: (bits & 0x04) != 0,
        }
    }

    /// Convert to D-Bus bit flags.
    pub fn to_bits(self) -> u32 {
        let mut bits = 0u32;
        if self.keyboard {
            bits |= 0x01;
        }
        if self.pointer {
            bits |= 0x02;
        }
        if self.touchscreen {
            bits |= 0x04;
        }
        bits
    }

    /// Check if any device type is enabled.
    pub fn any(&self) -> bool {
        self.keyboard || self.pointer || self.touchscreen
    }
}

/// Input event types that can be injected.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InputEvent {
    /// Keyboard event.
    Keyboard(KeyboardEvent),
    /// Pointer (mouse) event.
    Pointer(PointerEvent),
    /// Touch event.
    Touch(TouchEvent),
}

/// Keyboard input event.
#[derive(Debug, Clone)]
pub struct KeyboardEvent {
    /// Key code (evdev).
    pub keycode: u32,
    /// Key state.
    pub state: KeyState,
    /// Timestamp in microseconds.
    pub time_usec: u64,
}

/// Key press state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum KeyState {
    /// Key pressed down.
    Pressed,
    /// Key released.
    Released,
}

impl KeyState {
    /// Convert from D-Bus state value.
    pub fn from_dbus(state: u32) -> Self {
        if state == 1 {
            KeyState::Pressed
        } else {
            KeyState::Released
        }
    }

    /// Convert to D-Bus state value.
    pub fn to_dbus(self) -> u32 {
        match self {
            KeyState::Pressed => 1,
            KeyState::Released => 0,
        }
    }
}

/// Pointer (mouse) input event.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum PointerEvent {
    /// Relative motion.
    Motion {
        /// Horizontal delta in pixels.
        dx: f64,
        /// Vertical delta in pixels.
        dy: f64,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Absolute motion (for tablets, touchpads in absolute mode).
    ///
    /// `x` and `y` are pixel coordinates within a logical frame of size
    /// (`x_extent`, `y_extent`). The backend normalizes internally:
    /// `nx = x / x_extent`, `ny = y / y_extent`, clamped to `[0, 1]`.
    ///
    /// When `x_extent == 0` (or `y_extent == 0`), the corresponding `x`
    /// (or `y`) is treated as already-normalized `[0, 1]` — this preserves
    /// the legacy D-Bus contract for callers that haven't been updated.
    MotionAbsolute {
        /// Absolute X coordinate, in pixels within the source frame
        /// (or normalized `[0, 1]` when `x_extent == 0`).
        x: f64,
        /// Absolute Y coordinate, in pixels within the source frame
        /// (or normalized `[0, 1]` when `y_extent == 0`).
        y: f64,
        /// Width of the source frame in pixels, or `0` if `x` is normalized.
        x_extent: u32,
        /// Height of the source frame in pixels, or `0` if `y` is normalized.
        y_extent: u32,
        /// Stream ID for multi-monitor setups.
        stream: u32,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Button press/release.
    Button {
        /// Button code (evdev).
        button: u32,
        /// Button state (pressed/released).
        state: ButtonState,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Scroll event (continuous).
    Scroll {
        /// Horizontal scroll delta.
        dx: f64,
        /// Vertical scroll delta.
        dy: f64,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Discrete scroll (wheel clicks).
    ScrollDiscrete {
        /// Scroll axis (vertical or horizontal).
        axis: ScrollAxis,
        /// Number of discrete steps.
        steps: i32,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Scroll stop (axis finish).
    ///
    /// Sent when a scroll gesture ends. Indicates that the user has lifted
    /// their fingers from the touchpad or otherwise completed the scroll.
    ScrollStop {
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
}

/// Button press state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ButtonState {
    /// Button pressed.
    Pressed,
    /// Button released.
    Released,
}

impl ButtonState {
    /// Convert from D-Bus state value.
    pub fn from_dbus(state: u32) -> Self {
        if state == 1 {
            ButtonState::Pressed
        } else {
            ButtonState::Released
        }
    }

    /// Convert to D-Bus state value.
    pub fn to_dbus(self) -> u32 {
        match self {
            ButtonState::Pressed => 1,
            ButtonState::Released => 0,
        }
    }
}

/// Scroll axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScrollAxis {
    /// Vertical scroll.
    Vertical,
    /// Horizontal scroll.
    Horizontal,
}

impl ScrollAxis {
    /// Convert from D-Bus axis value.
    pub fn from_dbus(axis: u32) -> Self {
        if axis == 0 {
            ScrollAxis::Vertical
        } else {
            ScrollAxis::Horizontal
        }
    }
}

/// Touch input event.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TouchEvent {
    /// Touch point started.
    Down {
        /// Touch point identifier (for multi-touch tracking).
        id: i32,
        /// Touch X coordinate (0.0 to 1.0, normalized).
        x: f64,
        /// Touch Y coordinate (0.0 to 1.0, normalized).
        y: f64,
        /// Stream ID for multi-monitor setups.
        stream: u32,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Touch point moved.
    Motion {
        /// Touch point identifier (for multi-touch tracking).
        id: i32,
        /// Touch X coordinate (0.0 to 1.0, normalized).
        x: f64,
        /// Touch Y coordinate (0.0 to 1.0, normalized).
        y: f64,
        /// Stream ID for multi-monitor setups.
        stream: u32,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
    /// Touch point lifted.
    Up {
        /// Touch point identifier (for multi-touch tracking).
        id: i32,
        /// Event timestamp in microseconds.
        time_usec: u64,
    },
}

/// Clipboard data.
#[derive(Debug, Clone, Default)]
pub struct ClipboardData {
    /// Available MIME types.
    pub mime_types: Vec<String>,
    /// Data for each MIME type (lazily populated).
    pub data: HashMap<String, Vec<u8>>,
}

/// Cursor mode for screen capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum CursorMode {
    /// Hide cursor in capture.
    Hidden,
    /// Embed cursor in video stream.
    #[default]
    Embedded,
    /// Provide cursor as separate metadata.
    Metadata,
}

impl CursorMode {
    /// Convert from D-Bus bit flags.
    pub fn from_bits(bits: u32) -> Self {
        if bits & 0x04 != 0 {
            CursorMode::Metadata
        } else if bits & 0x02 != 0 {
            CursorMode::Embedded
        } else {
            CursorMode::Hidden
        }
    }

    /// Convert to D-Bus bit flags.
    pub fn to_bits(self) -> u32 {
        match self {
            CursorMode::Hidden => 0x01,
            CursorMode::Embedded => 0x02,
            CursorMode::Metadata => 0x04,
        }
    }
}

// === Pixel format normalization ===
//
// `wl_shm` format names describe channel order read as a big-endian 32-bit
// int, which is the *reverse* of the actual little-endian in-memory byte
// order. `xrgb8888`/`argb8888` land as [B,G,R,X]/[B,G,R,A] -- already the
// crate's canonical BGRx/BGRA output order, no correction needed. But
// `xbgr8888`/`abgr8888` land as [R,G,B,X]/[R,G,B,A] -- red and blue
// transposed relative to BGRx/BGRA. Compositors deliver whichever format
// their buffer allocator produces (e.g. wlroots + virtio-gpu emits
// xbgr8888), so every capture consumer that assumes BGRx unconditionally
// renders with red and blue swapped on those compositors.

/// `wl_shm` format value for `xbgr8888` (in-memory `[R,G,B,X]`).
const WL_SHM_FORMAT_XBGR8888: u32 = 0x3432_4258;
/// `wl_shm` format value for `abgr8888` (in-memory `[R,G,B,A]`).
const WL_SHM_FORMAT_ABGR8888: u32 = 0x3432_4241;

/// Whether a captured buffer in this `wl_shm` format needs its red and blue
/// channels swapped to reach the crate's canonical BGRx/BGRA output order.
///
/// Only `xbgr8888`/`abgr8888` need it; `argb8888`/`xrgb8888` are already
/// correctly ordered. An unrecognized format is treated as already-correct
/// rather than swapped speculatively -- it was going to be wrong either way,
/// and guessing risks turning an already-correct format incorrect.
pub fn wl_shm_format_needs_rb_swap(format_raw: u32) -> bool {
    matches!(format_raw, WL_SHM_FORMAT_XBGR8888 | WL_SHM_FORMAT_ABGR8888)
}

/// Swap the red and blue byte positions in place for every 4-byte pixel in
/// `data`, laid out with `stride` bytes per row and `height` rows (`width`
/// pixels used per row; `stride` may exceed `width * 4` for row padding,
/// which is left untouched).
pub fn swap_rb_channels_in_place(data: &mut [u8], width: u32, height: u32, stride: u32) {
    for y in 0..height {
        let row_start = (y * stride) as usize;
        for x in 0..width {
            let px = row_start + (x * 4) as usize;
            if px + 2 < data.len() {
                data.swap(px, px + 2);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pointer_barrier_valid_horizontal() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 100,
            x2: 1920,
            y2: 100,
        };
        assert!(barrier.is_valid());
    }

    #[test]
    fn test_pointer_barrier_valid_vertical() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 1920,
            y1: 0,
            x2: 1920,
            y2: 1080,
        };
        assert!(barrier.is_valid());
    }

    #[test]
    fn test_pointer_barrier_rejects_diagonal() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 0,
            x2: 100,
            y2: 100,
        };
        assert!(!barrier.is_valid());
    }

    #[test]
    fn test_pointer_barrier_rejects_degenerate_point() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 50,
            y1: 50,
            x2: 50,
            y2: 50,
        };
        assert!(!barrier.is_valid());
    }

    #[test]
    fn test_pointer_barrier_rejects_zero_id() {
        let barrier = PointerBarrier {
            barrier_id: 0,
            x1: 0,
            y1: 0,
            x2: 1920,
            y2: 0,
        };
        assert!(!barrier.is_valid());
    }

    #[test]
    fn test_device_types_bits() {
        let devices = DeviceTypes {
            keyboard: true,
            pointer: true,
            touchscreen: false,
        };

        assert_eq!(devices.to_bits(), 0x03);

        let parsed = DeviceTypes::from_bits(0x03);
        assert!(parsed.keyboard);
        assert!(parsed.pointer);
        assert!(!parsed.touchscreen);
    }

    #[test]
    fn test_device_types_all() {
        let all = DeviceTypes::all();
        assert!(all.keyboard);
        assert!(all.pointer);
        assert!(all.touchscreen);
        assert_eq!(all.to_bits(), 0x07);
    }

    #[test]
    fn test_source_type_bits() {
        assert_eq!(SourceType::Monitor.to_bits(), 0x01);
        assert_eq!(SourceType::Window.to_bits(), 0x02);
        assert_eq!(SourceType::Virtual.to_bits(), 0x04);

        let types = SourceType::from_bits(0x03);
        assert_eq!(types.len(), 2);
        assert!(types.contains(&SourceType::Monitor));
        assert!(types.contains(&SourceType::Window));
    }

    #[test]
    fn test_key_state_conversion() {
        assert_eq!(KeyState::from_dbus(1), KeyState::Pressed);
        assert_eq!(KeyState::from_dbus(0), KeyState::Released);
        assert_eq!(KeyState::Pressed.to_dbus(), 1);
        assert_eq!(KeyState::Released.to_dbus(), 0);
    }

    #[test]
    fn test_button_state_conversion() {
        assert_eq!(ButtonState::from_dbus(1), ButtonState::Pressed);
        assert_eq!(ButtonState::from_dbus(0), ButtonState::Released);
        assert_eq!(ButtonState::Pressed.to_dbus(), 1);
        assert_eq!(ButtonState::Released.to_dbus(), 0);
    }

    #[test]
    fn test_stream_output_mapping() {
        let mapping = StreamOutputMapping {
            stream_node_id: 42,
            x: 1920,
            y: 0,
            width: 2560,
            height: 1440,
        };
        assert_eq!(mapping.stream_node_id, 42);
        assert_eq!(mapping.x, 1920);
        assert_eq!(mapping.y, 0);
        assert_eq!(mapping.width, 2560);
        assert_eq!(mapping.height, 1440);
    }

    #[test]
    fn test_cursor_mode_bits() {
        assert_eq!(CursorMode::Hidden.to_bits(), 0x01);
        assert_eq!(CursorMode::Embedded.to_bits(), 0x02);
        assert_eq!(CursorMode::Metadata.to_bits(), 0x04);

        assert_eq!(CursorMode::from_bits(0x01), CursorMode::Hidden);
        assert_eq!(CursorMode::from_bits(0x02), CursorMode::Embedded);
        assert_eq!(CursorMode::from_bits(0x04), CursorMode::Metadata);
    }

    #[test]
    fn test_wl_shm_format_needs_rb_swap() {
        // argb8888 / xrgb8888: already BGR-ordered in memory, no swap.
        assert!(!wl_shm_format_needs_rb_swap(0));
        assert!(!wl_shm_format_needs_rb_swap(1));
        // xbgr8888 / abgr8888: RGB-ordered in memory, needs swap.
        assert!(wl_shm_format_needs_rb_swap(WL_SHM_FORMAT_XBGR8888));
        assert!(wl_shm_format_needs_rb_swap(WL_SHM_FORMAT_ABGR8888));
        // Unrecognized format: treated as already-correct, not swapped.
        assert!(!wl_shm_format_needs_rb_swap(0xdead_beef));
    }

    #[test]
    fn test_swap_rb_channels_in_place() {
        // 2x1 image, no row padding (stride == width * 4).
        // Pixel 0: R=10 G=20 B=30 X=40 (xbgr8888 in-memory order)
        // Pixel 1: R=50 G=60 B=70 X=80
        let mut data = vec![10, 20, 30, 40, 50, 60, 70, 80];
        swap_rb_channels_in_place(&mut data, 2, 1, 8);
        // After swap: B and R positions (0 and 2) exchanged per pixel.
        assert_eq!(data, vec![30, 20, 10, 40, 70, 60, 50, 80]);
    }

    #[test]
    fn test_swap_rb_channels_in_place_respects_stride_padding() {
        // 1x2 image, 4 bytes of row padding after each 1-pixel (4-byte) row.
        let mut data = vec![
            10, 20, 30, 40, 0xff, 0xff, 0xff, 0xff, // row 0: pixel + padding
            50, 60, 70, 80, 0xff, 0xff, 0xff, 0xff, // row 1: pixel + padding
        ];
        swap_rb_channels_in_place(&mut data, 1, 2, 8);
        assert_eq!(data[0..4], [30, 20, 10, 40]);
        assert_eq!(data[8..12], [70, 60, 50, 80]);
        // Padding bytes must be untouched.
        assert_eq!(data[4..8], [0xff, 0xff, 0xff, 0xff]);
        assert_eq!(data[12..16], [0xff, 0xff, 0xff, 0xff]);
    }
}
