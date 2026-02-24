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
pub struct StreamInfo {
    /// PipeWire node ID.
    pub node_id: u32,
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
    MotionAbsolute {
        /// Absolute X coordinate (0.0 to 1.0, normalized).
        x: f64,
        /// Absolute Y coordinate (0.0 to 1.0, normalized).
        y: f64,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
