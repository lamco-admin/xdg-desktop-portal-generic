# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-02-24

### Added

- ScreenCast v5 portal with ext-image-copy-capture-v1 and wlr-screencopy-v1 fallback
- RemoteDesktop v2 portal with EIS bridge mode and wlr virtual input fallback
- Clipboard v1 portal with ext-data-control-v1 and wlr-data-control-v1 fallback
- Settings v2 portal with environment variable configuration and GTK_THEME detection
- Screenshot v2 portal with single-frame capture to PNG and external color picker support
- PipeWire integration for screen capture frame delivery
- Session management with stale session cleanup
- Output hotplug detection and propagation
- External source picker and color picker tool support

### Note

docs.rs builds will fail for this crate because it requires system libraries
(`libpipewire-0.3`, `libwayland-client`, `libxkbcommon`) not available in the
docs.rs build environment. Build documentation locally with `cargo doc --no-deps`.
