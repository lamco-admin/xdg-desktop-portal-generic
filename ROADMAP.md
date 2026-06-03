# Roadmap

`xdg-desktop-portal-generic` is a standalone, compositor-agnostic XDG Desktop
Portal backend. It already implements the RemoteDesktop, ScreenCast, Clipboard,
Settings, and Screenshot portal interfaces against standard Wayland protocols,
with automatic per-domain protocol fallback. This file records direction; it is
not a release schedule.

## Public API surface

The crate currently re-exports most of its internal modules. A future release
will tighten the public surface to the intended contract — the `PortalBackend`
entry point, the capture/input/clipboard backend traits and factories, and the
shared event and type definitions — and move the implementation modules private.
This is a deliberate semantic-versioning break and will land in a dedicated
version bump with a clear changelog.

## Capture: ext-image-copy-capture

Screen capture is moving to `ext-image-copy-capture-v1` as the primary path,
with `wlr-screencopy-unstable-v1` retained as the fallback. Completing and
hardening the ext-image-copy-capture backend (multi-output, cursor modes, damage
tracking) is the next functional milestone.

## Dependency modernization

Keep the dependency set current. A non-breaking refresh of compatible
dependencies rides each release; the PipeWire / libspa 0.10 migration is a
larger, breaking step that will be sequenced into a version bump of its own.

## Distribution and attribution

Binary releases carry a `THIRD_PARTY_NOTICES.md` aggregating dependency license
texts (both MIT and Apache-2.0 require carrying notices in binary
distributions). It is generated from the lockfile by `cargo-about`
(`about.toml` + `about.hbs`).
