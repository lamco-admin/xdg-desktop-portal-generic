//! `ScreenCast` D-Bus interface implementation.
//!
//! Implements `org.freedesktop.impl.portal.ScreenCast` version 6.

use std::{collections::HashMap, sync::Arc};

use tokio::sync::Mutex;
use zbus::{ObjectPath, OwnedFd, OwnedVariant, Variant, interface};

use super::{Response, empty_results, get_option_bool, get_option_u32};
use crate::{
    error::PortalError,
    pipewire::PipeWireManager,
    services::{capture::CaptureBackend, input::InputBackend},
    session::{PersistMode, RestoreData, SessionManager},
    types::{CursorMode, SourceInfo, SourceType, StreamInfo},
};

/// `ScreenCast` portal interface implementation.
pub struct ScreenCastInterface {
    /// Session manager.
    session_manager: Arc<Mutex<SessionManager>>,
    /// Capture backend for screen capture operations.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// `PipeWire` manager for stream lifecycle (used by `OpenPipeWireRemote`).
    pipewire_manager: Arc<PipeWireManager>,
    /// Input backend for session cleanup.
    input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    /// How `SelectSources` asks for the user's choice, read from the
    /// environment at startup.
    source_selection: SourceSelectionConfig,
}

impl ScreenCastInterface {
    /// Create a new `ScreenCast` interface with a capture backend.
    ///
    /// Reads `XDP_GENERIC_SOURCE_PICKER` and `XDP_GENERIC_AUTO_SELECT` from
    /// the environment.
    pub fn new(
        session_manager: Arc<Mutex<SessionManager>>,
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        pipewire_manager: Arc<PipeWireManager>,
        input_backend: Arc<Mutex<Box<dyn InputBackend>>>,
    ) -> Self {
        Self {
            session_manager,
            capture_backend,
            pipewire_manager,
            input_backend,
            source_selection: SourceSelectionConfig::from_env(),
        }
    }

    /// Replace the source selection settings read at construction.
    #[cfg(test)]
    pub(crate) fn set_source_selection(&mut self, config: SourceSelectionConfig) {
        self.source_selection = config;
    }

    /// The backends this interface's sessions hold resources in.
    fn session_resources(&self) -> super::SessionResources {
        super::SessionResources::new(
            Arc::clone(&self.input_backend),
            Arc::clone(&self.capture_backend),
            Arc::clone(&self.pipewire_manager),
            None,
        )
    }

    /// Extract `persist_mode` from options.
    fn get_persist_mode(options: &HashMap<String, OwnedVariant>) -> PersistMode {
        get_option_u32(options, "persist_mode").map_or(PersistMode::None, PersistMode::from_dbus)
    }

    /// Extract source types from options.
    fn get_source_types(options: &HashMap<String, OwnedVariant>) -> Vec<SourceType> {
        get_option_u32(options, "types")
            .map_or_else(|| vec![SourceType::Monitor], SourceType::from_bits)
    }

    /// Extract cursor mode from options.
    fn get_cursor_mode(options: &HashMap<String, OwnedVariant>) -> CursorMode {
        get_option_u32(options, "cursor_mode")
            .map_or_else(CursorMode::default, CursorMode::from_bits)
    }

    /// Extract multiple sources flag.
    fn get_multiple(options: &HashMap<String, OwnedVariant>) -> bool {
        get_option_bool(options, "multiple").unwrap_or(false)
    }

    /// Try to parse `restore_data` from D-Bus options.
    ///
    /// The `restore_data` option is a `(suv)` tuple: (vendor, version, data).
    /// We only accept vendor `"generic"` and version
    /// [`RESTORE_DATA_VERSION`], whose data is the token of a restore grant.
    /// Version 1, which carried output names, is ignored: it was never
    /// backed by anything the backend held, so anyone could forge it.
    fn parse_restore_data(options: &HashMap<String, OwnedVariant>) -> Option<RestoreData> {
        let rd = options.get("restore_data")?;
        let value: &Variant<'_> = rd.downcast_ref().ok()?;
        let Variant::Structure(s) = value else {
            return None;
        };
        let fields = s.fields();
        if fields.len() != 3 {
            return None;
        }
        let vendor: &str = fields[0].downcast_ref().ok()?;
        if vendor != "generic" {
            tracing::debug!(vendor, "Unknown restore_data vendor, ignoring");
            return None;
        }
        let version = u32::try_from(&fields[1]).ok()?;
        if version != RESTORE_DATA_VERSION {
            tracing::debug!(version, "Unsupported restore_data version, ignoring");
            return None;
        }
        let Variant::Variant(inner) = &fields[2] else {
            return None;
        };
        let Variant::Str(token) = inner.as_ref() else {
            return None;
        };
        Some(RestoreData {
            vendor: vendor.to_string(),
            version,
            token: token.to_string(),
        })
    }

    /// Build stream results for D-Bus response.
    ///
    /// Includes `position`/`size`, `source_type`, the v5 `mapping_id`
    /// (persistent output identifier), and — when the compositor's PipeWire
    /// assigns one — the v6 `pipewire-serial` (`object.serial`) so clients can
    /// re-follow the stream across output reconfiguration.
    #[expect(
        clippy::expect_used,
        reason = "infallible zvariant Variant-to-OwnedVariant conversions"
    )]
    fn build_stream_results(streams: &[StreamInfo]) -> HashMap<String, OwnedVariant> {
        let stream_data: Vec<(u32, HashMap<String, OwnedVariant>)> = streams
            .iter()
            .map(|s| {
                let mut props: HashMap<String, OwnedVariant> = HashMap::new();
                props.insert(
                    "position".to_string(),
                    OwnedVariant::try_from(Variant::from((s.position.0, s.position.1)))
                        .expect("tuple Variant converts to OwnedVariant"),
                );
                props.insert(
                    "size".to_string(),
                    OwnedVariant::try_from(Variant::from((s.size.0, s.size.1)))
                        .expect("tuple Variant converts to OwnedVariant"),
                );
                props.insert(
                    "source_type".to_string(),
                    OwnedVariant::from(s.source_type.to_bits()),
                );
                if let Some(ref mapping_id) = s.mapping_id
                    && let Ok(val) = OwnedVariant::try_from(Variant::from(mapping_id.as_str()))
                {
                    props.insert("mapping_id".to_string(), val);
                }
                // ScreenCast v6: the PipeWire object.serial, emitted only when
                // present (it is filtered to a non-zero value upstream).
                if let Some(serial) = s.serial {
                    props.insert("pipewire-serial".to_string(), OwnedVariant::from(serial));
                }
                (s.node_id, props)
            })
            .collect();

        let mut results = HashMap::new();
        results.insert(
            "streams".to_string(),
            OwnedVariant::try_from(Variant::from(stream_data))
                .expect("stream data Variant converts to OwnedVariant"),
        );
        results
    }
}

#[interface(
    name = "org.freedesktop.impl.portal.ScreenCast",
    introspection_docs = false
)]
impl ScreenCastInterface {
    /// Create a new `ScreenCast` session.
    #[zbus(name = "CreateSession")]
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            app_id = %app_id,
            sender = %sender,
            "ScreenCast.CreateSession called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface =
            super::RequestInterface::new(Arc::clone(&self.session_manager), sender.clone());
        let request = super::RequestGuard::register(server, &handle, request_iface).await?;

        let persist_mode = Self::get_persist_mode(&options);

        let mut manager = self.session_manager.lock().await;

        let result = match manager.create_session(
            session_handle.to_owned(),
            sender,
            app_id.to_string(),
            persist_mode,
        ) {
            Ok(_session) => {
                // Register a Session D-Bus object at the session handle path
                let session_iface = super::SessionInterface::new(
                    Arc::clone(&self.session_manager),
                    session_handle.to_owned(),
                    self.session_resources(),
                );
                if let Err(e) = server.at(&session_handle, session_iface).await {
                    tracing::warn!(
                        session_handle = %session_handle,
                        error = %e,
                        "Failed to register Session D-Bus object"
                    );
                }

                let mut results = HashMap::new();
                results.insert(
                    "session_handle".to_string(),
                    OwnedVariant::from(session_handle.to_owned()),
                );
                Ok((Response::Success.to_u32(), results))
            }
            Err(e) => {
                tracing::error!(error = %e, "ScreenCast.CreateSession failed");
                Ok((Response::Other.to_u32(), empty_results()))
            }
        };

        // Remove Request object after method completes
        request.finish().await;
        result
    }

    /// Select sources for capture.
    #[zbus(name = "SelectSources")]
    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            app_id = %app_id,
            "SelectSources called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
            sender.clone(),
            self.session_resources(),
        );
        let request = super::RequestGuard::register(server, &handle, request_iface).await?;

        let source_types = Self::get_source_types(&options);
        let multiple = Self::get_multiple(&options);
        let restore_data = Self::parse_restore_data(&options);
        let persist_mode = Self::get_persist_mode(&options);

        // Check the caller, and redeem a restore grant if one is offered,
        // before asking anyone anything.
        let restored_names = {
            let mut manager = self.session_manager.lock().await;
            manager.validate_session(&session_handle, app_id, &sender)?;
            restore_data
                .as_ref()
                .and_then(|rd| manager.redeem_restore_grant(app_id, &rd.token))
        };
        if restore_data.is_some() && restored_names.is_none() {
            tracing::info!(
                app_id = %app_id,
                "Restore data names no grant this backend holds for the app, asking the user"
            );
        }

        // Get available sources from capture backend
        let sources = self
            .capture_backend
            .lock()
            .await
            .get_sources(&source_types)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        if sources.is_empty() {
            tracing::warn!("No sources available for capture");
            request.finish().await;
            return Ok((Response::Other.to_u32(), empty_results()));
        }

        let restored: Vec<SourceInfo> = restored_names
            .iter()
            .flatten()
            .filter_map(|name| sources.iter().find(|s| s.name == *name).cloned())
            .collect();

        let selection = if restored.is_empty() {
            // The picker waits on the user, so it runs on a blocking thread and
            // without the session manager lock.
            let config = self.source_selection.clone();
            tokio::task::spawn_blocking(move || {
                select_sources_interactively(&sources, multiple, &config)
            })
            .await
            .map_err(|e| zbus::fdo::Error::Failed(format!("Source selection failed: {e}")))?
        } else {
            tracing::info!(
                count = restored.len(),
                names = ?restored.iter().map(|s| &s.name).collect::<Vec<_>>(),
                "Restored sources from a restore grant"
            );
            SourceSelection::Selected(restored)
        };

        let SourceSelection::Selected(selected_sources) = selection else {
            request.finish().await;
            return Ok((Response::Cancelled.to_u32(), empty_results()));
        };

        let mut manager = self.session_manager.lock().await;
        // The session may have ended while the user was choosing.
        manager.validate_session(&session_handle, app_id, &sender)?;
        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        session.persist_mode = persist_mode;
        session.restore_data = restore_data;
        session.select_sources(selected_sources)?;
        drop(manager);

        // Remove Request object after method completes
        request.finish().await;

        Ok((Response::Success.to_u32(), empty_results()))
    }

    /// Start the `ScreenCast` session.
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
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedVariant>)> {
        let _ = parent_window;
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("Missing sender".to_string()))?
            .to_string();

        tracing::debug!(
            handle = %handle,
            session_handle = %session_handle,
            app_id = %app_id,
            "ScreenCast.Start called"
        );

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::for_session(
            Arc::clone(&self.session_manager),
            session_handle.to_string(),
            sender.clone(),
            self.session_resources(),
        );
        let request = super::RequestGuard::register(server, &handle, request_iface).await?;

        let cursor_mode = Self::get_cursor_mode(&options);

        let mut manager = self.session_manager.lock().await;
        manager.validate_session(&session_handle, app_id, &sender)?;

        let session = manager
            .get_session_mut(&session_handle)
            .ok_or_else(|| PortalError::SessionNotFound(session_handle.to_string()))?;

        if !session.sources_selected {
            return Err(PortalError::InvalidState {
                expected: "Sources selected".to_string(),
                actual: "No sources selected".to_string(),
            }
            .into());
        }

        let sources = session.sources.clone();
        let persist_mode = session.persist_mode;

        // Create capture streams via capture backend
        let mut backend = self.capture_backend.lock().await;
        let streams = backend
            .create_capture_session(&sources, cursor_mode)
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        drop(backend);

        // Start the session with streams
        session.start(streams.clone())?;

        let mut results = Self::build_stream_results(&streams);

        // If persist_mode is set, record the selection as a restore grant and
        // hand back its token as restore_data.
        let output_names: Vec<String> = sources.iter().map(|s| s.name.clone()).collect();
        if let Some(token) = manager.issue_restore_grant(app_id, output_names, persist_mode)
            && let Some(rd) = restore_data_value(&token)
        {
            results.insert("restore_data".to_string(), rd);
            results.insert(
                "persist_mode".to_string(),
                OwnedVariant::from(persist_mode.to_dbus()),
            );
        }
        drop(manager);

        tracing::info!(
            session_id = %session_handle,
            stream_count = streams.len(),
            persist = ?persist_mode,
            "ScreenCast session started"
        );

        // Remove Request object after method completes
        request.finish().await;

        Ok((Response::Success.to_u32(), results))
    }

    /// Open a `PipeWire` remote for the `ScreenCast` session.
    ///
    /// Returns a file descriptor that the client can use to connect to `PipeWire`
    /// and access the screen capture stream nodes. This is required by the
    /// xdg-desktop-portal spec for `ScreenCast` version 4+.
    #[zbus(name = "OpenPipeWireRemote")]
    async fn open_pipe_wire_remote(
        &self,
        session_handle: ObjectPath<'_>,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedFd> {
        let _ = &options;
        let sender = super::caller(&header)?;
        tracing::debug!(
            session_handle = %session_handle,
            sender = %sender,
            "OpenPipeWireRemote called"
        );

        // The fd is a general PipeWire client connection, so hand it only to
        // the peer that owns a started session.
        let manager = self.session_manager.lock().await;
        let session = manager.session_for_caller(&session_handle, &sender)?;

        if !session.is_started() {
            return Err(PortalError::InvalidState {
                expected: "Session started with streams".to_string(),
                actual: "Session not started".to_string(),
            }
            .into());
        }

        drop(manager);

        // Get a PipeWire remote fd from the manager
        let fd = self
            .pipewire_manager
            .open_remote()
            .await
            .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

        tracing::info!(
            session_handle = %session_handle,
            "PipeWire remote fd opened for client"
        );

        Ok(OwnedFd::from(fd))
    }

    // === Properties ===

    /// Available source types.
    #[zbus(property, name = "AvailableSourceTypes")]
    async fn available_source_types(&self) -> u32 {
        let backend = self.capture_backend.lock().await;
        backend.available_source_types()
    }

    /// Available cursor modes.
    #[zbus(property, name = "AvailableCursorModes")]
    async fn available_cursor_modes(&self) -> u32 {
        let backend = self.capture_backend.lock().await;
        backend.available_cursor_modes()
    }

    /// Interface version.
    #[zbus(property, name = "version")]
    #[expect(
        clippy::unused_async,
        clippy::unused_async_trait_impl,
        reason = "zbus interface requires async"
    )]
    async fn version(&self) -> u32 {
        6
    }
}

/// Format version of the `restore_data` this backend issues and accepts.
const RESTORE_DATA_VERSION: u32 = 2;

/// Build `restore_data` (`(suv)`: vendor, version, token) for a restore grant.
pub(super) fn restore_data_value(token: &str) -> Option<OwnedVariant> {
    OwnedVariant::try_from(Variant::from((
        "generic",
        RESTORE_DATA_VERSION,
        Variant::from(token),
    )))
    .ok()
}

/// What `SelectSources` selects when no picker is configured.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum AutoSelect {
    /// Select nothing, so the request is cancelled. The default: capturing
    /// the screen without anyone choosing to is not something to fall into.
    #[default]
    Disabled,
    /// Select the first source (`XDP_GENERIC_AUTO_SELECT=first`).
    First,
}

impl AutoSelect {
    /// Parse a `XDP_GENERIC_AUTO_SELECT` value.
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") => Self::Disabled,
            Some("first") => Self::First,
            Some(other) => {
                tracing::warn!(
                    value = other,
                    "Unknown XDP_GENERIC_AUTO_SELECT value (expected \"first\"), auto-select stays off"
                );
                Self::Disabled
            }
        }
    }
}

/// How `SelectSources` gets the user's choice of sources.
#[derive(Debug, Clone, Default)]
pub(crate) struct SourceSelectionConfig {
    /// External picker command (`XDP_GENERIC_SOURCE_PICKER`).
    pub(crate) picker: Option<String>,
    /// What to select when no picker is configured
    /// (`XDP_GENERIC_AUTO_SELECT`).
    pub(crate) auto_select: AutoSelect,
}

impl SourceSelectionConfig {
    /// Read the settings from the environment.
    pub(crate) fn from_env() -> Self {
        Self {
            picker: std::env::var("XDP_GENERIC_SOURCE_PICKER")
                .ok()
                .filter(|cmd| !cmd.trim().is_empty()),
            auto_select: AutoSelect::parse(
                std::env::var("XDP_GENERIC_AUTO_SELECT").ok().as_deref(),
            ),
        }
    }
}

/// The outcome of asking for sources.
#[derive(Debug)]
enum SourceSelection {
    /// The sources to capture.
    Selected(Vec<SourceInfo>),
    /// Nothing was chosen; the request ends with `Response::Cancelled`.
    Cancelled,
}

/// Ask the user for sources through the configured picker.
///
/// # Configuration
///
/// - `XDP_GENERIC_SOURCE_PICKER`: path to an external source picker tool.
///   The tool receives source names (one per line) on stdin and should write
///   the selected source name(s) to stdout (one per line).
/// - `XDP_GENERIC_AUTO_SELECT=first`: with no picker configured, select the
///   first source without asking.
///
/// A picker that exits with a non-zero status, can't be started, or selects
/// nothing cancels the request; so does having neither a picker nor
/// auto-select. Auto-select is never a fallback for a picker.
///
/// # Multi-select
///
/// When `multiple` is true, the picker may return multiple selections.
/// When false, only the first selection is used.
fn select_sources_interactively(
    sources: &[SourceInfo],
    multiple: bool,
    config: &SourceSelectionConfig,
) -> SourceSelection {
    if let Some(picker_cmd) = &config.picker {
        return match run_source_picker(picker_cmd, sources) {
            Ok(selected) if !selected.is_empty() => {
                let result: Vec<SourceInfo> = if multiple {
                    selected
                } else {
                    selected.into_iter().take(1).collect()
                };
                tracing::info!(
                    count = result.len(),
                    sources = ?result.iter().map(|s| &s.name).collect::<Vec<_>>(),
                    "Sources selected via picker"
                );
                SourceSelection::Selected(result)
            }
            Ok(_) => {
                tracing::info!("Source picker selected nothing, cancelling");
                SourceSelection::Cancelled
            }
            Err(e) => {
                tracing::info!(
                    error = %e,
                    picker = %picker_cmd,
                    "Source picker was cancelled or failed, cancelling"
                );
                SourceSelection::Cancelled
            }
        };
    }

    match (config.auto_select, sources.first()) {
        (AutoSelect::First, Some(first)) => {
            tracing::info!(
                source = %first.name,
                "No source picker configured, XDP_GENERIC_AUTO_SELECT=first selects the first source"
            );
            SourceSelection::Selected(vec![first.clone()])
        }
        (AutoSelect::First, None) => SourceSelection::Cancelled,
        (AutoSelect::Disabled, _) => {
            tracing::warn!(
                "No source picker configured (XDP_GENERIC_SOURCE_PICKER) and auto-select is off \
                 (XDP_GENERIC_AUTO_SELECT), so the screen cast request is cancelled"
            );
            SourceSelection::Cancelled
        }
    }
}

/// Run an external source picker tool.
///
/// Writes available source names to the tool's stdin and reads selected
/// source name(s) from stdout.
fn run_source_picker(picker_cmd: &str, sources: &[SourceInfo]) -> Result<Vec<SourceInfo>, String> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    // Build the input: one source per line as "name\tdescription\tWxH"
    let input: String = sources
        .iter()
        .map(|s| format!("{}\t{}\t{}x{}", s.name, s.description, s.width, s.height))
        .collect::<Vec<_>>()
        .join("\n");

    // Parse the command (support basic shell-like splitting)
    let parts: Vec<&str> = picker_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Err("Empty picker command".to_string());
    }

    let mut cmd = Command::new(parts[0]);
    for arg in &parts[1..] {
        cmd.arg(arg);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn picker: {e}"))?;

    // Write source list to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Picker process error: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "Picker exited with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    // Parse selected source names from stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    let selected_names: Vec<&str> = stdout
        .lines()
        .map(|line| {
            // Support tab-separated format (take first column = name)
            line.split('\t').next().unwrap_or(line).trim()
        })
        .filter(|name| !name.is_empty())
        .collect();

    // Match selected names to source objects
    let selected: Vec<SourceInfo> = selected_names
        .iter()
        .filter_map(|name| sources.iter().find(|s| s.name == *name).cloned())
        .collect();

    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SourceInfo;

    fn test_sources() -> Vec<SourceInfo> {
        vec![
            SourceInfo {
                id: 1,
                name: "eDP-1".to_string(),
                description: "Built-in Display".to_string(),
                width: 1920,
                height: 1080,
                refresh_rate: 60000,
                x: 0,
                y: 0,
                scale: 1,
                source_type: SourceType::Monitor,
            },
            SourceInfo {
                id: 2,
                name: "HDMI-A-1".to_string(),
                description: "External Monitor".to_string(),
                width: 2560,
                height: 1440,
                refresh_rate: 60000,
                x: 1920,
                y: 0,
                scale: 1,
                source_type: SourceType::Monitor,
            },
        ]
    }

    fn config(picker: Option<&str>, auto_select: AutoSelect) -> SourceSelectionConfig {
        SourceSelectionConfig {
            picker: picker.map(str::to_string),
            auto_select,
        }
    }

    fn selected_names(selection: &SourceSelection) -> Vec<&str> {
        match selection {
            SourceSelection::Selected(sources) => sources.iter().map(|s| s.name.as_str()).collect(),
            SourceSelection::Cancelled => panic!("expected a selection, got Cancelled"),
        }
    }

    #[test]
    fn test_no_picker_and_no_auto_select_cancels() {
        let selection = select_sources_interactively(
            &test_sources(),
            false,
            &config(None, AutoSelect::Disabled),
        );
        assert!(matches!(selection, SourceSelection::Cancelled));
    }

    #[test]
    fn test_auto_select_first_source() {
        let selection =
            select_sources_interactively(&test_sources(), false, &config(None, AutoSelect::First));
        assert_eq!(selected_names(&selection), ["eDP-1"]);
    }

    #[test]
    fn test_auto_select_empty_sources() {
        let selection = select_sources_interactively(&[], false, &config(None, AutoSelect::First));
        assert!(matches!(selection, SourceSelection::Cancelled));
    }

    #[test]
    fn test_picker_cancel_is_never_auto_selected() {
        let sources = test_sources();
        // Non-zero exit, a picker that can't be started, an empty answer and an
        // answer naming no source all cancel, auto-select or not.
        for picker in ["false", "/nonexistent/picker/tool", "true", "echo NOPE-1"] {
            for auto in [AutoSelect::Disabled, AutoSelect::First] {
                let selection =
                    select_sources_interactively(&sources, true, &config(Some(picker), auto));
                assert!(
                    matches!(selection, SourceSelection::Cancelled),
                    "picker {picker:?}"
                );
            }
        }
    }

    #[test]
    fn test_picker_selection() {
        let sources = test_sources();
        let both = config(Some("printf eDP-1\\nHDMI-A-1\\n"), AutoSelect::Disabled);
        assert_eq!(
            selected_names(&select_sources_interactively(&sources, true, &both)),
            ["eDP-1", "HDMI-A-1"]
        );
        assert_eq!(
            selected_names(&select_sources_interactively(&sources, false, &both)),
            ["eDP-1"]
        );
    }

    #[test]
    fn test_auto_select_parse() {
        assert_eq!(AutoSelect::parse(None), AutoSelect::Disabled);
        assert_eq!(AutoSelect::parse(Some("")), AutoSelect::Disabled);
        assert_eq!(AutoSelect::parse(Some("first")), AutoSelect::First);
        assert_eq!(AutoSelect::parse(Some("yes")), AutoSelect::Disabled);
    }

    #[test]
    fn test_restore_data_round_trip() {
        let mut options = HashMap::new();
        options.insert(
            "restore_data".to_string(),
            restore_data_value("abc123").unwrap(),
        );
        let parsed = ScreenCastInterface::parse_restore_data(&options).unwrap();
        assert_eq!(parsed.vendor, "generic");
        assert_eq!(parsed.version, RESTORE_DATA_VERSION);
        assert_eq!(parsed.token, "abc123");
    }

    #[test]
    fn test_version_1_restore_data_is_ignored() {
        let names = Variant::from(vec!["eDP-1".to_string()]);
        let mut options = HashMap::new();
        options.insert(
            "restore_data".to_string(),
            OwnedVariant::try_from(Variant::from(("generic", 1u32, names))).unwrap(),
        );
        assert!(ScreenCastInterface::parse_restore_data(&options).is_none());
    }

    #[test]
    fn test_picker_with_echo() {
        // Use echo as a trivial picker that outputs its argument
        let sources = test_sources();
        let selected = run_source_picker("echo HDMI-A-1", &sources);
        assert!(selected.is_ok());
        let selected = selected.unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].name, "HDMI-A-1");
    }

    #[test]
    fn test_picker_nonexistent_tool() {
        let sources = test_sources();
        let result = run_source_picker("/nonexistent/picker/tool", &sources);
        assert!(result.is_err());
    }

    #[test]
    fn test_picker_failing_tool() {
        let sources = test_sources();
        let result = run_source_picker("false", &sources);
        assert!(result.is_err());
    }
}
