//! `leveler-media` — the image attachment pipeline (spec §41, §45).
//!
//! Imports an image safely: the real MIME is detected from content (never the
//! extension), the image is decoded and bounded by pixel count and byte size,
//! EXIF is stripped by re-encoding, oversized images are downscaled, and the
//! result is hashed and written to a content-addressed store. The processed
//! bytes — not the original path — are what later requests reference, so a
//! session's images survive the source file being moved or deleted.

#![forbid(unsafe_code)]

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use image::imageops::FilterType;
use image::{ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};

/// Maximum accepted source file size (spec §41).
pub const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// Maximum decoded pixel count, to defeat decompression bombs (spec §45).
pub const MAX_PIXELS: u64 = 40_000_000;
/// Maximum memory the image decoder may allocate while importing one image.
const MAX_DECODE_ALLOC_BYTES: u64 = MAX_PIXELS * 8;
/// Longest-edge cap; larger images are downscaled (spec §41).
pub const MAX_DIMENSION: u32 = 2048;

/// Errors importing an image.
#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("io error: {0}")]
    Io(String),
    #[error("file too large: {0} bytes (max {MAX_IMAGE_BYTES})")]
    TooLarge(u64),
    #[error("unsupported media type: {0}")]
    Unsupported(String),
    #[error("image too large: {0} pixels (max {MAX_PIXELS})")]
    TooManyPixels(u64),
    #[error("decode error: {0}")]
    Decode(String),
}

/// A supported input image type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl ImageKind {
    fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Self::Png),
            "image/jpeg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }

    fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Webp => ImageFormat::WebP,
            Self::Gif => ImageFormat::Gif,
        }
    }
}

/// A processed, stored image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredImage {
    /// SHA-256 (hex) of the processed bytes — the store key.
    pub sha256: String,
    pub path: PathBuf,
    /// Normalized MIME type of the stored bytes (always `image/png`).
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
}

/// A content-addressed image store, rooted at a directory (`.leveler/media`).
pub struct MediaStore {
    root: PathBuf,
}

impl MediaStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Import an image from a file path.
    pub fn import_path(&self, path: &Path) -> Result<StoredImage, MediaError> {
        let bytes = fs::read(path).map_err(|e| MediaError::Io(e.to_string()))?;
        self.import_bytes(&bytes)
    }

    /// Import immutable base64-encoded source bytes received over the client
    /// protocol. Bound the encoded form before allocating the decoded buffer.
    pub fn import_base64(&self, encoded: &str) -> Result<StoredImage, MediaError> {
        const MAX_ENCODED_BYTES: usize = (MAX_IMAGE_BYTES as usize).div_ceil(3) * 4;
        if encoded.len() > MAX_ENCODED_BYTES {
            return Err(MediaError::TooLarge(MAX_IMAGE_BYTES + 1));
        }
        let bytes = BASE64
            .decode(encoded)
            .map_err(|error| MediaError::Decode(format!("invalid base64 attachment: {error}")))?;
        self.import_bytes(&bytes)
    }

    /// Import an image from raw bytes: validate, decode, bound, strip EXIF,
    /// downscale, hash, and store (deduplicating by hash).
    pub fn import_bytes(&self, bytes: &[u8]) -> Result<StoredImage, MediaError> {
        let len = bytes.len() as u64;
        if len > MAX_IMAGE_BYTES {
            return Err(MediaError::TooLarge(len));
        }

        // Real type from content, not the extension (spec §45).
        let mime = infer::get(bytes).map(|t| t.mime_type().to_string());
        let Some(kind) = mime.as_deref().and_then(ImageKind::from_mime) else {
            return Err(MediaError::Unsupported(
                mime.unwrap_or_else(|| "unknown".to_string()),
            ));
        };
        let format = kind.image_format();

        // Read dimensions without materializing the pixel buffer. This must
        // happen before decode so the pixel cap actually prevents image bombs.
        let (width, height) = ImageReader::with_format(Cursor::new(bytes), format)
            .into_dimensions()
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        validate_pixel_count(width, height)?;

        // Re-open the immutable byte slice for the full decode and enforce both
        // the dimensions observed above and a bounded decoder allocation budget.
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        let mut limits = Limits::default();
        limits.max_image_width = Some(width);
        limits.max_image_height = Some(height);
        limits.max_alloc = Some(MAX_DECODE_ALLOC_BYTES);
        reader.limits(limits);
        let decoded = reader
            .decode()
            .map_err(|e| MediaError::Decode(e.to_string()))?;

        // Downscale if the longest edge exceeds the cap.
        let processed = if decoded.width().max(decoded.height()) > MAX_DIMENSION {
            decoded.resize(MAX_DIMENSION, MAX_DIMENSION, FilterType::Lanczos3)
        } else {
            decoded
        };

        // Re-encode to PNG: deterministic and EXIF-free.
        let mut out = Vec::new();
        processed
            .write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .map_err(|e| MediaError::Decode(e.to_string()))?;

        let sha256 = hex(&Sha256::digest(&out));
        fs::create_dir_all(&self.root).map_err(|e| MediaError::Io(e.to_string()))?;
        let path = self.root.join(format!("{sha256}.png"));
        if !path.exists() {
            fs::write(&path, &out).map_err(|e| MediaError::Io(e.to_string()))?;
        }

        Ok(StoredImage {
            sha256,
            path,
            mime_type: "image/png".to_string(),
            width: processed.width(),
            height: processed.height(),
            size_bytes: out.len() as u64,
        })
    }

    /// Import raw RGBA pixels (e.g. from the clipboard) by encoding to PNG and
    /// running the normal import pipeline (spec §38.1).
    pub fn import_rgba(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<StoredImage, MediaError> {
        let pixels = validate_pixel_count(width, height)?;
        let expected_len = pixels
            .checked_mul(4)
            .and_then(|len| usize::try_from(len).ok())
            .ok_or_else(|| MediaError::Decode("clipboard dimensions overflow".to_string()))?;
        if rgba.len() != expected_len {
            return Err(MediaError::Decode(
                "clipboard pixel buffer size mismatch".to_string(),
            ));
        }
        // Copy only after all dimensions and lengths have been validated.
        let buffer = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .expect("validated RGBA dimensions and length");
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(buffer)
            .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
            .map_err(|e| MediaError::Decode(e.to_string()))?;
        self.import_bytes(&png)
    }

    /// Load a stored image as `(mime_type, base64)` for a provider request.
    /// Base64 is produced here, at the request boundary, and never logged.
    pub fn load_base64(&self, sha256: &str) -> Result<(String, String), MediaError> {
        let path = self.root.join(format!("{sha256}.png"));
        let bytes = fs::read(&path).map_err(|e| MediaError::Io(e.to_string()))?;
        Ok(("image/png".to_string(), BASE64.encode(bytes)))
    }
}

fn validate_pixel_count(width: u32, height: u32) -> Result<u64, MediaError> {
    let pixels = u64::from(width) * u64::from(height);
    if pixels > MAX_PIXELS {
        return Err(MediaError::TooManyPixels(pixels));
    }
    Ok(pixels)
}

/// Lowercase hex encoding of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::new(w, h));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), ImageFormat::Png)
            .unwrap();
        out
    }

    fn png_with_declared_dimensions(width: u32, height: u32) -> Vec<u8> {
        let mut png = png_bytes(1, 1);
        // PNG signature (8), IHDR length (4), type (4), then width and height.
        png[16..20].copy_from_slice(&width.to_be_bytes());
        png[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        png
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (0u32.wrapping_sub(crc & 1)));
            }
        }
        !crc
    }

    #[test]
    fn imports_and_stores_a_png() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let stored = store.import_bytes(&png_bytes(10, 8)).unwrap();
        assert_eq!(stored.width, 10);
        assert_eq!(stored.height, 8);
        assert!(stored.path.exists());
        assert_eq!(stored.mime_type, "image/png");
    }

    #[test]
    fn dedupes_identical_images_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let a = store.import_bytes(&png_bytes(4, 4)).unwrap();
        let b = store.import_bytes(&png_bytes(4, 4)).unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.path, b.path);
    }

    #[test]
    fn rejects_non_image_bytes_regardless_of_caller() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let err = store.import_bytes(b"this is not an image").unwrap_err();
        assert!(matches!(err, MediaError::Unsupported(_)));
    }

    #[test]
    fn downscales_oversized_images() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let stored = store.import_bytes(&png_bytes(4000, 1000)).unwrap();
        assert!(stored.width <= MAX_DIMENSION && stored.height <= MAX_DIMENSION);
        assert_eq!(
            stored.width, MAX_DIMENSION,
            "longest edge scaled to the cap"
        );
    }

    #[test]
    fn rejects_excessive_declared_dimensions_before_decode() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let width = 200_000;
        let height = 201;
        let err = store
            .import_bytes(&png_with_declared_dimensions(width, height))
            .unwrap_err();
        assert!(matches!(
            err,
            MediaError::TooManyPixels(pixels)
                if pixels == u64::from(width) * u64::from(height)
        ));
    }

    #[test]
    fn rejects_oversized_rgba_dimensions_before_copying() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let err = store.import_rgba(u32::MAX, u32::MAX, &[]).unwrap_err();
        assert!(matches!(err, MediaError::TooManyPixels(_)));
    }

    #[test]
    fn rejects_rgba_length_mismatch_before_copying() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let err = store.import_rgba(2, 2, &[0; 15]).unwrap_err();
        assert!(matches!(err, MediaError::Decode(message) if message.contains("size mismatch")));
    }

    #[test]
    fn load_base64_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let stored = store.import_bytes(&png_bytes(3, 3)).unwrap();
        let (mime, b64) = store.load_base64(&stored.sha256).unwrap();
        assert_eq!(mime, "image/png");
        assert!(!b64.is_empty());
    }

    #[test]
    fn import_base64_uses_the_encoded_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let png = png_bytes(1, 1);
        let encoded = BASE64.encode(&png);
        let stored = MediaStore::new(dir.path()).import_base64(&encoded).unwrap();
        assert_eq!(stored.mime_type, "image/png");
        assert_eq!(stored.width, 1);
        assert_eq!(stored.height, 1);
    }
}
