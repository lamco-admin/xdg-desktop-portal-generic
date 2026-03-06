//! Backend services for capture, clipboard, and input injection.
//!
//! Each feature domain has multiple protocol backends:
//!
//! - **[`capture`]**: Screen capture via `ext-image-copy-capture` or `wlr-screencopy`
//! - **[`clipboard`]**: Clipboard via `ext-data-control` or `wlr-data-control`
//! - **[`input`]**: Input injection via wlr-virtual-pointer/keyboard or EIS bridge
//!
//! # Input Protocol Selection
//!
//! ```ignore
//! let config = InputBackendConfig::from_env();
//! let backend = create_input_backend(&config, &protocols)?;
//! ```

pub mod capture;
pub mod clipboard;
pub mod input;

// Re-export input backend types
pub use input::{
    create_input_backend, AvailableProtocols, EisBridgeBackend, EisConfig, EisSession,
    InputBackend, InputBackendConfig, InputProtocol, ProtocolDetector, WlrConfig, WlrInputBackend,
};
