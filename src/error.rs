//! Error types for the portal backend.

use std::io;

/// Errors that can occur in the portal backend.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PortalError {
    /// The session handle is invalid or malformed.
    #[error("Invalid session: {0}")]
    InvalidSession(String),

    /// The requested session does not exist.
    #[error("Session not found: {0}")]
    SessionNotFound(String),

    /// The session is in an invalid state for the operation.
    #[error("Session in invalid state: expected {expected}, got {actual}")]
    InvalidState {
        /// The expected state.
        expected: String,
        /// The actual state.
        actual: String,
    },

    /// The app has exceeded its session limit.
    #[error("Session limit exceeded for app {0}: max {1}")]
    SessionLimitExceeded(String, usize),

    /// Permission was denied for the operation.
    #[error("Permission denied for {0}")]
    PermissionDenied(String),

    /// The user cancelled the operation.
    #[error("User cancelled operation")]
    UserCancelled,

    /// Failed to create an EIS context.
    #[error("Failed to create EIS context: {0}")]
    EisCreationFailed(String),

    /// Failed to create a PipeWire stream.
    #[error("Failed to create PipeWire stream: {0}")]
    PipeWireCreationFailed(String),

    /// The requested source was not found.
    #[error("Source not found: {0}")]
    SourceNotFound(u32),

    /// A Wayland protocol error occurred.
    #[error("Wayland error: {0}")]
    Wayland(String),

    /// A D-Bus error occurred.
    #[error("D-Bus error: {0}")]
    DBus(#[from] zbus::Error),

    /// Invalid D-Bus method arguments.
    #[error("Invalid D-Bus argument: {0}")]
    InvalidArgument(String),

    /// An I/O error occurred.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Failed to pass a file descriptor.
    #[error("Failed to pass file descriptor: {0}")]
    FdPassingFailed(String),

    /// Clipboard access was not enabled for the session.
    #[error("Clipboard not enabled for session")]
    ClipboardNotEnabled,

    /// Clipboard data exceeds the size limit.
    #[error("Clipboard data too large: {0} bytes (max {1})")]
    ClipboardDataTooLarge(usize, usize),

    /// The requested MIME type is not supported.
    #[error("Unsupported MIME type: {0}")]
    UnsupportedMimeType(String),

    /// A PipeWire error occurred.
    #[error("PipeWire error: {0}")]
    PipeWire(String),

    /// A screencopy error occurred.
    #[error("Screencopy error: {0}")]
    Screencopy(String),

    /// A configuration error occurred.
    #[error("Configuration error: {0}")]
    Config(String),

    /// The rate limit was exceeded.
    #[error("Rate limit exceeded for {0}")]
    RateLimitExceeded(String),
}

/// Result type using [`PortalError`].
pub type Result<T> = std::result::Result<T, PortalError>;

/// Convert portal errors to D-Bus errors.
impl From<PortalError> for zbus::fdo::Error {
    fn from(error: PortalError) -> Self {
        match error {
            PortalError::PermissionDenied(_) | PortalError::UserCancelled => {
                zbus::fdo::Error::AccessDenied(error.to_string())
            }

            PortalError::InvalidArgument(_) => zbus::fdo::Error::InvalidArgs(error.to_string()),

            _ => zbus::fdo::Error::Failed(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_portal_error_display() {
        let err = PortalError::SessionNotFound("session-123".to_string());
        assert!(err.to_string().contains("session-123"));

        let err = PortalError::InvalidState {
            expected: "Started".to_string(),
            actual: "Init".to_string(),
        };
        assert!(err.to_string().contains("Started"));
        assert!(err.to_string().contains("Init"));
    }

    #[test]
    fn test_portal_error_to_dbus_error() {
        // Session errors map to Failed
        let err: zbus::fdo::Error = PortalError::SessionNotFound("test".to_string()).into();
        assert!(matches!(err, zbus::fdo::Error::Failed(_)));

        // Permission errors map to AccessDenied
        let err: zbus::fdo::Error = PortalError::PermissionDenied("test".to_string()).into();
        assert!(matches!(err, zbus::fdo::Error::AccessDenied(_)));

        let err: zbus::fdo::Error = PortalError::UserCancelled.into();
        assert!(matches!(err, zbus::fdo::Error::AccessDenied(_)));

        // Invalid arguments map to InvalidArgs
        let err: zbus::fdo::Error = PortalError::InvalidArgument("test".to_string()).into();
        assert!(matches!(err, zbus::fdo::Error::InvalidArgs(_)));
    }

    #[test]
    fn test_wayland_error() {
        let err = PortalError::Wayland("connection lost".to_string());
        assert!(err.to_string().contains("connection lost"));
    }
}
