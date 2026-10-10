//! Codec decoder traits for client-side EGFX processing
//!
//! This module provides pluggable decoder traits that allow consumers
//! to bring their own codec implementations (e.g., openh264, ffmpeg,
//! hardware decoders). The traits are designed for core tier: no I/O,
//! `Send` only. They are intended for use in `std` environments;
//! `no_std` + `alloc` support is not currently guaranteed.
//!
//! # Protocol Context
//!
//! H.264 data arrives inside [RFX_AVC420_BITMAP_STREAM][1] payloads
//! within `RDPGFX_WIRE_TO_SURFACE_PDU_1` messages. The NAL units
//! are in AVC format (4-byte big-endian length prefix per NAL unit),
//! not Annex B (start code prefix).
//!
//! [1]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/d65c3f9c-2088-4302-90c0-53adc0e11a78

use core::fmt;

use ironrdp_pdu::geometry::ExclusiveRectangle;

// ============================================================================
// Decoded Frame
// ============================================================================

/// Decoded bitmap frame from an H.264 decoder
///
/// Contains RGBA pixel data for a decoded H.264 frame.
/// The pixel data is in RGBA format (4 bytes per pixel),
/// row-major, top-to-bottom, left-to-right.
#[derive(Clone)]
#[non_exhaustive]
pub struct DecodedFrame {
    data: Vec<u8>,
    width: u32,
    height: u32,
}

impl DecodedFrame {
    #[expect(
        clippy::as_conversions,
        reason = "usize to u64 is lossless on all supported platforms (32/64-bit)"
    )]
    pub fn new(data: Vec<u8>, width: u32, height: u32) -> Self {
        debug_assert_eq!(
            data.len() as u64,
            u64::from(width).saturating_mul(u64::from(height)).saturating_mul(4),
            "DecodedFrame buffer must be RGBA8888 (width * height * 4 bytes)",
        );
        Self { data, width, height }
    }

    /// RGBA pixel data (4 bytes per pixel).
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Frame width in pixels.
    pub fn width(&self) -> u32 {
        self.width
    }

    /// Frame height in pixels.
    pub fn height(&self) -> u32 {
        self.height
    }

    /// Consume the frame and return the owned RGBA buffer.
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

impl fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// RGBA for one AVC420 region rectangle, clipped to the decoded frame.
#[derive(Clone, PartialEq, Eq)]
pub struct DecodedRegion {
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
    /// Tightly packed RGBA, `width * height * 4` bytes.
    pub data: Vec<u8>,
}

impl fmt::Debug for DecodedRegion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedRegion")
            .field("left", &self.left)
            .field("top", &self.top)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("data_len", &self.data.len())
            .finish()
    }
}

/// Result of [`H264Decoder::decode_regions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedRegions {
    /// Decoded frame width, which may exceed the surface due to macroblock alignment.
    pub frame_width: u32,
    /// Decoded frame height.
    pub frame_height: u32,
    /// One entry per requested region that overlaps the frame, in request order.
    pub regions: Vec<DecodedRegion>,
}

/// Copy one AVC420 regionRect out of a tightly packed RGBA frame, clipped to the frame.
///
/// `rect` is in surface coordinates with **exclusive** right/bottom (RDPGFX_RECT16).
/// Treating it as inclusive over-copies one row/column per rect, which for full-height
/// rects comes from the 16-aligned frame's padding macroblocks (uninitialised YUV,
/// 1px green edge lines). Returns `None` for an empty / malformed / out-of-frame rect.
pub fn crop_region(frame: &[u8], frame_w: u32, frame_h: u32, rect: &ExclusiveRectangle) -> Option<DecodedRegion> {
    if rect.right < rect.left || rect.bottom < rect.top {
        return None;
    }
    // Frame dims are bounded by the surface size; saturate so the math stays in u16.
    let fw = u16::try_from(frame_w).unwrap_or(u16::MAX);
    let fh = u16::try_from(frame_h).unwrap_or(u16::MAX);
    let (left, top) = (rect.left, rect.top);
    let right = rect.right.min(fw);
    let bottom = rect.bottom.min(fh);
    if right <= left || bottom <= top {
        return None;
    }
    let width = right - left;
    let height = bottom - top;

    let src_stride = usize::try_from(frame_w).ok()? * 4;
    let left_off = usize::from(left) * 4;
    let dst_stride = usize::from(width) * 4;
    let mut data = Vec::with_capacity(dst_stride * usize::from(height));
    for row in top..bottom {
        let so = usize::from(row) * src_stride + left_off;
        let se = so + dst_stride;
        if se <= frame.len() {
            data.extend_from_slice(&frame[so..se]);
        } else {
            // Cannot happen once clipped to frame bounds; keep the buffer sized anyway.
            data.resize(data.len() + dst_stride, 0);
        }
    }
    Some(DecodedRegion {
        left,
        top,
        width,
        height,
        data,
    })
}

// ============================================================================
// Decoder Error
// ============================================================================

/// Error type for decoder operations
#[derive(Debug)]
#[non_exhaustive]
pub struct DecoderError {
    context: String,
    source: Option<Box<dyn core::error::Error + Send + Sync>>,
}

impl DecoderError {
    /// Create a decoder error with a source error
    pub fn new(context: impl Into<String>, source: impl core::error::Error + Send + Sync + 'static) -> Self {
        Self {
            context: context.into(),
            source: Some(Box::new(source)),
        }
    }

    /// Create a decoder error with only a message
    pub fn msg(context: impl Into<String>) -> Self {
        Self {
            context: context.into(),
            source: None,
        }
    }
}

impl fmt::Display for DecoderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decoder error: {}", self.context)?;
        if let Some(ref source) = self.source {
            write!(f, ": {source}")?;
        }
        Ok(())
    }
}

impl core::error::Error for DecoderError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        self.source.as_deref().map(|e| {
            let err: &(dyn core::error::Error + 'static) = e;
            err
        })
    }
}

/// Result type for decoder operations
pub type DecoderResult<T> = Result<T, DecoderError>;

// ============================================================================
// H.264 Decoder Trait
// ============================================================================

/// Trait for H.264 (AVC) decoders
///
/// Implement this trait to provide H.264 decode capability to the
/// EGFX client. The decoder receives AVC-format NAL units (length-prefixed,
/// not Annex B) from `RFX_AVC420_BITMAP_STREAM` payloads.
///
/// # Thread Safety
///
/// Implementations must be `Send` to work with the DVC framework.
///
/// # Example
///
/// ```ignore
/// use ironrdp_egfx::decode::{H264Decoder, DecodedFrame, DecoderResult};
///
/// struct MyH264Decoder { /* ... */ }
///
/// impl H264Decoder for MyH264Decoder {
///     fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
///         // Decode H.264 NAL units to RGBA
///         todo!()
///     }
/// }
/// ```
pub trait H264Decoder: Send {
    /// Decode AVC-format H.264 NAL units (4-byte BE length prefix, not Annex B)
    /// into an RGBA bitmap.
    ///
    /// Frame dimensions may exceed the destination rectangle due to
    /// macroblock alignment (16x16). The caller crops to fit.
    fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame>;

    /// Decode, then return RGBA for `regions` only (exclusive bounds, clipped to the frame;
    /// regions entirely outside it are dropped).
    ///
    /// AVC420 marks only its `regionRects` as valid ([MS-RDPEGFX] 2.2.4.4), so a decoder
    /// that can convert sub-rectangles straight from its native YUV output should override
    /// this and skip converting the rest of the frame. The default decodes the whole frame
    /// to RGBA and crops.
    ///
    /// [MS-RDPEGFX] 2.2.4.4: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpegfx/d65c3f9c-2088-4302-90c0-53adc0e11a78
    fn decode_regions(&mut self, data: &[u8], regions: &[ExclusiveRectangle]) -> DecoderResult<DecodedRegions> {
        let frame = self.decode(data)?;
        Ok(DecodedRegions {
            frame_width: frame.width(),
            frame_height: frame.height(),
            regions: regions
                .iter()
                .filter_map(|rect| crop_region(frame.data(), frame.width(), frame.height(), rect))
                .collect(),
        })
    }

    /// Reset the decoder state
    ///
    /// Called when surfaces are reset (e.g., on `ResetGraphics`).
    /// The decoder should drop any internal state and prepare for
    /// a new stream.
    fn reset(&mut self) {
        // Default: no-op
    }
}

// ============================================================================
// OpenH264 Implementation
// ============================================================================

#[cfg(feature = "openh264")]
mod openh264_impl {
    use openh264::formats::YUVSource;
    use tracing::warn;

    use super::{DecodedFrame, DecoderError, DecoderResult, H264Decoder};

    /// H.264 decoder backed by Cisco's OpenH264 library
    ///
    /// This decoder converts AVC-format NAL units to Annex B format
    /// (as required by OpenH264), decodes to YUV420p, then converts
    /// to RGBA for the client pipeline.
    ///
    /// # Feature Gates
    ///
    /// Two construction paths are available depending on the feature flags:
    ///
    /// - `openh264-bundled`: compiles OpenH264 from source at build time.
    ///   Use [`OpenH264Decoder::new()`] to construct.
    ///
    /// - `openh264-libloading`: loads a prebuilt Cisco OpenH264 binary at
    ///   runtime. Use [`OpenH264Decoder::from_library_path()`] to construct.
    ///   The library is verified against known Cisco release hashes.
    pub struct OpenH264Decoder {
        decoder: openh264::decoder::Decoder,
        annex_b_buffer: Vec<u8>,
    }

    impl OpenH264Decoder {
        /// Create a decoder using the bundled (source-compiled) OpenH264 library
        ///
        /// This compiles OpenH264 C code at build time. The resulting binary
        /// has no patent coverage from Cisco's license agreement.
        #[cfg(feature = "openh264-bundled")]
        pub fn new() -> DecoderResult<Self> {
            let decoder = openh264::decoder::Decoder::new()
                .map_err(|e| DecoderError::new("failed to create OpenH264 decoder", e))?;

            Ok(Self {
                decoder,
                annex_b_buffer: Vec::new(),
            })
        }

        /// Create a decoder using a dynamically loaded OpenH264 library
        ///
        /// `library_path` should point to a Cisco OpenH264 prebuilt binary,
        /// which is verified against known Cisco release hashes before loading.
        /// Cisco's prebuilt binaries carry patent coverage under their license.
        #[cfg(feature = "openh264-libloading")]
        pub fn from_library_path(library_path: &std::path::Path) -> DecoderResult<Self> {
            let api = openh264::OpenH264API::from_blob_path(library_path)
                .map_err(|e| DecoderError::new("failed to load OpenH264 library", e))?;
            let decoder = openh264::decoder::Decoder::with_api_config(api, Default::default())
                .map_err(|e| DecoderError::new("failed to create OpenH264 decoder", e))?;

            Ok(Self {
                decoder,
                annex_b_buffer: Vec::new(),
            })
        }
    }

    impl H264Decoder for OpenH264Decoder {
        fn decode(&mut self, data: &[u8]) -> DecoderResult<DecodedFrame> {
            crate::pdu::avc_to_annex_b_into(data, &mut self.annex_b_buffer);

            let yuv = self
                .decoder
                .decode(&self.annex_b_buffer)
                .map_err(|e| DecoderError::new("OpenH264 decode failed", e))?
                .ok_or_else(|| DecoderError::msg("OpenH264 returned no picture"))?;

            let (width, height) = YUVSource::dimensions(&yuv);
            let (y_stride, u_stride, v_stride) = YUVSource::strides(&yuv);

            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                reason = "H.264 frame dimensions are always within u32 range"
            )]
            let (w32, h32) = (width as u32, height as u32);
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                reason = "strides are bounded by frame dimensions, which fit in u32"
            )]
            let [y_stride, u_stride, v_stride] = [y_stride as u32, u_stride as u32, v_stride as u32];

            let rgba_stride = w32
                .checked_mul(4)
                .ok_or_else(|| DecoderError::msg("frame dimensions too large for RGBA allocation"))?;
            let rgba_size = width
                .checked_mul(height)
                .and_then(|s| s.checked_mul(4))
                .ok_or_else(|| DecoderError::msg("frame dimensions too large for RGBA allocation"))?;
            let mut rgba = vec![0u8; rgba_size];

            // MS-RDPEGFX AVC420 streams carry full-range BT.709 YUV420,
            // but openh264's `write_rgba8` assumes limited-range BT.601,
            // which distorts colors. Convert per [MS-RDPEGFX] 3.3.8.3.1.
            let planar = yuv::YuvPlanarImage {
                y_plane: yuv.y(),
                y_stride,
                u_plane: yuv.u(),
                u_stride,
                v_plane: yuv.v(),
                v_stride,
                width: w32,
                height: h32,
            };
            yuv::yuv420_to_rgba(
                &planar,
                &mut rgba,
                rgba_stride,
                yuv::YuvRange::Full,
                yuv::YuvStandardMatrix::Bt709,
            )
            .map_err(|e| DecoderError::new("failed to convert YUV420 to RGBA", e))?;

            Ok(DecodedFrame::new(rgba, w32, h32))
        }

        fn reset(&mut self) {
            // Recreate decoder from source when available
            #[cfg(feature = "openh264-bundled")]
            match openh264::decoder::Decoder::new() {
                Ok(new_decoder) => self.decoder = new_decoder,
                Err(e) => warn!("Failed to reset OpenH264 decoder, reusing existing state: {e}"),
            }
            // In libloading-only mode, we don't have the library path stored,
            // so we can't recreate. The existing decoder handles new SPS/PPS
            // transparently when the next I-frame arrives.
        }
    }
}

#[cfg(feature = "openh264")]
pub use openh264_impl::OpenH264Decoder;

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x2 frame whose pixel at (x, y) is `[x, y, 0, 255]`.
    struct FixedDecoder;

    impl H264Decoder for FixedDecoder {
        fn decode(&mut self, _data: &[u8]) -> DecoderResult<DecodedFrame> {
            let data = (0..2u8)
                .flat_map(|y| (0..4u8).flat_map(move |x| [x, y, 0, 255]))
                .collect();
            Ok(DecodedFrame::new(data, 4, 2))
        }
    }

    fn rect(left: u16, top: u16, right: u16, bottom: u16) -> ExclusiveRectangle {
        ExclusiveRectangle {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn default_decode_regions_crops_clips_and_drops() {
        let decoded = FixedDecoder
            .decode_regions(&[], &[rect(1, 1, 3, 2), rect(3, 0, 9, 9), rect(5, 5, 6, 6)])
            .unwrap();

        assert_eq!((decoded.frame_width, decoded.frame_height), (4, 2));
        assert_eq!(decoded.regions.len(), 2, "the out-of-frame region is dropped");
        let first = &decoded.regions[0];
        assert_eq!((first.left, first.top, first.width, first.height), (1, 1, 2, 1));
        assert_eq!(first.data, [1, 1, 0, 255, 2, 1, 0, 255]);
        let clipped = &decoded.regions[1];
        assert_eq!((clipped.left, clipped.top, clipped.width, clipped.height), (3, 0, 1, 2));
    }
}
