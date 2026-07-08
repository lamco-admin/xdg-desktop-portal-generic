# Roadmap

`xdg-desktop-portal-generic` is a standalone, compositor-agnostic XDG Desktop
Portal backend. It implements the RemoteDesktop, ScreenCast, Clipboard,
Settings, and Screenshot portal interfaces against standard Wayland protocols,
with automatic per-domain protocol fallback (`ext-` preferred, `wlr-` retained
for older compositors). This file records direction; it is not a release
schedule.

## ScreenCast protocol version

The backend advertises `ScreenCast` interface version 6. Each stream result
carries the v5 `mapping_id` and the v6 `pipewire-serial` property — the
PipeWire `object.serial` of the stream node — letting consumers re-follow a
stream across output reconfiguration without depending on the deprecated
`node_id`. The `xdg-desktop-portal` frontend (1.22 series) consumes
`pipewire-serial` and caps the app-visible version at `MIN(impl_version, 6)`.
Reading the serial needs `libpipewire` >= 0.3.64 at runtime; older PipeWire
degrades cleanly to v5 (the property is simply omitted).

## Capture: ext-image-copy-capture

Screen capture uses `ext-image-copy-capture-v1` as the primary path with
`wlr-screencopy-unstable-v1` retained as the fallback. Cursor modes (hidden /
embedded / metadata) and per-output capture are implemented. The remaining
functional work on the ext path is **damage-region tracking** — consuming the
protocol's per-frame `frame.damage` regions rather than always marking
full-frame damage — and, longer term, **per-window capture** via
`ext-image-capture-source-v1` foreign-toplevel sources on compositors that
expose them. The underlying protocols are frozen and stable, so this is
implementation work, not a protocol-version dependency.

## HDR / 10-bit capture

The current capture output is 8-bit SDR (a hardcoded `BGRx` SPA format carrying no
color metadata). A planned direction is HDR-aware capture: on the `ext-` path,
negotiate 10-bit (`xRGB2101010`) or FP16 (`RGBA_F16`) formats where the compositor
offers them; read the source output's color state via `wp-color-management`; and
publish the true colorimetry — primaries, transfer function, matrix, range — on
the PipeWire stream's SPA video format instead of dropping it. That makes the
encoding self-describing so consumers can handle HDR downstream. It builds on the
pixel-format propagation fix (carry the real captured format rather than assuming
`BGRx`), which is the prerequisite; the `wlr-screencopy` path stays 8-bit. Full
requirements (with fallback/no-regression bars) are tracked in the lamco-admin
planning notes for this project (`10-BIT-HDR-CAPTURE-REQUIREMENTS-2026-07-07.md`).

## Dependency modernization

A non-breaking refresh of compatible dependencies rides each release. The
deliberately-sequenced upgrades are:

- **PipeWire / libspa 0.10** — small in practice (the loop `iterate()` timeout
  argument becomes a `Timeout` enum); it also exposes SPA metadata wrappers
  (`Buffer::find_meta`) used for cursor and damage handling, complementing the
  ext-capture work above.
- **reis 0.7** — the EIS bridge tracks the libei 1.6 generation of the
  bindings (a small additive set of request variants).

## Public API surface

The crate currently re-exports most internal modules. A future release will
tighten the public surface to the intended contract — the `PortalBackend`
entry point, the capture/input/clipboard backend traits and factories, and the
shared event and type definitions — and make implementation modules private.
This is a deliberate semantic-versioning break and will land in a dedicated
version bump with a clear changelog.

## Tracking upstream

Dynamic screencast stream metadata *beyond* `pipewire-serial` — live geometry,
size, and scale as a stream reconfigures — is an active upstream design in
PipeWire (a well-known-tag vocabulary). The backend will adopt it once the
vocabulary stabilizes and ships in a PipeWire release; until then,
`pipewire-serial` plus stream re-follow covers the output-reconfiguration case.

## Distribution and attribution

Binary releases carry a `THIRD_PARTY_NOTICES.md` aggregating dependency license
texts (both MIT and Apache-2.0 require carrying notices in binary
distributions). It is generated from the lockfile by `cargo-about`
(`about.toml` + `about.hbs`).
