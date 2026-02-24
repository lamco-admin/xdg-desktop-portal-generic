//! Session management for portal sessions.
//!
//! This module handles the lifecycle of portal sessions, including creation,
//! state transitions, and cleanup.

mod manager;
mod state;

pub use manager::{SessionManager, SessionManagerConfig};
pub use state::{PersistMode, RestoreData, Session, SessionState};
