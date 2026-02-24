//! Demonstrates the portal session lifecycle.
//!
//! Shows how to create sessions, manage their state transitions,
//! and enforce session limits using the SessionManager API.
//! This example runs without a compositor connection.
//!
//! Run with: `cargo run --example session_lifecycle`

use xdg_desktop_portal_generic::{
    session::{PersistMode, SessionManager, SessionManagerConfig},
    types::DeviceTypes,
};

fn main() -> anyhow::Result<()> {
    // Custom configuration: limit to 3 sessions per app
    let config = SessionManagerConfig {
        max_sessions_per_app: 3,
    };
    let mut manager = SessionManager::with_config(config);

    // Generate a unique session handle (D-Bus object path)
    let handle = SessionManager::generate_session_handle();
    println!("Generated session handle: {handle}");

    // Create a session owned by an application
    let app_id = "org.example.rdp-client";
    let sender = ":1.42"; // D-Bus unique name of the client
    let session = manager.create_session(
        handle.clone(),
        sender.to_string(),
        app_id.to_string(),
        PersistMode::None,
    )?;
    println!("Session created for app: {}", session.app_id);

    // Select devices for this session (keyboard + pointer)
    let devices = DeviceTypes {
        keyboard: true,
        pointer: true,
        touchscreen: false,
    };
    if let Some(session) = manager.get_session_mut(&handle) {
        session.select_devices(devices)?;
        println!(
            "Selected devices: keyboard={}, pointer={}",
            devices.keyboard, devices.pointer
        );
    }

    // Start the session (requires at least one selected source or device)
    // Pass empty streams since we have no real PipeWire streams in this example
    if let Some(session) = manager.get_session_mut(&handle) {
        session.start(vec![])?;
        println!("Session state: {}", session.state);
    }

    // Query session state
    if let Some(session) = manager.get_session(&handle) {
        println!("Session {} is in state: {}", session.id, session.state);
        println!("  App ID: {}", session.app_id);
        println!(
            "  Devices: keyboard={}, pointer={}",
            session.device_types.keyboard, session.device_types.pointer
        );
    }

    // Close the session
    if let Some(closed) = manager.close_session(&handle) {
        println!("Session closed (was in state: {})", closed.state);
    }

    // Demonstrate session limits
    println!("\n--- Session Limit Demo ---");
    for i in 0..4 {
        let h = SessionManager::generate_session_handle();
        match manager.create_session(h, sender.to_string(), app_id.to_string(), PersistMode::None) {
            Ok(_) => println!("Session {i}: created"),
            Err(e) => println!("Session {i}: rejected ({e})"),
        }
    }

    Ok(())
}
