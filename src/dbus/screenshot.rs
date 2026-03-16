//! Screenshot D-Bus interface implementation.
//!
//! Implements `org.freedesktop.impl.portal.Screenshot` version 2.
//!
//! Provides two methods:
//! - `Screenshot` — captures a screen frame, encodes as PNG, returns file URI
//! - `PickColor` — captures a frame and returns the color at a point (stub)
//!
//! Uses the existing screencopy infrastructure for one-shot frame capture.

use std::{
    collections::HashMap,
    sync::{Arc, mpsc},
};

use tokio::sync::Mutex;
use zbus::{
    interface,
    zvariant::{ObjectPath, OwnedValue, Value},
};

use super::{Response, empty_results, get_option_bool};
use crate::{
    services::capture::CaptureBackend,
    types::SourceType,
    wayland::{CaptureCommand, ScreenshotData},
};

/// Screenshot portal interface implementation.
pub struct ScreenshotInterface {
    /// Capture backend for getting available sources.
    capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
    /// Capture command sender for one-shot screenshots via the Wayland event loop.
    capture_tx: mpsc::Sender<CaptureCommand>,
}

impl ScreenshotInterface {
    /// Create a new Screenshot interface.
    pub fn new(
        capture_backend: Arc<Mutex<Box<dyn CaptureBackend>>>,
        capture_tx: mpsc::Sender<CaptureCommand>,
    ) -> Self {
        Self {
            capture_backend,
            capture_tx,
        }
    }
}

#[interface(name = "org.freedesktop.impl.portal.Screenshot")]
impl ScreenshotInterface {
    /// Capture a screenshot of the screen.
    ///
    /// Captures a single frame via screencopy, encodes it as PNG, saves to
    /// a temporary file, and returns the file URI.
    #[zbus(name = "Screenshot")]
    async fn screenshot(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = parent_window;
        let _interactive = get_option_bool(&options, "interactive").unwrap_or(false);

        tracing::debug!(app_id = app_id, "Screenshot.Screenshot called");

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::standalone();
        let _ = server.at(&handle, request_iface).await;

        // Get the first available source (primary monitor)
        let output_id = {
            let backend = self.capture_backend.lock().await;
            let sources = backend
                .get_sources(&[SourceType::Monitor])
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

            if let Some(source) = sources.first() {
                source.id
            } else {
                tracing::error!("No outputs available for screenshot");
                return Ok((Response::Other.to_u32(), empty_results()));
            }
        };

        // Request a one-shot frame capture via the Wayland event loop
        let (reply_tx, reply_rx) =
            tokio::sync::oneshot::channel::<std::result::Result<ScreenshotData, String>>();

        self.capture_tx
            .send(CaptureCommand::CaptureScreenshot {
                output_global_name: output_id,
                reply: reply_tx,
            })
            .map_err(|e| {
                zbus::fdo::Error::Failed(format!("Failed to send screenshot command: {e}"))
            })?;

        // Wait for the frame data from the event loop
        let screenshot_data = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("Screenshot capture channel closed".to_string()))?
            .map_err(|e| zbus::fdo::Error::Failed(format!("Screenshot capture failed: {e}")))?;

        // Encode as PNG and save to temp file
        let uri = encode_and_save_png(&screenshot_data)
            .map_err(|e| zbus::fdo::Error::Failed(format!("PNG encoding failed: {e}")))?;

        tracing::info!(
            uri = %uri,
            width = screenshot_data.width,
            height = screenshot_data.height,
            "Screenshot captured"
        );

        let mut results = HashMap::new();
        if let Ok(val) = OwnedValue::try_from(Value::from(uri.as_str())) {
            results.insert("uri".to_string(), val);
        }

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;

        Ok((Response::Success.to_u32(), results))
    }

    /// Pick a color from the screen.
    ///
    /// Captures a frame and returns the color at the center point. A full
    /// implementation would present a crosshair UI for the user to click.
    #[zbus(name = "PickColor")]
    async fn pick_color(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> zbus::fdo::Result<(u32, HashMap<String, OwnedValue>)> {
        let _ = (parent_window, &options);
        tracing::debug!(app_id = app_id, "Screenshot.PickColor called");

        // Register Request object at handle path for cancellation support
        let request_iface = super::RequestInterface::standalone();
        let _ = server.at(&handle, request_iface).await;

        // Get the first available source
        let output_id = {
            let backend = self.capture_backend.lock().await;
            let sources = backend
                .get_sources(&[SourceType::Monitor])
                .map_err(|e| zbus::fdo::Error::Failed(e.to_string()))?;

            if let Some(source) = sources.first() {
                source.id
            } else {
                return Ok((Response::Other.to_u32(), empty_results()));
            }
        };

        // Capture a single frame
        let (reply_tx, reply_rx) =
            tokio::sync::oneshot::channel::<std::result::Result<ScreenshotData, String>>();

        self.capture_tx
            .send(CaptureCommand::CaptureScreenshot {
                output_global_name: output_id,
                reply: reply_tx,
            })
            .map_err(|e| {
                zbus::fdo::Error::Failed(format!("Failed to send screenshot command: {e}"))
            })?;

        let screenshot_data = reply_rx
            .await
            .map_err(|_| zbus::fdo::Error::Failed("Color pick channel closed".to_string()))?
            .map_err(|e| zbus::fdo::Error::Failed(format!("Color pick capture failed: {e}")))?;

        // Pick the pixel color (via external tool or center fallback)
        let (red, green, blue) = pick_color_from_frame(&screenshot_data);

        tracing::info!(
            red = red,
            green = green,
            blue = blue,
            "Color picked from center of screen"
        );

        let mut results = HashMap::new();
        if let Ok(color) = OwnedValue::try_from(Value::from((red, green, blue))) {
            results.insert("color".to_string(), color);
        }

        // Remove Request object after method completes
        let _ = server.remove::<super::RequestInterface, _>(&handle).await;

        Ok((Response::Success.to_u32(), results))
    }

    // === Properties ===

    /// Interface version.
    #[zbus(property)]
    #[expect(clippy::unused_async, reason = "zbus interface requires async")]
    async fn version(&self) -> u32 {
        2
    }
}

/// Convert `BGRx`/ARGB pixel data to RGBA and encode as PNG, saving to a temp file.
///
/// Returns the file URI (e.g., `file:///tmp/xdp-screenshot-XXXX.png`).
fn encode_and_save_png(data: &ScreenshotData) -> Result<String, String> {
    // Convert BGRx to RGBA
    let rgba = convert_bgrx_to_rgba(&data.data, data.width, data.height, data.stride);

    // Create temp file
    let dir = std::env::temp_dir();
    let filename = format!(
        "xdp-screenshot-{}.png",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let path = dir.join(filename);

    // Encode PNG
    let file =
        std::fs::File::create(&path).map_err(|e| format!("Failed to create temp file: {e}"))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), data.width, data.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);

    let mut writer = encoder
        .write_header()
        .map_err(|e| format!("PNG header error: {e}"))?;

    writer
        .write_image_data(&rgba)
        .map_err(|e| format!("PNG write error: {e}"))?;

    writer
        .finish()
        .map_err(|e| format!("PNG finish error: {e}"))?;

    let uri = format!("file://{}", path.display());
    Ok(uri)
}

/// Convert `BGRx` (32-bit, blue-green-red-padding) pixel data to RGBA.
///
/// Most Wayland compositors provide SHM buffers in ARGB8888 or XRGB8888
/// format (which in little-endian memory layout is `BGRx`). This function
/// swaps the channels for PNG encoding.
fn convert_bgrx_to_rgba(data: &[u8], width: u32, height: u32, stride: u32) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        let row_start = (y * stride) as usize;
        for x in 0..width {
            let pixel_offset = row_start + (x * 4) as usize;
            if pixel_offset + 3 < data.len() {
                let b = data[pixel_offset];
                let g = data[pixel_offset + 1];
                let r = data[pixel_offset + 2];
                let a = data[pixel_offset + 3];
                rgba.push(r);
                rgba.push(g);
                rgba.push(b);
                // Use alpha if available (ARGB8888), otherwise opaque (XRGB8888)
                rgba.push(if a == 0 { 255 } else { a });
            } else {
                rgba.extend_from_slice(&[0, 0, 0, 255]);
            }
        }
    }

    rgba
}

/// Pick a color from the captured frame.
///
/// If `XDP_GENERIC_COLOR_PICKER` is set, it is invoked as an external tool:
/// - Receives the screenshot path (temporary PNG) on stdin
/// - Should output `x y` coordinates (pixel position) to stdout
/// - The color at those coordinates is extracted
///
/// Falls back to the center pixel when no external tool is configured.
///
/// Returns (r, g, b) as f64 values in the range 0.0 to 1.0.
fn pick_color_from_frame(data: &ScreenshotData) -> (f64, f64, f64) {
    // Check for external color picker tool
    if let Ok(picker_cmd) = std::env::var("XDP_GENERIC_COLOR_PICKER") {
        match run_color_picker(&picker_cmd, data) {
            Ok(color) => return color,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    picker = %picker_cmd,
                    "Color picker failed, falling back to center pixel"
                );
            }
        }
    }

    pick_center_color(data)
}

/// Run an external color picker tool.
///
/// Saves the screenshot as a temporary PNG, passes the path to the tool's
/// stdin, and reads `x y` coordinates from stdout. Returns the color at
/// those coordinates.
fn run_color_picker(picker_cmd: &str, data: &ScreenshotData) -> Result<(f64, f64, f64), String> {
    use std::{
        io::Write,
        process::{Command, Stdio},
    };

    // Save the screenshot to a temporary PNG file
    let png_path = encode_and_save_png(data)?;
    let file_path = png_path.strip_prefix("file://").unwrap_or(&png_path);

    // Parse the command
    let parts: Vec<&str> = picker_cmd.split_whitespace().collect();
    if parts.is_empty() {
        return Err("Empty picker command".to_string());
    }

    let mut cmd = Command::new(parts[0]);
    for arg in &parts[1..] {
        cmd.arg(arg);
    }

    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to spawn color picker: {e}"))?;

    // Write the screenshot path to stdin
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(file_path.as_bytes());
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Color picker process error: {e}"))?;

    // Clean up temp file
    let _ = std::fs::remove_file(file_path);

    if !output.status.success() {
        return Err(format!(
            "Color picker exited with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    // Parse `x y` from stdout
    let stdout = String::from_utf8_lossy(&output.stdout);
    let coords: Vec<&str> = stdout.split_whitespace().collect();
    if coords.len() < 2 {
        return Err(format!(
            "Color picker output not 'x y': {:?}",
            stdout.trim()
        ));
    }

    let x: u32 = coords[0]
        .parse()
        .map_err(|e| format!("Bad x coordinate: {e}"))?;
    let y: u32 = coords[1]
        .parse()
        .map_err(|e| format!("Bad y coordinate: {e}"))?;

    // Extract color at the given coordinates
    Ok(pick_color_at(data, x, y))
}

/// Pick the color at specific coordinates in the captured frame.
///
/// Returns (r, g, b) as f64 values in the range 0.0 to 1.0.
fn pick_color_at(data: &ScreenshotData, px: u32, py: u32) -> (f64, f64, f64) {
    let px = px.min(data.width.saturating_sub(1));
    let py = py.min(data.height.saturating_sub(1));
    let offset = (py * data.stride + px * 4) as usize;

    if offset + 3 <= data.data.len() {
        // BGRx layout in memory
        let blue = f64::from(data.data[offset]) / 255.0;
        let green = f64::from(data.data[offset + 1]) / 255.0;
        let red = f64::from(data.data[offset + 2]) / 255.0;
        (red, green, blue)
    } else {
        (0.0, 0.0, 0.0)
    }
}

/// Pick the color at the center of the captured frame.
///
/// Returns (r, g, b) as f64 values in the range 0.0 to 1.0.
fn pick_center_color(data: &ScreenshotData) -> (f64, f64, f64) {
    let cx = data.width / 2;
    let cy = data.height / 2;
    pick_color_at(data, cx, cy)
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests use expect for clearer failure messages"
)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_bgrx_to_rgba() {
        // BGRx: B=0x10, G=0x20, R=0x30, X=0xFF
        let bgrx = vec![0x10, 0x20, 0x30, 0xFF];
        let rgba = convert_bgrx_to_rgba(&bgrx, 1, 1, 4);
        assert_eq!(rgba, vec![0x30, 0x20, 0x10, 0xFF]); // R, G, B, A
    }

    #[test]
    fn test_convert_bgrx_to_rgba_with_stride() {
        // 2x1 image with stride=12 (8 bytes of pixels + 4 bytes padding)
        let mut bgrx = vec![0u8; 12];
        // Pixel (0,0): B=0xFF, G=0x00, R=0x00, X=0xFF (blue)
        bgrx[0] = 0xFF;
        bgrx[1] = 0x00;
        bgrx[2] = 0x00;
        bgrx[3] = 0xFF;
        // Pixel (1,0): B=0x00, G=0xFF, R=0x00, X=0xFF (green)
        bgrx[4] = 0x00;
        bgrx[5] = 0xFF;
        bgrx[6] = 0x00;
        bgrx[7] = 0xFF;

        let rgba = convert_bgrx_to_rgba(&bgrx, 2, 1, 12);
        assert_eq!(rgba.len(), 8);
        // Pixel 0: R=0, G=0, B=255, A=255
        assert_eq!(rgba[0..4], [0x00, 0x00, 0xFF, 0xFF]);
        // Pixel 1: R=0, G=255, B=0, A=255
        assert_eq!(rgba[4..8], [0x00, 0xFF, 0x00, 0xFF]);
    }

    #[test]
    fn test_pick_color_at_specific_pixel() {
        // 2x2 image: pixel (0,0)=red, pixel (1,0)=green
        let mut data = vec![0u8; 2 * 2 * 4];
        // (0,0) BGRx: B=0, G=0, R=255, X=255
        data[0] = 0;
        data[1] = 0;
        data[2] = 255;
        data[3] = 255;
        // (1,0) BGRx: B=0, G=255, R=0, X=255
        data[4] = 0;
        data[5] = 255;
        data[6] = 0;
        data[7] = 255;

        let screenshot = ScreenshotData {
            data,
            width: 2,
            height: 2,
            stride: 8,
            format_raw: 0,
        };

        let (r, g, b) = pick_color_at(&screenshot, 0, 0);
        assert!((r - 1.0).abs() < 0.01);
        assert!(g.abs() < 0.01);
        assert!(b.abs() < 0.01);

        let (r, g, b) = pick_color_at(&screenshot, 1, 0);
        assert!(r.abs() < 0.01);
        assert!((g - 1.0).abs() < 0.01);
        assert!(b.abs() < 0.01);
    }

    #[test]
    fn test_pick_color_at_clamped() {
        // 1x1 image, should clamp out-of-bounds coordinates
        let data = vec![0, 0, 255, 255]; // BGRx: red
        let screenshot = ScreenshotData {
            data,
            width: 1,
            height: 1,
            stride: 4,
            format_raw: 0,
        };

        let (r, _g, _b) = pick_color_at(&screenshot, 100, 100);
        assert!((r - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_pick_center_color() {
        // 2x2 image, all pixels red (BGRx: B=0, G=0, R=255, X=255)
        let mut data = vec![0u8; 2 * 2 * 4];
        for i in (0..data.len()).step_by(4) {
            data[i] = 0; // B
            data[i + 1] = 0; // G
            data[i + 2] = 255; // R
            data[i + 3] = 255; // X
        }

        let screenshot = ScreenshotData {
            data,
            width: 2,
            height: 2,
            stride: 8,
            format_raw: 0,
        };

        let (r, g, b) = pick_center_color(&screenshot);
        assert!((r - 1.0).abs() < 0.01);
        assert!(g.abs() < 0.01);
        assert!(b.abs() < 0.01);
    }

    #[test]
    fn test_pick_center_color_empty() {
        let screenshot = ScreenshotData {
            data: vec![],
            width: 0,
            height: 0,
            stride: 0,
            format_raw: 0,
        };

        let (red, green, blue) = pick_center_color(&screenshot);
        assert!(red.abs() < f64::EPSILON);
        assert!(green.abs() < f64::EPSILON);
        assert!(blue.abs() < f64::EPSILON);
    }

    #[test]
    fn test_encode_and_save_png() {
        // Create a small 4x4 test image (BGRx format)
        let mut data = vec![0u8; 4 * 4 * 4];
        for i in (0..data.len()).step_by(4) {
            data[i] = 128; // B
            data[i + 1] = 64; // G
            data[i + 2] = 32; // R
            data[i + 3] = 255; // X
        }

        let screenshot = ScreenshotData {
            data,
            width: 4,
            height: 4,
            stride: 16,
            format_raw: 0,
        };

        let uri = encode_and_save_png(&screenshot).expect("PNG encoding should succeed");
        assert!(uri.starts_with("file:///"));
        assert!(
            std::path::Path::new(&uri)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
        );

        // Verify the file exists
        let path = uri
            .strip_prefix("file://")
            .expect("should have file:// prefix");
        assert!(std::path::Path::new(path).exists());

        // Clean up
        let _ = std::fs::remove_file(path);
    }
}
