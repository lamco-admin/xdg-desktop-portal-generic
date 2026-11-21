//! D-Bus interface implementations.
//!
//! This module contains the implementations of the portal D-Bus interfaces:
//! - `org.freedesktop.impl.portal.RemoteDesktop`
//! - `org.freedesktop.impl.portal.ScreenCast`
//! - `org.freedesktop.impl.portal.Clipboard`
//! - `org.freedesktop.impl.portal.Settings`
//! - `org.freedesktop.impl.portal.Screenshot`
//! - `org.freedesktop.impl.portal.InputCapture`

#[cfg(test)]
mod bus_tests;
mod clipboard;
mod input_capture;
mod remote_desktop;
mod request;
mod screencast;
mod screenshot;
mod session;
mod settings;

use std::collections::HashMap;

pub use clipboard::{
    ClipboardInterface, ClipboardSignal, PendingWriteEntry, PendingWrites, next_clipboard_serial,
};
pub use input_capture::{
    InputCaptureInterface, activated_options, deactivated_options, emit_zones_changed,
};
pub use remote_desktop::RemoteDesktopInterface;
pub use request::RequestInterface;
pub use screencast::ScreenCastInterface;
pub use screenshot::ScreenshotInterface;
pub use session::{SessionInterface, SessionResources};
pub(crate) use session::{end_session, expire_stale_sessions};
pub use settings::SettingsInterface;
use zbus::OwnedVariant;

/// Portal response codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u32)]
pub enum Response {
    /// Success.
    Success = 0,
    /// User cancelled.
    Cancelled = 1,
    /// Other error.
    Other = 2,
}

impl Response {
    /// Convert to D-Bus response code.
    #[must_use]
    pub fn to_u32(self) -> u32 {
        self as u32
    }
}

/// The unique bus name of the peer that sent a method call.
///
/// # Errors
///
/// Fails if the message carries no sender, which a message routed by a bus
/// daemon always does.
pub(crate) fn caller(header: &zbus::message::Header<'_>) -> zbus::fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))
}

/// Helper to create an empty results map.
#[must_use]
pub fn empty_results() -> HashMap<String, OwnedVariant> {
    HashMap::new()
}

/// Helper to extract a u32 from options.
#[must_use]
#[expect(clippy::implicit_hasher, reason = "only std HashMap used")]
#[expect(clippy::cast_sign_loss, reason = "D-Bus may encode u32 values as i32")]
pub fn get_option_u32(options: &HashMap<String, OwnedVariant>, key: &str) -> Option<u32> {
    options.get(key).and_then(|v| {
        // Try to get the value as u32
        if let Ok(val) = <u32 as TryFrom<&OwnedVariant>>::try_from(v) {
            return Some(val);
        }
        // Also try i32 and convert
        if let Ok(val) = <i32 as TryFrom<&OwnedVariant>>::try_from(v) {
            return Some(val as u32);
        }
        None
    })
}

/// Helper to extract a bool from options.
#[must_use]
#[expect(clippy::implicit_hasher, reason = "only std HashMap used")]
pub fn get_option_bool(options: &HashMap<String, OwnedVariant>, key: &str) -> Option<bool> {
    options
        .get(key)
        .and_then(|v| <bool as TryFrom<&OwnedVariant>>::try_from(v).ok())
}
