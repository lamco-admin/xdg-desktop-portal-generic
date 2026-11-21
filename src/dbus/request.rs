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

use crate::{error::PortalError, session::SessionManager};

/// Request interface for portal operations.
///
/// Each portal method call that may take time creates a Request object
/// that the caller can use to cancel the operation.
pub struct RequestInterface {
    /// Session manager for cleanup on cancel.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Handle of the session this request is for (if any).
    session_handle: Option<String>,
    /// Unique bus name of the peer whose method call created this request.
    /// Only that peer may close it. `None` for standalone requests, whose
    /// `Close` does nothing.
    owner: Option<String>,
}

impl RequestInterface {
    /// Create a new request interface for a call made by `owner`.
    pub fn new(session_manager: Arc<Mutex<SessionManager>>, owner: String) -> Self {
        Self {
            session_manager,
            session_handle: None,
            owner: Some(owner),
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
            owner: None,
        }
    }

    /// Create a new request interface for a call `owner` made on a session.
    pub fn for_session(
        session_manager: Arc<Mutex<SessionManager>>,
        session_handle: String,
        owner: String,
    ) -> Self {
        Self {
            session_manager,
            session_handle: Some(session_handle),
            owner: Some(owner),
        }
    }
}

#[interface(
    name = "org.freedesktop.impl.portal.Request",
    introspection_docs = false
)]
impl RequestInterface {
    /// Close the request.
    ///
    /// This cancels any in-progress operation and cleans up resources.
    async fn close(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let Some(owner) = &self.owner else {
            tracing::debug!("Request.Close called on a standalone request");
            return Ok(());
        };
        let sender = super::caller(&header)?;
        tracing::debug!(sender = %sender, "Request.Close called");

        if sender != *owner {
            tracing::warn!(
                expected_sender = %owner,
                actual_sender = %sender,
                "Request.Close from a peer that did not make the request"
            );
            return Err(PortalError::PermissionDenied(format!(
                "Sender {sender} tried to close a request made by {owner}"
            ))
            .into());
        }

        // If this request is associated with a session, close it
        if let Some(session_handle) = &self.session_handle {
            let mut manager = self.session_manager.lock().await;
            if let Ok(handle) = zbus::ObjectPath::try_from(session_handle.as_str())
                && manager.session_for_caller(&handle, &sender).is_ok()
                && let Some(session) = manager.close_session(&handle)
            {
                tracing::info!(
                    session_id = %session.id,
                    "Session closed via Request.Close"
                );
            }
        }

        Ok(())
    }
}
