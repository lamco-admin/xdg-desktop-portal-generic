//! wlr virtual input protocol backend.
//!
//! This backend uses the wlroots virtual input protocols to inject input events:
//! - `zwlr_virtual_pointer_v1` for pointer events
//! - `zwp_virtual_keyboard_v1` for keyboard events
//!
//! # How It Works
//!
//! 1. Portal creates a Wayland client connection to the compositor
//! 2. Portal binds to virtual pointer/keyboard manager globals
//! 3. Portal creates virtual devices for each session
//! 4. Input events are sent through the virtual devices

use std::{
    collections::{HashMap, VecDeque},
    os::unix::io::{AsFd, OwnedFd},
};

use wayland_client::{
    Connection, Dispatch, EventQueue, QueueHandle,
    globals::{GlobalList, GlobalListContents, registry_queue_init},
    protocol::{
        wl_pointer::ButtonState as WlButtonState, wl_registry::WlRegistry, wl_seat::WlSeat,
    },
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

use super::{InputBackend, InputProtocol, WlrConfig};
use crate::{
    error::{PortalError, Result},
    types::{
        ButtonState, DeviceTypes, InputEvent, KeyState, KeyboardEvent, PointerEvent, PointerRegion,
        ScrollAxis, StreamOutputMapping,
    },
};

/// Wrapper around xkbcommon types that are `!Send + !Sync` due to raw pointers.
///
/// xkbcommon's `Keymap` and `State` are internally reference-counted (via
/// `xkb_keymap_ref`/`xkb_state_ref`) and thread-safe. The Rust bindings don't
/// implement `Send`/`Sync` because they wrap `*mut` pointers, but the underlying
/// C library guarantees thread safety for these types.
///
/// Since `WlrInputBackend` is wrapped in `Arc<Mutex<>>` (ensuring exclusive
/// access), this is safe.
struct XkbData {
    keymap: xkbcommon::xkb::Keymap,
    /// Live xkb state — updated on every keyboard key event so the serialized
    /// modifier mask sent to virtual-keyboard `modifiers()` reflects the
    /// current depressed/latched/locked set.
    state: xkbcommon::xkb::State,
    /// Serialized form of `keymap` — the *currently active* keymap text,
    /// uploaded to every session's virtual keyboard. Changes whenever
    /// `dynamic_pool`'s bindings change (see [`WlrInputBackend::keysym_to_keycode`]).
    keymap_string: String,
    /// The original "us"-layout keymap text, as compiled at startup — never
    /// mutated. [`splice_dynamic_keysyms`] always splices from this, not from
    /// a previous splice, so re-splicing is idempotent rather than compounding.
    base_keymap_string: String,
    /// The base keymap's own symbolic name for each entry of
    /// [`DYNAMIC_POOL_EVDEV_CODES`], resolved once at startup by
    /// [`resolve_pool_key_names`]. Index `i` here always corresponds to
    /// `DYNAMIC_POOL_EVDEV_CODES[i]`.
    pool_key_names: [String; DYNAMIC_KEYSYM_POOL_SIZE],
    /// LRU pool of keycodes dynamically bound to whatever keysyms the base
    /// "us" layout has no key for (CJK, accented Latin, and other non-ASCII
    /// characters — see `EI-TEXT-SCOPING-2026-09-07.md` in lamco-admin).
    dynamic_pool: DynamicKeysymPool,
}

/// Number of keycodes reserved for dynamic keysym binding. Small on purpose:
/// each slot only needs to live long enough to be pressed and released
/// (typically within the same EIS frame), so a modest LRU pool comfortably
/// covers realistic typing/paste bursts without the keymap churning on every
/// distinct character seen over a session's lifetime.
const DYNAMIC_KEYSYM_POOL_SIZE: usize = 32;

/// Evdev keycodes reserved for the dynamic keysym pool, chosen instead of
/// appending brand-new keycodes past the base keymap's own maximum (the
/// pool's original design, which put them at evdev ~701-732).
///
/// That original range fell entirely outside Chromium/Electron's own
/// `dom_code_data.inc` evdev-to-DomCode lookup table (its highest covered
/// evdev value is 633, `PrivacyScreenToggle`, verified against upstream
/// Chromium source on 2026-09-09), which Chromium/Electron apps (Chrome, VS
/// Code, Slack, Discord, Signal) consult independently of any XKB keysym
/// binding. A keycode absent from that table gets dropped before Chromium
/// ever looks at the keysym our keymap bound to it — Qt/GTK apps, which
/// don't consult that table, worked the whole time, which is why this went
/// unnoticed. KWin hit and fixed the identical bug (commit `584bba0`,
/// 2026-08-14) for its own single scratch keycode, moving it from evdev 247
/// (`KEY_RFKILL`, absent from Chromium's table) to evdev 194 (`KEY_F24`,
/// present).
///
/// These 32 codes are F13-F23 plus a set of consumer/media/launch keys
/// (brightness, keyboard illumination, eject, media transport, launch-app,
/// browser navigation, zoom): each is both covered by Chromium's table and
/// already declared with a real symbolic name in the standard "evdev" XKB
/// keycodes ruleset (verified directly against a compiled keymap on this
/// host — see [`resolve_pool_key_names`]), so [`splice_dynamic_keysyms`]
/// only ever rebinds an existing key's symbol rather than adding a new one.
/// Rebinding is safe: our virtual keyboard is a synthetic Wayland device
/// with its own independent compositor-side XKB state, so it never coexists
/// with a real physical key using the same evdev code. `KEY_F24` (194)
/// itself is deliberately excluded — the standard evdev ruleset declares it
/// in `xkb_keycodes` but not in `xkb_symbols`, so there is no existing `key
/// <FK24> { ... };` line for [`splice_dynamic_keysyms`] to rebind.
const DYNAMIC_POOL_EVDEV_CODES: [u32; DYNAMIC_KEYSYM_POOL_SIZE] = [
    183, 184, 185, 186, 187, 188, 190, 191, 192, 193, // F13-F18, F20-F23
    179, 180, // NumpadParenLeft, NumpadParenRight
    225, 224, 244, 230, 229, // brightness up/down/auto, kbd illum up/down
    161, // Eject
    208, 168, 167, // media fast-forward, rewind, record
    144, 140, // LaunchApp1, LaunchApp2
    155, 156, // LaunchMail, BrowserFavorites
    173, 128, 159, 158, 172, // browser refresh, stop, forward, back, home
    217, // BrowserSearch
    418, // ZoomIn
];

/// LRU pool of dynamically-bound keycodes, layered on top of a base XKB
/// keymap via [`splice_dynamic_keysyms`]. Reserves [`DYNAMIC_KEYSYM_POOL_SIZE`]
/// keycode numbers above the base keymap's own maximum and rebinds them on
/// demand to whatever keysym is currently needed, evicting the
/// least-recently-used slot when the pool is full so a keymap re-upload
/// (and the `keyboard.keymap()` event it costs every connected session) only
/// happens when a binding actually changes, not on every already-resident
/// keysym.
#[derive(Debug, Clone)]
struct DynamicKeysymPool {
    /// Keysym currently bound to each pool slot, by slot index. `None` means
    /// the slot has never been used (bound to `NoSymbol` in the keymap).
    slots: [Option<u32>; DYNAMIC_KEYSYM_POOL_SIZE],
    /// Slot indices in least- to most-recently-used order. Always a
    /// permutation of `0..DYNAMIC_KEYSYM_POOL_SIZE`.
    recency: VecDeque<usize>,
}

impl DynamicKeysymPool {
    fn new() -> Self {
        Self {
            slots: [None; DYNAMIC_KEYSYM_POOL_SIZE],
            recency: (0..DYNAMIC_KEYSYM_POOL_SIZE).collect(),
        }
    }

    /// Resolve `keysym` to a pool slot index, reusing an existing binding
    /// when present. The second element of the return value is whether the
    /// pool's bindings changed as a result (i.e. whether the keymap needs
    /// recompiling and re-uploading) — `false` on a cache hit.
    fn resolve(&mut self, keysym: u32) -> (usize, bool) {
        if let Some(slot) = self.slots.iter().position(|s| *s == Some(keysym)) {
            self.touch(slot);
            return (slot, false);
        }

        #[expect(
            clippy::expect_used,
            reason = "recency is constructed with exactly DYNAMIC_KEYSYM_POOL_SIZE slots and \
                      only ever removed/re-pushed as a whole, never shrunk"
        )]
        let slot = self
            .recency
            .pop_front()
            .expect("recency holds all DYNAMIC_KEYSYM_POOL_SIZE slots");
        self.slots[slot] = Some(keysym);
        self.recency.push_back(slot);
        (slot, true)
    }

    fn touch(&mut self, slot: usize) {
        if let Some(pos) = self.recency.iter().position(|&s| s == slot) {
            self.recency.remove(pos);
        }
        self.recency.push_back(slot);
    }
}

/// Resolve the base keymap's own symbolic name for each entry of
/// [`DYNAMIC_POOL_EVDEV_CODES`], by scanning its `xkb_keycodes` section for
/// `<NAME> = <number>;` declarations rather than hard-coding names (which
/// are an xkeyboard-config implementation detail, not a stable XKB
/// interface). Returns `None` if the section can't be found or any entry's
/// XKB keycode (`evdev + 8`) has no declared name — a base keymap missing
/// one of these very ordinary consumer/function keycodes is not something
/// [`splice_dynamic_keysyms`] can safely rebind, so callers should fail
/// closed rather than silently run with a smaller effective pool.
///
/// `base_keymap` must be libxkbcommon's own `KEYMAP_FORMAT_TEXT_V1` output.
fn resolve_pool_key_names(base_keymap: &str) -> Option<[String; DYNAMIC_KEYSYM_POOL_SIZE]> {
    let keycodes_start = base_keymap.find("xkb_keycodes")?;
    let section_end = base_keymap[keycodes_start..].find("\n};\n")?;
    let section = &base_keymap[keycodes_start..keycodes_start + section_end];

    let mut names_by_xkb_code: HashMap<u32, &str> = HashMap::new();
    for line in section.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix('<') else {
            continue;
        };
        let Some((name, rest)) = rest.split_once('>') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(num_str) = rest.trim().strip_suffix(';') else {
            continue;
        };
        if let Ok(num) = num_str.trim().parse::<u32>() {
            names_by_xkb_code.insert(num, name);
        }
    }

    let mut names = Vec::with_capacity(DYNAMIC_KEYSYM_POOL_SIZE);
    for &evdev in &DYNAMIC_POOL_EVDEV_CODES {
        let xkb_code = evdev + 8;
        let name = names_by_xkb_code.get(&xkb_code)?;
        names.push((*name).to_string());
    }
    names.try_into().ok()
}

/// Splice `pool`'s current keysym bindings into `base_keymap`, rebinding the
/// `xkb_symbols` entry of each of [`DYNAMIC_POOL_EVDEV_CODES`]'s existing
/// keys (named per `pool_key_names`, from [`resolve_pool_key_names`]) to
/// whatever keysym its pool slot currently holds (`NoSymbol` for an unused
/// slot). Unlike an earlier version of this function, no new keycodes are
/// added and the keymap's declared `maximum` is untouched — every keycode
/// this pool uses already exists in the base keymap (see
/// [`DYNAMIC_POOL_EVDEV_CODES`] for why).
///
/// `base_keymap` must be libxkbcommon's own `KEYMAP_FORMAT_TEXT_V1` output
/// (i.e. [`xkbcommon::xkb::Keymap::get_as_string`]) and `pool_key_names`
/// must have been resolved from that same text — this relies on an exact,
/// unique `key <NAME>` textual match per slot, which [`resolve_pool_key_names`]
/// guarantees by construction. Verified directly against a real compiled
/// "us" keymap (recompiles cleanly via `Keymap::new_from_string`, spliced
/// keycodes resolve to the expected keysyms, unrelated keys are untouched)
/// — see the module tests below.
fn splice_dynamic_keysyms(
    base_keymap: &str,
    pool_key_names: &[String; DYNAMIC_KEYSYM_POOL_SIZE],
    pool: &DynamicKeysymPool,
) -> Option<String> {
    let mut out = base_keymap.to_string();

    for (name, slot) in pool_key_names.iter().zip(pool.slots.iter()) {
        let sym = match slot {
            Some(keysym) => format!("0x{keysym:08x}"),
            None => "NoSymbol".to_string(),
        };

        let needle = format!("key <{name}>");
        let Some(start) = out.find(&needle) else {
            tracing::error!(
                name,
                "splice_dynamic_keysyms: base keymap has no \"{needle}\" symbols line -- \
                 pool_key_names is stale or the keymap format changed"
            );
            return None;
        };
        let rel_end = out[start..].find("};")?;
        let end = start + rel_end + "};".len();

        let replacement = format!("key <{name}> {{ [ {sym} ] }};");
        out.replace_range(start..end, &replacement);
    }

    Some(out)
}

// SAFETY: xkbcommon Keymap and State are internally reference-counted and
// thread-safe. We only access them under a Mutex, ensuring no concurrent access.
#[expect(
    unsafe_code,
    reason = "xkbcommon types are !Send but safe to send across our dedicated thread"
)]
unsafe impl Send for XkbData {}
#[expect(
    unsafe_code,
    reason = "xkbcommon types are !Sync but safe under Arc<Mutex<>> exclusive access"
)]
unsafe impl Sync for XkbData {}

/// Wayland keyboard keymap format constant (XKB v1).
const WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1: u32 = 1;

/// wlr virtual input backend.
///
/// Implements the [`InputBackend`] trait using wlroots virtual input protocols.
/// Connects directly to the Wayland compositor as a client and creates virtual
/// keyboard/pointer devices for input injection.
///
/// # Why a separate Wayland connection?
///
/// This backend maintains its own `wayland_client::Connection` rather than
/// sharing the main `WaylandConnection` from [`crate::wayland`]. This is
/// intentional:
///
/// - `wayland_client::EventQueue` is `!Send`, so it cannot be shared across
///   tokio tasks or threads.
/// - Input injection calls (from D-Bus handlers in tokio) must `flush()` the
///   connection after sending protocol requests. Using the main connection
///   would require unsafe cross-thread access to its event queue.
/// - This is the same architecture used by `xdg-desktop-portal-wlr`: a
///   dedicated Wayland connection per backend that needs to send requests
///   from D-Bus handler context.
///
/// The tradeoff is two Wayland connections to the compositor, which is a
/// negligible cost compared to the thread-safety guarantees it provides.
pub struct WlrInputBackend {
    /// Wayland connection.
    connection: Connection,
    /// Event queue for the connection.
    event_queue: EventQueue<WlrState>,
    /// Queue handle for creating objects.
    queue_handle: QueueHandle<WlrState>,
    /// Backend state.
    state: WlrState,
    /// Active sessions with their virtual devices.
    sessions: HashMap<String, WlrSessionContext>,
    /// Mapping from PipeWire stream node IDs to output geometry.
    /// Used for multi-monitor absolute pointer positioning.
    stream_mappings: HashMap<u32, StreamOutputMapping>,
    /// Health event sender for input metrics.
    health_tx: Option<crate::health::HealthSender>,
    /// Total events successfully forwarded.
    events_forwarded: u64,
    /// Total flush failures (indicates broken Wayland connection).
    flush_failures: u64,
}

/// State for Wayland protocol handling.
#[derive(Default)]
struct WlrState {
    /// Virtual pointer manager global.
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    /// Virtual keyboard manager global.
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    /// Seat global (needed for keyboard creation).
    seat: Option<WlSeat>,
    /// Whether initialization is complete.
    initialized: bool,
    /// XKB data for keysym-to-keycode conversion and keymap transfer.
    xkb: Option<XkbData>,
}

/// Virtual devices for a session.
struct WlrSessionContext {
    /// Virtual pointer device (if enabled).
    pointer: Option<ZwlrVirtualPointerV1>,
    /// Virtual keyboard device (if enabled).
    keyboard: Option<ZwpVirtualKeyboardV1>,
}

impl WlrInputBackend {
    /// Create a new wlr virtual input backend.
    ///
    /// Connects to the Wayland compositor and binds the virtual input protocol
    /// globals. Fails if neither virtual pointer nor virtual keyboard is available.
    pub fn new(config: &WlrConfig) -> Result<Self> {
        tracing::info!("Initializing wlr virtual input backend");

        let connection = if let Some(ref wayland_display) = config.wayland_display {
            Connection::connect_to_env().map_err(|e| {
                tracing::debug!(
                    "Failed to connect to default display, trying {}",
                    wayland_display
                );
                PortalError::Config(format!(
                    "Failed to connect to Wayland display {wayland_display}: {e}"
                ))
            })?
        } else {
            Connection::connect_to_env()
                .map_err(|e| PortalError::Config(format!("Failed to connect to Wayland: {e}")))?
        };

        let (globals, event_queue) = registry_queue_init::<WlrState>(&connection).map_err(|e| {
            PortalError::Config(format!("Failed to initialize Wayland registry: {e}"))
        })?;

        let queue_handle = event_queue.handle();
        let mut state = WlrState::default();

        Self::bind_globals(&globals, &queue_handle, &mut state)?;

        // Initialize XKB keymap for keysym-to-keycode conversion and
        // virtual keyboard keymap setup.
        Self::init_xkb(&mut state)?;

        Ok(Self {
            connection,
            event_queue,
            queue_handle,
            state,
            sessions: HashMap::new(),
            stream_mappings: HashMap::new(),
            health_tx: None,
            events_forwarded: 0,
            flush_failures: 0,
        })
    }

    /// Initialize XKB context, keymap, and state.
    ///
    /// Creates a default "us" layout keymap. The serialized keymap string
    /// is stored for passing to virtual keyboards via memfd.
    fn init_xkb(state: &mut WlrState) -> Result<()> {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);

        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",   // rules (empty = default "evdev")
            "",   // model (empty = default)
            "",   // layout (empty = default "us")
            "",   // variant (empty = default)
            None, // options
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            PortalError::Config("Failed to create XKB keymap from default rules".to_string())
        })?;

        let keymap_string = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let xkb_state = xkb::State::new(&keymap);

        let pool_key_names = resolve_pool_key_names(&keymap_string).ok_or_else(|| {
            PortalError::Config(
                "Base XKB keymap is missing one or more of the consumer/function keycodes \
                 reserved for the dynamic keysym pool -- EIS text/keysym injection for \
                 out-of-layout characters (CJK, accented Latin, etc.) would silently degrade, \
                 so refusing to start rather than run with a corrupted pool"
                    .to_string(),
            )
        })?;

        tracing::info!(
            "XKB keymap initialized (default us layout, {} bytes, max keycode {})",
            keymap_string.len(),
            keymap.max_keycode().raw()
        );

        state.xkb = Some(XkbData {
            keymap,
            state: xkb_state,
            base_keymap_string: keymap_string.clone(),
            keymap_string,
            pool_key_names,
            dynamic_pool: DynamicKeysymPool::new(),
        });

        Ok(())
    }

    /// Create a memfd containing the keymap string, and send it to the virtual keyboard.
    ///
    /// The Wayland virtual keyboard protocol requires a keymap to be set via
    /// `keyboard.keymap(format, fd, size)` before any key events can be sent.
    fn set_keyboard_keymap(keyboard: &ZwpVirtualKeyboardV1, keymap_string: &str) -> Result<()> {
        use std::io::Write;

        use nix::sys::memfd;

        let keymap_bytes = keymap_string.as_bytes();
        let keymap_size = keymap_bytes.len() as u32;

        // Create a memfd for the keymap data
        let memfd = memfd::memfd_create(c"xdp-keymap", memfd::MFdFlags::MFD_CLOEXEC)
            .map_err(|e| PortalError::Config(format!("Failed to create memfd for keymap: {e}")))?;

        // Write keymap bytes to the memfd
        let mut file = std::fs::File::from(memfd);
        file.write_all(keymap_bytes)
            .map_err(|e| PortalError::Config(format!("Failed to write keymap to memfd: {e}")))?;

        // Send the keymap to the virtual keyboard
        // SAFETY: keyboard.keymap() is a Wayland protocol method that reads the fd.
        // The fd is valid and contains the complete keymap.
        let borrowed_fd = file.as_fd();
        keyboard.keymap(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, borrowed_fd, keymap_size);

        tracing::debug!("Set virtual keyboard keymap ({} bytes)", keymap_size);
        Ok(())
    }

    /// Bind to required Wayland globals.
    fn bind_globals(
        globals: &GlobalList,
        qh: &QueueHandle<WlrState>,
        state: &mut WlrState,
    ) -> Result<()> {
        // Bind virtual pointer manager
        match globals.bind::<ZwlrVirtualPointerManagerV1, _, _>(qh, 1..=2, ()) {
            Ok(manager) => {
                tracing::debug!("Bound zwlr_virtual_pointer_manager_v1");
                state.pointer_manager = Some(manager);
            }
            Err(e) => {
                tracing::warn!("zwlr_virtual_pointer_manager_v1 not available: {}", e);
            }
        }

        // Bind virtual keyboard manager
        match globals.bind::<ZwpVirtualKeyboardManagerV1, _, _>(qh, 1..=1, ()) {
            Ok(manager) => {
                tracing::debug!("Bound zwp_virtual_keyboard_manager_v1");
                state.keyboard_manager = Some(manager);
            }
            Err(e) => {
                tracing::warn!("zwp_virtual_keyboard_manager_v1 not available: {}", e);
            }
        }

        // Bind seat (needed for keyboard)
        match globals.bind::<WlSeat, _, _>(qh, 1..=9, ()) {
            Ok(seat) => {
                tracing::debug!("Bound wl_seat");
                state.seat = Some(seat);
            }
            Err(e) => {
                tracing::warn!("wl_seat not available: {}", e);
            }
        }

        // Check if at least one input protocol is available
        if state.pointer_manager.is_none() && state.keyboard_manager.is_none() {
            return Err(PortalError::Config(
                "Neither virtual pointer nor virtual keyboard protocols available".to_string(),
            ));
        }

        state.initialized = true;
        Ok(())
    }

    /// Create a virtual pointer for a session.
    fn create_pointer(&self) -> Option<ZwlrVirtualPointerV1> {
        self.state.pointer_manager.as_ref().map(|manager| {
            manager.create_virtual_pointer(self.state.seat.as_ref(), &self.queue_handle, ())
        })
    }

    /// Create a virtual keyboard for a session and set its keymap.
    fn create_keyboard(&self) -> Option<ZwpVirtualKeyboardV1> {
        if let (Some(manager), Some(seat)) = (&self.state.keyboard_manager, &self.state.seat) {
            let keyboard = manager.create_virtual_keyboard(seat, &self.queue_handle, ());

            // Set the keymap on the keyboard — required before any key events
            if let Some(ref xkb) = self.state.xkb {
                if let Err(e) = Self::set_keyboard_keymap(&keyboard, &xkb.keymap_string) {
                    tracing::error!("Failed to set keyboard keymap: {}", e);
                    keyboard.destroy();
                    return None;
                }
            } else {
                tracing::error!("No XKB keymap available for virtual keyboard");
                keyboard.destroy();
                return None;
            }

            Some(keyboard)
        } else {
            None
        }
    }

    /// Dispatch pending Wayland events.
    fn dispatch(&mut self) -> Result<()> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| PortalError::Wayland(format!("dispatch error: {e}")))?;
        Ok(())
    }

    /// Compute the total extent (bounding box) of all known outputs.
    ///
    /// Returns `(width, height)` covering all output regions. If no stream
    /// mappings are set, falls back to a reasonable default.
    ///
    /// Origin-unaware: assumes the layout's own origin is `(0, 0)`. Correct only
    /// when no output has a negative `x`/`y`. Kept for callers that only need a
    /// size, not a placement; [`Self::layout_bounds`] is origin-aware and should
    /// be preferred for anything that also needs to place a point within the
    /// layout (e.g. EIS region offsets).
    fn compute_total_extent(&self) -> (u32, u32) {
        let (_, _, width, height) = Self::layout_bounds(&self.stream_mappings);
        (width, height)
    }

    /// Compute the layout's bounding box: the smallest rectangle containing every
    /// known output, in the same compositor-global coordinate space `stream_mappings`
    /// entries use.
    ///
    /// Returns `(origin_x, origin_y, width, height)`. `origin_x`/`origin_y` are the
    /// top-left corner of that box — negative when an output sits left of or above
    /// compositor-global `(0, 0)` (a monitor placed left of or above the primary is a
    /// real, supported layout, not an edge case). Subtracting them from a
    /// compositor-global point shifts it into the layout's own top-left-anchored
    /// space, which is what EIS region offsets need (`ei_device.region`'s offsets are
    /// unsigned, so a negative compositor-global coordinate could not be advertised
    /// directly). Falls back to `(0, 0, 1920, 1080)` when no mappings are set.
    fn layout_bounds(mappings: &HashMap<u32, StreamOutputMapping>) -> (i32, i32, u32, u32) {
        if mappings.is_empty() {
            return (0, 0, 1920, 1080); // Reasonable default for single-monitor
        }

        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        for mapping in mappings.values() {
            min_x = min_x.min(mapping.x);
            min_y = min_y.min(mapping.y);
            max_x = max_x.max(mapping.x + mapping.width as i32);
            max_y = max_y.max(mapping.y + mapping.height as i32);
        }

        let width = (max_x - min_x).max(1) as u32;
        let height = (max_y - min_y).max(1) as u32;
        (min_x, min_y, width, height)
    }

    /// Per-output EIS regions for the layout's `PointerAbsolute` device: one region
    /// per known output, shifted so the layout's own top-left (per
    /// [`Self::layout_bounds`]) is the origin, each tagged with the same
    /// `mapping_id` string the matching ScreenCast stream advertises, so a
    /// client can correlate a region with that stream.
    ///
    /// Never empty: it is a libei implementation bug to advertise the
    /// absolute-pointer capability on a virtual device without advertising at
    /// least one region, so when no stream mappings are set yet, this falls back
    /// to a single region covering `layout_bounds`' own default extent (no
    /// `mapping_id`, since there is no real stream to correlate it with yet).
    pub(crate) fn pointer_regions(&self) -> Vec<PointerRegion> {
        Self::compute_pointer_regions(&self.stream_mappings)
    }

    /// Pure logic behind [`Self::pointer_regions`], separated out so it's testable
    /// against plain `StreamOutputMapping` data without a live Wayland connection.
    fn compute_pointer_regions(mappings: &HashMap<u32, StreamOutputMapping>) -> Vec<PointerRegion> {
        if mappings.is_empty() {
            let (_, _, width, height) = Self::layout_bounds(mappings);
            return vec![PointerRegion {
                mapping_id: None,
                offset_x: 0,
                offset_y: 0,
                width,
                height,
            }];
        }

        let (origin_x, origin_y, _, _) = Self::layout_bounds(mappings);
        mappings
            .values()
            .map(|mapping| PointerRegion {
                mapping_id: mapping.mapping_id.clone(),
                offset_x: (mapping.x - origin_x).max(0) as u32,
                offset_y: (mapping.y - origin_y).max(0) as u32,
                width: mapping.width,
                height: mapping.height,
            })
            .collect()
    }

    /// The layout's total pixel size — the same `(width, height)` its
    /// [`Self::pointer_regions`] are laid out within. Used to interpret an
    /// absolute-motion request that carries no per-stream target (the EIS wire
    /// protocol has no such field) as a coordinate within the whole layout rather
    /// than any single output's own region.
    pub(crate) fn layout_extent(&self) -> (u32, u32) {
        self.compute_total_extent()
    }

    /// Flush the Wayland connection.
    ///
    /// EAGAIN/WouldBlock is transient (compositor's socket buffer is full;
    /// the data we wrote stays buffered in wayland-client and will be sent
    /// on the next flush) and must NOT be treated as a permanent failure.
    /// Other errors indicate a broken connection (compositor crash, fd
    /// closed, etc.) and are propagated for session cleanup. Mirrors the
    /// equivalent fix applied to src/wayland/mod.rs in commit 8326a30.
    fn flush(&self) -> Result<()> {
        match self.connection.flush() {
            Ok(()) => Ok(()),
            Err(wayland_client::backend::WaylandError::Io(ref e))
                if e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                tracing::trace!(
                    "wlr input: Wayland flush returned WouldBlock, data buffered for next flush"
                );
                Ok(())
            }
            Err(e) => Err(PortalError::Wayland(format!(
                "flush failed (connection may be broken): {e}"
            ))),
        }
    }

    /// Apply one event's wl_pointer/wl_keyboard protocol calls, without
    /// committing a `pointer.frame()` or flushing.
    ///
    /// Split out of [`InputBackend::inject_event`] so
    /// [`InputBackend::inject_event_batch`] can apply several events and
    /// commit exactly one frame for the whole group. Committing each event
    /// of a coordinated group (move-then-click) individually lets the
    /// compositor process them as separate hardware events, opening a race
    /// where the click can be observed at the pre-move position.
    #[expect(
        clippy::too_many_lines,
        reason = "match arms for each input event variant are individually simple"
    )]
    fn apply_event(&mut self, session_id: &str, event: &InputEvent) -> Result<()> {
        let ctx = self
            .sessions
            .get(session_id)
            .ok_or_else(|| PortalError::SessionNotFound(session_id.to_string()))?;

        let time_ms = |time_usec: u64| (time_usec / 1000) as u32;

        match *event {
            InputEvent::Pointer(PointerEvent::Motion { dx, dy, time_usec }) => {
                if let Some(ref pointer) = ctx.pointer {
                    pointer.motion(time_ms(time_usec), dx, dy);
                }
            }

            InputEvent::Pointer(PointerEvent::MotionAbsolute {
                x,
                y,
                x_extent,
                y_extent,
                stream,
                time_usec,
            }) => {
                if let Some(ref pointer) = ctx.pointer {
                    // Normalize caller input to [0,1] within the source frame.
                    // x_extent==0 means caller already normalized (legacy D-Bus path).
                    let nx = if x_extent == 0 {
                        x
                    } else {
                        x / f64::from(x_extent)
                    };
                    let ny = if y_extent == 0 {
                        y
                    } else {
                        y / f64::from(y_extent)
                    };
                    let nx = nx.clamp(0.0, 1.0);
                    let ny = ny.clamp(0.0, 1.0);

                    let extent = 10000u32;
                    let mapping_hit = self.stream_mappings.contains_key(&stream);
                    let (abs_x, abs_y) = if let Some(mapping) = self.stream_mappings.get(&stream) {
                        // Translate normalized stream coords to compositor-global pixels
                        // (output position + normalized * output size), shift into the
                        // layout's own top-left-anchored space (an output left of or
                        // above compositor-global 0,0 is a real layout, not an edge
                        // case), then re-normalize against the layout extent for the
                        // wlr protocol.
                        let (origin_x, origin_y, total_w, total_h) =
                            Self::layout_bounds(&self.stream_mappings);
                        let pixel_x = f64::from(mapping.x) + nx * f64::from(mapping.width);
                        let pixel_y = f64::from(mapping.y) + ny * f64::from(mapping.height);
                        let layout_x = pixel_x - f64::from(origin_x);
                        let layout_y = pixel_y - f64::from(origin_y);
                        let ax = ((layout_x / f64::from(total_w)) * f64::from(extent)) as u32;
                        let ay = ((layout_y / f64::from(total_h)) * f64::from(extent)) as u32;
                        (ax, ay)
                    } else {
                        // No mapping: project normalized coords directly to wlr extent.
                        let ax = (nx * f64::from(extent)) as u32;
                        let ay = (ny * f64::from(extent)) as u32;
                        (ax, ay)
                    };

                    tracing::trace!(
                        x_in = x,
                        y_in = y,
                        x_extent,
                        y_extent,
                        stream,
                        nx,
                        ny,
                        abs_x,
                        abs_y,
                        wlr_extent = extent,
                        mapping_hit,
                        "wlr inject MotionAbsolute"
                    );

                    pointer.motion_absolute(time_ms(time_usec), abs_x, abs_y, extent, extent);
                }
            }

            InputEvent::Pointer(PointerEvent::Button {
                button,
                state,
                time_usec,
            }) => {
                if let Some(ref pointer) = ctx.pointer {
                    let wl_state = match state {
                        ButtonState::Pressed => WlButtonState::Pressed,
                        ButtonState::Released => WlButtonState::Released,
                    };
                    pointer.button(time_ms(time_usec), button, wl_state);
                }
            }

            InputEvent::Pointer(PointerEvent::Scroll { dx, dy, time_usec }) => {
                if let Some(ref pointer) = ctx.pointer {
                    use wayland_client::protocol::wl_pointer::Axis;

                    // Set axis source before axis events (protocol compliance)
                    pointer
                        .axis_source(wayland_client::protocol::wl_pointer::AxisSource::Continuous);

                    if dy.abs() > f64::EPSILON {
                        pointer.axis(time_ms(time_usec), Axis::VerticalScroll, dy);
                    }
                    if dx.abs() > f64::EPSILON {
                        pointer.axis(time_ms(time_usec), Axis::HorizontalScroll, dx);
                    }

                    // Send axis_stop when both values are zero (scroll end)
                    if dy.abs() <= f64::EPSILON && dx.abs() <= f64::EPSILON {
                        pointer.axis_stop(time_ms(time_usec), Axis::VerticalScroll);
                        pointer.axis_stop(time_ms(time_usec), Axis::HorizontalScroll);
                    }
                }
            }

            InputEvent::Pointer(PointerEvent::ScrollDiscrete {
                axis,
                steps,
                time_usec,
            }) => {
                if let Some(ref pointer) = ctx.pointer {
                    use wayland_client::protocol::wl_pointer::Axis;

                    // Discrete scroll uses wheel axis source
                    pointer.axis_source(wayland_client::protocol::wl_pointer::AxisSource::Wheel);

                    let wl_axis = match axis {
                        ScrollAxis::Vertical => Axis::VerticalScroll,
                        ScrollAxis::Horizontal => Axis::HorizontalScroll,
                    };
                    let value = (steps as f64) * 15.0;
                    pointer.axis_discrete(time_ms(time_usec), wl_axis, value, steps);
                }
            }

            InputEvent::Pointer(PointerEvent::ScrollStop { time_usec }) => {
                if let Some(ref pointer) = ctx.pointer {
                    use wayland_client::protocol::wl_pointer::Axis;
                    pointer.axis_stop(time_ms(time_usec), Axis::VerticalScroll);
                    pointer.axis_stop(time_ms(time_usec), Axis::HorizontalScroll);
                }
            }

            InputEvent::Keyboard(KeyboardEvent {
                keycode,
                state,
                time_usec,
            }) => {
                if let Some(ref keyboard) = ctx.keyboard {
                    use xkbcommon::xkb;

                    let wl_state = match state {
                        KeyState::Pressed => 1u32,
                        KeyState::Released => 0u32,
                    };

                    // Run the event through xkb state so we can serialize the
                    // current modifier mask, then emit modifiers() BEFORE key()
                    // so xkb-aware consumers see the modifier as held while the
                    // key arrives. Without modifiers(), key(c) with Ctrl held
                    // arrives as bare 'c' — observed in the field as Ctrl+C
                    // producing "C" in a terminal.
                    //
                    // xkbcommon's update_key takes xkb keycodes (evdev + 8);
                    // the virtual-keyboard protocol's key() takes evdev keycodes
                    // (the compositor adds 8 internally for keymap lookup).
                    if let Some(xkb_data) = self.state.xkb.as_mut() {
                        let direction = match state {
                            KeyState::Pressed => xkb::KeyDirection::Down,
                            KeyState::Released => xkb::KeyDirection::Up,
                        };
                        let xkb_keycode: xkb::Keycode = (keycode.saturating_add(8)).into();
                        xkb_data.state.update_key(xkb_keycode, direction);

                        let mods_depressed =
                            xkb_data.state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
                        let mods_latched = xkb_data.state.serialize_mods(xkb::STATE_MODS_LATCHED);
                        let mods_locked = xkb_data.state.serialize_mods(xkb::STATE_MODS_LOCKED);
                        let group = xkb_data.state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);

                        keyboard.modifiers(mods_depressed, mods_latched, mods_locked, group);
                    }

                    keyboard.key(time_ms(time_usec), keycode, wl_state);
                }
            }

            InputEvent::Touch(_) => {
                // wlr-virtual-pointer does not support real touch input.
                // Touch is not advertised in AvailableDeviceTypes, so clients
                // should not send touch events. Return a clean error.
                return Err(PortalError::Config(
                    "Touch input not supported via wlr virtual pointer protocol".to_string(),
                ));
            }
        }

        Ok(())
    }

    /// Commit the pointer's pending protocol calls with one `pointer.frame()`,
    /// then flush and account for the send (shared tail of
    /// [`InputBackend::inject_event`] and [`InputBackend::inject_event_batch`]).
    fn commit_and_flush(&mut self, session_id: &str, needs_pointer_frame: bool) -> Result<()> {
        if needs_pointer_frame {
            if let Some(ctx) = self.sessions.get(session_id) {
                if let Some(ref pointer) = ctx.pointer {
                    pointer.frame();
                }
            }
        }

        match self.flush() {
            Ok(()) => {
                self.events_forwarded += 1;

                // Emit periodic InputBatch health event
                if self.events_forwarded.is_multiple_of(100) {
                    if let Some(ref health_tx) = self.health_tx {
                        let _ = health_tx.try_send(crate::health::PortalHealthEvent::InputBatch {
                            events_forwarded: self.events_forwarded,
                            events_failed: self.flush_failures,
                            protocol: crate::health::InputProtocolType::WlrVirtual,
                        });
                    }
                }

                Ok(())
            }
            Err(e) => {
                self.flush_failures += 1;
                if let Some(ref health_tx) = self.health_tx {
                    let _ =
                        health_tx.try_send(crate::health::PortalHealthEvent::InputDisconnected {
                            reason: format!("Wayland flush failed: {e}"),
                            recoverable: false,
                        });
                }
                Err(e)
            }
        }
    }

    /// Search `keymap` for a keycode that already produces `keysym` at level
    /// 0 (unshifted) of layout 0 — i.e. without needing the dynamic pool.
    fn static_keysym_to_keycode(keymap: &xkbcommon::xkb::Keymap, keysym: u32) -> Option<u32> {
        use xkbcommon::xkb;

        for keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let xkb_keycode = xkb::Keycode::new(keycode);
            let num_levels = keymap.num_levels_for_key(xkb_keycode, 0);

            for level in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(xkb_keycode, 0, level);
                for sym in syms {
                    if sym.raw() == keysym {
                        // XKB keycodes are evdev keycodes + 8
                        return Some(keycode - 8);
                    }
                }
            }
        }

        None
    }
}

impl InputBackend for WlrInputBackend {
    fn protocol_type(&self) -> InputProtocol {
        InputProtocol::WlrVirtualInput
    }

    fn create_context(
        &mut self,
        session_id: &str,
        devices: DeviceTypes,
    ) -> Result<Option<OwnedFd>> {
        tracing::debug!(
            session_id = %session_id,
            device_types = ?devices,
            "Creating wlr virtual input context"
        );

        if self.sessions.contains_key(session_id) {
            return Err(PortalError::InvalidSession(format!(
                "wlr context already exists for session {session_id}"
            )));
        }

        let mut ctx = WlrSessionContext {
            pointer: None,
            keyboard: None,
        };

        if devices.pointer {
            if let Some(pointer) = self.create_pointer() {
                tracing::debug!(session_id = %session_id, "Created virtual pointer");
                ctx.pointer = Some(pointer);
            } else {
                tracing::warn!("Pointer requested but virtual pointer manager unavailable");
            }
        }

        if devices.keyboard {
            if let Some(keyboard) = self.create_keyboard() {
                tracing::debug!(session_id = %session_id, "Created virtual keyboard");
                ctx.keyboard = Some(keyboard);
            } else {
                tracing::warn!("Keyboard requested but virtual keyboard manager unavailable");
            }
        }

        self.flush()?;
        self.sessions.insert(session_id.to_string(), ctx);

        tracing::info!(session_id = %session_id, "wlr virtual input context created");

        // wlr protocol doesn't use fd passing
        Ok(None)
    }

    fn destroy_context(&mut self, session_id: &str) -> Result<()> {
        if let Some(ctx) = self.sessions.remove(session_id) {
            if let Some(pointer) = ctx.pointer {
                pointer.destroy();
            }
            if let Some(keyboard) = ctx.keyboard {
                keyboard.destroy();
            }

            self.flush()?;
            tracing::info!(session_id = %session_id, "wlr virtual input context destroyed");
        }
        Ok(())
    }

    fn inject_event(&mut self, session_id: &str, event: InputEvent) -> Result<()> {
        let needs_pointer_frame = matches!(event, InputEvent::Pointer(_));
        self.apply_event(session_id, &event)?;
        self.commit_and_flush(session_id, needs_pointer_frame)
    }

    /// Apply every event in the batch, then commit exactly one
    /// `pointer.frame()` (if any pointer event was present) and one flush --
    /// see [`InputBackend::inject_event_batch`] for why atomicity matters here.
    fn inject_event_batch(&mut self, session_id: &str, events: &[InputEvent]) -> Result<()> {
        let mut needs_pointer_frame = false;
        for event in events {
            needs_pointer_frame |= matches!(event, InputEvent::Pointer(_));
            self.apply_event(session_id, event)?;
        }
        self.commit_and_flush(session_id, needs_pointer_frame)
    }

    fn process_events(&mut self) -> Result<Vec<(String, InputEvent)>> {
        self.dispatch()?;
        // wlr backend doesn't receive input events, only sends them
        Ok(vec![])
    }

    fn has_context(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    fn context_count(&self) -> usize {
        self.sessions.len()
    }

    fn keysym_to_keycode(&mut self, keysym: u32) -> Option<u32> {
        use xkbcommon::xkb;

        // Fast path: the keysym already has a keycode in the base "us"
        // layout (the common case -- ASCII/Latin characters). Doesn't touch
        // the dynamic pool, so it never costs a keymap re-upload.
        if let Some(keycode) =
            Self::static_keysym_to_keycode(&self.state.xkb.as_ref()?.keymap, keysym)
        {
            return Some(keycode);
        }

        // Fallback: dynamically bind a pool keycode to this keysym. Covers
        // CJK, accented Latin, and any other character the base layout has
        // no key for -- see EI-TEXT-SCOPING-2026-09-07.md in lamco-admin
        // (`~/lamco-admin/projects/xdg-desktop-portal-generic/`).
        let xkb_data = self.state.xkb.as_mut()?;
        let (slot, changed) = xkb_data.dynamic_pool.resolve(keysym);
        let evdev_keycode = DYNAMIC_POOL_EVDEV_CODES[slot];

        if changed {
            let Some(spliced) = splice_dynamic_keysyms(
                &xkb_data.base_keymap_string,
                &xkb_data.pool_key_names,
                &xkb_data.dynamic_pool,
            ) else {
                tracing::error!(keysym, "Failed to splice dynamic keysym into keymap text");
                return None;
            };

            let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let Some(new_keymap) = xkb::Keymap::new_from_string(
                &context,
                spliced.clone(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            ) else {
                tracing::error!(
                    keysym,
                    "Spliced keymap failed to recompile; keeping prior keymap"
                );
                return None;
            };

            xkb_data.state = xkb::State::new(&new_keymap);
            xkb_data.keymap = new_keymap;
            xkb_data.keymap_string = spliced;

            tracing::debug!(
                keysym,
                keycode = evdev_keycode,
                "Dynamically bound keycode for keysym; re-uploading keymap to active sessions"
            );

            // xkb_data's borrow ends here (last use above); re-borrow self
            // to reach `sessions` and `flush()`.
            let keymap_string = self.state.xkb.as_ref()?.keymap_string.clone();
            for ctx in self.sessions.values() {
                if let Some(ref keyboard) = ctx.keyboard {
                    if let Err(e) = Self::set_keyboard_keymap(keyboard, &keymap_string) {
                        tracing::warn!(
                            error = %e,
                            "Failed to re-upload extended keymap to a session's keyboard"
                        );
                    }
                }
            }
            if let Err(e) = self.flush() {
                tracing::warn!(error = %e, "Failed to flush after keymap re-upload");
            }
        }

        Some(evdev_keycode)
    }

    fn set_health_sender(&mut self, tx: crate::health::HealthSender) {
        self.health_tx = Some(tx);
    }

    fn set_stream_mappings(&mut self, mappings: Vec<StreamOutputMapping>) {
        self.stream_mappings.clear();
        for mapping in mappings {
            tracing::debug!(
                stream = mapping.stream_node_id,
                x = mapping.x,
                y = mapping.y,
                width = mapping.width,
                height = mapping.height,
                "Stream output mapping set"
            );
            self.stream_mappings.insert(mapping.stream_node_id, mapping);
        }
    }
}

// Wayland dispatch implementations

impl Dispatch<WlRegistry, GlobalListContents> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &WlRegistry,
        _event: <WlRegistry as wayland_client::Proxy>::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for WlrState {
    fn event(
        _state: &mut Self,
        _proxy: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        use wayland_client::protocol::wl_seat::Event;
        match event {
            Event::Capabilities { capabilities } => {
                tracing::trace!("Seat capabilities: {:?}", capabilities);
            }
            Event::Name { name } => {
                tracing::trace!("Seat name: {}", name);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wlr_state_default() {
        let state = WlrState::default();
        assert!(state.pointer_manager.is_none());
        assert!(state.keyboard_manager.is_none());
        assert!(state.seat.is_none());
        assert!(!state.initialized);
        assert!(state.xkb.is_none());
    }

    #[test]
    fn test_xkb_initialization() {
        let mut state = WlrState::default();
        WlrInputBackend::init_xkb(&mut state).expect("XKB init should succeed");

        let xkb = state.xkb.as_ref().expect("XKB data should be set");
        assert!(
            !xkb.keymap_string.is_empty(),
            "Keymap string should be non-empty"
        );
        // A valid XKB keymap starts with "xkb_keymap"
        assert!(
            xkb.keymap_string.starts_with("xkb_keymap"),
            "Keymap string should start with 'xkb_keymap'"
        );
        assert_eq!(xkb.base_keymap_string, xkb.keymap_string);
        assert_eq!(xkb.pool_key_names.len(), DYNAMIC_KEYSYM_POOL_SIZE);
        assert!(
            xkb.pool_key_names.iter().all(|n| !n.is_empty()),
            "every dynamic pool slot must resolve to a real key name"
        );
    }

    #[test]
    fn test_dynamic_keysym_pool_reuses_existing_binding() {
        let mut pool = DynamicKeysymPool::new();
        let (slot_a, changed_a) = pool.resolve(0x0100_30AB);
        assert!(changed_a);
        let (slot_a_again, changed_again) = pool.resolve(0x0100_30AB);
        assert_eq!(slot_a, slot_a_again);
        assert!(
            !changed_again,
            "resolving the same keysym twice must not re-change the pool"
        );
    }

    #[test]
    fn test_dynamic_keysym_pool_evicts_lru_when_full() {
        let mut pool = DynamicKeysymPool::new();
        // Fill every slot with a distinct keysym.
        let mut slots = Vec::new();
        for i in 0..DYNAMIC_KEYSYM_POOL_SIZE {
            let (slot, changed) = pool.resolve(0x0100_0000 + i as u32);
            assert!(changed);
            slots.push(slot);
        }
        // Touch keysym 0 so it's most-recently-used, keysym 1 stays least-recently-used.
        pool.resolve(0x0100_0000);
        // A brand-new keysym must evict slot 1 (the LRU one), not slot 0.
        let (evicted_slot, changed) = pool.resolve(0x0100_0000 + DYNAMIC_KEYSYM_POOL_SIZE as u32);
        assert!(changed);
        assert_eq!(evicted_slot, slots[1]);
        // The evicted keysym (0x0100_0001) must no longer resolve to a cache hit.
        let (_, changed_after_eviction) = pool.resolve(0x0100_0001);
        assert!(changed_after_eviction);
    }

    #[test]
    fn test_resolve_pool_key_names_finds_every_slot() {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let base_keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile base keymap");
        let base_text = base_keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);

        let names =
            resolve_pool_key_names(&base_text).expect("every reserved evdev code must resolve");
        assert_eq!(names.len(), DYNAMIC_KEYSYM_POOL_SIZE);
        assert!(names.iter().all(|n| !n.is_empty()));

        // Known-stable name for the first entry (evdev 183 = XKB F13, named
        // <FK13> in the standard "evdev" ruleset) -- a spot check that the
        // scan is reading real names, not placeholders.
        assert_eq!(names[0], "FK13");
    }

    #[test]
    fn test_splice_dynamic_keysyms_recompiles_and_resolves() {
        use xkbcommon::xkb;

        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let base_keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compile base keymap");
        let base_text = base_keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        let pool_key_names =
            resolve_pool_key_names(&base_text).expect("pool key names must resolve");

        // Two keysyms with no keycode anywhere in a plain "us" layout:
        // Katakana KA (U+30AB) and Latin small e-acute (U+00E9).
        let mut pool = DynamicKeysymPool::new();
        pool.resolve(0x0100_30AB);
        pool.resolve(0x0100_00E9);

        let spliced = splice_dynamic_keysyms(&base_text, &pool_key_names, &pool)
            .expect("splice should succeed");

        let new_keymap = xkb::Keymap::new_from_string(
            &context,
            spliced,
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("spliced keymap must recompile");

        assert_eq!(
            new_keymap.max_keycode().raw(),
            base_keymap.max_keycode().raw(),
            "splice must not change the keymap's declared maximum keycode"
        );

        // Both dynamically-bound keycodes resolve to the expected keysym, at
        // the fixed evdev codes DYNAMIC_POOL_EVDEV_CODES reserves for them.
        for (i, expected) in [(0usize, 0x0100_30AB_u32), (1, 0x0100_00E9)] {
            let keycode = xkb::Keycode::new(DYNAMIC_POOL_EVDEV_CODES[i] + 8);
            let syms = new_keymap.key_get_syms_by_level(keycode, 0, 0);
            assert!(
                syms.iter().any(|s| s.raw() == expected),
                "slot {i} should resolve to keysym 0x{expected:08x}, got {syms:x?}"
            );
        }

        // A base-layout key (AC01 = 'a', keycode 38) must be untouched by the splice.
        let a_key = xkb::Keycode::new(38);
        let a_syms = new_keymap.key_get_syms_by_level(a_key, 0, 0);
        assert!(
            a_syms.iter().any(|s| s.raw() == 0x61),
            "base 'a' key must still resolve"
        );

        // An untouched pool slot (never resolved above) must be NoSymbol, not
        // leak the key's original consumer/media symbol.
        let unused_slot = 2;
        let unused_keycode = xkb::Keycode::new(DYNAMIC_POOL_EVDEV_CODES[unused_slot] + 8);
        let unused_syms = new_keymap.key_get_syms_by_level(unused_keycode, 0, 0);
        assert!(
            unused_syms.is_empty(),
            "unused pool slot {unused_slot} should be NoSymbol, got {unused_syms:x?}"
        );
    }

    #[test]
    fn test_splice_dynamic_keysyms_missing_name_fails_closed() {
        let pool = DynamicKeysymPool::new();
        // A `pool_key_names` set whose entries don't exist anywhere in this
        // minimal keymap text -- simulates stale/mismatched names.
        let missing_names: [String; DYNAMIC_KEYSYM_POOL_SIZE] =
            std::array::from_fn(|i| format!("NOSUCHKEY{i}"));
        let result = splice_dynamic_keysyms("xkb_keymap {\n};\n", &missing_names, &pool);
        assert!(result.is_none());
    }

    #[test]
    fn test_keysym_to_keycode_via_xkb() {
        let mut state = WlrState::default();
        WlrInputBackend::init_xkb(&mut state).unwrap();

        let xkb_data = state.xkb.as_ref().unwrap();
        let keymap = &xkb_data.keymap;

        // Test that we can look up a well-known keysym (XKB_KEY_a = 0x61).
        // Should map to evdev KEY_A = 30.
        let keysym_a = 0x61u32; // XKB_KEY_a
        let mut found = false;

        for keycode in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let xkb_keycode = xkbcommon::xkb::Keycode::new(keycode);
            let num_levels = keymap.num_levels_for_key(xkb_keycode, 0);

            for level in 0..num_levels {
                let syms = keymap.key_get_syms_by_level(xkb_keycode, 0, level);
                for sym in syms {
                    if sym.raw() == keysym_a {
                        // XKB keycodes are evdev keycodes + 8
                        let evdev_keycode = keycode - 8;
                        assert_eq!(
                            evdev_keycode, 30,
                            "XKB_KEY_a should map to evdev KEY_A (30)"
                        );
                        found = true;
                    }
                }
            }
        }
        assert!(found, "Should find a keycode for XKB_KEY_a");
    }

    #[test]
    fn test_layout_bounds_empty() {
        // No stream mappings → reasonable default, origin at (0, 0)
        let mappings = HashMap::new();
        let bounds = WlrInputBackend::layout_bounds(&mappings);
        assert_eq!(bounds, (0, 0, 1920, 1080));
    }

    #[test]
    fn test_layout_bounds_single_monitor() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                mapping_id: Some("output:mon1".to_string()),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );

        let bounds = WlrInputBackend::layout_bounds(&mappings);
        assert_eq!(bounds, (0, 0, 2560, 1440));
    }

    #[test]
    fn test_layout_bounds_dual_monitor_side_by_side() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                mapping_id: Some("output:mon1".to_string()),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                mapping_id: Some("output:mon2".to_string()),
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );

        let bounds = WlrInputBackend::layout_bounds(&mappings);
        assert_eq!(bounds, (0, 0, 4480, 1440)); // 1920 + 2560, max(1080, 1440)
    }

    #[test]
    fn test_layout_bounds_stacked_monitors() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                mapping_id: Some("output:mon1".to_string()),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                mapping_id: Some("output:mon2".to_string()),
                x: 0,
                y: 1080,
                width: 1920,
                height: 1080,
            },
        );

        let bounds = WlrInputBackend::layout_bounds(&mappings);
        assert_eq!(bounds, (0, 0, 1920, 2160)); // same width, 1080 + 1080
    }

    #[test]
    fn test_layout_bounds_monitor_left_of_origin() {
        // A monitor placed left of the primary sits at a negative x -- a real,
        // supported layout, not an edge case. The old bounding-box logic assumed
        // the origin was always (0, 0) and silently mispositioned this case.
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                mapping_id: Some("output:mon1".to_string()),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                mapping_id: Some("output:mon2".to_string()),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );

        let bounds = WlrInputBackend::layout_bounds(&mappings);
        assert_eq!(bounds, (-1920, 0, 3840, 1080));
    }

    #[test]
    fn test_pointer_regions_no_mappings_falls_back_to_single_region() {
        let regions = WlrInputBackend::compute_pointer_regions(&HashMap::new());
        assert_eq!(
            regions,
            vec![PointerRegion {
                mapping_id: None,
                offset_x: 0,
                offset_y: 0,
                width: 1920,
                height: 1080,
            }]
        );
    }

    #[test]
    fn test_pointer_regions_dual_monitor_side_by_side() {
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                mapping_id: Some("output:mon1".to_string()),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                mapping_id: Some("output:mon2".to_string()),
                x: 1920,
                y: 0,
                width: 2560,
                height: 1440,
            },
        );

        let mut regions = WlrInputBackend::compute_pointer_regions(&mappings);
        regions.sort_by_key(|r| r.offset_x);
        assert_eq!(
            regions,
            vec![
                PointerRegion {
                    mapping_id: Some("output:mon1".to_string()),
                    offset_x: 0,
                    offset_y: 0,
                    width: 1920,
                    height: 1080,
                },
                PointerRegion {
                    mapping_id: Some("output:mon2".to_string()),
                    offset_x: 1920,
                    offset_y: 0,
                    width: 2560,
                    height: 1440,
                },
            ]
        );
    }

    #[test]
    fn test_pointer_regions_shift_to_layout_origin_for_negative_offset_monitor() {
        // Same layout as test_layout_bounds_monitor_left_of_origin: region offsets
        // are unsigned on the wire, so the left-of-origin monitor's real x (-1920)
        // must come out shifted to 0, and the primary's real x (0) shifted to 1920.
        let mut mappings = HashMap::new();
        mappings.insert(
            1,
            StreamOutputMapping {
                stream_node_id: 1,
                mapping_id: Some("output:mon1".to_string()),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );
        mappings.insert(
            2,
            StreamOutputMapping {
                stream_node_id: 2,
                mapping_id: Some("output:mon2".to_string()),
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        );

        let mut regions = WlrInputBackend::compute_pointer_regions(&mappings);
        regions.sort_by_key(|r| r.mapping_id.clone());
        assert_eq!(
            regions,
            vec![
                PointerRegion {
                    mapping_id: Some("output:mon1".to_string()),
                    offset_x: 1920,
                    offset_y: 0,
                    width: 1920,
                    height: 1080,
                },
                PointerRegion {
                    mapping_id: Some("output:mon2".to_string()),
                    offset_x: 0,
                    offset_y: 0,
                    width: 1920,
                    height: 1080,
                },
            ]
        );
    }
}
