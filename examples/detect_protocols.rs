//! Detect available Wayland protocols from the compositor.
//!
//! This example connects to the running Wayland compositor and reports
//! which protocols are available for capture, input, and clipboard.
//! Useful for debugging compositor support.
//!
//! Run with: `cargo run --example detect_protocols`

use xdg_desktop_portal_generic::wayland::WaylandConnection;

fn main() -> anyhow::Result<()> {
    // Connect to the compositor via $WAYLAND_DISPLAY
    let wayland = WaylandConnection::connect()?;
    let protocols = wayland.available_protocols();

    println!("=== Wayland Protocol Detection ===\n");

    // Capture protocols
    println!("Screen Capture:");
    println!(
        "  ext-image-copy-capture-v1: {}",
        if protocols.ext_image_copy_capture {
            "available (preferred)"
        } else {
            "not available"
        }
    );
    println!(
        "  wlr-screencopy-unstable-v1: {}",
        if protocols.wlr_screencopy {
            "available (fallback)"
        } else {
            "not available"
        }
    );

    // Input protocols
    println!("\nInput Injection:");
    println!(
        "  wlr-virtual-pointer-v1: {}",
        if protocols.wlr_virtual_pointer {
            "available"
        } else {
            "not available"
        }
    );
    println!(
        "  zwp-virtual-keyboard-v1: {}",
        if protocols.zwp_virtual_keyboard {
            "available"
        } else {
            "not available"
        }
    );

    // Clipboard protocols
    println!("\nClipboard:");
    println!(
        "  ext-data-control-v1: {}",
        if protocols.ext_data_control {
            "available (preferred)"
        } else {
            "not available"
        }
    );
    println!(
        "  zwlr-data-control-manager-v1: {}",
        if protocols.wlr_data_control {
            "available (fallback)"
        } else {
            "not available"
        }
    );

    // Core
    println!("\nCore:");
    println!(
        "  wl_seat: {}",
        if protocols.seat {
            "available"
        } else {
            "not available"
        }
    );
    println!("  wl_output count: {}", protocols.output_count);

    // Summary
    println!("\n=== Capability Summary ===");
    println!(
        "  Capture:   {}",
        if protocols.has_capture() { "YES" } else { "NO" }
    );
    println!(
        "  Input:     {}",
        if protocols.has_input() { "YES" } else { "NO" }
    );
    println!(
        "  Clipboard: {}",
        if protocols.has_clipboard() {
            "YES"
        } else {
            "NO"
        }
    );

    // Discovered output sources
    let sources = wayland.state().get_sources();
    if sources.is_empty() {
        println!("\nNo output sources detected.");
    } else {
        println!("\nOutput Sources:");
        for source in &sources {
            println!(
                "  {} ({}): {}x{} @ {:.2} Hz",
                source.name,
                source.description,
                source.width,
                source.height,
                f64::from(source.refresh_rate) / 1000.0
            );
        }
    }

    Ok(())
}
