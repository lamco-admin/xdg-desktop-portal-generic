# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
