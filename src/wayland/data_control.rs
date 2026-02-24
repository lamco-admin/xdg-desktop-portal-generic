//! Data control clipboard state and protocol management.
//!
//! This module manages the clipboard integration via Wayland data control
//! protocols (`ext-data-control-v1` or `zwlr-data-control-unstable-v1`).
//!
//! # Architecture
//!
//! The clipboard operates across two threads:
//!
//! - **Wayland event loop thread**: Owns protocol objects, receives events
//!   (selection changes, source send requests), and creates sources.
//! - **D-Bus/tokio thread**: Receives clipboard commands from portal clients
//!   and reads selection data.
//!
//! Communication uses:
//! - `mpsc::Sender<ClipboardCommand>` — D-Bus → event loop
//! - `Arc<Mutex<SharedClipboardState>>` — event loop → D-Bus (selection state)
//!
//! # Flows
//!
//! **SetSelection (portal → compositor):**
//! 1. D-Bus client calls `SetSelection(mime_types)`
//! 2. Backend sends `SetSelection` command to event loop
//! 3. Event loop creates data control source, advertises MIME types
//! 4. Event loop calls `device.set_selection(source)`
//! 5. When compositor's `send` event arrives, writes cached data to fd
//!
//! **Selection changed (compositor → portal):**
//! 1. Compositor sends `data_offer` → `offer` × N → `selection` events
//! 2. Event loop records MIME types in shared clipboard state
//! 3. Calls change notification callback → D-Bus emits `SelectionOwnerChanged`
//!
//! **SelectionRead (portal reads compositor clipboard):**
//! 1. D-Bus client calls `SelectionRead(mime_type)`
//! 2. Backend creates pipe, sends `ReceiveFromOffer` command
//! 3. Event loop calls `offer.receive(mime_type, write_fd)`
//! 4. Backend reads from read_fd, returns data

use std::{
    collections::HashMap,
    os::unix::io::OwnedFd,
    sync::{Arc, Mutex},
};

use wayland_client::{protocol::wl_seat::WlSeat, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::ExtDataControlDeviceV1,
    ext_data_control_manager_v1::ExtDataControlManagerV1,
    ext_data_control_offer_v1::ExtDataControlOfferV1,
    ext_data_control_source_v1::ExtDataControlSourceV1,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::ZwlrDataControlDeviceV1,
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::ZwlrDataControlOfferV1,
    zwlr_data_control_source_v1::ZwlrDataControlSourceV1,
};

use super::dispatch::WaylandState;

// === Protocol object enums ===
// These wrap both ext and wlr variants so DataControlState can work
// with either protocol transparently.

/// Data control manager (either ext or wlr).
#[non_exhaustive]
pub enum DataControlManager {
    /// ext-data-control-v1 manager.
    Ext(ExtDataControlManagerV1),
    /// wlr-data-control-unstable-v1 manager.
    Wlr(ZwlrDataControlManagerV1),
}

/// Data control device (either ext or wlr).
#[non_exhaustive]
pub enum DataControlDevice {
    /// ext-data-control-v1 device.
    Ext(ExtDataControlDeviceV1),
    /// wlr-data-control-unstable-v1 device.
    Wlr(ZwlrDataControlDeviceV1),
}

/// Data control offer (either ext or wlr).
#[non_exhaustive]
pub enum DataControlOffer {
    /// ext-data-control-v1 offer.
    Ext(ExtDataControlOfferV1),
    /// wlr-data-control-unstable-v1 offer.
    Wlr(ZwlrDataControlOfferV1),
}

impl DataControlOffer {
    /// Request data transfer for a MIME type.
    ///
    /// Tells the source client to write data to the provided fd.
    pub fn receive(&self, mime_type: &str, fd: &OwnedFd) {
        use std::os::unix::io::AsFd;
        match self {
            DataControlOffer::Ext(offer) => offer.receive(mime_type.to_string(), fd.as_fd()),
            DataControlOffer::Wlr(offer) => offer.receive(mime_type.to_string(), fd.as_fd()),
        }
    }

    /// Destroy this offer.
    pub fn destroy(&self) {
        match self {
            DataControlOffer::Ext(offer) => offer.destroy(),
            DataControlOffer::Wlr(offer) => offer.destroy(),
        }
    }
}

/// Data control source (either ext or wlr).
enum DataControlSource {
    /// ext-data-control-v1 source.
    Ext(ExtDataControlSourceV1),
    /// wlr-data-control-unstable-v1 source.
    Wlr(ZwlrDataControlSourceV1),
}

impl DataControlSource {
    /// Advertise a MIME type on this source.
    fn offer(&self, mime_type: &str) {
        match self {
            DataControlSource::Ext(source) => source.offer(mime_type.to_string()),
            DataControlSource::Wlr(source) => source.offer(mime_type.to_string()),
        }
    }

    /// Destroy this source.
    fn destroy(&self) {
        match self {
            DataControlSource::Ext(source) => source.destroy(),
            DataControlSource::Wlr(source) => source.destroy(),
        }
    }
}

// === Commands and shared state ===

/// Commands sent from clipboard backends to the Wayland event loop thread.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClipboardCommand {
    /// Set the clipboard selection on the compositor.
    ///
    /// Creates a data control source with the offered MIME types and
    /// stores the data for responding to `send` events.
    SetSelection {
        /// MIME types to advertise.
        mime_types: Vec<String>,
        /// Data for each MIME type (written to fd on `send` event).
        data: HashMap<String, Vec<u8>>,
    },
    /// Receive clipboard data from the current compositor offer.
    ///
    /// Calls `offer.receive(mime_type, fd)` on the event loop thread.
    /// The caller reads from the other end of the pipe.
    ReceiveFromOffer {
        /// MIME type to request.
        mime_type: String,
        /// Write end of the pipe (compositor writes data here).
        fd: OwnedFd,
    },
}

/// Shared clipboard state readable from any thread.
///
/// Updated by the event loop thread when the compositor's selection changes.
/// Read by clipboard backends to report current state.
#[derive(Default)]
pub struct SharedClipboardState {
    /// MIME types of the current compositor selection.
    pub mime_types: Vec<String>,
    /// Serial number, incremented on each selection change.
    pub serial: u32,
    /// Change notification callback.
    ///
    /// Called on the event loop thread when selection changes.
    /// Typically captures a tokio channel sender for async notification.
    pub on_change: Option<Arc<dyn Fn(Vec<String>) + Send + Sync>>,
}

// === Data control state ===

/// Accumulated MIME types for a pending data offer.
///
/// Between `data_offer` and `selection` events, the compositor sends
/// `offer` events with MIME types. We collect them here.
#[derive(Default)]
struct PendingOffer {
    /// MIME types accumulated from `offer` events.
    mime_types: Vec<String>,
}

/// Central data control state, stored in [`WaylandState`].
///
/// Manages the lifecycle of data control protocol objects and routes
/// events to the shared clipboard state.
pub struct DataControlState {
    /// The data control manager global.
    pub manager: Option<DataControlManager>,
    /// The data control device (per-seat).
    pub device: Option<DataControlDevice>,
    /// The current selection offer from the compositor.
    current_offer: Option<DataControlOffer>,
    /// The data source we created for SetSelection (if any).
    current_source: Option<DataControlSource>,
    /// Data cached for our source's `send` events.
    source_data: HashMap<String, Vec<u8>>,
    /// Pending offer being built up (between data_offer and selection events).
    pending_offer: Option<(DataControlOffer, PendingOffer)>,
    /// Shared clipboard state for cross-thread access.
    pub shared_state: Arc<Mutex<SharedClipboardState>>,
}

impl Default for DataControlState {
    fn default() -> Self {
        Self {
            manager: None,
            device: None,
            current_offer: None,
            current_source: None,
            source_data: HashMap::new(),
            pending_offer: None,
            shared_state: Arc::new(Mutex::new(SharedClipboardState::default())),
        }
    }
}

impl DataControlState {
    /// Create a data control device from the manager and seat.
    ///
    /// Must be called after both the manager and seat are bound.
    pub fn create_device(&mut self, seat: &WlSeat, qh: &QueueHandle<WaylandState>) {
        let device = match &self.manager {
            Some(DataControlManager::Ext(mgr)) => {
                DataControlDevice::Ext(mgr.get_data_device(seat, qh, ()))
            }
            Some(DataControlManager::Wlr(mgr)) => {
                DataControlDevice::Wlr(mgr.get_data_device(seat, qh, ()))
            }
            None => {
                tracing::error!("Cannot create data control device: manager not bound");
                return;
            }
        };

        tracing::debug!("Created data control device");
        self.device = Some(device);
    }

    /// Handle a `data_offer` event from the device.
    ///
    /// A new offer is being introduced. Store it and start collecting
    /// MIME types from subsequent `offer` events.
    pub fn on_data_offer_ext(&mut self, offer: ExtDataControlOfferV1) {
        self.set_pending_offer(DataControlOffer::Ext(offer));
    }

    /// Handle a `data_offer` event from the device (wlr variant).
    pub fn on_data_offer_wlr(&mut self, offer: ZwlrDataControlOfferV1) {
        self.set_pending_offer(DataControlOffer::Wlr(offer));
    }

    fn set_pending_offer(&mut self, offer: DataControlOffer) {
        // Destroy any previous pending offer that wasn't used
        if let Some((old_offer, _)) = self.pending_offer.take() {
            old_offer.destroy();
        }
        self.pending_offer = Some((offer, PendingOffer::default()));
    }

    /// Handle an `offer` event on a data offer (MIME type offered).
    pub fn on_offer_mime_type(&mut self, mime_type: String) {
        if let Some((_, ref mut pending)) = self.pending_offer {
            pending.mime_types.push(mime_type);
        }
    }

    /// Handle the `selection` event from the device.
    ///
    /// The compositor's selection has changed. The pending offer
    /// (with accumulated MIME types) becomes the current offer.
    pub fn on_selection(&mut self) {
        // Destroy the old current offer
        if let Some(old) = self.current_offer.take() {
            old.destroy();
        }

        // Promote the pending offer to current
        let mime_types = if let Some((offer, pending)) = self.pending_offer.take() {
            let types = pending.mime_types;
            self.current_offer = Some(offer);
            types
        } else {
            // NULL selection (clipboard cleared)
            Vec::new()
        };

        tracing::debug!(
            mime_types = ?mime_types,
            "Compositor selection changed"
        );

        // Update shared state and notify
        if let Ok(mut shared) = self.shared_state.lock() {
            shared.serial += 1;
            shared.mime_types.clone_from(&mime_types);

            if let Some(ref callback) = shared.on_change {
                callback(mime_types);
            }
        }
    }

    /// Handle the `selection` event with a NULL offer (selection cleared).
    pub fn on_selection_cleared(&mut self) {
        if let Some(old) = self.current_offer.take() {
            old.destroy();
        }
        // Clear any pending offer too
        if let Some((offer, _)) = self.pending_offer.take() {
            offer.destroy();
        }

        tracing::debug!("Compositor selection cleared");

        if let Ok(mut shared) = self.shared_state.lock() {
            shared.serial += 1;
            shared.mime_types.clear();

            if let Some(ref callback) = shared.on_change {
                callback(Vec::new());
            }
        }
    }

    /// Handle a `send` event on our data source.
    ///
    /// The compositor (or another client pasting) wants data in the
    /// specified MIME type. Write it to the provided fd.
    pub fn on_source_send(&self, mime_type: &str, fd: OwnedFd) {
        use std::io::Write;

        if let Some(data) = self.source_data.get(mime_type) {
            let mut file = fd_to_file(fd);
            if let Err(e) = file.write_all(data) {
                tracing::error!(
                    mime_type,
                    error = %e,
                    "Failed to write clipboard data for send event"
                );
            }
            // File (and fd) is dropped/closed here
        } else {
            tracing::warn!(mime_type, "Source send event for unknown MIME type");
            // fd is dropped/closed here, signaling no data
        }
    }

    /// Handle the `cancelled` event on our data source.
    ///
    /// Our source has been replaced by another. Clean up.
    pub fn on_source_cancelled(&mut self) {
        tracing::debug!("Data control source cancelled");
        if let Some(source) = self.current_source.take() {
            source.destroy();
        }
        self.source_data.clear();
    }

    /// Handle the `finished` event on the device.
    ///
    /// The data control device is no longer valid.
    pub fn on_device_finished(&mut self) {
        tracing::debug!("Data control device finished");
        self.device = None;

        if let Some(offer) = self.current_offer.take() {
            offer.destroy();
        }
        if let Some((offer, _)) = self.pending_offer.take() {
            offer.destroy();
        }
        if let Some(source) = self.current_source.take() {
            source.destroy();
        }
        self.source_data.clear();
    }

    /// Process a SetSelection command.
    ///
    /// Creates a new data control source, advertises MIME types, and
    /// sets it as the selection on the device.
    pub fn set_selection(
        &mut self,
        mime_types: &[String],
        data: HashMap<String, Vec<u8>>,
        qh: &QueueHandle<WaylandState>,
    ) {
        // Destroy previous source
        if let Some(source) = self.current_source.take() {
            source.destroy();
        }

        let new_source = match &self.manager {
            Some(DataControlManager::Ext(mgr)) => {
                DataControlSource::Ext(mgr.create_data_source(qh, ()))
            }
            Some(DataControlManager::Wlr(mgr)) => {
                DataControlSource::Wlr(mgr.create_data_source(qh, ()))
            }
            None => {
                tracing::error!("Cannot set selection: manager not bound");
                return;
            }
        };

        // Advertise MIME types
        for mime_type in mime_types {
            new_source.offer(mime_type);
        }

        // Set as selection on the device
        match (&self.device, &new_source) {
            (Some(DataControlDevice::Ext(dev)), DataControlSource::Ext(src)) => {
                dev.set_selection(Some(src));
            }
            (Some(DataControlDevice::Wlr(dev)), DataControlSource::Wlr(src)) => {
                dev.set_selection(Some(src));
            }
            _ => {
                tracing::error!(
                    "Cannot set selection: device/source protocol mismatch or no device"
                );
                new_source.destroy();
                return;
            }
        }

        tracing::debug!(
            mime_types = ?mime_types,
            "Set clipboard selection on compositor"
        );

        self.source_data = data;
        self.current_source = Some(new_source);
    }

    /// Process a ReceiveFromOffer command.
    ///
    /// Calls `offer.receive(mime_type, fd)` to request data from the
    /// compositor. The caller reads from the other end of the pipe.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "OwnedFd must be owned so it is dropped after the receive call"
    )]
    pub fn receive_from_offer(&self, mime_type: &str, fd: OwnedFd) {
        match &self.current_offer {
            Some(offer) => {
                offer.receive(mime_type, &fd);
                tracing::debug!(mime_type, "Requested clipboard data from compositor offer");
            }
            None => {
                tracing::warn!(mime_type, "ReceiveFromOffer but no current offer");
                // fd is dropped, closing the pipe — caller will get EOF
            }
        }
    }
}

/// Convert an OwnedFd to a File, taking ownership.
#[expect(
    unsafe_code,
    reason = "from_raw_fd requires unsafe to take ownership of the file descriptor"
)]
fn fd_to_file(fd: OwnedFd) -> std::fs::File {
    use std::os::unix::io::{AsRawFd, FromRawFd};
    let file = unsafe { std::fs::File::from_raw_fd(fd.as_raw_fd()) };
    std::mem::forget(fd);
    file
}

#[cfg(test)]
#[expect(
    unsafe_code,
    reason = "tests use from_raw_fd to create OwnedFd from pipe file descriptors"
)]
mod tests {
    use std::os::unix::io::FromRawFd;

    use super::*;

    #[test]
    fn test_shared_clipboard_state_default() {
        let state = SharedClipboardState::default();
        assert!(state.mime_types.is_empty());
        assert_eq!(state.serial, 0);
        assert!(state.on_change.is_none());
    }

    #[test]
    fn test_data_control_state_default() {
        let state = DataControlState::default();
        assert!(state.manager.is_none());
        assert!(state.device.is_none());
        assert!(state.current_offer.is_none());
        assert!(state.current_source.is_none());
        assert!(state.source_data.is_empty());
        assert!(state.pending_offer.is_none());
    }

    #[test]
    fn test_on_selection_cleared() {
        let mut state = DataControlState::default();

        // Register a callback to verify it's called
        let called = Arc::new(Mutex::new(false));
        let called_clone = Arc::clone(&called);
        if let Ok(mut shared) = state.shared_state.lock() {
            shared.on_change = Some(Arc::new(move |types: Vec<String>| {
                assert!(types.is_empty());
                *called_clone.lock().unwrap() = true;
            }));
        }

        state.on_selection_cleared();

        assert!(*called.lock().unwrap());
        assert!(state.shared_state.lock().unwrap().mime_types.is_empty());
        assert_eq!(state.shared_state.lock().unwrap().serial, 1);
    }

    #[test]
    fn test_on_source_cancelled() {
        let mut state = DataControlState::default();
        state
            .source_data
            .insert("text/plain".to_string(), b"hello".to_vec());

        state.on_source_cancelled();

        assert!(state.current_source.is_none());
        assert!(state.source_data.is_empty());
    }

    #[test]
    fn test_on_device_finished() {
        let mut state = DataControlState::default();
        state
            .source_data
            .insert("text/plain".to_string(), b"hello".to_vec());

        state.on_device_finished();

        assert!(state.device.is_none());
        assert!(state.current_offer.is_none());
        assert!(state.current_source.is_none());
        assert!(state.source_data.is_empty());
    }

    #[test]
    fn test_on_offer_mime_type_without_pending() {
        let mut state = DataControlState::default();
        // Should not panic when there's no pending offer
        state.on_offer_mime_type("text/plain".to_string());
    }

    #[test]
    fn test_on_source_send_unknown_mime() {
        let state = DataControlState::default();
        // Should not panic for unknown MIME type
        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        std::mem::forget(read_fd); // leak the read end for the test
        let owned_fd =
            unsafe { OwnedFd::from_raw_fd(std::os::unix::io::AsRawFd::as_raw_fd(&write_fd)) };
        std::mem::forget(write_fd);
        state.on_source_send("text/unknown", owned_fd);
    }

    #[test]
    fn test_on_source_send_writes_data() {
        let mut state = DataControlState::default();
        state
            .source_data
            .insert("text/plain".to_string(), b"hello world".to_vec());

        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let write_owned =
            unsafe { OwnedFd::from_raw_fd(std::os::unix::io::AsRawFd::as_raw_fd(&write_fd)) };
        std::mem::forget(write_fd);

        state.on_source_send("text/plain", write_owned);

        // Read from the pipe
        use std::io::Read;
        let mut file =
            unsafe { std::fs::File::from_raw_fd(std::os::unix::io::AsRawFd::as_raw_fd(&read_fd)) };
        std::mem::forget(read_fd);
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello world");
    }
}
