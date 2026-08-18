//! `InputCapture` D-Bus interface implementation.
//!
//! Implements `org.freedesktop.impl.portal.InputCapture` version 2.
//!
//! # Phase 1 scope
//!
//! This is a fully spec-shaped, fully tested implementation of the D-Bus
//! interface: session lifecycle, live zone geometry (backed by the same
//! output tracking [`RemoteDesktopInterface`](super::RemoteDesktopInterface)
//! and `ScreenCastInterface` already use), and barrier structural
//! validation, all real. `ConnectToEIS` reuses the existing
//! [`InputBackend`] the same way `RemoteDesktop.ConnectToEIS` does.
//!
//! # Phase 2a scope
//!
//! `Enable()` now sends the session's accepted barriers to the Wayland
//! event loop, which creates real, invisible `wlr-layer-shell-v1` surfaces
//! mapped at each barrier's position ([`crate::wayland::input_capture`]).
//! `Disable()`/`Release()`/session close tear those surfaces down.
//!
//! **Still not implemented:** the barrier surfaces are inert -- nothing
//! locks the pointer on entry or forwards relative motion, so
//! `Activated`/`Deactivated` still never fire. Real enforcement needs
//! `zwp_pointer_constraints_v1` (lock the pointer on entry) and
//! `zwp_relative_pointer_v1` (motion after lock), both bound but unused
//! until Phase 2b. `Start()` logs a `tracing::warn!` the first time a
//! client requests `POINTER` capability, so this gap is visible in logs
//! rather than silent.

use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
};

use tokio::sync::Mutex;
use zbus::{
    interface,
    object_server::SignalEmitter,
    zvariant::{Fd, ObjectPath, OwnedValue, Value},
};

use super::{Response, empty_results, get_option_u32};
use crate::{
    error::PortalError,
    pipewire::PipeWireManager,
    services::{
        capture::CaptureBackend,
        input::{InputBackend, InputProtocol},
    },
    session::{PersistMode, SessionManager},
    types::{DeviceTypes, InputCaptureZone, PointerBarrier},
    wayland::{AvailableProtocols, InputCaptureCommand, SharedWaylandState},
};

/// A barrier submitted via `SetPointerBarriers`, parsed but not yet
/// validated: `(barrier_id, position)`, where `position` is `None` if the
/// dict was missing or malformed.
type ParsedBarrier = (u32, Option<(i32, i32, i32, i32)>);

/// `InputCapture` portal interface implementation.
pub struct InputCaptureInterface {
    /// Session manager.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Input backend, reused for `ConnectToEIS` exactly as `RemoteDesktop` does.
    input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    /// Capture backend, needed only to construct `SessionInterface` for cleanup on close.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// `PipeWire` manager, needed only to construct `SessionInterface` for cleanup on close.
    pipewire_manager: Arc<PipeWireManager>,
    /// Available protocols for capability queries.
    available_protocols: AvailableProtocols,
    /// Shared Wayland state for live zone geometry. `None` in configurations
    /// that don't wire it up (`GetZones` degrades to empty zones + `zone_set=0`
    /// with a one-time warning rather than failing).
    shared_wayland_state: Option<Arc<std::sync::Mutex<SharedWaylandState>>>,
    /// Command sender to the Wayland event loop for barrier surface
    /// lifecycle. `None` in configurations that don't wire it up (`Enable`
    /// then negotiates capabilities but creates no surfaces, logged once).
    input_capture_tx: Option<mpsc::Sender<InputCaptureCommand>>,
}

impl InputCaptureInterface {
    /// Create a new `InputCapture` interface.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<PipeWireManager>,
        available_protocols: AvailableProtocols,
        shared_wayland_state: Option<Arc<std::sync::Mutex<SharedWaylandState>>>,
        input_capture_tx: Option<mpsc::Sender<InputCaptureCommand>>,
    ) -> Self {
        Self {
            session_manager,
            input_backend,
            capture_backend,
            pipewire_manager,
            available_protocols,
            shared_wayland_state,
            input_capture_tx,
        }
    }

    /// Register the `Session` D-Bus object for a newly-created InputCapture session.
    async fn register_session_object(
        &self,
        server: &zbus::ObjectServer,
        session_handle: &ObjectPath<'_>,
    ) {
        let session_iface = super::SessionInterface::new(
            Arc::clone(&self.session_manager),
            session_handle.to_owned(),
            Arc::clone(&self.input_backend),
            Arc::clone(&self.capture_backend),
            Arc::clone(&self.pipewire_manager),
            self.input_capture_tx.clone(),
        );
        if let Err(e) = server.at(session_handle, session_iface).await {
            tracing::warn!(
                session_handle = %session_handle,
                error = %e,
                "Failed to register Session D-Bus object"
            );
        }
    }

    /// Extract `persist_mode` from options.
    fn get_persist_mode(options: &HashMap<String, OwnedValue>) -> PersistMode {
        get_option_u32(options, "persist_mode").map_or(PersistMode::None, PersistMode::from_dbus)
    }

    /// Extract the required `capabilities` bitmask from options.
    ///
    /// Per spec, `capabilities` is required in `Start`'s options and must
    /// not be zero.
    fn get_required_capabilities(
        options: &HashMap<String, OwnedValue>,
    ) -> zbus::fdo::Result<DeviceTypes> {
        let bits = get_option_u32(options, "capabilities").ok_or_else(|| {
            PortalError::InvalidArgument("missing required option 'capabilities'".to_string())
        })?;
        if bits == 0 {
            return Err(PortalError::InvalidArgument(
                "'capabilities' must not be zero".to_string(),
            )
            .into());
        }
        Ok(DeviceTypes::from_bits(bits))
    }

    /// Extract a `(i32, i32, i32, i32)` tuple from an options-style value.
    ///
    /// zvariant has no direct `TryFrom<&OwnedValue>` for raw tuples, so this
    /// downcasts to `&Value` and unpacks the `Structure` manually, the same
    /// approach `ScreenCastInterface::parse_restore_data` already uses for
    /// its `(suv)` tuple.
    fn get_option_i32_tuple4(
        options: &HashMap<String, OwnedValue>,
        key: &str,
    ) -> Option<(i32, i32, i32, i32)> {
        let v = options.get(key)?;
        let value: &Value<'_> = v.downcast_ref().ok()?;
        let Value::Structure(s) = value else {
            return None;
        };
        let fields = s.fields();
        if fields.len() != 4 {
            return None;
        }
        let x1 = i32::try_from(&fields[0]).ok()?;
        let y1 = i32::try_from(&fields[1]).ok()?;
        let x2 = i32::try_from(&fields[2]).ok()?;
        let y2 = i32::try_from(&fields[3]).ok()?;
        Some((x1, y1, x2, y2))
    }

    /// Validate that a session exists, belongs to the caller, and was
    /// created via `InputCapture` (not `RemoteDesktop`/`ScreenCast`).
    fn validate_input_capture_session(
        manager: &SessionManager,
        session_handle: &ObjectPath<'_>,
        app_id: &str,
        sender: &str,
    ) -> zbus::fdo::Result<()> {
        manager.validate_session(session_handle, app_id, sender)?;
        let session = manager
            .get_session(session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;
        if !session.is_input_capture_session {
            return Err(PortalError::InvalidSession(format!(
                "{session_handle} is not an InputCapture session"
            ))
            .into());
        }
        Ok(())
    }

    /// Read the current zones and zone-set from shared Wayland state.
    ///
    /// Returns `(zones, zone_set)`. Logs a warning exactly once (via the
    /// `warn!` call itself, which is cheap enough not to need a guard) if
    /// shared state was never wired up — that's a startup configuration
    /// gap, not a per-call error, so it degrades to empty zones rather than
    /// failing the request.
    fn current_zones(&self) -> (Vec<InputCaptureZone>, u32) {
        let Some(shared) = &self.shared_wayland_state else {
            tracing::warn!(
                "InputCapture.GetZones: shared Wayland state not configured, returning empty zones"
            );
            return (Vec::new(), 0);
        };
        let Ok(state) = shared.lock() else {
            tracing::warn!("InputCapture.GetZones: shared Wayland state lock poisoned");
            return (Vec::new(), 0);
        };
        (state.zones.clone(), state.zone_set)
    }

    /// Build the `a(uuii)` zones result plus `zone_set`.
    #[expect(
        clippy::expect_used,
        reason = "Value-to-OwnedValue conversion is infallible for tuple/array types"
    )]
    fn build_zones_results(
        zones: &[InputCaptureZone],
        zone_set: u32,
    ) -> HashMap<String, OwnedValue> {
        let zone_tuples: Vec<(u32, u32, i32, i32)> = zones
            .iter()
            .map(|z| (z.width, z.height, z.x, z.y))
            .collect();

        let mut results = HashMap::new();
        results.insert(
            "zones".to_string(),
            OwnedValue::try_from(Value::from(zone_tuples))
                .expect("zone tuple array converts to OwnedValue"),
        );
        results.insert("zone_set".to_string(), OwnedValue::from(zone_set));
        results
    }

    /// Parse and validate submitted barriers, partitioning into accepted/failed.
    ///
    /// If `submitted_zone_set` doesn't match the currently-known zone set,
    /// every barrier fails — the app's view of the geometry is stale.
    fn partition_barriers(
        barrier_dicts: &[HashMap<String, OwnedValue>],
        submitted_zone_set: u32,
        current_zone_set: u32,
    ) -> (Vec<PointerBarrier>, Vec<u32>) {
        let parsed: Vec<ParsedBarrier> = barrier_dicts
            .iter()
            .map(|dict| {
                let id = get_option_u32(dict, "barrier_id").unwrap_or(0);
                let position = Self::get_option_i32_tuple4(dict, "position");
                (id, position)
            })
            .collect();

        if submitted_zone_set != current_zone_set {
            return (Vec::new(), parsed.into_iter().map(|(id, _)| id).collect());
        }

        let mut accepted = Vec::new();
        let mut failed = Vec::new();
        let mut seen_ids = std::collections::HashSet::new();

        for (barrier_id, position) in parsed {
            let Some((x1, y1, x2, y2)) = position else {
                failed.push(barrier_id);
                continue;
            };
            let barrier = PointerBarrier {
                barrier_id,
                x1,
                y1,
                x2,
                y2,
            };
            if !barrier.is_valid() || !seen_ids.insert(barrier_id) {
                failed.push(barrier_id);
                continue;
            }
            accepted.push(barrier);
        }

        (accepted, failed)
    }
}

#[interface(
    name = "org.freedesktop.impl.portal.InputCapture",
    introspection_docs = false
)]
impl InputCaptureInterface {
    /// Create a new `InputCapture` session (deprecated v1 form).
    ///
    /// Unlike `CreateSession2`, this creates and starts the session in one
    /// call, negotiating capabilities immediately from `options`.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session could not be created or if
    /// the sender is missing from the D-Bus header.
    #[zbus(name = "CreateSession")]
    #[expect(
        clippy::too_many_arguments,
        reason = "D-Bus method signature requires all parameters"
    )]
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = parent_window;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            app_id = %app_id,
            "InputCapture.CreateSession (v1, deprecated) called"
        );

        let request_iface = super::RequestInterface::new(Arc::clone(&self.session_manager));
        let _ = server.at(&handle, request_iface).await;

        let persist_mode = Self::get_persist_mode(&options);
        let capabilities = get_option_u32(&options, "capabilities")
            .map_or_else(DeviceTypes::all, DeviceTypes::from_bits);

        let mut manager = self.session_manager.lock().await;
        let result = match manager.create_session(
            session_handle.to_owned(),
            sender,
            app_id.to_string(),
            persist_mode,
        ) {
            Ok(session) => {
                session.is_input_capture_session = true;
                let _ = session.select_devices(capabilities);
                let negotiated = session.device_types;
                let _ = session.start(vec![]);
                drop(manager);

                self.register_session_object(server, &session_handle).await;

                let mut results = HashMap::new();
                results.insert(
                    "capabilities".to_string(),
                    OwnedValue::from(negotiated.to_bits()),
                );
                Ok((Response::Success.to_u32(), results))
            }
            Err(e) => {
                tracing::error!(error = %e, "InputCapture.CreateSession failed");
                Ok((Response::Other.to_u32(), empty_results()))
            }
        };

        let _ = server.remove::<super::RequestInterface, _>(&handle).await;
        result
    }

    /// Create a new `InputCapture` session without starting it.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session could not be created or if
    /// the sender is missing from the D-Bus header.
    #[zbus(name = "CreateSession2")]
    async fn create_session2(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            session_handle = %session_handle,
            app_id = %app_id,
            "InputCapture.CreateSession2 called"
        );

        let persist_mode = Self::get_persist_mode(&options);

        let mut manager = self.session_manager.lock().await;
        let session = manager.create_session(
            session_handle.to_owned(),
            sender,
            app_id.to_string(),
            persist_mode,
        )?;
        session.is_input_capture_session = true;
        drop(manager);

        self.register_session_object(server, &session_handle).await;

        // Spec defines no result keys for CreateSession2 today.
        Ok(empty_results())
    }

    /// Start the `InputCapture` session, negotiating capabilities.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session cannot be started or the
    /// sender is invalid.
    #[zbus(name = "Start")]
    #[expect(
        clippy::too_many_arguments,
        reason = "D-Bus method signature requires all parameters"
    )]
    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = parent_window;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            app_id = %app_id,
            "InputCapture.Start called"
        );

        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        let requested = Self::get_required_capabilities(&options)?;

        let mut manager = self.session_manager.lock().await;
        Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        session.select_devices(requested)?;
        let negotiated = session.device_types;
        session.start(vec![])?;
        let clipboard_enabled = session.clipboard_enabled;
        let persist_mode = session.persist_mode;

        if negotiated.pointer {
            tracing::warn!(
                session_id = %session_handle,
                "InputCapture pointer capability negotiated, but barrier enforcement is not \
                 implemented yet -- Enable() will not produce Activated/Deactivated signals"
            );
        }

        let mut results = HashMap::new();
        results.insert(
            "capabilities".to_string(),
            OwnedValue::from(negotiated.to_bits()),
        );
        results.insert(
            "clipboard_enabled".to_string(),
            OwnedValue::from(clipboard_enabled),
        );

        if persist_mode != PersistMode::None {
            // No natural "selected sources" concept for InputCapture; persist
            // the negotiated capabilities bitmask as the opaque data payload,
            // matching ScreenCast's (vendor, version, data) restore_data shape.
            let data_value = Value::from(negotiated.to_bits());
            let rd_tuple = Value::from(("generic", 1u32, data_value));
            if let Ok(rd_owned) = OwnedValue::try_from(rd_tuple) {
                results.insert("restore_data".to_string(), rd_owned);
            }
            results.insert(
                "persist_mode".to_string(),
                OwnedValue::from(persist_mode.to_dbus()),
            );
        }

        tracing::info!(
            session_id = %session_handle,
            capabilities = negotiated.to_bits(),
            "InputCapture session started"
        );

        let _ = server.remove::<super::RequestInterface, _>(&handle).await;
        Ok((Response::Success.to_u32(), results))
    }

    /// Get the current input capture zones.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or is not an
    /// `InputCapture` session.
    #[zbus(name = "GetZones")]
    async fn get_zones(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        {
            let manager = self.session_manager.lock().await;
            Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;
        }

        let (zones, zone_set) = self.current_zones();
        tracing::debug!(
            session_id = %session_handle,
            zone_count = zones.len(),
            zone_set = zone_set,
            "InputCapture.GetZones"
        );

        let results = Self::build_zones_results(&zones, zone_set);

        let _ = server.remove::<super::RequestInterface, _>(&handle).await;
        Ok((Response::Success.to_u32(), results))
    }

    /// Set pointer barriers for the session.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or is not an
    /// `InputCapture` session.
    #[zbus(name = "SetPointerBarriers")]
    #[expect(
        clippy::too_many_arguments,
        reason = "D-Bus method signature requires all parameters"
    )]
    async fn set_pointer_barriers(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        barriers: Vec<HashMap<String, OwnedValue>>,
        zone_set: u32,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
        );
        let _ = server.at(&handle, request_iface).await;

        let mut manager = self.session_manager.lock().await;
        Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;

        let (_, current_zone_set) = self.current_zones();
        let (accepted, failed_barriers) =
            Self::partition_barriers(&barriers, zone_set, current_zone_set);

        tracing::info!(
            session_id = %session_handle,
            submitted = barriers.len(),
            accepted = accepted.len(),
            failed = failed_barriers.len(),
            "InputCapture.SetPointerBarriers"
        );

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;
        session.set_pointer_barriers(accepted);

        let mut results = HashMap::new();
        results.insert(
            "failed_barriers".to_string(),
            owned_value_from_u32_array(failed_barriers),
        );

        let _ = server.remove::<super::RequestInterface, _>(&handle).await;
        Ok((Response::Success.to_u32(), results))
    }

    /// Enable input capturing for the session.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found, is not an
    /// `InputCapture` session, or is not in the `Started` state.
    #[zbus(name = "Enable")]
    async fn enable(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        let mut manager = self.session_manager.lock().await;
        Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;
        session.set_input_capture_enabled(true)?;
        let barriers = session.input_capture_barriers.clone();
        drop(manager);

        if !barriers.is_empty() {
            if let Some(tx) = &self.input_capture_tx {
                let _ = tx.send(InputCaptureCommand::CreateBarrierSurfaces {
                    session_id: session_handle.to_string(),
                    barriers,
                });
            } else {
                tracing::warn!(
                    session_id = %session_handle,
                    "InputCapture enabled with barriers set, but no Wayland command channel is \
                     configured -- no barrier surfaces will be created"
                );
            }
        }

        tracing::info!(session_id = %session_handle, "InputCapture enabled");
        Ok((Response::Success.to_u32(), empty_results()))
    }

    /// Disable input capturing for the session.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found, is not an
    /// `InputCapture` session, or is not in the `Started` state.
    #[zbus(name = "Disable")]
    async fn disable(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        let mut manager = self.session_manager.lock().await;
        Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        let was_active = session.input_capture_active;
        session.set_input_capture_enabled(false)?;
        drop(manager);

        if let Some(tx) = &self.input_capture_tx {
            let _ = tx.send(InputCaptureCommand::DestroySession {
                session_id: session_handle.to_string(),
            });
        }

        if was_active {
            let _ = Self::deactivated(&emitter, session_handle.to_owned(), empty_results()).await;
        }
        let _ = Self::disabled(&emitter, session_handle.to_owned(), empty_results()).await;

        tracing::info!(session_id = %session_handle, "InputCapture disabled");
        Ok((Response::Success.to_u32(), empty_results()))
    }

    /// Release an active capture, returning control to the compositor.
    ///
    /// No capture is ever active yet (`Activated` never fires -- barrier
    /// surfaces exist but nothing locks the pointer on entry until Phase
    /// 2b), so this tears down the session's barrier surfaces the same as
    /// `Disable` and returns success. That's a placeholder, not the real
    /// semantics: per spec `Release` should give back pointer control
    /// while leaving the session's barriers armed to re-trigger, not
    /// dismantle them. Revisit alongside Phase 2b's real activation path,
    /// once there's an actual "release without disabling" case to get
    /// right.
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not found or is not an
    /// `InputCapture` session.
    #[zbus(name = "Release")]
    async fn release(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        let manager = self.session_manager.lock().await;
        Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;
        drop(manager);

        if let Some(tx) = &self.input_capture_tx {
            let _ = tx.send(InputCaptureCommand::DestroySession {
                session_id: session_handle.to_string(),
            });
        }

        tracing::debug!(
            session_id = %session_handle,
            "InputCapture.Release: barrier surfaces torn down, nothing else active in this build"
        );
        Ok(())
    }

    /// Connect to EIS for the captured input stream.
    ///
    /// Reuses [`InputBackend`] exactly as `RemoteDesktop.ConnectToEIS`
    /// does. Unlike `RemoteDesktop`, `InputCapture` has no `Notify*`
    /// fallback methods, so a `WlrVirtualInput`-only backend is a hard
    /// failure here (not just a degraded mode).
    ///
    /// # Errors
    ///
    /// Returns `zbus::fdo::Error` if the session is not in a valid state,
    /// is not an `InputCapture` session, or EIS is unavailable.
    #[zbus(name = "ConnectToEIS")]
    async fn connect_to_eis(
        &self,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<Fd<'static>> {
        let _ = &options;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        let mut manager = self.session_manager.lock().await;
        Self::validate_input_capture_session(&manager, &session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.can_connect_to_eis() {
            return Err(PortalError::InvalidState {
                expected: "Started with devices selected, not yet connected".to_string(),
                actual: format!(
                    "state={}, devices_selected={}, uses_eis={}",
                    session.state, session.devices_selected, session.uses_eis
                ),
            }
            .into());
        }

        let device_types = session.device_types;
        let session_id = session_handle.to_string();

        let mut backend = self.input_backend.lock().await;
        if backend.protocol_type() != InputProtocol::Eis {
            return Err(zbus::fdo::Error::NotSupported(
                "InputCapture requires EIS; this compositor is only using wlr virtual input, \
                 which has no equivalent Notify* fallback for InputCapture"
                    .to_string(),
            ));
        }

        let fd = backend
            .create_context(&session_id, device_types)
            .map_err(|e| PortalError::EisCreationFailed(e.to_string()))?
            .ok_or_else(|| {
                PortalError::EisCreationFailed("EIS backend returned no fd".to_string())
            })?;
        drop(backend);

        static CONTEXT_ID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let context_id = CONTEXT_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;
        session.connect_to_eis(context_id)?;

        tracing::info!(
            session_id = %session_handle,
            context_id = context_id,
            "InputCapture EIS connection established"
        );

        Ok(Fd::from(fd))
    }

    // === Signals ===

    /// Emitted when the application stops receiving captured input.
    #[zbus(signal)]
    async fn disabled(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// Emitted when input capture starts (a barrier fires).
    #[zbus(signal)]
    async fn activated(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// Emitted when input capture stops.
    #[zbus(signal)]
    async fn deactivated(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    /// Emitted when the session's zones change; the application should
    /// call `GetZones` again.
    #[zbus(signal)]
    async fn zones_changed(
        emitter: &SignalEmitter<'_>,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;

    // === Properties ===

    /// Bitmask of capabilities this backend can actually deliver
    /// (1=KEYBOARD, 2=POINTER, 4=TOUCHSCREEN).
    ///
    /// Requires an actually-active EIS backend (a compositor forced into
    /// `WlrVirtualInput` mode has no `Notify*` fallback for InputCapture,
    /// unlike `RemoteDesktop`) AND the Wayland protocols that create and
    /// enforce barrier surfaces (`wl_compositor`, `wlr-layer-shell-v1`,
    /// pointer-constraints, relative-pointer -- see
    /// [`AvailableProtocols::has_input_capture_barriers`]). Advertising
    /// capability before the only mechanism that can ever activate a
    /// session is bound would be a lie to the client. Touchscreen is never
    /// advertised -- no multi-touch injection path exists, same rationale
    /// as `RemoteDesktop.AvailableDeviceTypes`.
    #[zbus(property, name = "SupportedCapabilities")]
    async fn supported_capabilities(&self) -> u32 {
        let backend = self.input_backend.lock().await;
        let eis_active = backend.protocol_type() == InputProtocol::Eis;
        drop(backend);

        if eis_active && self.available_protocols.has_input_capture_barriers() {
            DeviceTypes {
                keyboard: true,
                pointer: true,
                touchscreen: false,
            }
            .to_bits()
        } else {
            0
        }
    }

    /// Interface version.
    #[expect(
        clippy::unused_async,
        reason = "zbus requires async for property methods"
    )]
    #[zbus(property, name = "version")]
    async fn version(&self) -> u32 {
        2
    }
}

/// Emit a `ZonesChanged` D-Bus signal.
///
/// Public wrapper around the zbus-generated signal method, allowing the
/// output-hotplug monitor to emit signals from outside the interface
/// implementation, matching `ClipboardInterface::emit_selection_owner_changed`.
///
/// # Errors
///
/// Returns a `zbus::Error` if the D-Bus signal emission fails.
pub async fn emit_zones_changed(
    signal_emitter: &SignalEmitter<'_>,
    session_handle: ObjectPath<'_>,
) -> zbus::Result<()> {
    InputCaptureInterface::zones_changed(signal_emitter, session_handle, empty_results()).await
}

/// Convert a `Vec<u32>` to `OwnedValue` (`au`).
#[expect(
    clippy::expect_used,
    reason = "Value-to-OwnedValue conversion is infallible for a u32 array"
)]
fn owned_value_from_u32_array(values: Vec<u32>) -> OwnedValue {
    OwnedValue::try_from(Value::from(values)).expect("u32 array converts to OwnedValue")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn barrier_dict(id: u32, pos: (i32, i32, i32, i32)) -> HashMap<String, OwnedValue> {
        let mut dict = HashMap::new();
        dict.insert("barrier_id".to_string(), OwnedValue::from(id));
        let position_value = Value::from(pos)
            .try_into()
            .expect("tuple converts to OwnedValue");
        dict.insert("position".to_string(), position_value);
        dict
    }

    #[test]
    fn test_partition_barriers_accepts_valid() {
        let barriers = vec![barrier_dict(1, (0, 100, 1920, 100))];
        let (accepted, failed) = InputCaptureInterface::partition_barriers(&barriers, 5, 5);
        assert_eq!(accepted.len(), 1);
        assert!(failed.is_empty());
        assert_eq!(accepted[0].barrier_id, 1);
    }

    #[test]
    fn test_partition_barriers_rejects_diagonal() {
        let barriers = vec![barrier_dict(1, (0, 0, 100, 100))];
        let (accepted, failed) = InputCaptureInterface::partition_barriers(&barriers, 5, 5);
        assert!(accepted.is_empty());
        assert_eq!(failed, vec![1]);
    }

    #[test]
    fn test_partition_barriers_rejects_zero_id() {
        let barriers = vec![barrier_dict(0, (0, 100, 1920, 100))];
        let (accepted, failed) = InputCaptureInterface::partition_barriers(&barriers, 5, 5);
        assert!(accepted.is_empty());
        assert_eq!(failed, vec![0]);
    }

    #[test]
    fn test_partition_barriers_rejects_duplicate_id() {
        let barriers = vec![
            barrier_dict(1, (0, 100, 1920, 100)),
            barrier_dict(1, (0, 200, 1920, 200)),
        ];
        let (accepted, failed) = InputCaptureInterface::partition_barriers(&barriers, 5, 5);
        assert_eq!(accepted.len(), 1);
        assert_eq!(failed, vec![1]);
    }

    #[test]
    fn test_partition_barriers_stale_zone_set_fails_all() {
        let barriers = vec![
            barrier_dict(1, (0, 100, 1920, 100)),
            barrier_dict(2, (0, 200, 1920, 200)),
        ];
        let (accepted, failed) = InputCaptureInterface::partition_barriers(&barriers, 4, 5);
        assert!(accepted.is_empty());
        assert_eq!(failed.len(), 2);
        assert!(failed.contains(&1));
        assert!(failed.contains(&2));
    }

    #[test]
    fn test_get_option_i32_tuple4_roundtrip() {
        let mut options = HashMap::new();
        let value: OwnedValue = Value::from((10i32, 20i32, 30i32, 40i32))
            .try_into()
            .unwrap();
        options.insert("position".to_string(), value);

        let parsed = InputCaptureInterface::get_option_i32_tuple4(&options, "position");
        assert_eq!(parsed, Some((10, 20, 30, 40)));
    }

    #[test]
    fn test_get_option_i32_tuple4_missing_key() {
        let options = HashMap::new();
        let parsed = InputCaptureInterface::get_option_i32_tuple4(&options, "position");
        assert_eq!(parsed, None);
    }

    #[test]
    fn test_build_zones_results() {
        let zones = vec![InputCaptureZone {
            width: 1920,
            height: 1080,
            x: 0,
            y: 0,
        }];
        let results = InputCaptureInterface::build_zones_results(&zones, 3);
        assert!(results.contains_key("zones"));
        assert_eq!(u32::try_from(&results["zone_set"]).unwrap(), 3);
    }
}
