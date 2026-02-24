//! Input backend configuration and protocol selection.
//!
//! Shows how to configure input protocol preferences and how the
//! protocol detector selects between EIS and wlr-virtual-input
//! based on compositor capabilities.
//!
//! Run with: `cargo run --example input_config`

use xdg_desktop_portal_generic::services::input::{
    AvailableProtocols, InputBackendConfig, InputProtocol, ProtocolDetector,
};

fn main() {
    // Default configuration prefers EIS with wlr fallback
    let default_config = InputBackendConfig::default();
    println!("Default config:");
    println!("  Preferred: {}", default_config.preferred);
    println!("  Allow fallback: {}", default_config.allow_fallback);

    // Configuration from environment variables:
    //   XDP_GENERIC_INPUT_PROTOCOL=eis|wlr
    //   XDP_GENERIC_INPUT_NO_FALLBACK=1
    //   XDP_GENERIC_EIS_SOCKET=/run/user/1000/eis-custom
    let env_config = InputBackendConfig::from_env();
    println!("\nFrom environment:");
    println!("  Preferred: {}", env_config.preferred);
    println!("  Allow fallback: {}", env_config.allow_fallback);
    println!("  EIS socket: {:?}", env_config.eis.socket_path);

    // Simulate protocol detection with different compositor capabilities
    println!("\n--- Protocol Selection Scenarios ---\n");

    // Scenario 1: Sway (wlr protocols only)
    let sway_protocols = AvailableProtocols {
        eis: false,
        wlr_virtual_input: true,
    };
    let selected = ProtocolDetector::select(&default_config, &sway_protocols);
    println!(
        "Sway (wlr only): {}",
        match selected {
            Ok(p) => p.to_string(),
            Err(e) => format!("error: {e}"),
        }
    );

    // Scenario 2: COSMIC (both EIS and wlr available)
    let cosmic_protocols = AvailableProtocols {
        eis: true,
        wlr_virtual_input: true,
    };
    let selected = ProtocolDetector::select(&default_config, &cosmic_protocols);
    println!(
        "COSMIC (both): {}",
        match selected {
            Ok(p) => p.to_string(),
            Err(e) => format!("error: {e}"),
        }
    );

    // Scenario 3: Force wlr, no fallback
    let strict_wlr = InputBackendConfig {
        preferred: InputProtocol::WlrVirtualInput,
        allow_fallback: false,
        ..InputBackendConfig::default()
    };
    let no_wlr = AvailableProtocols {
        eis: true,
        wlr_virtual_input: false,
    };
    let selected = ProtocolDetector::select(&strict_wlr, &no_wlr);
    println!(
        "Forced wlr, no fallback, wlr unavailable: {}",
        match selected {
            Ok(p) => p.to_string(),
            Err(e) => format!("error: {e}"),
        }
    );
}
