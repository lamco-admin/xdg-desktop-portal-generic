//! InputCapture barrier surface lifecycle (Phase 2a).
//!
//! Creates invisible `wlr-layer-shell-v1` surfaces positioned at each
//! accepted [`PointerBarrier`], drives the `configure -> ack_configure ->
//! attach buffer -> commit` mapping lifecycle so the surfaces genuinely map
//! on a live compositor, and tears them down on `Disable`/`Release`/session
//! close.
//!
//! **Phase 2a scope:** surfaces map, nothing more. The pointer is never
//! locked (`zwp_pointer_constraints_v1`) and relative motion is never read
//! (`zwp_relative_pointer_v1`) -- both managers are bound here (see
//! [`InputCaptureBarrierState`]) but unused until Phase 2b, which also
//! wires `wl_pointer` enter/leave detection on these surfaces to actually
//! trigger a lock. See the Phase 2b design section of the implementation
//! plan for the full activation path.

use std::collections::HashMap;

use wayland_client::{
    QueueHandle,
    protocol::{
        wl_compositor::WlCompositor, wl_output::WlOutput, wl_shm::WlShm, wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::{
    pointer_constraints::zv1::client::zwp_pointer_constraints_v1::ZwpPointerConstraintsV1,
    relative_pointer::zv1::client::zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{self, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use super::{
    WaylandState,
    screencopy::{BufferFormatInfo, ShmFrameBuffer},
};
use crate::types::{InputCaptureZone, PointerBarrier};

/// Thickness, in pixels, of a barrier's invisible hot-strip along the edge
/// it's anchored to. The barrier itself is a zero-width line; the strip
/// needs *some* thickness for the compositor to deliver pointer-enter
/// events at all.
const BARRIER_STRIP_THICKNESS_PX: u32 = 2;

/// A barrier's computed layer-shell surface placement.
///
/// Output of the pure [`barrier_to_layer_geometry`] function, kept separate
/// from the actual Wayland object creation so the geometry logic is
/// testable without a live `wayland_client::Connection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayerSurfaceGeometry {
    /// `wl_output` global name the barrier lies on.
    pub output_global_name: u32,
    /// Anchor bitmask (`zwlr_layer_surface_v1::Anchor`).
    pub anchor: zwlr_layer_surface_v1::Anchor,
    /// Margin (top, right, bottom, left), matching `set_margin`'s argument order.
    pub margin: (i32, i32, i32, i32),
    /// Surface width in pixels.
    pub width: u32,
    /// Surface height in pixels.
    pub height: u32,
}

/// Translate a compositor-global barrier line into a layer-shell surface
/// placement on the output it lies on.
///
/// Returns `None` (caller logs and skips that barrier rather than failing
/// the whole `Enable()` call) if the barrier doesn't lie on any known
/// zone's boundary, or ambiguously spans more than one zone.
///
/// A horizontal barrier (`y1 == y2`) anchors to the top or bottom edge of
/// whichever zone it touches; a vertical barrier (`x1 == x2`) anchors to
/// the left or right edge. [`PointerBarrier::is_valid`] already guarantees
/// the barrier is one or the other, never both/neither.
pub fn barrier_to_layer_geometry(
    barrier: &PointerBarrier,
    zones: &[(u32, InputCaptureZone)],
) -> Option<LayerSurfaceGeometry> {
    let horizontal = barrier.y1 == barrier.y2;

    let matching: Vec<&(u32, InputCaptureZone)> = zones
        .iter()
        .filter(|(_, z)| {
            let zone_right = z.x + i32::try_from(z.width).unwrap_or(i32::MAX);
            let zone_bottom = z.y + i32::try_from(z.height).unwrap_or(i32::MAX);
            if horizontal {
                // Barrier is horizontal: it must lie on the zone's top or
                // bottom edge and fit entirely within its width. A barrier
                // whose span exceeds a single zone's width doesn't belong to
                // any one output (see test_barrier_spanning_two_zones_returns_none).
                (barrier.y1 == z.y || barrier.y1 == zone_bottom)
                    && barrier.x1.min(barrier.x2) >= z.x
                    && barrier.x1.max(barrier.x2) <= zone_right
            } else {
                // Vertical: lies on the zone's left or right edge, fits
                // entirely within its height.
                (barrier.x1 == z.x || barrier.x1 == zone_right)
                    && barrier.y1.min(barrier.y2) >= z.y
                    && barrier.y1.max(barrier.y2) <= zone_bottom
            }
        })
        .collect();

    // Unmapped: the barrier doesn't lie on any known zone's edge, or its
    // span exceeds a single zone's extent -- bail out. Multiple matches
    // happen when the barrier sits exactly on the shared seam between two
    // equal-extent adjacent zones (the common side-by-side monitor case);
    // either output works, since the strip lands at the same global
    // coordinate either way, so just take the first.
    let (output_global_name, zone) = match matching.as_slice() {
        [] => return None,
        [first, ..] => *first,
    };

    let zone_right = zone.x + i32::try_from(zone.width).unwrap_or(i32::MAX);
    let zone_bottom = zone.y + i32::try_from(zone.height).unwrap_or(i32::MAX);

    let (anchor, margin, width, height) = if horizontal {
        let on_top = barrier.y1 == zone.y;
        let anchor = if on_top {
            zwlr_layer_surface_v1::Anchor::Top | zwlr_layer_surface_v1::Anchor::Left
        } else {
            zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Left
        };
        let span_width = barrier.x2.min(zone_right) - barrier.x1.max(zone.x);
        (
            anchor,
            (0, 0, 0, 0),
            span_width.max(0).unsigned_abs(),
            BARRIER_STRIP_THICKNESS_PX,
        )
    } else {
        let on_left = barrier.x1 == zone.x;
        let anchor = if on_left {
            zwlr_layer_surface_v1::Anchor::Left | zwlr_layer_surface_v1::Anchor::Top
        } else {
            zwlr_layer_surface_v1::Anchor::Right | zwlr_layer_surface_v1::Anchor::Top
        };
        let span_height = barrier.y2.min(zone_bottom) - barrier.y1.max(zone.y);
        (
            anchor,
            (0, 0, 0, 0),
            BARRIER_STRIP_THICKNESS_PX,
            span_height.max(0).unsigned_abs(),
        )
    };

    Some(LayerSurfaceGeometry {
        output_global_name: *output_global_name,
        anchor,
        margin,
        width,
        height,
    })
}

/// A single barrier's layer-shell surface and lifecycle state.
pub struct BarrierSurface {
    /// Owning session handle.
    pub session_id: String,
    /// Barrier this surface enforces.
    pub barrier_id: u32,
    /// The underlying Wayland surface.
    pub wl_surface: WlSurface,
    /// The layer-shell surface role object.
    pub layer_surface: ZwlrLayerSurfaceV1,
    /// Whether the surface has received its first `Configure`, been
    /// `ack_configure`'d, and committed with a buffer attached (i.e. is
    /// actually mapped, not just requested).
    pub configured: bool,
    /// Transparent SHM buffer attached once configured. `None` until the
    /// first `Configure` arrives (dimensions aren't known before then).
    pub shm_buffer: Option<ShmFrameBuffer>,
}

impl std::fmt::Debug for BarrierSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BarrierSurface")
            .field("session_id", &self.session_id)
            .field("barrier_id", &self.barrier_id)
            .field("configured", &self.configured)
            .finish_non_exhaustive()
    }
}

/// State for InputCapture barrier surfaces: bound protocol managers plus
/// every barrier surface currently alive, keyed by `(session_id,
/// barrier_id)`.
#[derive(Default)]
pub struct InputCaptureBarrierState {
    /// `wlr-layer-shell-v1` manager, used to create barrier surfaces.
    pub layer_shell: Option<ZwlrLayerShellV1>,
    /// `zwp-pointer-constraints-v1` manager. Bound now, unused until Phase 2b.
    pub pointer_constraints: Option<ZwpPointerConstraintsV1>,
    /// `zwp-relative-pointer-v1` manager. Bound now, unused until Phase 2b.
    pub relative_pointer_manager: Option<ZwpRelativePointerManagerV1>,
    /// `wl_compositor`, used to create the `wl_surface` for each barrier.
    pub compositor: Option<WlCompositor>,
    /// `wl_shm`, used to allocate each barrier surface's (transparent) buffer.
    pub shm: Option<WlShm>,
    /// Live barrier surfaces, keyed by `(session_id, barrier_id)`.
    pub surfaces: HashMap<(String, u32), BarrierSurface>,
}

impl InputCaptureBarrierState {
    /// Create layer-shell surfaces for a session's accepted barriers.
    ///
    /// Barriers that can't be geometrically mapped to a known output (see
    /// [`barrier_to_layer_geometry`]) are logged and skipped, not treated
    /// as a hard failure for the whole call -- a session with some valid
    /// and some unmappable barriers still gets surfaces for the valid ones.
    pub fn create_barrier_surfaces(
        &mut self,
        qh: &QueueHandle<WaylandState>,
        session_id: &str,
        barriers: &[PointerBarrier],
        zones: &[(u32, InputCaptureZone)],
        find_output: impl Fn(u32) -> Option<WlOutput>,
    ) {
        let (Some(layer_shell), Some(compositor)) = (&self.layer_shell, &self.compositor) else {
            tracing::warn!(
                session_id = %session_id,
                "Cannot create InputCapture barrier surfaces: layer-shell or wl_compositor not bound"
            );
            return;
        };

        for barrier in barriers {
            let Some(geometry) = barrier_to_layer_geometry(barrier, zones) else {
                tracing::warn!(
                    session_id = %session_id,
                    barrier_id = barrier.barrier_id,
                    "Barrier does not lie on any known zone boundary (or spans more than one) -- skipping"
                );
                continue;
            };
            let Some(output) = find_output(geometry.output_global_name) else {
                tracing::warn!(
                    session_id = %session_id,
                    barrier_id = barrier.barrier_id,
                    output_global_name = geometry.output_global_name,
                    "Barrier's mapped output is no longer known -- skipping"
                );
                continue;
            };

            let wl_surface = compositor.create_surface(qh, ());
            let layer_surface = layer_shell.get_layer_surface(
                &wl_surface,
                Some(&output),
                zwlr_layer_shell_v1::Layer::Overlay,
                "xdp-generic-input-capture".to_string(),
                qh,
                (session_id.to_string(), barrier.barrier_id),
            );
            layer_surface.set_size(geometry.width, geometry.height);
            layer_surface.set_anchor(geometry.anchor);
            let (top, right, bottom, left) = geometry.margin;
            layer_surface.set_margin(top, right, bottom, left);
            layer_surface.set_exclusive_zone(-1);
            layer_surface
                .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
            // Layer-shell requires an initial commit with no buffer before
            // the compositor sends the first Configure.
            wl_surface.commit();

            tracing::info!(
                session_id = %session_id,
                barrier_id = barrier.barrier_id,
                width = geometry.width,
                height = geometry.height,
                "InputCapture barrier surface requested"
            );

            self.surfaces.insert(
                (session_id.to_string(), barrier.barrier_id),
                BarrierSurface {
                    session_id: session_id.to_string(),
                    barrier_id: barrier.barrier_id,
                    wl_surface,
                    layer_surface,
                    configured: false,
                    shm_buffer: None,
                },
            );
        }
    }

    /// Handle a `Configure` event: ack it, attach a (transparent) buffer,
    /// and commit -- completing the surface's mapping.
    pub fn on_configure(
        &mut self,
        qh: &QueueHandle<WaylandState>,
        session_id: &str,
        barrier_id: u32,
        serial: u32,
        width: u32,
        height: u32,
    ) {
        let Some(surface) = self.surfaces.get_mut(&(session_id.to_string(), barrier_id)) else {
            tracing::warn!(
                session_id = %session_id,
                barrier_id,
                "Configure for unknown barrier surface"
            );
            return;
        };

        surface.layer_surface.ack_configure(serial);

        let Some(shm) = &self.shm else {
            tracing::warn!("Cannot map InputCapture barrier surface: wl_shm not bound");
            return;
        };

        // A freshly memfd_create'd region is zero-initialized by the
        // kernel, and an all-zero ARGB8888 buffer is already fully
        // transparent (alpha=0) -- no pixel-writing code needed.
        let format = BufferFormatInfo {
            format: wayland_client::protocol::wl_shm::Format::Argb8888,
            format_raw: wayland_client::protocol::wl_shm::Format::Argb8888 as u32,
            width,
            height,
            stride: width * 4,
        };
        match ShmFrameBuffer::new(shm, qh, &format) {
            Ok(buffer) => {
                surface.wl_surface.attach(Some(&buffer.buffer), 0, 0);
                surface.wl_surface.commit();
                surface.shm_buffer = Some(buffer);
                surface.configured = true;
                tracing::info!(
                    session_id = %session_id,
                    barrier_id,
                    width,
                    height,
                    "InputCapture barrier surface mapped"
                );
            }
            Err(e) => {
                tracing::warn!(
                    session_id = %session_id,
                    barrier_id,
                    error = %e,
                    "Failed to allocate InputCapture barrier surface buffer"
                );
            }
        }
    }

    /// Handle a `Closed` event: the compositor tore the surface down
    /// unexpectedly (e.g. its output was removed).
    pub fn on_closed(&mut self, session_id: &str, barrier_id: u32) {
        if self
            .surfaces
            .remove(&(session_id.to_string(), barrier_id))
            .is_some()
        {
            tracing::warn!(
                session_id = %session_id,
                barrier_id,
                "InputCapture barrier surface closed by compositor"
            );
        }
    }

    /// Destroy every barrier surface for a session (`Disable`/`Release`/close).
    pub fn destroy_session(&mut self, session_id: &str) {
        let keys: Vec<(String, u32)> = self
            .surfaces
            .keys()
            .filter(|(sid, _)| sid == session_id)
            .cloned()
            .collect();

        for key in keys {
            if let Some(surface) = self.surfaces.remove(&key) {
                surface.layer_surface.destroy();
                surface.wl_surface.destroy();
            }
        }

        tracing::debug!(session_id = %session_id, "InputCapture barrier surfaces destroyed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(x: i32, y: i32, width: u32, height: u32) -> InputCaptureZone {
        InputCaptureZone {
            width,
            height,
            x,
            y,
        }
    }

    #[test]
    fn test_horizontal_barrier_top_edge() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 0,
            x2: 1920,
            y2: 0,
        };
        let zones = vec![(1u32, zone(0, 0, 1920, 1080))];
        let geometry = barrier_to_layer_geometry(&barrier, &zones).expect("should map");
        assert_eq!(geometry.output_global_name, 1);
        assert_eq!(geometry.width, 1920);
        assert_eq!(geometry.height, BARRIER_STRIP_THICKNESS_PX);
        assert!(geometry.anchor.contains(zwlr_layer_surface_v1::Anchor::Top));
    }

    #[test]
    fn test_horizontal_barrier_bottom_edge() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 1080,
            x2: 1920,
            y2: 1080,
        };
        let zones = vec![(1u32, zone(0, 0, 1920, 1080))];
        let geometry = barrier_to_layer_geometry(&barrier, &zones).expect("should map");
        assert!(
            geometry
                .anchor
                .contains(zwlr_layer_surface_v1::Anchor::Bottom)
        );
    }

    #[test]
    fn test_vertical_barrier_right_edge() {
        // Two side-by-side monitors; barrier on the boundary between them,
        // touching the left zone's right edge.
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 1920,
            y1: 0,
            x2: 1920,
            y2: 1080,
        };
        let zones = vec![
            (1u32, zone(0, 0, 1920, 1080)),
            (2u32, zone(1920, 0, 1920, 1080)),
        ];
        let geometry = barrier_to_layer_geometry(&barrier, &zones).expect("should map");
        assert_eq!(geometry.width, BARRIER_STRIP_THICKNESS_PX);
        assert_eq!(geometry.height, 1080);
    }

    #[test]
    fn test_barrier_off_any_zone_returns_none() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 5000,
            y1: 0,
            x2: 5000,
            y2: 1080,
        };
        let zones = vec![(1u32, zone(0, 0, 1920, 1080))];
        assert!(barrier_to_layer_geometry(&barrier, &zones).is_none());
    }

    #[test]
    fn test_barrier_spanning_two_zones_returns_none() {
        // Horizontal barrier at y=0 spanning both zones' top edges --
        // ambiguous which output it belongs to.
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 0,
            x2: 3840,
            y2: 0,
        };
        let zones = vec![
            (1u32, zone(0, 0, 1920, 1080)),
            (2u32, zone(1920, 0, 1920, 1080)),
        ];
        assert!(barrier_to_layer_geometry(&barrier, &zones).is_none());
    }

    #[test]
    fn test_empty_zones_returns_none() {
        let barrier = PointerBarrier {
            barrier_id: 1,
            x1: 0,
            y1: 0,
            x2: 1920,
            y2: 0,
        };
        assert!(barrier_to_layer_geometry(&barrier, &[]).is_none());
    }
}
