//! Kernel-level pointer injection via `/dev/uinput`.
//!
//! This backend creates a virtual absolute pointing device at the kernel level,
//! bypassing Wayland protocol requirements entirely. The compositor sees it as
//! real hardware (like a graphics tablet).
//!
//! # When Is This Used?
//!
//! On compositors that have `zwp_virtual_keyboard_v1` but NOT `wlr_virtual_pointer_v1`
//! (e.g., COSMIC). The wlr keyboard backend handles keyboard input; this backend
//! handles pointer input via `/dev/uinput`.
//!
//! # Permissions
//!
//! Requires write access to `/dev/uinput`. Typically: `input` group membership.

use std::path::Path;

use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, InputEvent, InputId, RelativeAxisCode,
    UinputAbsSetup, uinput::VirtualDevice,
};

use super::InputBackend;
use crate::{
    error::{PortalError, Result},
    types::{ButtonState, DeviceTypes, InputEvent as PortalInputEvent, PointerEvent},
};

/// Maximum absolute axis value (standard tablet range).
const ABS_MAX: i32 = 32767;

// Linux input event type constants
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_ABS: u16 = 0x03;

// Button codes defined via evdev::KeyCode, raw constants kept for reference
// BTN_LEFT = 0x110, BTN_RIGHT = 0x111, BTN_MIDDLE = 0x112

/// uinput-based pointer backend.
pub struct UinputPointerBackend {
    device: VirtualDevice,
    health_tx: Option<crate::health::HealthSender>,
    events_forwarded: u64,
    events_failed: u64,
}

impl UinputPointerBackend {
    /// Create a new uinput pointer backend.
    ///
    /// # Errors
    ///
    /// Returns an error if `/dev/uinput` is not accessible or device creation fails.
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing uinput pointer backend (/dev/uinput)");

        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, ABS_MAX, 0, 0, 1),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, ABS_MAX, 0, 0, 1),
        );

        // Button keys
        let mut keys = AttributeSet::new();
        keys.insert(evdev::KeyCode::BTN_LEFT);
        keys.insert(evdev::KeyCode::BTN_RIGHT);
        keys.insert(evdev::KeyCode::BTN_MIDDLE);

        // Scroll axes
        let mut rel_axes = AttributeSet::new();
        rel_axes.insert(RelativeAxisCode::REL_WHEEL);
        rel_axes.insert(RelativeAxisCode::REL_HWHEEL);

        let device = VirtualDevice::builder()
            .map_err(|e| PortalError::Config(format!("Failed to open /dev/uinput: {e}")))?
            .name("lamco-rdp-pointer")
            .input_id(InputId::new(BusType::BUS_USB, 0x4C41, 0x4D43, 1))
            .with_absolute_axis(&abs_x)
            .map_err(|e| PortalError::Config(format!("Failed to set ABS_X: {e}")))?
            .with_absolute_axis(&abs_y)
            .map_err(|e| PortalError::Config(format!("Failed to set ABS_Y: {e}")))?
            .with_keys(&keys)
            .map_err(|e| PortalError::Config(format!("Failed to set keys: {e}")))?
            .with_relative_axes(&rel_axes)
            .map_err(|e| PortalError::Config(format!("Failed to set relative axes: {e}")))?
            .build()
            .map_err(|e| PortalError::Config(format!("Failed to create uinput device: {e}")))?;

        tracing::info!("uinput pointer device created (ABS 0-{ABS_MAX}, buttons, scroll)");

        Ok(Self {
            device,
            health_tx: None,
            events_forwarded: 0,
            events_failed: 0,
        })
    }

    fn inject_motion(&mut self, x: f64, y: f64) -> Result<()> {
        let abs_x = (x.clamp(0.0, 1.0) * ABS_MAX as f64) as i32;
        let abs_y = (y.clamp(0.0, 1.0) * ABS_MAX as f64) as i32;

        self.device
            .emit(&[
                InputEvent::new_now(EV_ABS, AbsoluteAxisCode::ABS_X.0, abs_x),
                InputEvent::new_now(EV_ABS, AbsoluteAxisCode::ABS_Y.0, abs_y),
                InputEvent::new_now(EV_SYN, 0, 0),
            ])
            .map_err(|e| PortalError::Wayland(format!("uinput motion failed: {e}")))?;

        self.events_forwarded += 1;
        Ok(())
    }

    fn inject_button(&mut self, button: u32, pressed: bool) -> Result<()> {
        let code = button as u16; // RDP uses Linux BTN_* codes directly
        let value = i32::from(pressed);

        self.device
            .emit(&[
                InputEvent::new_now(EV_KEY, code, value),
                InputEvent::new_now(EV_SYN, 0, 0),
            ])
            .map_err(|e| PortalError::Wayland(format!("uinput button failed: {e}")))?;

        self.events_forwarded += 1;
        Ok(())
    }

    fn inject_scroll(&mut self, dx: f64, dy: f64) -> Result<()> {
        let mut events = Vec::new();

        if dy.abs() > f64::EPSILON {
            let value = if dy > 0.0 { 1 } else { -1 };
            events.push(InputEvent::new_now(
                EV_REL,
                RelativeAxisCode::REL_WHEEL.0,
                value,
            ));
        }
        if dx.abs() > f64::EPSILON {
            let value = if dx > 0.0 { 1 } else { -1 };
            events.push(InputEvent::new_now(
                EV_REL,
                RelativeAxisCode::REL_HWHEEL.0,
                value,
            ));
        }

        if !events.is_empty() {
            events.push(InputEvent::new_now(EV_SYN, 0, 0));
            self.device
                .emit(&events)
                .map_err(|e| PortalError::Wayland(format!("uinput scroll failed: {e}")))?;
            self.events_forwarded += 1;
        }

        Ok(())
    }

    fn maybe_emit_health(&self) {
        if self.events_forwarded % 100 == 0 {
            if let Some(ref health_tx) = self.health_tx {
                let _ = health_tx.try_send(crate::health::PortalHealthEvent::InputBatch {
                    events_forwarded: self.events_forwarded,
                    events_failed: self.events_failed,
                    protocol: crate::health::InputProtocolType::Uinput,
                });
            }
        }
    }
}

/// Check if `/dev/uinput` is accessible for writing.
pub fn uinput_available() -> bool {
    Path::new("/dev/uinput").exists()
        && std::fs::OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .is_ok()
}

impl InputBackend for UinputPointerBackend {
    fn protocol_type(&self) -> super::InputProtocol {
        super::InputProtocol::WlrVirtualInput
    }

    fn create_context(
        &mut self,
        session_id: &str,
        _devices: DeviceTypes,
    ) -> Result<Option<std::os::unix::io::OwnedFd>> {
        tracing::info!(session_id = %session_id, "uinput pointer context ready");
        Ok(None)
    }

    fn destroy_context(&mut self, session_id: &str) -> Result<()> {
        tracing::info!(session_id = %session_id, "uinput pointer context released");
        Ok(())
    }

    #[expect(
        clippy::match_same_arms,
        reason = "each arm has distinct semantic meaning"
    )]
    fn inject_event(&mut self, _session_id: &str, event: PortalInputEvent) -> Result<()> {
        match event {
            PortalInputEvent::Pointer(PointerEvent::MotionAbsolute { x, y, .. }) => {
                self.inject_motion(x, y)?;
            }
            PortalInputEvent::Pointer(PointerEvent::Motion { .. }) => {
                // Relative motion not supported on absolute device
            }
            PortalInputEvent::Pointer(PointerEvent::Button { button, state, .. }) => {
                self.inject_button(button, matches!(state, ButtonState::Pressed))?;
            }
            PortalInputEvent::Pointer(PointerEvent::Scroll { dx, dy, .. }) => {
                self.inject_scroll(dx, dy)?;
            }
            PortalInputEvent::Pointer(PointerEvent::ScrollDiscrete { axis, steps, .. }) => {
                use crate::types::ScrollAxis;
                match axis {
                    ScrollAxis::Vertical => self.inject_scroll(0.0, steps as f64)?,
                    ScrollAxis::Horizontal => self.inject_scroll(steps as f64, 0.0)?,
                }
            }
            PortalInputEvent::Pointer(PointerEvent::ScrollStop { .. }) => {}
            // Keyboard and touch handled by other backends
            PortalInputEvent::Keyboard(_) | PortalInputEvent::Touch(_) => {}
        }

        self.maybe_emit_health();
        Ok(())
    }

    fn process_events(&mut self) -> Result<Vec<(String, PortalInputEvent)>> {
        Ok(vec![])
    }

    fn has_context(&self, _session_id: &str) -> bool {
        true
    }

    fn context_count(&self) -> usize {
        1
    }

    fn keysym_to_keycode(&self, _keysym: u32) -> Option<u32> {
        None
    }

    fn set_health_sender(&mut self, tx: crate::health::HealthSender) {
        self.health_tx = Some(tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_uinput_availability_check() {
        let _ = uinput_available();
    }
}
