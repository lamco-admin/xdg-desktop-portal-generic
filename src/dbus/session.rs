//! Session D-Bus interface implementation.
//!
//! Implements `org.freedesktop.impl.portal.Session` version 1.
//!
//! Each portal session has a corresponding D-Bus object registered at the
//! session handle path. This allows clients and the frontend to close sessions
//! via the D-Bus interface.

use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
    time::Duration,
};

use tokio::sync::Mutex;
use zbus::{ObjectPath, Variant, interface, object_server::SignalEmitter};

use crate::{
    pipewire::PipeWireManager,
    services::{capture::CaptureBackend, input::InputBackend},
    session::{Session, SessionManager},
    wayland::InputCaptureCommand,
};

/// The backends that hold a session's resources outside the session manager.
///
/// Every path that ends a session (`Session.Close`, `Request.Close`, the
/// owner leaving the bus, stale-session expiry) releases through
/// [`SessionResources::release`], so none of them can leave a capture
/// running or an input context registered for a session that is gone.
#[derive(Clone)]
pub struct SessionResources {
    /// Input backend whose per-session contexts are destroyed.
    pub input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    /// Capture backend whose per-session captures are stopped.
    pub capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// PipeWire manager whose per-session streams are destroyed.
    pub pipewire_manager: Arc<PipeWireManager>,
    /// InputCapture barrier-surface command sender. `None` where nothing
    /// creates InputCapture sessions.
    pub input_capture_tx: Option<mpsc::Sender<InputCaptureCommand>>,
}

impl SessionResources {
    /// Bundle the backends a session's resources live in.
    pub fn new(
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<PipeWireManager>,
        input_capture_tx: Option<mpsc::Sender<InputCaptureCommand>>,
    ) -> Self {
        Self {
            input_backend,
            capture_backend,
            pipewire_manager,
            input_capture_tx,
        }
    }

    /// Stop the captures behind `stream_ids` and destroy their PipeWire
    /// streams.
    pub async fn release_streams(&self, stream_ids: Vec<u32>) {
        if stream_ids.is_empty() {
            return;
        }
        let mut capture = self.capture_backend.lock().await;
        let _ = capture.destroy_capture_session(&stream_ids);
        drop(capture);

        for node_id in stream_ids {
            let _ = self.pipewire_manager.destroy_stream(node_id).await;
        }
    }

    /// Release everything `session` holds in the backends: its input
    /// context, its captures and PipeWire streams, and any InputCapture
    /// barrier surfaces.
    ///
    /// The session must already have been taken out of the session manager.
    pub async fn release(&self, session: &Session) {
        let session_id = session.id.to_string();

        let mut input = self.input_backend.lock().await;
        let _ = input.destroy_context(&session_id);
        drop(input);

        self.release_streams(session.stream_ids()).await;

        if session.is_input_capture_session
            && let Some(tx) = &self.input_capture_tx
        {
            let _ = tx.send(InputCaptureCommand::DestroySession { session_id });
        }
    }
}

/// Finish ending a session the session manager has already given up.
///
/// Releases its backend resources, emits `Closed` on its object when
/// `emit_closed` is set (the backend is ending the session, so the frontend
/// has to be told), and removes the object from the bus.
pub(crate) async fn end_session(
    server: &zbus::ObjectServer,
    resources: &SessionResources,
    session: &Session,
    emit_closed: bool,
) {
    resources.release(session).await;

    if emit_closed && let Ok(iface) = server.interface::<_, SessionInterface>(&session.id).await {
        let details: HashMap<&str, Variant<'_>> = HashMap::new();
        let _ = SessionInterface::closed(iface.signal_emitter(), details).await;
    }

    let _ = server.remove::<SessionInterface, _>(&session.id).await;
}

/// Expire sessions that never started, with the same teardown as a close.
///
/// Returns how many sessions were ended. Started sessions are never expired
/// here: they last until `Session.Close` or until their owner leaves the bus.
pub(crate) async fn expire_stale_sessions(
    server: &zbus::ObjectServer,
    session_manager: &Mutex<SessionManager>,
    resources: &SessionResources,
    max_idle: Duration,
) -> usize {
    let stale = session_manager
        .lock()
        .await
        .cleanup_stale_sessions(max_idle);
    for session in &stale {
        tracing::info!(
            session_id = %session.id,
            app_id = %session.app_id,
            "Ending stale session"
        );
        end_session(server, resources, session, true).await;
    }
    stale.len()
}

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
    /// Backends to release the session's resources in on close.
    resources: SessionResources,
}

impl SessionInterface {
    /// Create a new session interface.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        session_handle: ObjectPath<'static>,
        resources: SessionResources,
    ) -> Self {
        Self {
            session_manager,
            session_handle,
            resources,
        }
    }
}

#[interface(
    name = "org.freedesktop.impl.portal.Session",
    introspection_docs = false
)]
impl SessionInterface {
    /// Close the session.
    ///
    /// Closes the session and emits the `Closed` signal. The session D-Bus
    /// object is removed from the bus after this call.
    async fn close(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<()> {
        let sender = super::caller(&header)?;
        tracing::debug!(
            session_handle = %self.session_handle,
            sender = %sender,
            "Session.Close called"
        );

        let mut manager = self.session_manager.lock().await;
        // Only the peer that created the session may close it. A session
        // already gone from the manager has nothing left to protect, so the
        // stale object below is removed whoever asks.
        if manager.get_session(&self.session_handle).is_some() {
            manager.session_for_caller(&self.session_handle, &sender)?;
        }
        let closed = manager.close_session(&self.session_handle);
        drop(manager);

        if let Some(session) = &closed {
            self.resources.release(session).await;
        }

        // Emit the Closed signal
        let details: HashMap<&str, Variant<'_>> = HashMap::new();
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
        details: HashMap<&str, Variant<'_>>,
    ) -> zbus::Result<()>;

    // === Properties ===

    /// Interface version.
    #[zbus(property, name = "version")]
    #[expect(
        clippy::unused_async_trait_impl,
        reason = "zbus::interface requires literal `async fn`; `-> impl Future` is rejected by the macro"
    )]
    async fn version(&self) -> u32 {
        1
    }
}
