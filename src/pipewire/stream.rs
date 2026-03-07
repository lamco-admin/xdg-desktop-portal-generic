//! PipeWire video source stream management.
//!
//! Each stream corresponds to one captured output. The stream is created as a
//! PipeWire Video/Source node and connected with `ALLOC_BUFFERS` so PipeWire
//! allocates the buffer pool. Frames are queued by copying screencopy data
//! into dequeued buffers.

use pipewire::{
    properties::properties,
    stream::{StreamBox, StreamFlags},
};

use crate::error::PortalError;

/// Configuration for creating a PipeWire video stream.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    /// Source ID (wl_output global name).
    pub source_id: u32,
    /// Video width in pixels.
    pub width: u32,
    /// Video height in pixels.
    pub height: u32,
    /// Target framerate.
    pub framerate: u32,
}

/// A PipeWire video source stream.
///
/// Wraps a `pw_stream` that advertises as a Video/Source node. Consumers
/// (like the xdg-desktop-portal frontend) connect to this node to receive
/// screen capture frames.
pub struct PipeWireVideoStream {
    /// The underlying PipeWire stream.
    // SAFETY: StreamBox borrows from CoreBox, but we ensure streams are destroyed
    // before the core in run_thread() cleanup. The 'static is a lifetime erasure
    // needed because we store streams in a HashMap that outlives the borrow scope.
    stream: StreamBox<'static>,
    /// The PipeWire node ID assigned to this stream.
    node_id: u32,
}

impl PipeWireVideoStream {
    /// Create and connect a new video source stream.
    ///
    /// The stream is created with `media.class=Video/Source` and connected
    /// as an output direction with `ALLOC_BUFFERS` flag.
    pub fn create(
        core: &pipewire::core::CoreBox<'_>,
        config: &StreamConfig,
    ) -> Result<Self, PortalError> {
        let source_id = config.source_id;
        let node_name = format!("xdp-capture-{source_id}");
        let node_desc = format!("Screen Capture Source {source_id}");

        // Create stream with media properties identifying it as a video source.
        // stream.is-live: this stream produces data in real-time.
        // node.want-driver: request a driver to schedule graph cycles.
        let stream = StreamBox::new(
            core,
            "xdp-screen-capture",
            properties! {
                "media.class" => "Video/Source",
                "media.role" => "Screen",
                "media.category" => "Capture",
                "node.name" => node_name,
                "node.description" => node_desc,
                "stream.is-live" => "true",
                "node.want-driver" => "true",
            },
        )
        .map_err(|e| PortalError::PipeWire(format!("Failed to create stream: {e}")))?;

        // SAFETY: Erase lifetime to 'static. The stream is stored in a HashMap
        // alongside the core. Streams are always destroyed before the core in
        // run_thread() cleanup, maintaining the borrow invariant.
        #[expect(
            unsafe_code,
            reason = "lifetime erasure for HashMap storage; drop order enforced"
        )]
        let stream: StreamBox<'static> = unsafe { std::mem::transmute(stream) };

        // Build the SPA video format parameter for the stream.
        let format_pod_bytes =
            Self::build_video_format_pod(config.width, config.height, config.framerate);

        let format_pod_bytes = format_pod_bytes.ok_or_else(|| {
            PortalError::PipeWire("Failed to build SPA video format pod".to_string())
        })?;

        // The connect() method expects &mut [&libspa::pod::Pod].
        // We cast our serialized bytes to a Pod reference.
        // SAFETY: format_pod_bytes was just serialized by PodSerializer and contains
        // a valid SPA pod. Pod::from_raw creates an immutable reference to it.
        #[expect(
            unsafe_code,
            reason = "SPA pod pointer cast requires unsafe; alignment is validated by libspa serializer"
        )]
        #[expect(
            clippy::cast_ptr_alignment,
            reason = "SPA pod alignment is validated by libspa"
        )]
        let pod = unsafe {
            libspa::pod::Pod::from_raw(format_pod_bytes.as_ptr().cast::<libspa::sys::spa_pod>())
        };

        // Connect as an output (we produce frames).
        // MAP_BUFFERS: we want CPU-accessible buffers to copy screencopy data into.
        // ALLOC_BUFFERS: PipeWire allocates the buffer pool.
        // DRIVER: this stream drives the graph timing (it's a live source).
        stream
            .connect(
                libspa::utils::Direction::Output,
                None,
                StreamFlags::MAP_BUFFERS | StreamFlags::ALLOC_BUFFERS | StreamFlags::DRIVER,
                &mut [pod],
            )
            .map_err(|e| PortalError::PipeWire(format!("Failed to connect stream: {e}")))?;

        // node_id() may return SPA_ID_INVALID (u32::MAX) immediately after
        // connect() because node assignment is asynchronous. The caller must
        // run main loop iterations and call refresh_node_id() to get the real ID.
        let node_id = stream.node_id();

        Ok(Self { stream, node_id })
    }

    /// Build a raw SPA video format POD.
    ///
    /// Creates an Object pod with SPA_TYPE_OBJECT_Format containing:
    /// - mediaType = video
    /// - mediaSubtype = raw
    /// - format = BGRx
    /// - size = width x height
    /// - framerate = num/1
    fn build_video_format_pod(width: u32, height: u32, framerate: u32) -> Option<Vec<u8>> {
        use libspa::{
            pod::{self, serialize::PodSerializer, Value},
            utils::Id,
        };

        // Use the libspa-sys generated constants for SPA format properties.
        // These are the raw C enum values from spa/param/format.h.
        let spa_type_object_format = libspa::sys::SPA_TYPE_OBJECT_Format;
        let spa_param_enum_format = libspa::sys::SPA_PARAM_EnumFormat;
        let spa_format_media_type = libspa::sys::SPA_FORMAT_mediaType;
        let spa_format_media_subtype = libspa::sys::SPA_FORMAT_mediaSubtype;
        let spa_format_video_format = libspa::sys::SPA_FORMAT_VIDEO_format;
        let spa_format_video_size = libspa::sys::SPA_FORMAT_VIDEO_size;
        let spa_format_video_framerate = libspa::sys::SPA_FORMAT_VIDEO_framerate;

        // SPA media type and subtype IDs
        let spa_media_type_video = libspa::sys::SPA_MEDIA_TYPE_video;
        let spa_media_subtype_raw = libspa::sys::SPA_MEDIA_SUBTYPE_raw;
        // SPA video format BGRx
        let spa_video_format_bgrx = libspa::sys::SPA_VIDEO_FORMAT_BGRx;

        let mut buffer = Vec::<u8>::with_capacity(1024);
        let mut cursor = std::io::Cursor::new(&mut buffer);

        let result = PodSerializer::serialize(
            &mut cursor,
            &Value::Object(pod::Object {
                type_: spa_type_object_format,
                id: spa_param_enum_format,
                properties: vec![
                    pod::Property {
                        key: spa_format_media_type,
                        flags: pod::PropertyFlags::empty(),
                        value: Value::Id(Id(spa_media_type_video)),
                    },
                    pod::Property {
                        key: spa_format_media_subtype,
                        flags: pod::PropertyFlags::empty(),
                        value: Value::Id(Id(spa_media_subtype_raw)),
                    },
                    pod::Property {
                        key: spa_format_video_format,
                        flags: pod::PropertyFlags::empty(),
                        value: Value::Id(Id(spa_video_format_bgrx)),
                    },
                    pod::Property {
                        key: spa_format_video_size,
                        flags: pod::PropertyFlags::empty(),
                        value: Value::Rectangle(libspa::utils::Rectangle { width, height }),
                    },
                    pod::Property {
                        key: spa_format_video_framerate,
                        flags: pod::PropertyFlags::empty(),
                        value: Value::Fraction(libspa::utils::Fraction {
                            num: framerate,
                            denom: 1,
                        }),
                    },
                ],
            }),
        );

        match result {
            Ok((_cursor, _)) => Some(buffer),
            Err(e) => {
                tracing::error!("Failed to serialize video format pod: {:?}", e);
                None
            }
        }
    }

    /// Get the PipeWire node ID for this stream.
    pub fn node_id(&self) -> u32 {
        self.node_id
    }

    /// Re-read the node ID from the underlying PipeWire stream.
    ///
    /// After connect(), the node ID is not immediately available.
    /// Call this after running main loop iterations to get the real ID
    /// once PipeWire has assigned it.
    pub fn refresh_node_id(&mut self) -> u32 {
        self.node_id = self.stream.node_id();
        self.node_id
    }

    /// Queue a frame of pixel data into the stream.
    ///
    /// Dequeues a buffer from PipeWire, copies the frame data in, then
    /// the buffer is automatically queued back when dropped (RAII).
    pub fn queue_frame(
        &mut self,
        data: &[u8],
        _width: u32,
        height: u32,
        stride: u32,
        _format: u32,
    ) -> Result<(), PortalError> {
        // Dequeue a buffer from PipeWire
        let mut buffer = self.stream.dequeue_buffer().ok_or_else(|| {
            PortalError::PipeWire("No buffer available from PipeWire stream".to_string())
        })?;

        // Get the data pointer from the first data block
        let datas = buffer.datas_mut();
        if datas.is_empty() {
            return Err(PortalError::PipeWire(
                "PipeWire buffer has no data blocks".to_string(),
            ));
        }

        let pw_data = &mut datas[0];

        // Get the mapped data slice and copy frame pixels
        if let Some(dest_slice) = pw_data.data() {
            let copy_len = data.len().min(dest_slice.len());
            dest_slice[..copy_len].copy_from_slice(&data[..copy_len]);
        } else {
            return Err(PortalError::PipeWire(
                "PipeWire buffer data not mapped".to_string(),
            ));
        }

        // Set the chunk metadata
        let chunk = pw_data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.size_mut() = stride * height;
        *chunk.stride_mut() = stride as i32;

        // Buffer is automatically queued when dropped (RAII via Drop impl)
        Ok(())
    }

    /// Disconnect and clean up the stream.
    pub fn disconnect(self) {
        let _ = self.stream.disconnect();
        // Stream is dropped here, calling pw_stream_destroy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_config_creation() {
        let config = StreamConfig {
            source_id: 1,
            width: 1920,
            height: 1080,
            framerate: 30,
        };
        assert_eq!(config.width, 1920);
        assert_eq!(config.height, 1080);
    }

    #[test]
    fn test_build_video_format_pod() {
        // Initialize PipeWire/SPA so the pod serializer works
        pipewire::init();

        let pod = PipeWireVideoStream::build_video_format_pod(1920, 1080, 30);
        assert!(pod.is_some());
        let bytes = pod.unwrap();
        // The pod should have some reasonable size (header + properties)
        assert!(bytes.len() > 32);
    }
}
