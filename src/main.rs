//! XDG Desktop Portal backend for Wayland compositors.
//!
//! Standalone D-Bus service that connects to the compositor as a Wayland client.

use std::sync::Arc;

use anyhow::Result;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
use xdg_desktop_portal_generic::{
    pipewire::PipeWireManager,
    services::{
        capture::{create_capture_backend, CapturePreference},
        clipboard::{create_clipboard_backend, ClipboardPreference},
        input::{create_input_backend, InputBackendConfig},
    },
    wayland::WaylandConnection,
    PortalBackend,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::from_default_env()
                .add_directive("xdg_desktop_portal_generic=debug".parse()?),
        )
        .init();

    tracing::info!("Starting xdg-desktop-portal-generic");

    // Connect to compositor as a Wayland client
    let mut wayland = WaylandConnection::connect()?;
    let protocols = wayland.available_protocols().clone();
    let sources = wayland.state().get_sources();
    tracing::info!("Discovered {} output sources", sources.len());

    // Start PipeWire manager on a dedicated thread.
    // PipeWire starts BEFORE the Wayland event loop because the event loop
    // needs a PipeWire reference to deliver captured frames directly.
    let pipewire_manager = Arc::new(PipeWireManager::start()?);

    // Standalone mode: use env-based preferences (no server to pass hints)
    let capture_prefs = CapturePreference::from_env();

    // Configure ext-capture handshake timeout before spawning event loop
    if capture_prefs.handshake_timeout_ms > 0 {
        wayland.set_ext_capture_handshake_timeout(std::time::Duration::from_millis(
            capture_prefs.handshake_timeout_ms,
        ));
    }

    // Spawn the Wayland event loop on a dedicated thread.
    // This continuously dispatches Wayland events (screencopy frames,
    // output hotplug, data control) and updates the shared state.
    // The PipeWire manager is given to the event loop for frame delivery.
    let (
        wayland_stop,
        shared_wayland_state,
        capture_tx,
        clipboard_tx,
        shared_clipboard,
        _wayland_thread,
    ) = wayland.spawn_event_loop(Arc::clone(&pipewire_manager));

    // Create backends based on detected protocols
    let input_config = InputBackendConfig::from_env();
    let input_backend = create_input_backend(&input_config, &protocols)?;

    // Clone capture_tx before passing to backend — Screenshot needs its own sender
    let screenshot_capture_tx = capture_tx.clone();

    let capture_backend = create_capture_backend(
        &protocols,
        &capture_prefs,
        sources,
        Arc::clone(&pipewire_manager),
        capture_tx,
    )?;

    let clipboard_prefs = ClipboardPreference::from_env();
    let clipboard_backend =
        create_clipboard_backend(&protocols, &clipboard_prefs, clipboard_tx, shared_clipboard);

    // Create and run the portal backend
    let mut backend = PortalBackend::new(
        input_backend,
        capture_backend,
        clipboard_backend,
        Arc::clone(&pipewire_manager),
        protocols,
        screenshot_capture_tx,
    );
    backend.set_shared_wayland_state(shared_wayland_state);

    tracing::info!("Registering D-Bus interfaces...");
    backend.run().await?;

    // Clean shutdown
    wayland_stop.store(true, std::sync::atomic::Ordering::Relaxed);
    pipewire_manager.shutdown();

    Ok(())
}
