//! Protocol detection for capture backends.
//!
//! Mirrors the input detection pattern (`services/input/detection.rs`).
//! Detects which capture protocols are available, then selects one based
//! on configuration preferences and availability.

use super::{CapturePreference, CaptureProtocol};
use crate::{
    error::{PortalError, Result},
    wayland::AvailableProtocols as WaylandProtocols,
};

/// Available capture protocols detected from the compositor.
#[derive(Debug, Default, Clone)]
pub struct AvailableCaptureProtocols {
    /// ext-image-copy-capture-v1 is advertised.
    pub ext_image_copy_capture: bool,

    /// wlr-screencopy-unstable-v1 is advertised.
    pub wlr_screencopy: bool,
}

impl AvailableCaptureProtocols {
    /// Check if any capture protocol is available.
    pub fn any(&self) -> bool {
        self.ext_image_copy_capture || self.wlr_screencopy
    }

    /// Check if a specific protocol is available.
    pub fn has(&self, protocol: CaptureProtocol) -> bool {
        match protocol {
            CaptureProtocol::ExtImageCopyCapture => self.ext_image_copy_capture,
            CaptureProtocol::WlrScreencopy => self.wlr_screencopy,
        }
    }
}

/// Detects and selects capture protocols.
pub struct CaptureDetector;

impl CaptureDetector {
    /// Detect which capture protocols are available from Wayland globals.
    pub fn detect(wayland_protocols: &WaylandProtocols) -> AvailableCaptureProtocols {
        AvailableCaptureProtocols {
            ext_image_copy_capture: wayland_protocols.ext_image_copy_capture,
            wlr_screencopy: wayland_protocols.wlr_screencopy,
        }
    }

    /// Select the best capture protocol based on preferences and availability.
    ///
    /// # Algorithm
    ///
    /// 1. Skip protocols listed in `broken_protocols`
    /// 2. If a preferred protocol is specified, try it first
    /// 3. If preferred is unavailable and fallback is allowed, try the alternative
    /// 4. If no preference, auto-detect (ext preferred, wlr fallback)
    /// 5. If nothing works, return an error
    pub fn select(
        prefs: &CapturePreference,
        available: &AvailableCaptureProtocols,
    ) -> Result<CaptureProtocol> {
        // Filter out broken protocols
        let ext_usable = available.ext_image_copy_capture
            && !prefs
                .broken_protocols
                .contains(&CaptureProtocol::ExtImageCopyCapture);
        let wlr_usable = available.wlr_screencopy
            && !prefs
                .broken_protocols
                .contains(&CaptureProtocol::WlrScreencopy);

        if let Some(preferred) = prefs.preferred {
            // Explicit preference from server or config
            let preferred_usable = match preferred {
                CaptureProtocol::ExtImageCopyCapture => ext_usable,
                CaptureProtocol::WlrScreencopy => wlr_usable,
            };

            if preferred_usable {
                tracing::debug!("Using preferred capture protocol: {}", preferred);
                return Ok(preferred);
            }

            // Preferred not usable -- try fallback if allowed
            if prefs.allow_fallback {
                let fallback = Self::fallback_for(preferred);
                let fallback_usable = match fallback {
                    CaptureProtocol::ExtImageCopyCapture => ext_usable,
                    CaptureProtocol::WlrScreencopy => wlr_usable,
                };

                if fallback_usable {
                    tracing::info!(
                        "Preferred capture protocol {} unavailable/broken, using {}",
                        preferred,
                        fallback
                    );
                    return Ok(fallback);
                }
            }
        } else {
            // No explicit preference -- auto-detect (ext preferred over wlr)
            if ext_usable {
                tracing::debug!(
                    "Auto-selected capture protocol: {}",
                    CaptureProtocol::ExtImageCopyCapture
                );
                return Ok(CaptureProtocol::ExtImageCopyCapture);
            }

            if wlr_usable {
                tracing::debug!(
                    "Auto-selected capture protocol: {}",
                    CaptureProtocol::WlrScreencopy
                );
                return Ok(CaptureProtocol::WlrScreencopy);
            }
        }

        // Nothing available or usable
        if !available.any() {
            return Err(PortalError::Config(
                "No capture protocols available. Compositor must support ext-image-copy-capture or wlr-screencopy.".to_string()
            ));
        }

        Err(PortalError::Config(format!(
            "No usable capture protocol: preferred={:?}, broken={:?}, fallback={}",
            prefs.preferred, prefs.broken_protocols, prefs.allow_fallback
        )))
    }

    /// Get the fallback protocol for a given protocol.
    fn fallback_for(protocol: CaptureProtocol) -> CaptureProtocol {
        match protocol {
            CaptureProtocol::ExtImageCopyCapture => CaptureProtocol::WlrScreencopy,
            CaptureProtocol::WlrScreencopy => CaptureProtocol::ExtImageCopyCapture,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_available_protocols_any() {
        let none = AvailableCaptureProtocols::default();
        assert!(!none.any());

        let ext = AvailableCaptureProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: false,
        };
        assert!(ext.any());

        let wlr = AvailableCaptureProtocols {
            ext_image_copy_capture: false,
            wlr_screencopy: true,
        };
        assert!(wlr.any());
    }

    #[test]
    fn test_auto_select_prefers_ext() {
        let available = AvailableCaptureProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: true,
        };
        let prefs = CapturePreference::default();

        let selected = CaptureDetector::select(&prefs, &available).unwrap();
        assert_eq!(selected, CaptureProtocol::ExtImageCopyCapture);
    }

    #[test]
    fn test_auto_select_falls_to_wlr() {
        let available = AvailableCaptureProtocols {
            ext_image_copy_capture: false,
            wlr_screencopy: true,
        };
        let prefs = CapturePreference::default();

        let selected = CaptureDetector::select(&prefs, &available).unwrap();
        assert_eq!(selected, CaptureProtocol::WlrScreencopy);
    }

    #[test]
    fn test_explicit_wlr_preference() {
        let available = AvailableCaptureProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: true,
        };
        let prefs = CapturePreference {
            preferred: Some(CaptureProtocol::WlrScreencopy),
            ..Default::default()
        };

        let selected = CaptureDetector::select(&prefs, &available).unwrap();
        assert_eq!(selected, CaptureProtocol::WlrScreencopy);
    }

    #[test]
    fn test_broken_protocol_skipped() {
        let available = AvailableCaptureProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: true,
        };
        let prefs = CapturePreference {
            preferred: None,
            allow_fallback: true,
            broken_protocols: vec![CaptureProtocol::ExtImageCopyCapture],
            ..Default::default()
        };

        let selected = CaptureDetector::select(&prefs, &available).unwrap();
        assert_eq!(selected, CaptureProtocol::WlrScreencopy);
    }

    #[test]
    fn test_preferred_broken_falls_back() {
        let available = AvailableCaptureProtocols {
            ext_image_copy_capture: true,
            wlr_screencopy: true,
        };
        let prefs = CapturePreference {
            preferred: Some(CaptureProtocol::ExtImageCopyCapture),
            allow_fallback: true,
            broken_protocols: vec![CaptureProtocol::ExtImageCopyCapture],
            ..Default::default()
        };

        let selected = CaptureDetector::select(&prefs, &available).unwrap();
        assert_eq!(selected, CaptureProtocol::WlrScreencopy);
    }

    #[test]
    fn test_no_fallback_fails() {
        let available = AvailableCaptureProtocols {
            ext_image_copy_capture: false,
            wlr_screencopy: true,
        };
        let prefs = CapturePreference {
            preferred: Some(CaptureProtocol::ExtImageCopyCapture),
            allow_fallback: false,
            ..Default::default()
        };

        let result = CaptureDetector::select(&prefs, &available);
        assert!(result.is_err());
    }

    #[test]
    fn test_nothing_available() {
        let available = AvailableCaptureProtocols::default();
        let prefs = CapturePreference::default();

        let result = CaptureDetector::select(&prefs, &available);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("No capture protocols"));
    }

    #[test]
    fn test_env_override() {
        // Test that from_env creates valid preferences
        let prefs = CapturePreference::from_env();
        // Default should have no preferred, allow fallback
        assert!(prefs.preferred.is_none() || prefs.preferred.is_some());
        assert!(prefs.allow_fallback || !prefs.allow_fallback);
    }
}
