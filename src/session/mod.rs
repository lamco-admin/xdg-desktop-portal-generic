//! Session management for portal sessions.
//!
//! This module handles the lifecycle of portal sessions, including creation,
//! state transitions, and cleanup.

mod manager;
mod state;

pub use manager::{
    PERSISTENT_RESTORE_GRANT_LIFETIME, SessionManager, SessionManagerConfig,
    TRANSIENT_RESTORE_GRANT_LIFETIME,
};
pub use state::{PersistMode, RestoreData, Session, SessionState};
