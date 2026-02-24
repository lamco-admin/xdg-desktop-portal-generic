//! Session D-Bus interface implementation.
//!
//! Implements `org.freedesktop.impl.portal.Session` version 1.
//!
//! Each portal session has a corresponding D-Bus object registered at the
//! session handle path. This allows clients and the frontend to close sessions
//! via the D-Bus interface.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{ObjectPath, Value},
};

use crate::{
    pipewire::PipeWireManager,
    services::{capture::CaptureBackend, input::InputBackend},
    session::SessionManager,
};

/// Session D-Bus interface.
///
/// Registered at the session handle path when a session is created.
/// Provides the `Close()` method and `Closed` signal per the
/// xdg-desktop-portal Session spec.
pub struct SessionInterface {
    /// Session manager for cleanup.
    session_manager: Arc<Mutex<SessionManager>>,
    /// The session handle path this interface is registered at.
    session_handle: ObjectPath<'static>,
    /// Input backend for destroying contexts on close.
    input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    /// Capture backend for destroying streams on close.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// PipeWire manager for destroying streams on close.
    pipewire_manager: Arc<PipeWireManager>,
}

impl SessionInterface {
    /// Create a new session interface.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        session_handle: ObjectPath<'static>,
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<PipeWireManager>,
    ) -> Self {
        Self {
            session_manager,
            session_handle,
            input_backend,
            capture_backend,
            pipewire_manager,
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionInterface {
    /// Close the session.
    ///
    /// Closes the session and emits the `Closed` signal. The session D-Bus
    /// object is removed from the bus after this call.
    async fn close(
        &self,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<()> {
        tracing::debug!(
            session_handle = %self.session_handle,
            "Session.Close called"
        );

        let mut manager = self.session_manager.lock().await;
        let closed = manager.close_session(&self.session_handle);
        drop(manager);

        // Perform full resource cleanup (same as monitor_client_disconnects)
        if let Some(session) = &closed {
            let session_id = session.id.to_string();

            // Destroy input contexts
            let mut input = self.input_backend.lock().await;
            let _ = input.destroy_context(&session_id);
            drop(input);

            // Destroy capture streams
            if !session.streams.is_empty() {
                let stream_ids = session.stream_ids();
                let mut capture = self.capture_backend.lock().await;
                let _ = capture.destroy_capture_session(&stream_ids);
                drop(capture);

                // Destroy PipeWire streams
                for node_id in stream_ids {
                    let _ = self.pipewire_manager.destroy_stream(node_id).await;
                }
            }
        }

        // Emit the Closed signal
        let details: HashMap<&str, Value<'_>> = HashMap::new();
        let _ = Self::closed(&emitter, details).await;

        // Remove this session object from the D-Bus bus
        let _ = server.remove::<Self, _>(&self.session_handle).await;

        tracing::info!(
            session_handle = %self.session_handle,
            "Session closed via D-Bus"
        );

        Ok(())
    }

    /// Signal emitted when the session is closed.
    #[zbus(signal)]
    async fn closed(
        emitter: &SignalEmitter<'_>,
        details: HashMap<&str, Value<'_>>,
    ) -> zbus::Result<()>;

    // === Properties ===

    /// Interface version.
    #[zbus(property)]
    async fn version(&self) -> u32 {
        1
    }
}
