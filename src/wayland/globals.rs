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

    // === InputCapture (barrier surfaces) ===
    /// wl_compositor is available (needed to create wl_surfaces for barriers).
    pub wl_compositor: bool,
    /// wlr-layer-shell-unstable-v1.
    pub wlr_layer_shell: bool,
    /// zwp-pointer-constraints-unstable-v1.
    pub wp_pointer_constraints: bool,
    /// zwp-relative-pointer-unstable-v1.
    pub wp_relative_pointer: bool,

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

    /// Check if InputCapture barrier surfaces can be created and enforced.
    ///
    /// Requires all three: `wl_compositor` (to create the barrier surface),
    /// `wlr-layer-shell-v1` (to place it precisely, invisibly), and both
    /// pointer-constraints and relative-pointer (to actually lock the
    /// pointer and read motion once captured -- Phase 2b).
    pub fn has_input_capture_barriers(&self) -> bool {
        self.wl_compositor
            && self.wlr_layer_shell
            && self.wp_pointer_constraints
            && self.wp_relative_pointer
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
        tracing::info!(
            "  InputCapture barriers: wl_compositor={}, wlr-layer-shell={}, \
             pointer-constraints={}, relative-pointer={}",
            self.wl_compositor,
            self.wlr_layer_shell,
            self.wp_pointer_constraints,
            self.wp_relative_pointer
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

    #[test]
    fn test_input_capture_barriers_requires_all_four() {
        let mut p = AvailableProtocols::default();
        assert!(!p.has_input_capture_barriers());

        p.wl_compositor = true;
        p.wlr_layer_shell = true;
        p.wp_pointer_constraints = true;
        assert!(!p.has_input_capture_barriers(), "missing relative-pointer");

        p.wp_relative_pointer = true;
        assert!(p.has_input_capture_barriers());

        p.wl_compositor = false;
        assert!(
            !p.has_input_capture_barriers(),
            "missing wl_compositor should fail even with everything else present"
        );
    }
}
