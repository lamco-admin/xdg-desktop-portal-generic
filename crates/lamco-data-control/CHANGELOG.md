# Changelog

## [1.0.2] - 2026-11-25

### Changed
- `nix` 0.32 is accepted as well as 0.31 (`>=0.31, <0.33`), so a build that already carries
  either doesn't get a second copy. No code change.

## [1.0.1] - 2026-09-30

### Changed
- `homepage` points at the lamco.ai page for the project this crate belongs to
  (xdg-desktop-portal-generic) instead of the bare organisation homepage, and the
  README carries the website, documentation and source links. No code change.

## [1.0.0] - 2026-09-30

First release. Extracted from the clipboard client in
`xdg-desktop-portal-generic` so it can be used without that project.

- `DataControl` handle over a dedicated Wayland thread.
- `ext-data-control-v1` and `wlr-data-control-unstable-v1`, chosen by
  `Options`.
- Delayed rendering through `on_transfer` and `TransferRequest`.
- `find_mime_match` for charset-tolerant type matching.
- Reads are size-limited and time out on a stalled source.
- The change callback runs outside the shared-state lock, so it may call back
  into the handle.
