# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-09-10

### Fixed

- **Chromium/Electron silently dropped EIS text/keysym injection for any
  out-of-layout character** (CJK, accented Latin, EuroSign, etc). The
  `DynamicKeysymPool` reserved keycodes past the base keymap's own maximum
  (evdev ~701-732), entirely outside Chromium's `dom_code_data.inc`
  evdev-to-DomCode lookup table (highest covered code: 633,
  `PrivacyScreenToggle`), so Chromium's own keyboard dispatch dropped the
  key before ever consulting the keymap. The identical bug class KWin fixed
  upstream (commit `584bba048`, 2026-08-14) for its own single scratch
  keycode. Fixed by reserving 32 existing F13-F23/consumer keycodes instead
  (all covered by Chromium's table, all already declared in the standard
  "evdev" XKB ruleset), resolved by name at startup rather than hardcoded,
  with startup failing closed if an expected keycode is missing.
- **The EIS absolute-pointer region's `mapping_id` never matched the
  corresponding ScreenCast stream's own `mapping_id` property.** The
  capture side tagged a stream `"output:<name>"`; the EIS side sent a
  stringified PipeWire node ID instead. A spec-compliant multi-monitor
  client correlating a video stream to its input region by `mapping_id`
  string equality (the mechanism GNOME and KDE both use) could never
  succeed. `PointerRegion.mapping_id` now carries the same string the
  ScreenCast stream advertises.
- **`StreamInfo.position` was hardcoded to `(0, 0)`** instead of the
  source's real compositor-global x/y, even though `SourceInfo` already
  had the real value. This fed directly into both the RemoteDesktop and
  ScreenCast D-Bus `position` property and the input backend's region
  offsets, so every monitor other than one already at the origin reported
  and used the wrong position.
- **`ei_scroll.scroll_discrete` events were forwarded roughly 120x too
  fast.** The wire value is "fractions or multiples of 120" per the libei
  protocol (one full wheel click == 120); a single real click was passed
  straight through as 120 discrete steps to both wlr and uinput injection,
  which expect a plain click count. Fixed with a per-session remainder
  accumulator that folds the raw value into whole clicks and carries any
  leftover fraction forward, so high-resolution devices sending sub-120
  deltas per event also scroll correctly instead of losing their input to
  integer-division-to-zero.
- **`ei_device.region`'s scale argument was hardcoded to `1.0`** regardless
  of the output's real buffer scale. Nothing in the crate tracked
  `wl_output.scale` at all; the event fell through a wildcard match and was
  discarded. Now bound and threaded through `SourceInfo`/`StreamInfo`/
  `StreamOutputMapping` into `PointerRegion.scale`, matching Mutter's
  `eis_region_set_physical_scale` and cosmic-comp/Smithay's `EiRegion.scale`.
- **`InputCapture.ConnectToEIS`'s `NotSupported` error return could crash
  the entire portal daemon on an unpatched `flatpak/xdg-desktop-portal`
  frontend** (issue #2138, filed 2026-09-09), not just the failing session.
  `CreateSession` (v1) and `Start` (v2) now refuse a session on a non-EIS
  backend with a normal `Response::Other` before the session is considered
  started, so `ConnectToEIS` is never reached for this avoidable case. The
  `NotSupported` check itself remains as defense in depth.
- **An ungraceful client disconnect (process killed, D-Bus name lost
  without an explicit `Close()` call) never removed the session's own
  D-Bus object**, unlike the explicit `Session.Close()` path. Every other
  cleanup step (EIS context teardown, InputCapture barrier-surface
  destruction, capture stream/PipeWire teardown) was already identical
  between the two paths.

### Breaking

- **`PointerRegion.mapping_id` changed from `Option<u32>` to
  `Option<String>`.** It now carries the same string
  `StreamOutputMapping::mapping_id`/`StreamInfo::mapping_id` use, not a
  stringified PipeWire node ID. `PointerRegion` also gained a `scale: f32`
  field and lost its `Copy`/`Eq` derives (kept `Clone`/`PartialEq`), since
  a `String` field can't be `Copy` and `f32` isn't `Eq`. `PointerRegion`
  is public because `EisSession::new` takes a `Vec<PointerRegion>`
  directly; callers going through the `InputBackend` trait object (which
  computes the list internally via `WlrInputBackend::pointer_regions`) are
  unaffected. `lamco-rdp-server-dev` uses only the trait-object path and
  needs no changes.
- **`StreamOutputMapping` gained `mapping_id: Option<String>` and
  `scale: i32` fields.** `StreamInfo` and `SourceInfo` each gained a
  `scale: i32` field. All three structs have public fields with no
  `#[non_exhaustive]`, so any external code constructing one via struct
  literal (rather than receiving one from this crate's own APIs) needs to
  add the new fields.

## [0.8.0] - 2026-09-09

### Fixed

- **The EIS bridge's `PointerAbsolute` device never advertised any region,
  and multi-monitor absolute pointer targeting was silently broken as a
  result.** `eis_backend.rs` created the device with the absolute-pointer
  capability but never called `ei_device.region()` on it — a libei
  implementation bug in its own right (the protocol says advertising the
  capability without a region is one) — and `eis_bridge.rs` then treated
  every incoming `ei_pointer_absolute` request as already-normalized
  `[0, 1]` coordinates with a hardcoded, meaningless target stream. On any
  multi-monitor layout, an absolute motion intended for any monitor other
  than whichever one happened to be registered first got mapped against
  the wrong monitor's geometry (or, for real compositor-pixel-scale
  coordinates, clamped to the edge of whatever the fallback interpreted
  as "normalized"). Dates to `678d074c` (2026-05-22), a documented but
  never-tracked punt; found by reading `xdg-desktop-portal-hyprland#426`
  in full, which independently landed the same class of fix. Full
  root-cause writeup:
  `lamco-admin/projects/xdg-desktop-portal-generic/EIS-ABSOLUTE-POINTER-MULTI-MONITOR-GAP-2026-09-09.md`.
- **The layout bounding-box computation assumed the layout's own origin
  was always `(0, 0)`.** A monitor placed left of or above the primary
  (a real, supported layout) has a negative `x`/`y` in compositor-global
  coordinates; the old logic only tracked the maximum right/bottom edge,
  so it silently mispositioned this case for the D-Bus
  `NotifyPointerMotionAbsolute` multi-monitor path too. Replaced with an
  origin-aware `layout_bounds` that tracks the true minimum as well.

### Added

- **EIS regions, one per known output, on the `PointerAbsolute` device**,
  each shifted into the layout's own top-left-anchored coordinate space
  and tagged via `ei_device.region_mapping_id` with the corresponding
  PipeWire stream node ID, so a client can correlate a region with its
  matching ScreenCast stream the same way the D-Bus
  `NotifyPointerMotionAbsolute` caller already can explicitly. Falls back
  to a single region covering the default layout size when no outputs are
  known yet (never zero regions, per the libei requirement above).

### Breaking

- **`EisSession::new` gained a required `pointer_regions: Vec<PointerRegion>`
  parameter.** `EisSession` is public and directly constructible, so this
  breaks any external caller that doesn't go through the `InputBackend`
  trait object (which computes the list internally via
  `WlrInputBackend::pointer_regions` and is unaffected).
  `lamco-rdp-server-dev` uses only the trait-object path and needs no
  changes. `PointerRegion` itself is a new public struct.

## [0.7.0] - 2026-09-07

### Breaking

### Breaking

- **`SourceInfo` gained public `x`/`y` fields.** All of its fields are
  public with no `#[non_exhaustive]`, so this is a breaking change for
  any external code constructing `SourceInfo` by struct literal (reading
  its fields, or constructing via the crate's own APIs, is unaffected).
  Bumping the minor version per this project's 0.x SemVer convention
  rather than treating it as a patch.
- **`RawFrame` gained public `damage_regions: Vec<DamageRect>`.** Same
  situation as `SourceInfo` above — public field, no `#[non_exhaustive]`,
  breaks external struct-literal construction only.

### Added

- **`InputCapture` now delivers composed text from a real input method
  during an active capture, not just raw keystrokes.** Built ahead of any
  actual consumer (`lamco-rdp-server-dev` doesn't consume `InputCapture`
  yet) so this side is complete when one exists. When `zwp_text_input_v3`
  is bound, a barrier surface enables it for as long as it holds real
  keyboard focus; a real input method's `commit_string` (applied at
  `done`) is forwarded as an `ei_text.utf8` event to the capturing client,
  chunked on UTF-8 character boundaries under the protocol's 254-byte
  cap. Additive to the existing per-keystroke forwarding, and entirely
  optional — its absence never affects barrier/lock/keyboard-focus
  behavior.
- **`ei_text.utf8` injection requests are now realized**, completing the
  `TextKeysym` fix below: a whole string is decomposed into a
  press+release keypress sequence via the same dynamic keysym pool,
  since the underlying virtual keyboard protocol has no "type this
  string" primitive.
- **`ext-image-copy-capture` damage-region tracking.** The compositor's
  per-frame `damage` events (previously logged and discarded) are now
  accumulated and exposed as `RawFrame::damage_regions` on the direct
  in-process frame channel — an empty list still means "whole frame
  changed" (the `wlr-screencopy` path's own real, unavoidable behavior),
  but the `ext` path now reports real incremental regions when the
  compositor provides them. `PortalHealthEvent::FrameCaptured`'s
  `damage_region_count` reports the real count instead of a hardcoded
  `1`. Publishing the same regions over the PipeWire wire protocol
  (`SPA_META_VideoDamage`, for non-embedded consumers) is a separate,
  not-yet-implemented remainder — see `DAMAGE-REGION-TRACKING-2026-09-07.md`
  in the lamco-admin planning notes for this project.

### Fixed

- **Multi-monitor output enumeration collapsed every output to the first
  one's name, mode, and position.** The startup `wl_output` binding loop
  used `GlobalList::bind()`, which is designed for singleton globals
  (`wl_compositor`, `wl_seat`, ...) and always binds the *first* global
  matching an interface — looped over N outputs, it silently rebound the
  same first-found output N times. Each `OutputInfo` still got the right
  `global_name`, but every Wayland event describing the output (name,
  mode, geometry) came from that one real output, so all sources reported
  identical name/dimensions and every capture stream but the first
  allocated the wrong buffer size for its actual target. Fixed by binding
  each `wl_output` global individually by its registry name via
  `WlRegistry::bind()`, matching the pattern the hotplug path already
  used correctly.
- **`SourceInfo` carried no position at all.** Even with the binding fix
  above, every source reported (0, 0) — there was nowhere to put a real
  position. Added `x`/`y` fields to `SourceInfo`, populated from
  `OutputInfo` (which already tracked them from `wl_output.geometry`).
- **Captured pixel data was not normalized to the format it was declared
  as, causing red/blue channel swaps on compositors that don't deliver
  `xrgb8888`.** Both the PipeWire output path (`src/pipewire/stream.rs`,
  which always advertises `BGRx`) and the screenshot PNG encoder
  (`src/dbus/screenshot.rs`) copied captured buffers through unconditionally,
  ignoring the actual `wl_shm` format the compositor delivered. On
  `wlroots + virtio-gpu` (and any other compositor emitting `xbgr8888`/
  `abgr8888`), the true in-memory byte order is RGBx/RGBA, not BGRx/BGRA —
  e.g. Breeze attention-blue `#3daee9` rendered as `#e9ae3d` (golden brown).
  Added `wl_shm_format_needs_rb_swap`/`swap_rb_channels_in_place` (`src/types.rs`)
  and wired both paths to normalize red/blue when the source format needs it.
  Also fixed `RawFrame::format_raw`'s doc comment, which claimed it was an
  SPA format when it's actually the raw `wl_shm::Format` value.
- **systemd user unit installed to a path `systemctl --user` never searches.**
  The Makefile installed to `$(LIBEXECDIR)/systemd/user`
  (`/usr/libexec/systemd/user`), which is not one of the search paths
  `systemd.unit(5)` documents — the unit was silently invisible to
  `systemctl --user`. D-Bus activation (the separately-installed `.service`
  file) worked regardless, which is why this went unnoticed. Now resolves
  the path via `pkg-config --variable=systemduserunitdir systemd`, falling
  back to the standard `$(PREFIX)/lib/systemd/user` if pkg-config or
  `systemd.pc` aren't available.
- **`ei_text.keysym` injection requests were silently dropped, and the
  capability was never advertised in the first place.** `lamco-rdp-server`
  already sends `ei_text.keysym()` for every RDP-side Unicode keyboard
  character that has no direct evdev keycode (CJK, accented Latin, and
  other non-ASCII input) — but on any wlroots-family compositor going
  through this bridge, that request went nowhere: the `ei_text` device
  capability was never negotiated, so the sending side's own guard silently
  no-op'd. Net effect: typing or pasting non-ASCII Unicode through
  `lamco-rdp-server` on Sway, Hyprland, or any other compositor answering
  `ConnectToEIS` via this crate was a no-op. Now advertises
  `DeviceCapability::Text` alongside `DeviceCapability::Keyboard`, and
  `EisRequest::TextKeysym` resolves to a real keypress: `keysym_to_keycode`
  (used by both the EIS path and `NotifyKeyboardKeysym`) now falls back to
  a small LRU pool of dynamically-bound keycodes, layered on top of the
  self-generated "us" XKB keymap via on-the-fly keymap text splicing and
  re-upload, when a keysym has no keycode in the static base layout.
  `EisRequest::TextUtf8` is realized too now (see Added, above).
- **MSRV (Rust 1.87) build broken by a let-chain.** The keymap re-upload
  loop added above used `if let ... && let ...`, a Rust 1.88 feature — one
  minor version past this crate's declared MSRV. Passed every local check
  because local verification only built against the ambient toolchain
  (newer than MSRV); only caught by CI's separate `msrv` job, which stayed
  red across two pushes before being noticed. Un-chained into nested
  `if let`.
- **`cargo doc` broken by a private intra-doc link.** `queue_frame`'s doc
  comment (from the color-format fix above) linked to
  `Self::build_video_format_pod`, a private method — rejected under
  `rustdoc::private_intra_doc_links` with `-D warnings`, which CI's `doc`
  job sets. Same root cause as the MSRV break above: local verification
  didn't run `cargo doc` either. Dropped the link, kept the method name as
  plain text. Local verification now runs all 7 of this crate's CI jobs
  directly (fmt, clippy, test, doc, msrv, deny, package), not a subset.

## [0.6.1] - 2026-08-26

### Fixed

- **EIS frame-boundary loss**: events staged by a client between two
  `ei_device.frame()` calls were forwarded to the wlr backend one at a
  time instead of committed together, so a client that correctly
  batched a coordinated group (e.g. move immediately followed by a
  click) still had it split into separate compositor-visible events
  here. `EisBridgeBackend` now stages converted events per session and
  forwards them as one atomic batch when the frame boundary arrives,
  via a new `InputBackend::inject_event_batch` method (default:
  forwards to `inject_event` per event, so other implementors are
  unaffected).
- **Missing value-range validation** on incoming button codes and
  keycodes: reis's own request converter doesn't bounds-check these
  fields, so an out-of-range value from the wire reached the wlr
  backend unfiltered. Added a check against the evdev `KEY_CNT` bound
  (768); out-of-range values are logged and dropped.
- **`ei_device.resumed()` sent before the client acknowledged
  readiness**: a protocol v3+ device is supposed to withhold `resumed`
  until the client's `ei_device.ready()` request arrives, but it was
  being sent unconditionally right after device creation. Now gated on
  the negotiated device version.
- **Simultaneous diagonal scroll silently dropped one axis**: an
  `ei_scroll.scroll_discrete` request carrying both X and Y in one
  event only produced a single `ScrollDiscrete` `InputEvent`. Now
  emits one event per nonzero axis.

### Added

- New unit tests for the value-range validation and diagonal-scroll
  splitting logic.

## [0.6.0] - 2026-08-24

### Added

- **`org.freedesktop.impl.portal.InputCapture`**, implemented across four
  phases:
  - **Phase 1**: fully spec-shaped D-Bus interface (all 9 methods, 4
    signals, 2 properties), real session state, live monitor geometry for
    `GetZones` (wraparound-safe `zone_set` versioning on hotplug/resize),
    and full structural validation for `SetPointerBarriers`
    (axis-aligned, non-degenerate, non-zero id, stale-`zone_set`
    rejection). `ConnectToEIS` reuses the existing EIS bridge directly.
    New additive types `InputCaptureZone`/`PointerBarrier`.
  - **Phase 2a**: real, invisible `wlr-layer-shell-v1` barrier surfaces
    created on `Enable()`, one per accepted barrier, positioned by a
    pure barrier-to-geometry function. `Disable()`/`Release()`/session
    close tear them down. `SupportedCapabilities` now gates on the
    protocols that actually place and enforce barriers.
  - **Phase 2b**: the pointer is locked on barrier entry
    (`zwp_pointer_constraints_v1`), relative motion
    (`zwp_relative_pointer_v1`) is delivered to the connected EIS client
    in receiver context (the reverse direction from RemoteDesktop's
    existing sender-context EIS use). `Activated`/`Deactivated` now carry
    the spec's `activation_id` correlation. `Release()` ends only the
    current activation, leaving barriers armed to re-trigger.
  - **Phase 2c**: keyboard focus is grabbed for a captured session (a
    real `wl_keyboard` bound in parallel to `wl_pointer`, toggling
    `keyboard_interactivity` on the barrier surface), and
    `cursor_position` (spec-optional on `Activated`/`Deactivated`/
    `Release`) is computed from real barrier/zone geometry instead of
    left unset. `InputBackend` gained three new trait methods
    (`forward_captured_key`, `forward_captured_modifiers`,
    `set_shared_wayland_state`) with default loud-error implementations,
    so existing `InputBackend` implementors are not broken.

### Fixed

- **Pace `wlr-screencopy`/`ext-image-copy-capture` requests to avoid
  wasted frames.** Both capture paths re-requested the next frame the
  instant the previous one was delivered, with no rate limit. On
  compositors that don't throttle fulfillment to their own repaint cycle
  (observed: wayfire), this served requests as fast as asked, so most
  captured frames were discarded by the downstream bounded channel before
  reaching a consumer — real render + SHM-copy work wasted every time.
  Both paths now support an opt-in `min_frame_interval`.
- Suppress a clippy pedantic false positive
  (`unused_async_trait_impl`) on `#[zbus::interface]` property getters —
  the zbus macro requires literal `async fn` syntax and rejects the
  lint's own suggested rewrite.
- Clear the `anyhow` RUSTSEC-2026-0190 advisory (1.0.102 → 1.0.103).
  `quick-xml` RUSTSEC-2026-0194/0195 is ignored in `deny.toml` with
  rationale: it's a build-time-only proc-macro dependency of
  `wayland-scanner` parsing trusted first-party protocol XML, absent
  from the runtime binary, and a lock fix is blocked on
  `wayland-scanner` bumping past its `quick-xml` < 0.41 pin upstream.

## [0.5.0] - 2026-06-18

### Breaking

- `PipeWireManager::create_stream` now returns `StreamIds` (the PipeWire node
  id plus the optional `object.serial`) instead of `u32`; callers read
  `.node_id` (and the new `.serial`).
- `StreamInfo` gained a `serial: Option<u64>` field and is now
  `#[non_exhaustive]`.
- `PipeWireCommand::CreateStream`'s reply channel now carries `StreamIds`.

### Added

- **ScreenCast v6**: the backend advertises `ScreenCast` interface version 6
  and emits the `pipewire-serial` stream property (the PipeWire node's
  `object.serial`, D-Bus type `t`) alongside the v5 `mapping_id`. Clients use
  it to re-follow a stream across output reconfiguration without relying on
  the deprecated node id. Emitted only when PipeWire supplies a non-zero
  serial (`libpipewire` >= 0.3.64); older PipeWire degrades cleanly to v5.

### Changed

- **Dependencies modernized**: PipeWire / libspa 0.9 -> 0.10 (the loop
  `iterate()` timeout argument is now a `Timeout` enum), reis 0.6 -> 0.7,
  nix 0.30 -> 0.31, png 0.17 -> 0.18, plus a compatible refresh of the rest
  of the tree (tokio 1.52, zbus 5.16, zvariant 5.12, wayland-client 0.31.14,
  wayland-protocols 0.32.12, and others). MSRV remains 1.87.
- **Publishing metadata**: `homepage` now points to the product page
  (`https://lamco.ai/open-source/xdg-desktop-portal-generic/`) and
  `documentation` to docs.rs; the README carries Website / Documentation /
  Source links, per the website-link publishing standard. CI gained a
  `cargo package` verification job.

### Fixed

- **Standalone D-Bus service path now works end-to-end** (it had only been
  exercised via the embedded library API). Three latent defects were fixed,
  verified against a live `xdg-desktop-portal` ScreenCast session on COSMIC:
  - Introspection XML is valid again. Doc comments are no longer emitted into
    introspection (`introspection_docs = false` on every interface); a `--`
    inside a generated XML comment produced malformed introspection that broke
    property reads for strict clients (sd-bus/`busctl`, expat).
  - Each interface's `version` property is exposed under its spec name
    `version` (lowercase) rather than the auto-PascalCased `Version`, so the
    portal frontend's version negotiation can read it.
  - `ScreenCast.Start` and `Session.Close` no longer panic ("cannot start a
    runtime from within a runtime"); the synchronous capture create/destroy
    calls bridge to the async PipeWire manager via `block_in_place`.
- EIS bridge: a client device `Release` (surfaced by reis 0.7 as the new
  `DeviceClosed` request) now completes teardown via `Device::remove()`,
  emitting the protocol `destroyed` events that reis 0.6 dropped silently.

## [0.4.0] - 2026-06-02

### Added

- **ext-image-copy-capture direct frame channel**: a `frame_tx` sender on the
  ext-capture state mirrors the screencopy backend, so captured frames are
  delivered in-process to the active consumer regardless of which capture
  protocol the compositor selected for the session.
- Unit tests for the health module (`CaptureMetrics` moving-average, failure
  counting, FPS floor; health-channel ordered delivery and buffering within
  capacity).
- `THIRD_PARTY_NOTICES.md` aggregating dependency license texts for binary
  distribution (generated by `cargo-about`), and a `ROADMAP.md`.

### Changed

- Copyright holder is now **Lamco Development LLC** (`LICENSE-MIT`,
  `LICENSE-APACHE`).
- **MSRV raised to 1.87** (required by `zbus` 5.14 / `zvariant` 5.10).
- CI raised to the full gate: fmt, clippy `-D warnings` across all targets and
  features, test, doc `-D warnings`, an MSRV 1.87 build, and `cargo-deny`.
- Dropped the deprecated `authors` field and the redundant top-level `LICENSE`
  file (the dual `LICENSE-MIT` + `LICENSE-APACHE` remain canonical).

### Fixed

- Clippy: `assigning_clones` in the capture wiring; replaced a tautological
  capture-detection test assertion with real default-value checks.

## [0.3.0] - 2026-03-15

### Added

- **Health monitoring module** (`health.rs`): `PortalHealthEvent` enum with
  capture latency, input counters, EIS serial tracking, clipboard metrics.
  `HealthSender`/`HealthReceiver` channel for downstream consumers.
- **Capture health instrumentation**: screencopy and ext-capture backends
  measure capture latency (time from `copy()` to `ready` event) and emit
  `FrameCaptured`/`FrameFailed` health events.
- **Input health instrumentation**: wlr backend emits periodic `InputBatch`
  events with forwarded/failed counts. EIS bridge harvests Frame serial
  numbers, device lifecycle events, and typed disconnect reasons.
- **Clipboard health instrumentation**: `ClipboardTransferResult` events on
  `set_clipboard` operations.
- **`uinput` feature** (optional): Kernel-level pointer injection via
  `/dev/uinput` for compositors without `wlr-virtual-pointer` (e.g., COSMIC).
  Uses `evdev` 0.13 `VirtualDeviceBuilder` with ABS_X/ABS_Y absolute
  positioning. Requires `input` group membership.
- `set_health_sender()` method on `InputBackend` and `ClipboardBackend` traits.
- `WaylandConnection::set_health_sender()` to wire health channel to capture
  backends before event loop spawn.
- `InputProtocolType::Uinput` variant for health event reporting.
- `EisFrameReceived` and `EisDeviceStateChanged` health event variants.

### Changed

- **Edition upgraded to 2024** (Rust 1.85 minimum).
- Replaced manual `Default` impl for `ScreencopyState` with derive.
- Fixed pre-existing clippy pedantic warnings: needless borrow, derivable
  impls, expect_used annotations, type complexity, missing docs.

### Fixed

- Pre-existing `set_stream_mappings` missing documentation warning.

## [0.2.1] - 2026-03-04

### Fixed

- Handle `WouldBlock` in Wayland event loop dispatch.

## [0.2.0] - 2026-03-01

### Added

- **EIS bridge backend**: Accept EIS connections from portal clients, parse
  input events using reis 0.6 high-level API, forward to compositor through
  wlr virtual keyboard/pointer protocols.
- Clipboard MIME charset fallback in `read_selection` and source sends.
- `update_source_data` API for post-announcement clipboard data provision.
- `event_created_child` for data control device dispatchers.

### Changed

- Upgraded nix to 0.30, xkbcommon to 0.9.
- Removed unsafe pipe workarounds (replaced by nix safe APIs).

## [0.1.0] - 2026-02-24

### Added

- ScreenCast v5 portal with ext-image-copy-capture-v1 and wlr-screencopy-v1 fallback.
- RemoteDesktop v2 portal with EIS bridge mode and wlr virtual input fallback.
- Clipboard v1 portal with ext-data-control-v1 and wlr-data-control-v1 fallback.
- Settings v2 portal with environment variable configuration and GTK_THEME detection.
- Screenshot v2 portal with single-frame capture to PNG and external color picker support.
- PipeWire integration for screen capture frame delivery.
- Session management with stale session cleanup.
- Output hotplug detection and propagation.
- External source picker and color picker tool support.

### Note

docs.rs builds will fail for this crate because it requires system libraries
(`libpipewire-0.3`, `libwayland-client`, `libxkbcommon`) not available in the
docs.rs build environment. Build documentation locally with `cargo doc --no-deps`.
