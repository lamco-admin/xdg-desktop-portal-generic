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
    ClipboardInterface, RemoteDesktopInterface, RequestInterface, ScreenCastInterface,
    SessionInterface, SessionResources,
    screencast::{AutoSelect, SourceSelectionConfig},
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
const REQUEST: &str = "org.freedesktop.impl.portal.Request";
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";
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
    captures_created: Arc<StdMutex<usize>>,
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
        *self.recorded.captures_created.lock().unwrap() += 1;
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
    pending_writes: super::PendingWrites,
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

        let clipboard = ClipboardInterface::new(Arc::clone(&session_manager), None);
        let pending_writes = clipboard.pending_writes();

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
            .serve_at(PATH, clipboard)
            .build()
            .await
            .unwrap();

        Some(Self {
            server,
            session_manager,
            resources,
            pending_writes,
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

    /// Whether a `Request` object is registered at `handle`.
    async fn request_exists(&self, handle: &str) -> bool {
        self.server
            .object_server()
            .interface::<_, RequestInterface>(path(handle))
            .await
            .is_ok()
    }

    /// Wait up to two seconds for the `Request` at `handle` to go away.
    async fn request_removed(&self, handle: &str) -> bool {
        for _ in 0..100 {
            if !self.request_exists(handle).await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    /// Change how ScreenCast.SelectSources asks for sources.
    async fn set_source_selection(&self, picker: Option<&str>, auto_select: AutoSelect) {
        self.server
            .object_server()
            .interface::<_, ScreenCastInterface>(PATH)
            .await
            .unwrap()
            .get_mut()
            .await
            .set_source_selection(SourceSelectionConfig {
                picker: picker.map(str::to_string),
                auto_select,
            });
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

/// Start a RemoteDesktop session as `owner` with one capture stream (node 101).
async fn started_with_stream(portal: &Portal, owner: &Connection, tag: &str) -> String {
    let session = create_remote_desktop(owner, tag).await;
    select_devices(owner, &session, tag).await;
    portal
        .session_manager
        .lock()
        .await
        .get_session_mut(&path(&session))
        .unwrap()
        .select_sources(vec![test_source(1, "eDP-1")])
        .unwrap();
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
    let session = started_with_stream(&portal, &owner, "long").await;

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

#[tokio::test]
async fn failed_call_does_not_leave_its_request_behind() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = create_remote_desktop(&owner, "victim").await;

    // Another peer aims a call at the victim's session; it fails the sender
    // check after its Request object was registered.
    let handle = "/org/freedesktop/portal/desktop/request/test/stale_request";
    let refused = call(
        &other,
        PATH,
        SCREEN_CAST,
        "SelectSources",
        &(path(handle), path(&session), APP_ID, no_options()),
    )
    .await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));
    assert!(portal.request_removed(handle).await);

    // With the Request gone there is nothing to close the victim's session through.
    let close = call(&other, handle, REQUEST, "Close", &()).await;
    assert!(error_name(&close).is_some());
    assert!(portal.session_exists(&session).await);
}

#[tokio::test]
async fn request_handle_already_in_use_is_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let session = create_remote_desktop(&owner, "dup").await;

    let handle = "/org/freedesktop/portal/desktop/request/test/dup_d";
    let existing = RequestInterface::new(
        Arc::clone(&portal.session_manager),
        owner.unique_name().unwrap().to_string(),
    );
    assert!(
        portal
            .server
            .object_server()
            .at(path(handle), existing)
            .await
            .unwrap()
            .is_none()
    );

    let mut options = no_options();
    options.insert("types", Variant::from(3u32));
    let refused = call(
        &owner,
        PATH,
        REMOTE_DESKTOP,
        "SelectDevices",
        &(path(handle), path(&session), APP_ID, options),
    )
    .await;
    assert_eq!(error_name(&refused).as_deref(), Some(INVALID_ARGS));
    // The call didn't take over the existing request, or remove it.
    assert!(portal.request_exists(handle).await);
}

#[tokio::test]
async fn request_close_checks_the_sender_and_tears_down() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = started_with_stream(&portal, &owner, "cancel").await;

    let handle = "/org/freedesktop/portal/desktop/request/test/cancel_r";
    let request = RequestInterface::for_session(
        Arc::clone(&portal.session_manager),
        session.clone(),
        owner.unique_name().unwrap().to_string(),
        portal.resources.clone(),
    );
    assert!(
        portal
            .server
            .object_server()
            .at(path(handle), request)
            .await
            .unwrap()
            .is_none()
    );

    let refused = call(&other, handle, REQUEST, "Close", &()).await;
    assert_eq!(error_name(&refused).as_deref(), Some(ACCESS_DENIED));
    assert!(portal.session_exists(&session).await);
    assert!(portal.recorded.destroyed_streams.lock().unwrap().is_empty());

    let closed = call(&owner, handle, REQUEST, "Close", &()).await;
    assert_eq!(error_name(&closed), None);
    assert!(!portal.session_exists(&session).await);
    assert!(!portal.session_object_exists(&session).await);
    assert_eq!(
        portal.recorded.destroyed_streams.lock().unwrap().as_slice(),
        [vec![101]]
    );
}

#[tokio::test]
async fn selection_transfer_is_answerable_only_by_the_owner() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let owner_session = create_remote_desktop(&owner, "clip_owner").await;
    let other_session = create_remote_desktop(&other, "clip_other").await;
    for (conn, session) in [(&owner, &owner_session), (&other, &other_session)] {
        let request = (path(session), no_options());
        let reply = call(conn, PATH, CLIPBOARD, "RequestClipboard", &request).await;
        assert_eq!(error_name(&reply), None);
    }

    let mut options = no_options();
    options.insert("mime_types", Variant::from(vec!["text/plain"]));
    let set = call(
        &owner,
        PATH,
        CLIPBOARD,
        "SetSelection",
        &(path(&owner_session), options),
    )
    .await;
    assert_eq!(error_name(&set), None);

    let serial = 4242;
    let target = super::issue_selection_transfer(
        &*portal.session_manager.lock().await,
        &portal.pending_writes,
        "text/plain",
        serial,
    );
    assert_eq!(
        target.as_ref().map(ObjectPath::as_str),
        Some(owner_session.as_str())
    );

    // The other session has the clipboard too, but the serial isn't its own.
    let write = call(
        &other,
        PATH,
        CLIPBOARD,
        "SelectionWrite",
        &(path(&other_session), serial),
    )
    .await;
    assert_eq!(error_name(&write).as_deref(), Some(ACCESS_DENIED));
    let done = call(
        &other,
        PATH,
        CLIPBOARD,
        "SelectionWriteDone",
        &(path(&other_session), serial, true),
    )
    .await;
    assert_eq!(error_name(&done).as_deref(), Some(ACCESS_DENIED));
    assert!(portal.pending_writes.lock().unwrap().contains_key(&serial));

    let write = call(
        &owner,
        PATH,
        CLIPBOARD,
        "SelectionWrite",
        &(path(&owner_session), serial),
    )
    .await;
    assert_eq!(error_name(&write), None);
    // Dropping the reply closes our end of the pipe, so the reader finishes.
    drop(write);
}

/// Create a ScreenCast session as `owner` and call SelectSources on it.
/// Returns the session handle and the SelectSources response code.
async fn screen_cast_select(
    owner: &Connection,
    tag: &str,
    app_id: &str,
    options: HashMap<&'static str, Variant<'static>>,
) -> (String, u32) {
    let session = format!("/org/freedesktop/portal/desktop/session/test/{tag}");
    let reply = call(
        owner,
        PATH,
        SCREEN_CAST,
        "CreateSession",
        &(
            path(&format!(
                "/org/freedesktop/portal/desktop/request/test/{tag}_c"
            )),
            path(&session),
            app_id,
            no_options(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(response_code(&reply), 0);

    let reply = call(
        owner,
        PATH,
        SCREEN_CAST,
        "SelectSources",
        &(
            path(&format!(
                "/org/freedesktop/portal/desktop/request/test/{tag}_ss"
            )),
            path(&session),
            app_id,
            options,
        ),
    )
    .await
    .unwrap();
    let code = response_code(&reply);
    (session, code)
}

/// Start `owner`'s ScreenCast session; returns the reply's results.
async fn screen_cast_start(
    owner: &Connection,
    session: &str,
    tag: &str,
    app_id: &str,
) -> HashMap<String, OwnedVariant> {
    let reply = call(
        owner,
        PATH,
        SCREEN_CAST,
        "Start",
        &(
            path(&format!(
                "/org/freedesktop/portal/desktop/request/test/{tag}_st"
            )),
            path(session),
            app_id,
            "",
            no_options(),
        ),
    )
    .await
    .unwrap();
    let (code, results): (u32, HashMap<String, OwnedVariant>) = reply.body().deserialize().unwrap();
    assert_eq!(code, 0);
    results
}

async fn selected_source_names(portal: &Portal, session: &str) -> Vec<String> {
    let manager = portal.session_manager.lock().await;
    let session = manager.get_session(&path(session)).unwrap();
    session.sources.iter().map(|s| s.name.clone()).collect()
}

#[tokio::test]
async fn select_sources_without_consent_is_cancelled() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;

    // No picker and auto-select off: nothing is captured.
    portal
        .set_source_selection(None, AutoSelect::Disabled)
        .await;
    let (session, code) = screen_cast_select(&owner, "noconsent", APP_ID, no_options()).await;
    assert_eq!(code, 1);
    assert_eq!(
        selected_source_names(&portal, &session).await,
        Vec::<String>::new()
    );

    // A picker that is cancelled doesn't fall back to auto-select.
    portal
        .set_source_selection(Some("false"), AutoSelect::First)
        .await;
    let (session, code) = screen_cast_select(&owner, "cancelled", APP_ID, no_options()).await;
    assert_eq!(code, 1);
    assert_eq!(
        selected_source_names(&portal, &session).await,
        Vec::<String>::new()
    );

    // Auto-select only when it is asked for and there is no picker.
    portal.set_source_selection(None, AutoSelect::First).await;
    let (session, code) = screen_cast_select(&owner, "auto", APP_ID, no_options()).await;
    assert_eq!(code, 0);
    assert_eq!(selected_source_names(&portal, &session).await, ["eDP-1"]);
}

#[tokio::test]
async fn restore_data_is_honoured_only_for_a_grant_the_backend_issued() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;

    // The user picks HDMI-A-1 for a persistent cast.
    portal
        .set_source_selection(Some("echo HDMI-A-1"), AutoSelect::Disabled)
        .await;
    let mut options = no_options();
    options.insert("persist_mode", Variant::from(2u32));
    let (session, code) = screen_cast_select(&owner, "first", APP_ID, options).await;
    assert_eq!(code, 0);
    let results = screen_cast_start(&owner, &session, "first", APP_ID).await;
    let restore_data = results.get("restore_data").unwrap().try_clone().unwrap();

    // From here on the picker always cancels, so only restore can select.
    portal
        .set_source_selection(Some("false"), AutoSelect::First)
        .await;

    // Forged restore data in the old format, naming an output, restores nothing.
    let mut forged = no_options();
    forged.insert(
        "restore_data",
        Variant::from(("generic", 1u32, Variant::from(vec!["eDP-1"]))),
    );
    let (_, code) = screen_cast_select(&owner, "forged", APP_ID, forged).await;
    assert_eq!(code, 1);

    // The real grant presented by another app restores nothing.
    let mut stolen = no_options();
    stolen.insert(
        "restore_data",
        Variant::from(restore_data.try_clone().unwrap()),
    );
    let (_, code) = screen_cast_select(&owner, "stolen", "org.example.Other", stolen).await;
    assert_eq!(code, 1);

    // The app the grant was issued to gets its selection back without a picker.
    let mut restore = no_options();
    restore.insert(
        "restore_data",
        Variant::from(restore_data.try_clone().unwrap()),
    );
    let (session, code) = screen_cast_select(&owner, "restored", APP_ID, restore).await;
    assert_eq!(code, 0);
    assert_eq!(selected_source_names(&portal, &session).await, ["HDMI-A-1"]);

    // A grant is used once.
    let mut again = no_options();
    again.insert("restore_data", Variant::from(restore_data));
    let (_, code) = screen_cast_select(&owner, "again", APP_ID, again).await;
    assert_eq!(code, 1);
}

#[tokio::test]
async fn second_start_creates_no_capture() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let captures = || *portal.recorded.captures_created.lock().unwrap();

    let remote = started_with_stream(&portal, &owner, "rd_twice").await;
    assert_eq!(captures(), 1);
    let again = call(
        &owner,
        PATH,
        REMOTE_DESKTOP,
        "Start",
        &(
            path("/org/freedesktop/portal/desktop/request/test/rd_twice_s2"),
            path(&remote),
            APP_ID,
            "",
            no_options(),
        ),
    )
    .await;
    assert!(error_name(&again).is_some());
    assert_eq!(captures(), 1);

    portal.set_source_selection(None, AutoSelect::First).await;
    let (cast, code) = screen_cast_select(&owner, "sc_twice", APP_ID, no_options()).await;
    assert_eq!(code, 0);
    screen_cast_start(&owner, &cast, "sc_twice", APP_ID).await;
    assert_eq!(captures(), 2);
    let again = call(
        &owner,
        PATH,
        SCREEN_CAST,
        "Start",
        &(
            path("/org/freedesktop/portal/desktop/request/test/sc_twice_st2"),
            path(&cast),
            APP_ID,
            "",
            no_options(),
        ),
    )
    .await;
    assert!(error_name(&again).is_some());
    assert_eq!(captures(), 2);
}

#[tokio::test]
async fn out_of_range_codes_and_foreign_streams_are_refused() {
    let Some(portal) = Portal::start().await else {
        return;
    };
    let owner = portal.client().await;
    let other = portal.client().await;
    let session = started_with_stream(&portal, &owner, "codes").await;
    let other_session = started_with_stream(&portal, &other, "codes_other").await;
    let injected = || portal.recorded.injected.lock().unwrap().len();

    for keycode in [-1i32, 768, i32::MAX] {
        let body = (path(&session), no_options(), keycode, 1u32);
        let refused = call(&owner, PATH, REMOTE_DESKTOP, "NotifyKeyboardKeycode", &body).await;
        assert_eq!(error_name(&refused).as_deref(), Some(INVALID_ARGS));
    }
    let body = (path(&session), no_options(), -5i32, 1u32);
    let refused = call(&owner, PATH, REMOTE_DESKTOP, "NotifyPointerButton", &body).await;
    assert_eq!(error_name(&refused).as_deref(), Some(INVALID_ARGS));
    assert_eq!(injected(), 0);

    // Both sessions have a stream 101, so use one neither has.
    let body = (path(&session), no_options(), 999u32, 0.5f64, 0.5f64);
    let refused = call(
        &owner,
        PATH,
        REMOTE_DESKTOP,
        "NotifyPointerMotionAbsolute",
        &body,
    )
    .await;
    assert_eq!(error_name(&refused).as_deref(), Some(INVALID_ARGS));
    assert_eq!(injected(), 0);

    let body = (path(&session), no_options(), 101u32, 0.5f64, 0.5f64);
    let allowed = call(
        &owner,
        PATH,
        REMOTE_DESKTOP,
        "NotifyPointerMotionAbsolute",
        &body,
    )
    .await;
    assert_eq!(error_name(&allowed), None);
    let body = (path(&session), no_options(), 0x110i32, 1u32);
    let allowed = call(&owner, PATH, REMOTE_DESKTOP, "NotifyPointerButton", &body).await;
    assert_eq!(error_name(&allowed), None);
    assert_eq!(injected(), 2);
    assert!(portal.session_exists(&other_session).await);
}
