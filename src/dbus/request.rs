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
use zbus::{ObjectPath, interface};

use super::SessionResources;
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
    /// Backends to release the session's resources in when `Close` ends
    /// the session. Set exactly when `session_handle` is.
    resources: Option<SessionResources>,
}

impl RequestInterface {
    /// Create a new request interface for a call made by `owner`.
    pub fn new(session_manager: Arc<Mutex<SessionManager>>, owner: String) -> Self {
        Self {
            session_manager,
            session_handle: None,
            owner: Some(owner),
            resources: None,
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
            resources: None,
        }
    }

    /// Create a new request interface for a call `owner` made on a session.
    ///
    /// Closing the request ends the session, releasing its resources in
    /// `resources`.
    pub fn for_session(
        session_manager: Arc<Mutex<SessionManager>>,
        session_handle: String,
        owner: String,
        resources: SessionResources,
    ) -> Self {
        Self {
            session_manager,
            session_handle: Some(session_handle),
            owner: Some(owner),
            resources: Some(resources),
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
        #[zbus(object_server)] server: &zbus::ObjectServer,
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

        // If this request is associated with a session, end it the same way
        // Session.Close does.
        let (Some(session_handle), Some(resources)) = (&self.session_handle, &self.resources)
        else {
            return Ok(());
        };
        let Ok(handle) = ObjectPath::try_from(session_handle.as_str()) else {
            return Ok(());
        };
        let mut manager = self.session_manager.lock().await;
        if manager.get_session(&handle).is_none() {
            return Ok(());
        }
        manager.session_for_caller(&handle, &sender)?;
        let closed = manager.close_session(&handle);
        drop(manager);

        if let Some(session) = closed {
            super::end_session(server, resources, &session, true).await;
            tracing::info!(
                session_id = %session.id,
                "Session closed via Request.Close"
            );
        }

        Ok(())
    }
}

/// Keeps a `Request` object registered for the duration of one method call.
///
/// [`RequestGuard::register`] refuses a handle that already has a `Request`
/// on it, so a call can't adopt or replace another call's request. The
/// object is removed by [`RequestGuard::finish`] or, on an early return,
/// when the guard is dropped.
pub(crate) struct RequestGuard {
    server: zbus::ObjectServer,
    handle: ObjectPath<'static>,
    armed: bool,
}

impl RequestGuard {
    /// Register `request` at `handle`.
    ///
    /// # Errors
    ///
    /// `InvalidArgs` if a `Request` is already registered at `handle`, and
    /// `Failed` if the object server refuses the path.
    pub(crate) async fn register(
        server: &zbus::ObjectServer,
        handle: &ObjectPath<'_>,
        request: RequestInterface,
    ) -> zbus::fdo::Result<Self> {
        match server.at(handle, request).await {
            Ok(None) => Ok(Self {
                server: server.clone(),
                handle: handle.clone().into_owned(),
                armed: true,
            }),
            Ok(Some(_)) => {
                tracing::warn!(handle = %handle, "Request handle is already in use");
                Err(zbus::fdo::Error::InvalidArgs(format!(
                    "Request handle {handle} is already in use"
                )))
            }
            Err(e) => Err(zbus::fdo::Error::Failed(format!(
                "Failed to register Request at {handle}: {e}"
            ))),
        }
    }

    /// Remove the `Request` object now that the call has finished.
    pub(crate) async fn finish(mut self) {
        self.armed = false;
        let _ = self
            .server
            .remove::<RequestInterface, _>(&self.handle)
            .await;
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Removal is async; hand it to the runtime the call ran on.
        let server = self.server.clone();
        let handle = self.handle.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn(async move {
                    let _ = server.remove::<RequestInterface, _>(&handle).await;
                });
            }
            Err(_) => {
                tracing::warn!(handle = %handle, "No runtime to remove an abandoned Request object");
            }
        }
    }
}
