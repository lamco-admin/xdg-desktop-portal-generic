# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
