//! The data-control protocol state machine.
//!
//! [`State`] owns the protocol objects (manager, device, current offer, our
//! source) and turns compositor events into changes of the shared selection
//! state. It is deliberately free of any event loop: the worker in
//! [`crate::worker`] feeds it events and commands.
//!
//! # Flows
//!
//! **Set selection (client to compositor):**
//! 1. A [`Command::SetSelection`] arrives with the MIME types and any data.
//! 2. A data-control source is created, the types are advertised on it and it
//!    is made the device's selection; the previous source is destroyed only
//!    afterwards, so the clipboard is never empty in between.
//! 3. A `send` event for an advertised type writes the cached data to the
//!    requested file descriptor on a worker thread. With no cached data and an
//!    `on_transfer` callback set, the paste is held until
//!    [`Command::CompleteTransfer`] answers it (delayed rendering).
//!
//! **Selection changed (compositor to client):**
//! 1. `data_offer`, then one `offer` per MIME type, then `selection`.
//! 2. The types are stored in [`Shared`] and the change callback is called,
//!    unless the selection is the one this client set itself.
//!
//! **Read (client reads the compositor's selection):**
//! [`Command::ReceiveFromOffer`] calls `offer.receive(mime, fd)`; the caller
//! reads the other end of the pipe.

use std::{
    collections::HashMap,
    os::unix::io::OwnedFd,
    sync::{Arc, Mutex},
};

use wayland_client::{Dispatch, QueueHandle, protocol::wl_seat::WlSeat};
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

// === Protocol object enums ===
// These wrap both ext and wlr variants so State can work
// with either protocol transparently.

/// Data control manager (either ext or wlr).
pub(crate) enum DataControlManager {
    /// ext-data-control-v1 manager.
    Ext(ExtDataControlManagerV1),
    /// wlr-data-control-unstable-v1 manager.
    Wlr(ZwlrDataControlManagerV1),
}

/// Data control device (either ext or wlr).
pub(crate) enum DataControlDevice {
    /// ext-data-control-v1 device.
    Ext(ExtDataControlDeviceV1),
    /// wlr-data-control-unstable-v1 device.
    Wlr(ZwlrDataControlDeviceV1),
}

/// Data control offer (either ext or wlr).
pub(crate) enum DataControlOffer {
    /// ext-data-control-v1 offer.
    Ext(ExtDataControlOfferV1),
    /// wlr-data-control-unstable-v1 offer.
    Wlr(ZwlrDataControlOfferV1),
}

impl DataControlOffer {
    /// Request data transfer for a MIME type.
    ///
    /// Tells the source client to write data to the provided fd.
    pub(crate) fn receive(&self, mime_type: &str, fd: &OwnedFd) {
        use std::os::unix::io::AsFd;
        match self {
            DataControlOffer::Ext(offer) => offer.receive(mime_type.to_string(), fd.as_fd()),
            DataControlOffer::Wlr(offer) => offer.receive(mime_type.to_string(), fd.as_fd()),
        }
    }

    /// Destroy this offer.
    pub(crate) fn destroy(&self) {
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
pub(crate) enum Command {
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
    /// Update source data for a MIME type without re-creating the source.
    ///
    /// Used when data wasn't available at `SetSelection` time (eager fetch
    /// from a remote clipboard). The Wayland data source stays unchanged;
    /// only the cached data map is updated so the next `send` event can
    /// serve the requested MIME type.
    UpdateSourceData {
        /// MIME type key for the data.
        mime_type: String,
        /// Data bytes to cache.
        data: Vec<u8>,
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
    /// Answer a transfer raised through `Shared::on_transfer`.
    ///
    /// Writes `data` to every paste waiting on that transfer and caches it
    /// for later pastes of the same MIME type. `None` closes the waiting
    /// pastes with no data.
    CompleteTransfer {
        /// Serial passed to the `on_transfer` callback.
        serial: u32,
        /// The data, or `None` if the remote could not supply it.
        data: Option<Vec<u8>>,
    },
    /// Give up our selection, if we still own it.
    ClearSelection,
}

/// Shared clipboard state readable from any thread.
///
/// Updated by the event loop thread when the compositor's selection changes.
/// Read by clipboard backends to report current state.
#[derive(Default)]
pub(crate) struct Shared {
    /// MIME types of the current compositor selection.
    pub(crate) mime_types: Vec<String>,
    /// Serial number, incremented on each selection change.
    pub(crate) serial: u32,
    /// Change notification callback.
    ///
    /// Called on the event loop thread when selection changes.
    /// Typically captures a tokio channel sender for async notification.
    pub(crate) on_change: Option<Arc<dyn Fn(Vec<String>) + Send + Sync>>,
    /// Paste callback for data not supplied up front.
    ///
    /// When set, a paste of an advertised MIME type with no cached data is
    /// held open and this is called with a serial and the MIME type; the
    /// answer comes back as `Command::CompleteTransfer`. Without
    /// it such a paste gets no data.
    pub(crate) on_transfer: Option<Arc<dyn Fn(u32, String) + Send + Sync>>,
    /// Whether our own source is still the compositor's selection source.
    ///
    /// While it is, data we cached ourselves can be read back without a
    /// round trip; once another client takes the selection it must not be.
    pub(crate) own_source_live: bool,
}

// === Data control state ===

/// A paste waiting on data from `on_transfer`, with every paste of the same
/// MIME type that arrived meanwhile.
struct PendingTransfer {
    serial: u32,
    waiters: Vec<OwnedFd>,
}

/// Accumulated MIME types for a pending data offer.
///
/// Between `data_offer` and `selection` events, the compositor sends
/// `offer` events with MIME types. We collect them here.
#[derive(Default)]
struct PendingOffer {
    /// MIME types accumulated from `offer` events.
    mime_types: Vec<String>,
}

/// Central data control state.
///
/// Manages the lifecycle of data control protocol objects and routes
/// events to the shared clipboard state.
pub(crate) struct State {
    /// The data control manager global.
    pub(crate) manager: Option<DataControlManager>,
    /// The data control device (per-seat).
    pub(crate) device: Option<DataControlDevice>,
    /// The current selection offer from the compositor.
    current_offer: Option<DataControlOffer>,
    /// The data source we created for `SetSelection` (if any).
    current_source: Option<DataControlSource>,
    /// MIME types advertised on `current_source`.
    current_source_mime_types: Vec<String>,
    /// Data cached for our source's `send` events.
    source_data: HashMap<String, Vec<u8>>,
    /// Pending offer being built up (between `data_offer` and `selection` events).
    pending_offer: Option<(DataControlOffer, PendingOffer)>,
    /// Pastes waiting on `on_transfer`, by requested MIME type.
    pending_transfers: HashMap<String, PendingTransfer>,
    /// Next serial handed to `on_transfer`.
    next_transfer_serial: u32,
    /// Shared clipboard state for cross-thread access.
    #[expect(clippy::struct_field_names)]
    pub(crate) shared_state: Arc<Mutex<Shared>>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            manager: None,
            device: None,
            current_offer: None,
            current_source: None,
            current_source_mime_types: Vec::new(),
            source_data: HashMap::new(),
            pending_offer: None,
            pending_transfers: HashMap::new(),
            next_transfer_serial: 1,
            shared_state: Arc::new(Mutex::new(Shared::default())),
        }
    }
}

impl State {
    /// Create a data control device from the manager and seat.
    ///
    /// Must be called after both the manager and seat are bound.
    pub(crate) fn create_device<D>(&mut self, seat: &WlSeat, qh: &QueueHandle<D>)
    where
        D: Dispatch<ExtDataControlDeviceV1, ()> + Dispatch<ZwlrDataControlDeviceV1, ()> + 'static,
    {
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
    pub(crate) fn on_data_offer_ext(&mut self, offer: ExtDataControlOfferV1) {
        self.set_pending_offer(DataControlOffer::Ext(offer));
    }

    /// Handle a `data_offer` event from the device (wlr variant).
    pub(crate) fn on_data_offer_wlr(&mut self, offer: ZwlrDataControlOfferV1) {
        self.set_pending_offer(DataControlOffer::Wlr(offer));
    }

    fn set_own_source_live(&self, live: bool) {
        if let Ok(mut shared) = self.shared_state.lock() {
            shared.own_source_live = live;
        }
    }

    fn set_pending_offer(&mut self, offer: DataControlOffer) {
        // Destroy any previous pending offer that wasn't used
        if let Some((old_offer, _)) = self.pending_offer.take() {
            old_offer.destroy();
        }
        self.pending_offer = Some((offer, PendingOffer::default()));
    }

    /// Handle an `offer` event on a data offer (MIME type offered).
    pub(crate) fn on_offer_mime_type(&mut self, mime_type: String) {
        if let Some((_, ref mut pending)) = self.pending_offer {
            pending.mime_types.push(mime_type);
        }
    }

    /// Handle the `selection` event from the device.
    ///
    /// The compositor's selection has changed. The pending offer
    /// (with accumulated MIME types) becomes the current offer.
    pub(crate) fn on_selection(&mut self) {
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

        // The device reports every selection, our own included. Reporting
        // our own back as a change makes the consumer treat it as another
        // client's copy.
        let own = is_own_selection(
            self.current_source.is_some(),
            &self.current_source_mime_types,
            &mime_types,
        );
        tracing::debug!(
            mime_types = ?mime_types,
            own,
            "Compositor selection changed"
        );

        // The callback runs after the lock is released: it may call back into
        // the handle (`serial`, `selection_mime_types`), which locks the same state.
        let callback = self.shared_state.lock().ok().and_then(|mut shared| {
            shared.serial += 1;
            shared.mime_types.clone_from(&mime_types);
            if own { None } else { shared.on_change.clone() }
        });
        if let Some(callback) = callback {
            callback(mime_types);
        }
    }

    /// Handle the `selection` event with a NULL offer (selection cleared).
    pub(crate) fn on_selection_cleared(&mut self) {
        if let Some(old) = self.current_offer.take() {
            old.destroy();
        }
        // Clear any pending offer too
        if let Some((offer, _)) = self.pending_offer.take() {
            offer.destroy();
        }

        tracing::debug!("Compositor selection cleared");

        let callback = self.shared_state.lock().ok().and_then(|mut shared| {
            shared.serial += 1;
            shared.mime_types.clear();
            shared.on_change.clone()
        });
        if let Some(callback) = callback {
            callback(Vec::new());
        }
    }

    /// Update cached source data for a MIME type.
    ///
    /// Inserts or replaces data in the `source_data` map without
    /// re-creating the Wayland data source. Used when data arrives
    /// after `set_selection` was called with an empty data map
    /// (eager fetch from a remote clipboard).
    pub(crate) fn update_source_data(&mut self, mime_type: String, data: Vec<u8>) {
        tracing::debug!(
            mime_type = %mime_type,
            bytes = data.len(),
            "Source data updated (post-announcement)"
        );
        self.source_data.insert(mime_type, data);
    }

    /// Handle a `send` event on our data source.
    ///
    /// The compositor (or another client pasting) wants data in the
    /// specified MIME type. Cached data is written at once; otherwise, with
    /// an `on_transfer` callback set, the paste is held until the data
    /// arrives through `CompleteTransfer`.
    pub(crate) fn on_source_send(&mut self, mime_type: &str, fd: OwnedFd) {
        if let Some(data) = self.cached_data(mime_type) {
            write_in_background(fd, data);
            return;
        }

        if let Some(pending) = self.pending_transfers.get_mut(mime_type) {
            pending.waiters.push(fd);
            return;
        }

        let on_transfer = self
            .shared_state
            .lock()
            .ok()
            .and_then(|shared| shared.on_transfer.clone());
        let advertised = self
            .current_source_mime_types
            .iter()
            .any(|m| m == mime_type);
        let (Some(on_transfer), true) = (on_transfer, advertised) else {
            tracing::warn!(mime_type, "Source send event for unknown MIME type");
            // fd is dropped/closed here, signaling no data
            return;
        };

        let serial = self.next_transfer_serial;
        self.next_transfer_serial = self.next_transfer_serial.wrapping_add(1);
        self.pending_transfers.insert(
            mime_type.to_string(),
            PendingTransfer {
                serial,
                waiters: vec![fd],
            },
        );
        tracing::debug!(mime_type, serial, "Paste held until its data arrives");
        on_transfer(serial, mime_type.to_string());
    }

    /// Data cached for `mime_type`, tolerating a charset parameter
    /// (compositors commonly request `text/plain;charset=utf-8` for
    /// `text/plain`).
    fn cached_data(&self, mime_type: &str) -> Option<Arc<Vec<u8>>> {
        self.source_data
            .get(mime_type)
            .or_else(|| {
                let base = mime_type.split(';').next()?.trim();
                self.source_data.get(base)
            })
            .map(|data| Arc::new(data.clone()))
    }

    /// Process a `CompleteTransfer` command.
    pub(crate) fn complete_transfer(&mut self, serial: u32, data: Option<Vec<u8>>) {
        let Some(mime_type) = self
            .pending_transfers
            .iter()
            .find(|(_, pending)| pending.serial == serial)
            .map(|(mime, _)| mime.clone())
        else {
            tracing::debug!(serial, "Transfer answered after its selection was replaced");
            return;
        };
        let Some(pending) = self.pending_transfers.remove(&mime_type) else {
            return;
        };
        let Some(data) = data.filter(|d| !d.is_empty()) else {
            tracing::debug!(
                mime_type,
                waiters = pending.waiters.len(),
                "Transfer returned no data"
            );
            return;
        };
        let shared = Arc::new(data);
        for fd in pending.waiters {
            write_in_background(fd, Arc::clone(&shared));
        }
        self.source_data
            .insert(mime_type, Arc::unwrap_or_clone(shared));
    }

    /// Process a `ClearSelection` command: give up our selection if the
    /// compositor still has it.
    pub(crate) fn clear_selection(&mut self) {
        let Some(source) = self.current_source.take() else {
            return;
        };
        match &self.device {
            Some(DataControlDevice::Ext(dev)) => dev.set_selection(None),
            Some(DataControlDevice::Wlr(dev)) => dev.set_selection(None),
            None => {}
        }
        source.destroy();
        self.current_source_mime_types.clear();
        self.source_data.clear();
        self.pending_transfers.clear();
        self.set_own_source_live(false);
        tracing::debug!("Released our clipboard selection");
    }

    /// Handle the `cancelled` event on our data source.
    ///
    /// Our source has been replaced by another. Clean up.
    pub(crate) fn on_source_cancelled(&mut self) {
        tracing::debug!("Data control source cancelled");
        if let Some(source) = self.current_source.take() {
            source.destroy();
        }
        self.current_source_mime_types.clear();
        self.source_data.clear();
        // Waiting pastes see end-of-file.
        self.pending_transfers.clear();
        self.set_own_source_live(false);
    }

    /// Handle the `finished` event on the device.
    ///
    /// The data control device is no longer valid.
    pub(crate) fn on_device_finished(&mut self) {
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
        self.current_source_mime_types.clear();
        self.source_data.clear();
        self.pending_transfers.clear();
        self.set_own_source_live(false);
    }

    /// Process a `SetSelection` command.
    ///
    /// Creates a new data control source, advertises MIME types, and
    /// sets it as the selection on the device.
    pub(crate) fn set_selection<D>(
        &mut self,
        mime_types: &[String],
        data: HashMap<String, Vec<u8>>,
        qh: &QueueHandle<D>,
    ) where
        D: Dispatch<ExtDataControlSourceV1, ()> + Dispatch<ZwlrDataControlSourceV1, ()> + 'static,
    {
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

        // Replace, then destroy: destroying the current source first would
        // leave the clipboard empty for a moment, which clipboard managers
        // (Klipper's "prevent empty clipboard") answer by re-offering their
        // last history item.
        if let Some(old) = self.current_source.take() {
            old.destroy();
        }
        self.pending_transfers.clear();
        self.source_data = data;
        self.current_source = Some(new_source);
        self.current_source_mime_types = mime_types.to_vec();
        self.set_own_source_live(true);
    }

    /// Process a `ReceiveFromOffer` command.
    ///
    /// Calls `offer.receive(mime_type, fd)` to request data from the
    /// compositor. The caller reads from the other end of the pipe.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "OwnedFd must be owned so it is dropped after the receive call"
    )]
    pub(crate) fn receive_from_offer(&self, mime_type: &str, fd: OwnedFd) {
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

/// Whether a selection event reports the selection this client set itself.
///
/// Another client taking the selection cancels our source first, so while
/// our source is live the selection is still ours. The offered MIME set must
/// also match what we advertised: that keeps a foreign copy from being
/// swallowed on a compositor that delivers the selection before the cancel.
fn is_own_selection(source_live: bool, advertised: &[String], offered: &[String]) -> bool {
    if !source_live || advertised.len() != offered.len() {
        return false;
    }
    offered.iter().all(|mime| advertised.contains(mime))
}

/// Answer a paste on a worker thread: a large image written into a slow
/// reader would otherwise block every other Wayland event.
fn write_in_background(fd: OwnedFd, data: Arc<Vec<u8>>) {
    use std::io::Write;

    let spawned = std::thread::Builder::new()
        .name("data-control-send".into())
        .spawn(move || {
            let mut file = std::fs::File::from(fd);
            if let Err(e) = file.write_all(&data) {
                tracing::debug!(error = %e, "Paste reader went away");
            }
        });
    if let Err(e) = spawned {
        tracing::error!(error = %e, "Failed to start a paste writer");
    }
}

#[cfg(test)]
#[expect(
    unsafe_code,
    reason = "tests use from_raw_fd to create OwnedFd from pipe file descriptors"
)]
mod tests {
    use std::{io::Read, os::unix::io::FromRawFd};

    use super::*;

    #[test]
    fn test_shared_clipboard_state_default() {
        let state = Shared::default();
        assert_eq!(state.mime_types, Vec::<String>::new());
        assert_eq!(state.serial, 0);
        assert!(state.on_change.is_none());
    }

    #[test]
    fn test_data_control_state_default() {
        let state = State::default();
        assert!(state.manager.is_none());
        assert!(state.device.is_none());
        assert!(state.current_offer.is_none());
        assert!(state.current_source.is_none());
        assert!(state.source_data.is_empty());
        assert!(state.pending_offer.is_none());
    }

    fn mimes(types: &[&str]) -> Vec<String> {
        types.iter().map(|t| (*t).to_string()).collect()
    }

    #[test]
    fn own_selection_is_recognised_while_our_source_is_live() {
        let ours = mimes(&["text/plain;charset=utf-8"]);
        assert!(is_own_selection(
            true,
            &ours,
            &mimes(&["text/plain;charset=utf-8"])
        ));
    }

    #[test]
    fn a_selection_after_our_source_was_cancelled_is_foreign() {
        let ours = mimes(&["text/plain;charset=utf-8"]);
        assert!(!is_own_selection(
            false,
            &ours,
            &mimes(&["text/plain;charset=utf-8"])
        ));
    }

    #[test]
    fn a_different_offer_while_our_source_is_live_is_foreign() {
        let ours = mimes(&["text/plain;charset=utf-8"]);
        let wl_copy = mimes(&[
            "text/plain;charset=utf-8",
            "text/plain",
            "TEXT",
            "STRING",
            "UTF8_STRING",
        ]);
        assert!(!is_own_selection(true, &ours, &wl_copy));
    }

    #[test]
    fn own_selection_ignores_offer_order() {
        let ours = mimes(&["image/png", "text/html"]);
        assert!(is_own_selection(
            true,
            &ours,
            &mimes(&["text/html", "image/png"])
        ));
    }

    #[test]
    fn test_on_selection_cleared() {
        let mut state = State::default();

        // Register a callback to verify it's called
        let called = Arc::new(Mutex::new(false));
        let called_clone = Arc::clone(&called);
        if let Ok(mut shared) = state.shared_state.lock() {
            shared.on_change = Some(Arc::new(move |types: Vec<String>| {
                assert_eq!(types, Vec::<String>::new());
                *called_clone.lock().unwrap() = true;
            }));
        }

        state.on_selection_cleared();

        assert!(*called.lock().unwrap());
        assert_eq!(
            state.shared_state.lock().unwrap().mime_types,
            Vec::<String>::new()
        );
        assert_eq!(state.shared_state.lock().unwrap().serial, 1);
    }

    #[test]
    fn the_change_callback_may_lock_the_shared_state() {
        let mut state = State::default();
        let shared = Arc::clone(&state.shared_state);
        let relocked = Arc::new(Mutex::new(false));
        let seen = Arc::clone(&relocked);
        state.shared_state.lock().unwrap().on_change = Some(Arc::new(move |_types| {
            // A callback that reads the handle locks this state; it must not be held.
            *seen.lock().unwrap() = shared.try_lock().is_ok();
        }));

        state.on_selection_cleared();

        assert!(*relocked.lock().unwrap());
    }

    #[test]
    fn test_on_source_cancelled() {
        let mut state = State::default();
        state
            .source_data
            .insert("text/plain".to_string(), b"hello".to_vec());

        state.on_source_cancelled();

        assert!(state.current_source.is_none());
        assert!(state.source_data.is_empty());
    }

    #[test]
    fn test_on_device_finished() {
        let mut state = State::default();
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
        let mut state = State::default();
        // Should not panic when there's no pending offer
        state.on_offer_mime_type("text/plain".to_string());
    }

    #[test]
    fn test_on_source_send_unknown_mime() {
        let mut state = State::default();
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
        let mut state = State::default();
        state
            .source_data
            .insert("text/plain".to_string(), b"hello world".to_vec());

        let (read_fd, write_fd) = nix::unistd::pipe().unwrap();
        let write_owned =
            unsafe { OwnedFd::from_raw_fd(std::os::unix::io::AsRawFd::as_raw_fd(&write_fd)) };
        std::mem::forget(write_fd);

        state.on_source_send("text/plain", write_owned);

        // Read from the pipe
        let mut file =
            unsafe { std::fs::File::from_raw_fd(std::os::unix::io::AsRawFd::as_raw_fd(&read_fd)) };
        std::mem::forget(read_fd);
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello world");
    }

    fn read_all(fd: OwnedFd) -> Vec<u8> {
        use std::io::Read;
        let mut buf = Vec::new();
        std::fs::File::from(fd).read_to_end(&mut buf).unwrap();
        buf
    }

    type TransferLog = Arc<Mutex<Vec<(u32, String)>>>;

    /// State advertising `image/png` with a transfer callback that records
    /// what it was asked for.
    fn state_with_transfer_callback() -> (State, TransferLog) {
        let state = State {
            current_source_mime_types: vec!["image/png".to_string()],
            ..Default::default()
        };
        let requests: TransferLog = Arc::default();
        let seen = Arc::clone(&requests);
        state.shared_state.lock().unwrap().on_transfer = Some(Arc::new(move |serial, mime| {
            seen.lock().unwrap().push((serial, mime));
        }));
        (state, requests)
    }

    #[test]
    fn paste_without_data_is_held_until_the_transfer_completes() {
        let (mut state, requests) = state_with_transfer_callback();
        let (first_read, first_write) = nix::unistd::pipe().unwrap();
        let (second_read, second_write) = nix::unistd::pipe().unwrap();

        state.on_source_send("image/png", first_write);
        state.on_source_send("image/png", second_write);
        // Both pastes share one transfer.
        let requested = requests.lock().unwrap().clone();
        assert_eq!(requested, vec![(1, "image/png".to_string())]);

        state.complete_transfer(1, Some(b"png bytes".to_vec()));
        assert_eq!(read_all(first_read), b"png bytes");
        assert_eq!(read_all(second_read), b"png bytes");

        // A later paste is served from the cache without a new transfer.
        let (third_read, third_write) = nix::unistd::pipe().unwrap();
        state.on_source_send("image/png", third_write);
        assert_eq!(read_all(third_read), b"png bytes");
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[test]
    fn held_paste_gets_nothing_when_the_selection_is_lost() {
        let (mut state, _requests) = state_with_transfer_callback();
        let (read, write) = nix::unistd::pipe().unwrap();
        state.on_source_send("image/png", write);

        state.on_source_cancelled();
        assert_eq!(read_all(read), Vec::<u8>::new());
        // An answer for the lost selection is ignored.
        state.complete_transfer(1, Some(b"late".to_vec()));
        assert!(state.source_data.is_empty());
    }

    #[test]
    fn paste_of_an_unadvertised_type_raises_no_transfer() {
        let (mut state, requests) = state_with_transfer_callback();
        let (read, write) = nix::unistd::pipe().unwrap();
        state.on_source_send("text/html", write);
        assert_eq!(read_all(read), Vec::<u8>::new());
        assert!(requests.lock().unwrap().is_empty());
    }
}
