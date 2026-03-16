//! Protocol detection for input backends.
//!
//! This module handles detecting which input protocols are available
//! based on Wayland globals advertised by the compositor.

use super::{InputBackendConfig, InputProtocol};
use crate::{
    error::{PortalError, Result},
    wayland::AvailableProtocols as WaylandProtocols,
};

/// Available input protocols detected from the compositor.
#[derive(Debug, Default, Clone)]
pub struct AvailableProtocols {
    /// EIS (Emulated Input Server) is available.
    pub eis: bool,

    /// wlr virtual input protocols are available.
    pub wlr_virtual_input: bool,
}

impl AvailableProtocols {
    /// Check if any protocol is available.
    pub fn any(&self) -> bool {
        self.eis || self.wlr_virtual_input
    }

    /// Check if a specific protocol is available.
    pub fn has(&self, protocol: InputProtocol) -> bool {
        match protocol {
            InputProtocol::Eis => self.eis,
            InputProtocol::WlrVirtualInput => self.wlr_virtual_input,
        }
    }
}

/// Detects and selects input protocols.
pub struct ProtocolDetector;

impl ProtocolDetector {
    /// Detect which input protocols are available from Wayland globals.
    pub fn detect(wayland_protocols: &WaylandProtocols) -> AvailableProtocols {
        let wlr_available =
            wayland_protocols.wlr_virtual_pointer || wayland_protocols.zwp_virtual_keyboard;

        AvailableProtocols {
            // EIS runs in bridge mode: portal acts as EIS server, translating
            // client events into wlr virtual input protocol calls. EIS is
            // therefore available whenever the wlr virtual input backend is.
            eis: wlr_available,
            wlr_virtual_input: wlr_available,
        }
    }

    /// Select the best protocol based on configuration and availability.
    ///
    /// # Algorithm
    ///
    /// 1. Try the preferred protocol from config
    /// 2. If unavailable and fallback is allowed, try the alternative
    /// 3. If nothing works, return an error
    pub fn select(
        config: &InputBackendConfig,
        available: &AvailableProtocols,
    ) -> Result<InputProtocol> {
        // Try preferred protocol first
        if available.has(config.preferred) {
            tracing::debug!("Using preferred protocol: {:?}", config.preferred);
            return Ok(config.preferred);
        }

        // Try fallback if allowed
        if config.allow_fallback {
            let fallback = Self::fallback_for(config.preferred);

            if available.has(fallback) {
                tracing::info!(
                    "Preferred protocol {:?} unavailable, falling back to {:?}",
                    config.preferred,
                    fallback
                );
                return Ok(fallback);
            }
        }

        // Nothing available
        if !available.any() {
            return Err(PortalError::Config(
                "No input protocols available. Compositor must support either EIS or wlr virtual input.".to_string()
            ));
        }

        // Preferred unavailable and fallback disabled
        Err(PortalError::Config(format!(
            "Preferred protocol {:?} unavailable and fallback disabled",
            config.preferred
        )))
    }

    /// Get the fallback protocol for a given protocol.
    fn fallback_for(protocol: InputProtocol) -> InputProtocol {
        match protocol {
            InputProtocol::Eis => InputProtocol::WlrVirtualInput,
            InputProtocol::WlrVirtualInput => InputProtocol::Eis,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_available_protocols_any() {
        let none = AvailableProtocols::default();
        assert!(!none.any());

        let eis = AvailableProtocols {
            eis: true,
            wlr_virtual_input: false,
        };
        assert!(eis.any());

        let wlr = AvailableProtocols {
            eis: false,
            wlr_virtual_input: true,
        };
        assert!(wlr.any());

        let both = AvailableProtocols {
            eis: true,
            wlr_virtual_input: true,
        };
        assert!(both.any());
    }

    #[test]
    fn test_select_preferred_eis() {
        let available = AvailableProtocols {
            eis: true,
            wlr_virtual_input: true,
        };

        let config = InputBackendConfig {
            preferred: InputProtocol::Eis,
            allow_fallback: true,
            ..Default::default()
        };

        let selected = ProtocolDetector::select(&config, &available).unwrap();
        assert_eq!(selected, InputProtocol::Eis);
    }

    #[test]
    fn test_select_preferred_wlr() {
        let available = AvailableProtocols {
            eis: true,
            wlr_virtual_input: true,
        };

        let config = InputBackendConfig {
            preferred: InputProtocol::WlrVirtualInput,
            allow_fallback: true,
            ..Default::default()
        };

        let selected = ProtocolDetector::select(&config, &available).unwrap();
        assert_eq!(selected, InputProtocol::WlrVirtualInput);
    }

    #[test]
    fn test_select_fallback() {
        let available = AvailableProtocols {
            eis: false,
            wlr_virtual_input: true,
        };

        let config = InputBackendConfig {
            preferred: InputProtocol::Eis,
            allow_fallback: true,
            ..Default::default()
        };

        let selected = ProtocolDetector::select(&config, &available).unwrap();
        assert_eq!(selected, InputProtocol::WlrVirtualInput);
    }

    #[test]
    fn test_select_no_fallback_fails() {
        let available = AvailableProtocols {
            eis: false,
            wlr_virtual_input: true,
        };

        let config = InputBackendConfig {
            preferred: InputProtocol::Eis,
            allow_fallback: false,
            ..Default::default()
        };

        let result = ProtocolDetector::select(&config, &available);
        assert!(result.is_err());
    }

    #[test]
    fn test_select_none_available() {
        let available = AvailableProtocols::default();

        let config = InputBackendConfig::default();

        let result = ProtocolDetector::select(&config, &available);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("No input protocols")
        );
    }

    #[test]
    fn test_detect_eis_available_when_wlr_available() {
        let wayland = WaylandProtocols {
            wlr_virtual_pointer: true,
            ..Default::default()
        };
        let detected = ProtocolDetector::detect(&wayland);
        assert!(
            detected.eis,
            "EIS should be available in bridge mode when wlr virtual input is available"
        );
        assert!(detected.wlr_virtual_input);
    }

    #[test]
    fn test_detect_eis_unavailable_without_wlr() {
        let wayland = WaylandProtocols::default();
        let detected = ProtocolDetector::detect(&wayland);
        assert!(!detected.eis);
        assert!(!detected.wlr_virtual_input);
    }
}
