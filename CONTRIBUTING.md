# Contributing

Contributions are welcome. This document covers the development workflow,
testing, and code quality requirements.

## Development Setup

Install dependencies (see README.md), then:

```sh
git clone https://github.com/lamco-admin/xdg-desktop-portal-generic
cd xdg-desktop-portal-generic
git config core.hooksPath .githooks
cargo build
cargo test
```

The pre-commit hook runs formatting, clippy, and unit tests automatically.

## Code Quality

This project uses strict clippy pedantic linting. The lint configuration lives
in `Cargo.toml` under `[lints.clippy]` -- this is the single source of truth
for lint levels.

### Formatting

```sh
rustup run nightly cargo fmt
```

The `.rustfmt.toml` uses nightly-only import ordering features
(`imports_granularity`, `group_imports`). Stable rustfmt will format everything
else correctly but skip import ordering.

### Linting

```sh
cargo clippy
```

Key rules:

- `clippy::pedantic` is enforced at deny level
- `#[allow(...)]` is forbidden -- use `#[expect(..., reason = "...")]` instead
- Every `#[expect]` must include a reason string
- Every `unsafe` block must have a `// SAFETY: ...` comment
- `unsafe_code` is denied at crate level; modules with legitimate unsafe use
  `#[expect(unsafe_code, reason = "...")]`

### Tests

```sh
cargo test        # all tests
cargo test --lib  # unit tests only (faster)
```

## Testing Portal Integration

### Without Installing

Set `XDG_DESKTOP_PORTAL_DIR` to point to the `data/` directory to test without
a system-wide install:

```sh
XDG_DESKTOP_PORTAL_DIR=./data/ \
    RUST_LOG=xdg_desktop_portal_generic=debug \
    cargo run
```

### With Flatpak Apps

Use the [portal-test](https://github.com/matthiasclasen/portal-test) Flatpak
app to exercise all portal interfaces interactively.

### D-Bus Debugging

Monitor portal requests:

```sh
dbus-monitor --session "interface='org.freedesktop.impl.portal.ScreenCast'"
```

Introspect the service:

```sh
busctl --user introspect org.freedesktop.impl.portal.desktop.generic /
```

### Manual D-Bus Calls

You can trigger portal methods directly with `busctl`:

```sh
# Check if the service is running
busctl --user status org.freedesktop.impl.portal.desktop.generic

# Read a setting
busctl --user call org.freedesktop.impl.portal.desktop.generic \
    /org/freedesktop/portal/desktop \
    org.freedesktop.impl.portal.Settings \
    Read ss "org.freedesktop.appearance" "color-scheme"
```

## Project Structure

```
src/
  dbus/          D-Bus interface implementations (ScreenCast, RemoteDesktop, etc.)
  wayland/       Wayland protocol dispatch and state management
  services/      Backend trait abstractions (capture, clipboard, input)
  pipewire/      PipeWire stream creation and buffer management
  session/       Session lifecycle and state machine
  error.rs       Error types
  types.rs       Shared data types
  lib.rs         Library root and PortalBackend orchestration
  main.rs        Binary entry point
data/
  generic.portal                                    Portal interface declaration
  org.freedesktop.impl.portal.desktop.generic.service  D-Bus activation service
  xdg-desktop-portal-generic.service                Systemd user service unit
```

## Commit Messages

Use [conventional commits](https://www.conventionalcommits.org/):

- `feat:` new functionality
- `fix:` bug fix
- `refactor:` code restructuring without behavior change
- `docs:` documentation only
- `test:` test additions or fixes
- `chore:` build, CI, or tooling changes

## License

By contributing, you agree that your contributions will be licensed under the
same terms as the project (MIT OR Apache-2.0).
