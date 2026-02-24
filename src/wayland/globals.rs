//! Wayland global protocol detection.
//!
//! Detects which Wayland protocols are available from the compositor by
//! inspecting the global registry.

/// Available Wayland protocols detected from the compositor.
///
/// This struct covers ALL protocol domains needed by the portal backend:
/// capture, input, clipboard, and seat. Protocol availability is determined
/// during the initial Wayland registry roundtrip.
#[derive(Debug, Default, Clone)]
pub struct AvailableProtocols {
    // === Screen Capture ===
    /// ext-image-copy-capture-v1 (preferred, staging standard).
    pub ext_image_copy_capture: bool,
    /// wlr-screencopy-unstable-v1 (fallback).
    pub wlr_screencopy: bool,

    // === Input ===
    /// wlr-virtual-pointer-v1.
    pub wlr_virtual_pointer: bool,
    /// zwp-virtual-keyboard-v1.
    pub zwp_virtual_keyboard: bool,

    // === Clipboard ===
    /// ext-data-control-v1 (preferred, staging standard).
    pub ext_data_control: bool,
    /// zwlr-data-control-manager-v1 (fallback).
    pub wlr_data_control: bool,

    // === Core ===
    /// wl_seat is available.
    pub seat: bool,
    /// wl_output globals count.
    pub output_count: u32,
}

impl AvailableProtocols {
    /// Check if any capture protocol is available.
    pub fn has_capture(&self) -> bool {
        self.ext_image_copy_capture || self.wlr_screencopy
    }

    /// Check if any input protocol is available.
    pub fn has_input(&self) -> bool {
        self.wlr_virtual_pointer || self.zwp_virtual_keyboard
    }

    /// Check if any clipboard protocol is available.
    pub fn has_clipboard(&self) -> bool {
        self.ext_data_control || self.wlr_data_control
    }

    /// Log a summary of detected protocols.
    pub fn log_summary(&self) {
        tracing::info!("Detected Wayland protocols:");
        tracing::info!(
            "  Capture: ext-image-copy-capture={}, wlr-screencopy={}",
            self.ext_image_copy_capture,
            self.wlr_screencopy
        );
        tracing::info!(
            "  Input: wlr-virtual-pointer={}, zwp-virtual-keyboard={}",
            self.wlr_virtual_pointer,
            self.zwp_virtual_keyboard
        );
        tracing::info!(
            "  Clipboard: ext-data-control={}, wlr-data-control={}",
            self.ext_data_control,
            self.wlr_data_control
        );
        tracing::info!("  Core: seat={}, outputs={}", self.seat, self.output_count);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_protocols_empty() {
        let p = AvailableProtocols::default();
        assert!(!p.has_capture());
        assert!(!p.has_input());
        assert!(!p.has_clipboard());
    }

    #[test]
    fn test_capture_detection() {
        let mut p = AvailableProtocols::default();
        assert!(!p.has_capture());

        p.ext_image_copy_capture = true;
        assert!(p.has_capture());

        p.ext_image_copy_capture = false;
        p.wlr_screencopy = true;
        assert!(p.has_capture());
    }

    #[test]
    fn test_input_detection() {
        let mut p = AvailableProtocols::default();
        assert!(!p.has_input());

        p.wlr_virtual_pointer = true;
        assert!(p.has_input());
    }

    #[test]
    fn test_clipboard_detection() {
        let mut p = AvailableProtocols::default();
        assert!(!p.has_clipboard());

        p.ext_data_control = true;
        assert!(p.has_clipboard());

        p.ext_data_control = false;
        p.wlr_data_control = true;
        assert!(p.has_clipboard());
    }
}
