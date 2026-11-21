//! Tests that drive the portal interfaces over a real, private bus.
//!
//! Caller checks are about the unique name a bus daemon writes into each
//! method call's header, which a peer-to-peer connection never carries, so
//! each test starts its own `dbus-daemon`. Where `dbus-daemon` isn't
//! installed a test prints a note and returns without asserting anything.

use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex as StdMutex},
};

use tokio::sync::Mutex;
use zbus::{Connection, ObjectPath, OwnedVariant, Variant};

use super::{
    ClipboardInterface, RemoteDesktopInterface, ScreenCastInterface, SessionInterface,
    SessionResources,
};
use crate::{
    error::Result,
    pipewire::PipeWireManager,
    services::{
        capture::{CaptureBackend, CaptureProtocol},
        input::{InputBackend, InputProtocol},
    },
    session::SessionManager,
    types::{CursorMode, DeviceTypes, InputEvent, SourceInfo, SourceType, StreamInfo},
    wayland::AvailableProtocols,
};

const NAME: &str = "org.freedesktop.impl.portal.desktop.generic";
const PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.impl.portal.RemoteDesktop";
const SCREEN_CAST: &str = "org.freedesktop.impl.portal.ScreenCast";
const CLIPBOARD: &str = "org.freedesktop.impl.portal.Clipboard";
const SESSION: &str = "org.freedesktop.impl.portal.Session";
const ACCESS_DENIED: &str = "org.freedesktop.DBus.Error.AccessDenied";
const APP_ID: &str = "org.example.App";

/// A private `dbus-daemon`, killed on drop.
struct Bus {
    daemon: Child,
    address: String,
}

impl Bus {
    fn start() -> Option<Self> {
        let mut daemon = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut line = String::new();
        let read = daemon
            .stdout
            .take()
            .map(|out| BufReader::new(out).read_line(&mut line));
        let address = line.trim().to_string();
        if !matches!(read, Some(Ok(_))) || address.is_empty() {
            let _ = daemon.kill();
            let _ = daemon.wait();
            return None;
        }
        Some(Self { daemon, address })
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.daemon.kill();
        let _ = self.daemon.wait();
    }
}

/// What the mock backends were asked to do.
#[derive(Default, Clone)]
struct Recorded {
    injected: Arc<StdMutex<Vec<(String, InputEvent)>>>,
    destroyed_contexts: Arc<StdMutex<Vec<String>>>,
    destroyed_streams: Arc<StdMutex<Vec<Vec<u32>>>>,
}

struct MockInput(Recorded);

impl InputBackend for MockInput {
    fn protocol_type(&self) -> InputProtocol {
        InputProtocol::WlrVirtualInput
    }

    fn create_context(
        &mut self,
        _session_id: &str,
        _devices: DeviceTypes,
    ) -> Result<Option<std::os::unix::io::OwnedFd>> {
        Ok(None)
    }

    fn destroy_context(&mut self, session_id: &str) -> Result<()> {
        self.0
            .destroyed_contexts
            .lock()
            .unwrap()
            .push(session_id.to_string());
        Ok(())
    }

    fn inject_event(&mut self, session_id: &str, event: InputEvent) -> Result<()> {
        self.0
            .injected
            .lock()
            .unwrap()
            .push((session_id.to_string(), event));
        Ok(())
    }

    fn process_events(&mut self) -> Result<Vec<(String, InputEvent)>> {
        Ok(Vec::new())
    }

    fn has_context(&self, _session_id: &str) -> bool {
        false
    }

    fn context_count(&self) -> usize {
        0
    }

    fn keysym_to_keycode(&mut self, _keysym: u32) -> Option<u32> {
        Some(30)
    }
}

struct MockCapture {
    recorded: Recorded,
    sources: Vec<SourceInfo>,
}

impl CaptureBackend for MockCapture {
    fn protocol_type(&self) -> CaptureProtocol {
        CaptureProtocol::WlrScreencopy
    }

    fn get_sources(&self, _source_types: &[SourceType]) -> Result<Vec<SourceInfo>> {
        Ok(self.sources.clone())
    }

    fn create_capture_session(
        &mut self,
        sources: &[SourceInfo],
        _cursor_mode: CursorMode,
    ) -> Result<Vec<StreamInfo>> {
        Ok(sources
            .iter()
            .map(|s| StreamInfo {
                node_id: 100 + s.id,
                serial: None,
                source_id: s.id,
                position: (s.x, s.y),
                size: (s.width, s.height),
                source_type: s.source_type,
                mapping_id: None,
                scale: 1,
                properties: HashMap::new(),
            })
            .collect())
    }

    fn destroy_capture_session(&mut self, stream_ids: &[u32]) -> Result<()> {
        self.recorded
            .destroyed_streams
            .lock()
            .unwrap()
            .push(stream_ids.to_vec());
        Ok(())
    }

    fn update_sources(&mut self, sources: Vec<SourceInfo>) {
        self.sources = sources;
    }
}

fn test_source(id: u32, name: &str) -> SourceInfo {
    SourceInfo {
        id,
        name: name.to_string(),
        description: format!("Output {name}"),
        width: 1920,
        height: 1080,
        refresh_rate: 60000,
        x: 0,
        y: 0,
        scale: 1,
        source_type: SourceType::Monitor,
    }
}

/// The portal interfaces served on a private bus, with mock backends.
struct Portal {
    /// The portal's own connection.
    server: Connection,
    session_manager: Arc<Mutex<SessionManager>>,
    resources: SessionResources,
    recorded: Recorded,
    address: String,
    // Declared last so the connections above close before the daemon dies.
    _bus: Bus,
}

impl Portal {
    async fn start() -> Option<Self> {
        let Some(bus) = Bus::start() else {
            eprintln!("dbus-daemon is not available; skipping this bus test");
            return None;
        };
        let recorded = Recorded::default();
        let session_manager = Arc::new(Mutex::new(SessionManager::new()));
        let input: Arc<Mutex<Box<dyn InputBackend>>> =
            Arc::new(Mutex::new(Box::new(MockInput(recorded.clone()))));
        let capture: Arc<Mutex<Box<dyn CaptureBackend>>> =
            Arc::new(Mutex::new(Box::new(MockCapture {
                recorded: recorded.clone(),
                sources: vec![test_source(1, "eDP-1"), test_source(2, "HDMI-A-1")],
            })));
        let pipewire = Arc::new(PipeWireManager::detached());
        let resources = SessionResources::new(
            Arc::clone(&input),
            Arc::clone(&capture),
            Arc::clone(&pipewire),
            None,
        );

        let server = zbus::connection::Builder::address(bus.address.as_str())
            .name(NAME)
            .serve_at(
                PATH,
                RemoteDesktopInterface::new(
                    Arc::clone(&session_manager),
                    Arc::clone(&input),
                    Arc::clone(&capture),
                    Arc::clone(&pipewire),
                    AvailableProtocols::default(),
                ),
            )
            .serve_at(
                PATH,
                ScreenCastInterface::new(
                    Arc::clone(&session_manager),
                    Arc::clone(&capture),
                    Arc::clone(&pipewire),
                    Arc::clone(&input),
                ),
            )
            .serve_at(
                PATH,
                ClipboardInterface::new(Arc::clone(&session_manager), None),
            )
            .build()
            .await
            .unwrap();

        Some(Self {
            server,
            session_manager,
            resources,
            recorded,
            address: bus.address.clone(),
            _bus: bus,
        })
    }

    /// A new, separate peer on the bus.
    async fn client(&self) -> Connection {
        zbus::connection::Builder::address(self.address.as_str())
            .build()
            .await
            .unwrap()
    }

    /// Whether a `Session` object is still registered at `session`.
    async fn session_object_exists(&self, session: &str) -> bool {
        self.server
            .object_server()
            .interface::<_, SessionInterface>(path(session))
            .await
            .is_ok()
    }

    /// Make a session look as if it was created (and started) a day ago.
    async fn backdate(&self, session: &str) {
        let day_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(86_400);
        let mut manager = self.session_manager.lock().await;
        let session = manager.get_session_mut(&path(session)).unwrap();
        session.created_at = day_ago;
        session.last_activity = day_ago;
        if session.started_at.is_some() {
            session.started_at = Some(day_ago);
        }
    }

    async fn expire(&self) -> usize {
        super::expire_stale_sessions(
            self.server.object_server(),
            &self.session_manager,
            &self.resources,
            std::time::Duration::from_secs(300),
        )
        .await
    }

    async fn session_exists(&self, session: &str) -> bool {
        let handle = ObjectPath::try_from(session).unwrap();
        self.session_manager
            .lock()
            .await
            .get_session(&handle)
            .is_some()
    }
}

fn path(p: &str) -> ObjectPath<'_> {
    ObjectPath::try_from(p).unwrap()
}

fn no_options() -> HashMap<&'static str, Variant<'static>> {
    HashMap::new()
}

/// The D-Bus error name of a failed call, or `None` if it succeeded.
fn error_name(result: &zbus::Result<zbus::Message>) -> Option<String> {
    match result {
        Ok(_) => None,
        Err(zbus::Error::MethodError(name, _, _)) => Some(name.to_string()),
        Err(zbus::Error::FDO(e)) => Some(zbus::DBusError::name(e.as_ref()).to_string()),
        Err(e) => Some(format!("non-method error: {e}")),
    }
}

async fn call<B>(
    conn: &Connection,
    object: &str,
    interface: &str,
    method: &str,
    body: &B,
) -> zbus::Result<zbus::Message>
where
    B: zbus::export::serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    conn.call_method(Some(NAME), path(object), Some(interface), method, body)
        .await
}

/// Response code of a `(ua{sv})` portal reply.
fn response_code(reply: &zbus::Message) -> u32 {
    let (code, _): (u32, HashMap<String, OwnedVariant>) = reply.body().deserialize().unwrap();
    code
}

/// Create a RemoteDesktop session as `owner`, leaving it in `Init`.
async fn create_remote_desktop(owner: &Connection, tag: &str) -> String {
    let session = format!("/org/freedesktop/portal/desktop/session/test/{tag}");
    let reply = call(
        owner,
        PATH,
        REMOTE_DESKTOP,
        "CreateSession",
        &(
            path(&format!(
                "/org/freedesktop/portal/desktop/request/test/{tag}_c"
            )),
            path(&session),
            APP_ID,
            no_options(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response_code(&reply), 0);
    session
}

/// Select pointer and keyboard on `owner`'s session.
async fn select_devices(owner: &Connection, session: &str, tag: &str) {
    let mut options = no_options();
    options.insert("types", Variant::from(3u32));
    let reply = call(
        owner,
        PATH,
        REMOTE_DESKTOP,
        "SelectDevices",
        &(
            path(&format!(
                "/org/freedesktop/portal/desktop/request/test/{tag}_d"
            )),
            path(session),
            APP_ID,
            options,
        ),
    )
    .await
    .unwrap();
    assert_eq!(response_code(&reply), 0);
}

/// Create, select devices on and start a RemoteDesktop session as `owner`.
async fn started_remote_desktop(owner: &Connection, tag: &str) -> String {
    let session = create_remote_desktop(owner, tag).await;
    select_devices(owner, &session, tag).await;
    let reply = call(
        owner,
        PATH,
        REMOTE_DESKTOP,
        "Start",
        &(
            path(&format!(
                "/org/freedesktop/portal/desktop/request/test/{tag}_s"
            )),
            path(&session),
            APP_ID,
            "",
            no_options(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response_code(&reply), 0);
    session
}

#[tokio::test]
async fn notify_from_another_peer_is_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = started_remote_desktop(&owner, "notify").await;

    let body = (path(&session), no_options(), 5.0f64, 5.0f64);
    let refused = call(&other, PATH, REMOTE_DESKTOP, "NotifyPointerMotion", &body).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));

    let keys = (path(&session), no_options(), 30i32, 1u32);
    let refused = call(&other, PATH, REMOTE_DESKTOP, "NotifyKeyboardKeycode", &keys).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));
    assert!(portal.recorded.injected.lock().unwrap().is_empty());

    let allowed = call(&owner, PATH, REMOTE_DESKTOP, "NotifyPointerMotion", &body).await;
    assert_eq!(error_name(&allowed), None);
    assert_eq!(portal.recorded.injected.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn notify_before_start_is_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let session = create_remote_desktop(&owner, "early").await;
    select_devices(&owner, &session, "early").await;

    let body = (path(&session), no_options(), 5.0f64, 5.0f64);
    let refused = call(&owner, PATH, REMOTE_DESKTOP, "NotifyPointerMotion", &body).await;
    assert!(error_name(&refused).is_some());
    assert!(portal.recorded.injected.lock().unwrap().is_empty());
}

#[tokio::test]
async fn open_pipewire_remote_from_another_peer_is_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = started_remote_desktop(&owner, "remote").await;

    let body = (path(&session), no_options());
    let refused = call(&other, PATH, SCREEN_CAST, "OpenPipeWireRemote", &body).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));

    // The owner gets past the check; a detached PipeWire manager then has
    // no remote to give it, which fails differently.
    let owner_result = call(&owner, PATH, SCREEN_CAST, "OpenPipeWireRemote", &body).await;
    let owner_error = error_name(&owner_result);
    assert!(owner_error.is_some());
    assert_ne!(owner_error.as_deref(), Some(ACCESS_DENIED));
}

#[tokio::test]
async fn session_close_from_another_peer_is_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = started_remote_desktop(&owner, "close").await;

    let refused = call(&other, &session, SESSION, "Close", &()).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));
    assert!(portal.session_exists(&session).await);

    let closed = call(&owner, &session, SESSION, "Close", &()).await;
    assert_eq!(error_name(&closed), None);
    assert!(!portal.session_exists(&session).await);
}

#[tokio::test]
async fn clipboard_from_another_peer_is_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = create_remote_desktop(&owner, "clip").await;

    let request = (path(&session), no_options());
    let refused = call(&other, PATH, CLIPBOARD, "RequestClipboard", &request).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));

    let allowed = call(&owner, PATH, CLIPBOARD, "RequestClipboard", &request).await;
    assert_eq!(error_name(&allowed), None);

    let mut options = no_options();
    options.insert("mime_types", Variant::from(vec!["text/plain"]));
    let selection = (path(&session), options);
    let refused = call(&other, PATH, CLIPBOARD, "SetSelection", &selection).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));

    let read = (path(&session), "text/plain");
    let refused = call(&other, PATH, CLIPBOARD, "SelectionRead", &read).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));

    let done = (path(&session), 1u32, true);
    let refused = call(&other, PATH, CLIPBOARD, "SelectionWriteDone", &done).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));

    let write = (path(&session), 1u32);
    let refused = call(&other, PATH, CLIPBOARD, "SelectionWrite", &write).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));
}

#[tokio::test]
async fn expired_init_session_gets_full_teardown() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let session = create_remote_desktop(&owner, "stale").await;
    assert!(portal.session_object_exists(&session).await);

    // Not yet stale: nothing happens.
    assert_eq!(portal.expire().await, 0);

    portal.backdate(&session).await;
    assert_eq!(portal.expire().await, 1);

    assert!(!portal.session_exists(&session).await);
    assert!(!portal.session_object_exists(&session).await);
    assert_eq!(
        portal
            .recorded
            .destroyed_contexts
            .lock()
            .unwrap()
            .as_slice(),
        std::slice::from_ref(&session)
    );
}

#[tokio::test]
async fn started_session_survives_expiry_and_close_tears_it_down() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let session = create_remote_desktop(&owner, "long").await;
    select_devices(&owner, &session, "long").await;
    portal
        .session_manager
        .lock()
        .await
        .get_session_mut(&path(&session))
        .unwrap()
        .select_sources(vec![test_source(1, "eDP-1")])
        .unwrap();
    let reply = call(
        &owner,
        PATH,
        REMOTE_DESKTOP,
        "Start",
        &(
            path("/org/freedesktop/portal/desktop/request/test/long_s"),
            path(&session),
            APP_ID,
            "",
            no_options(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response_code(&reply), 0);

    portal.backdate(&session).await;
    assert_eq!(portal.expire().await, 0);
    assert!(portal.session_exists(&session).await);
    assert!(portal.session_object_exists(&session).await);
    assert!(portal.recorded.destroyed_streams.lock().unwrap().is_empty());

    let closed = call(&owner, &session, SESSION, "Close", &()).await;
    assert_eq!(error_name(&closed), None);
    assert_eq!(
        portal.recorded.destroyed_streams.lock().unwrap().as_slice(),
        [vec![101]]
    );
    assert!(!portal.session_object_exists(&session).await);
}
