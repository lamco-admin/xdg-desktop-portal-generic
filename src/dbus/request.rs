//! Request interface implementation.
//!
//! The Request interface is used by xdg-desktop-portal to allow cancellation
//! of in-progress portal operations.
//!
//! **Note:** In the xdg-desktop-portal architecture, Request objects at
//! the `handle` paths are typically managed by the *frontend* daemon
//! (`xdg-desktop-portal`), not by backend implementations. The frontend
//! creates Request objects and monitors them for cancellation.
//!
//! This module is retained for use cases where the backend needs to
//! support Request cancellation directly (e.g., when showing a
//! permission dialog).

use std::sync::Arc;

use tokio::sync::Mutex;
use zbus::interface;

use crate::session::SessionManager;

/// Request interface for portal operations.
///
/// Each portal method call that may take time creates a Request object
/// that the caller can use to cancel the operation.
pub struct RequestInterface {
    /// Session manager for cleanup on cancel.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Handle of the session this request is for (if any).
    session_handle: Option<String>,
}

impl RequestInterface {
    /// Create a new request interface.
    pub fn new(session_manager: Arc<Mutex<SessionManager>>) -> Self {
        Self {
            session_manager,
            session_handle: None,
        }
    }

    /// Create a standalone request interface (no session association).
    ///
    /// Used for operations like Screenshot that don't belong to a session.
    /// The `Close` method will be a no-op.
    pub fn standalone() -> Self {
        Self {
            session_manager: Arc::new(Mutex::new(SessionManager::new())),
            session_handle: None,
        }
    }

    /// Create a new request interface for a specific session.
    pub fn for_session(
        session_manager: Arc<Mutex<SessionManager>>,
        session_handle: String,
    ) -> Self {
        Self {
            session_manager,
            session_handle: Some(session_handle),
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestInterface {
    /// Close the request.
    ///
    /// This cancels any in-progress operation and cleans up resources.
    async fn close(&self) {
        tracing::debug!("Request.Close called");

        // If this request is associated with a session, close it
        if let Some(session_handle) = &self.session_handle {
            let mut manager = self.session_manager.lock().await;
            if let Ok(handle) = zbus::zvariant::ObjectPath::try_from(session_handle.as_str()) {
                if let Some(session) = manager.close_session(&handle) {
                    tracing::info!(
                        session_id = %session.id,
                        "Session closed via Request.Close"
                    );
                }
            }
        }
    }
}
