//! `leveler-media` — the image attachment pipeline (spec §41, §45).
//!
//! Imports an image safely: the real MIME is detected from content (never the
//! extension), the image is decoded and bounded by pixel count and byte size,
//! EXIF is stripped by re-encoding, oversized images are downscaled, and the
//! result is hashed and written to a content-addressed store. The processed
//! bytes — not the original path — are what later requests reference, so a
//! session's images survive the source file being moved or deleted.

#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

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
    #[error("attachment not found")]
    NotFound,
    #[error("invalid attachment id")]
    InvalidId,
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

/// The MIME type every processed image carries. Processing always re-encodes,
/// so a stored or request-bound image is always a PNG whatever came in.
pub const PROCESSED_MIME: &str = "image/png";

/// An image that has been through the import pipeline: bounded, decoded,
/// downscaled if needed, and re-encoded to PNG (which is what strips EXIF).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessedImage {
    /// The normalized PNG bytes.
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Validate and normalize source image bytes.
///
/// The ONE image pipeline: the real type comes from the CONTENT and never from
/// a filename (spec §45), the byte size and pixel count are bounded before the
/// decoder allocates anything, an oversized image is downscaled to
/// [`MAX_DIMENSION`], and the result is re-encoded to PNG — which is how EXIF
/// (camera model, GPS) stops reaching a provider.
///
/// Both callers go through here: [`MediaStore::import_bytes`], which then
/// hashes and stores the result, and the `view_image` tool, which base64-encodes
/// it for the next model request. `view_image` used to carry its own path —
/// extension-derived MIME, a byte cap and nothing else — so an image the model
/// was shown could be mislabelled, unbounded in pixels, and still carrying its
/// EXIF.
pub fn process_image(bytes: &[u8]) -> Result<ProcessedImage, MediaError> {
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
    let mut png = Vec::new();
    processed
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| MediaError::Decode(e.to_string()))?;

    Ok(ProcessedImage {
        png,
        width: processed.width(),
        height: processed.height(),
    })
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

    /// Import an image from raw bytes: process it through [`process_image`],
    /// then hash and store the result (deduplicating by hash).
    pub fn import_bytes(&self, bytes: &[u8]) -> Result<StoredImage, MediaError> {
        let processed = process_image(bytes)?;
        let sha256 = hex(&Sha256::digest(&processed.png));
        self.ensure_root()?;
        let path = self.root.join(format!("{sha256}.png"));
        self.store_object(&path, &processed.png, &sha256)?;

        Ok(StoredImage {
            sha256,
            path,
            mime_type: PROCESSED_MIME.to_string(),
            width: processed.width,
            height: processed.height,
            size_bytes: processed.png.len() as u64,
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
        let (mime, bytes) = self.load_bytes(sha256)?;
        Ok((mime, BASE64.encode(bytes)))
    }

    /// Store non-image bytes under their content hash. Images still go through
    /// [`Self::import_bytes`] so they stay decoded, bounded, and EXIF-stripped.
    ///
    /// The phone fetches by this same hash; this is not a second filesystem.
    pub fn put_blob(&self, bytes: &[u8], mime_type: &str) -> Result<(String, u64), MediaError> {
        let len = bytes.len() as u64;
        if len > MAX_IMAGE_BYTES {
            return Err(MediaError::TooLarge(len));
        }
        let sha256 = hex(&Sha256::digest(bytes));
        self.ensure_root()?;
        let path = self.root.join(&sha256);
        self.store_object(&path, bytes, &sha256)?;
        let mime = sanitize_mime(mime_type);
        self.write_text_if_changed(&self.root.join(format!("{sha256}.mime")), &mime)?;
        Ok((sha256, len))
    }

    /// Decode immutable base64 and [`Self::put_blob`].
    pub fn put_base64(&self, encoded: &str, mime_type: &str) -> Result<(String, u64), MediaError> {
        const MAX_ENCODED_BYTES: usize = (MAX_IMAGE_BYTES as usize).div_ceil(3) * 4;
        if encoded.len() > MAX_ENCODED_BYTES {
            return Err(MediaError::TooLarge(MAX_IMAGE_BYTES + 1));
        }
        let bytes = BASE64
            .decode(encoded)
            .map_err(|error| MediaError::Decode(format!("invalid base64 attachment: {error}")))?;
        self.put_blob(&bytes, mime_type)
    }

    /// Load processed bytes by content hash. `sha256` must be 64 lowercase hex
    /// characters — anything else is refused before it touches the filesystem.
    pub fn load_bytes(&self, sha256: &str) -> Result<(String, Vec<u8>), MediaError> {
        if !is_sha256_hex(sha256) {
            return Err(MediaError::InvalidId);
        }
        let png = self.root.join(format!("{sha256}.png"));
        if let Some(bytes) = read_regular_file(&png) {
            return Ok(("image/png".to_string(), bytes));
        }
        let blob = self.root.join(sha256);
        if let Some(bytes) = read_regular_file(&blob) {
            let mime = read_regular_file(&self.root.join(format!("{sha256}.mime")))
                .and_then(|raw| String::from_utf8(raw).ok())
                .map(|value| sanitize_mime(value.trim()))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "application/octet-stream".to_string());
            return Ok((mime, bytes));
        }
        Err(MediaError::NotFound)
    }

    /// Create the store root if needed and make it owner-only.
    ///
    /// A root that exists as anything but a real directory is refused rather
    /// than followed: writing through a symlinked root would put user images
    /// outside the store.
    fn ensure_root(&self) -> Result<(), MediaError> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if !metadata.is_dir() => {
                return Err(MediaError::Io("media root is not a directory".to_string()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(&self.root).map_err(|e| MediaError::Io(e.to_string()))?;
            }
            Err(error) => return Err(MediaError::Io(error.to_string())),
        }
        restrict_dir(&self.root).map_err(|e| MediaError::Io(e.to_string()))
    }

    /// Store a content-addressed object atomically and privately.
    ///
    /// An existing regular object is trusted only after its bytes still hash to
    /// the name; a truncated object from an older non-atomic writer is repaired
    /// instead of being served forever. A symlink or directory under the object
    /// name is refused, never followed or replaced.
    fn store_object(
        &self,
        path: &Path,
        bytes: &[u8],
        expected_sha256: &str,
    ) -> Result<(), MediaError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => {
                if let Ok(existing) = fs::read(path) {
                    if hex(&Sha256::digest(&existing)) == expected_sha256 {
                        restrict_file(path).map_err(|e| MediaError::Io(e.to_string()))?;
                        return Ok(());
                    }
                }
            }
            Ok(_) => {
                return Err(MediaError::Io(
                    "refusing a non-regular media object".to_string(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(MediaError::Io(error.to_string())),
        }
        self.write_atomic(path, bytes)
    }

    fn write_text_if_changed(&self, path: &Path, value: &str) -> Result<(), MediaError> {
        if let Some(existing) = read_regular_file(path) {
            if existing == value.as_bytes() {
                restrict_file(path).map_err(|e| MediaError::Io(e.to_string()))?;
                return Ok(());
            }
        }
        self.write_atomic(path, value.as_bytes())
    }

    /// Write a sibling temp file, fsync it, then rename it over the target. The
    /// rename is the publish point, so a reader never observes a partial object
    /// even if this process dies mid-import.
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> Result<(), MediaError> {
        let parent = path
            .parent()
            .ok_or_else(|| MediaError::Io("media object has no parent directory".to_string()))?;
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| MediaError::Io("media object name is not valid UTF-8".to_string()))?;
        let mut temp = None;
        for _ in 0..8 {
            let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(".{name}.{}.{nonce}.tmp", std::process::id()));
            let options = private_create_options();
            match options.open(&candidate) {
                Ok(file) => {
                    temp = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(MediaError::Io(error.to_string())),
            }
        }
        let Some((temp_path, mut file)) = temp else {
            return Err(MediaError::Io(
                "could not create a unique media temp file".to_string(),
            ));
        };
        let written = (|| -> std::io::Result<()> {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            restrict_file(&temp_path)?;
            fs::rename(&temp_path, path)?;
            restrict_file(path)?;
            Ok(())
        })();
        if let Err(error) = written {
            let _ = fs::remove_file(&temp_path);
            return Err(MediaError::Io(error.to_string()));
        }
        Ok(())
    }
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn private_create_options() -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

#[cfg(unix)]
fn restrict_file(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_file(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn restrict_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn restrict_dir(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Read a path only when it is a regular file. `Path::is_file` follows a
/// symlink; a planted link inside the store must not become readable media.
fn read_regular_file(path: &Path) -> Option<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    fs::read(path).ok()
}

/// Age and size policy for [`MediaStore::sweep`].
///
/// Deletion is reference-aware: only objects absent from the caller's live set
/// are candidates, and only after `min_age`. `max_bytes` is a soft budget for
/// the whole store; reachable objects are never deleted to satisfy it, so an
/// over-budget report asks the user to remove sessions instead of silently
/// discarding history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepPolicy {
    pub max_bytes: Option<u64>,
    pub min_age: Duration,
}

impl Default for SweepPolicy {
    fn default() -> Self {
        Self {
            // Enough for a large active workspace; not a hard cap on history.
            max_bytes: Some(2 * 1024 * 1024 * 1024),
            // A turn may hold a just-imported object before its message row is
            // durable. One day is far longer than any single import window.
            min_age: Duration::from_secs(24 * 60 * 60),
        }
    }
}

/// What one [`MediaStore::sweep`] removed, kept, or failed to remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Bytes of objects present in the caller's live set.
    pub live_bytes: u64,
    /// Bytes of unreachable objects kept because they are younger than the
    /// grace window.
    pub unreferenced_kept_bytes: u64,
    /// Objects fully removed by this sweep.
    pub removed_objects: u64,
    /// Bytes removed by this sweep.
    pub removed_bytes: u64,
    /// Files under the store root that are not content-addressed objects,
    /// including symlinks. They are reported, never deleted.
    pub unknown_files: u64,
    /// Per-object removal failures. A failure is reported, never counted as a
    /// successful removal; the next sweep retries it.
    pub failures: Vec<String>,
    /// Bytes left under the root after the sweep.
    pub remaining_bytes: u64,
    /// True when `remaining_bytes` exceeds `max_bytes`. Reachable data is never
    /// deleted to clear this.
    pub over_budget: bool,
}

impl MediaStore {
    /// Delete store objects that no live session references and that are older
    /// than the grace window. Unknown files and symlinks are left untouched.
    ///
    /// The caller's `live` set must be an authoritative snapshot taken with no
    /// turn that could add a reference. The runtime runs this before accepting
    /// work; a failed reference query must skip the sweep rather than pass an
    /// empty set.
    pub fn sweep(
        &self,
        live: &HashSet<String>,
        now: SystemTime,
        policy: SweepPolicy,
    ) -> Result<SweepReport, MediaError> {
        let root_metadata = match fs::symlink_metadata(&self.root) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(SweepReport::default());
            }
            Err(error) => return Err(MediaError::Io(error.to_string())),
        };
        if !root_metadata.is_dir() {
            return Err(MediaError::Io("media root is not a directory".to_string()));
        }
        restrict_dir(&self.root).map_err(|e| MediaError::Io(e.to_string()))?;

        let mut objects: HashMap<String, SweepObject> = HashMap::new();
        let mut temp_files: Vec<(PathBuf, u64, SystemTime)> = Vec::new();
        let mut unknown_files = 0u64;
        let mut total_bytes = 0u64;
        let entries = fs::read_dir(&self.root).map_err(|e| MediaError::Io(e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| MediaError::Io(e.to_string()))?;
            let path = entry.path();
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(MediaError::Io(error.to_string())),
            };
            let size = metadata.len();
            total_bytes = total_bytes.saturating_add(size);
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                unknown_files += 1;
                continue;
            };
            if is_temp_object_name(&name) {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                temp_files.push((path, size, modified));
                continue;
            }
            if !metadata.file_type().is_file() {
                unknown_files += 1;
                continue;
            }
            let Some(hash) = object_hash(&name) else {
                unknown_files += 1;
                continue;
            };
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            let object = objects
                .entry(hash.to_owned())
                .or_insert_with(|| SweepObject {
                    files: Vec::new(),
                    size: 0,
                    newest: SystemTime::UNIX_EPOCH,
                });
            object.size = object.size.saturating_add(size);
            if modified > object.newest {
                object.newest = modified;
            }
            object.files.push((path, size, modified));
        }

        let mut report = SweepReport {
            unknown_files,
            ..SweepReport::default()
        };
        // Existing objects may predate the private-mode writer. Tighten them
        // even when they are live; never follow a symlink (not in `objects`).
        for (path, _, _) in objects.values().flat_map(|object| &object.files) {
            if let Err(error) = restrict_file(path) {
                report
                    .failures
                    .push(format!("restrict {}: {error}", path.display()));
            }
        }

        let mut vanished_bytes = 0u64;
        for (hash, object) in &objects {
            if live.contains(hash) {
                report.live_bytes = report.live_bytes.saturating_add(object.size);
                continue;
            }
            let age = now.duration_since(object.newest).unwrap_or(Duration::ZERO);
            if age < policy.min_age {
                report.unreferenced_kept_bytes =
                    report.unreferenced_kept_bytes.saturating_add(object.size);
                continue;
            }
            // Remove companions first; the content object itself last. A
            // partially removed object is retried by the next sweep.
            let mut files = object.files.clone();
            files.sort_by_key(|(path, _, _)| {
                if path.extension().and_then(|ext| ext.to_str()) == Some("mime") {
                    0usize
                } else {
                    1usize
                }
            });
            let mut all_removed = true;
            for (path, size, _) in files {
                match fs::remove_file(&path) {
                    Ok(()) => {
                        report.removed_bytes = report.removed_bytes.saturating_add(size);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        vanished_bytes = vanished_bytes.saturating_add(size);
                    }
                    Err(error) => {
                        all_removed = false;
                        report
                            .failures
                            .push(format!("remove {}: {error}", path.display()));
                    }
                }
            }
            if all_removed {
                report.removed_objects += 1;
            }
        }

        for (path, size, modified) in temp_files {
            let age = now.duration_since(modified).unwrap_or(Duration::ZERO);
            if age < policy.min_age {
                continue;
            }
            match fs::remove_file(&path) {
                Ok(()) => {
                    report.removed_bytes = report.removed_bytes.saturating_add(size);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    vanished_bytes = vanished_bytes.saturating_add(size);
                }
                Err(error) => report
                    .failures
                    .push(format!("remove {}: {error}", path.display())),
            }
        }

        report.remaining_bytes = total_bytes
            .saturating_sub(report.removed_bytes)
            .saturating_sub(vanished_bytes);
        report.over_budget = policy
            .max_bytes
            .is_some_and(|max| report.remaining_bytes > max);
        Ok(report)
    }
}

struct SweepObject {
    files: Vec<(PathBuf, u64, SystemTime)>,
    size: u64,
    newest: SystemTime,
}

/// The content hash a store-owned file belongs to.
fn object_hash(name: &str) -> Option<&str> {
    if is_sha256_hex(name) {
        return Some(name);
    }
    for suffix in [".png", ".mime"] {
        if let Some(hash) = name.strip_suffix(suffix)
            && is_sha256_hex(hash)
        {
            return Some(hash);
        }
    }
    None
}

/// True only for the temp names [`MediaStore::write_atomic`] creates. Anything
/// else that merely ends in `.tmp` is unknown and left alone.
fn is_temp_object_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = rest.split('.');
    let (Some(hash), Some(pid), Some(nonce), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    is_sha256_hex(hash)
        && !pid.is_empty()
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && !nonce.is_empty()
        && nonce.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn sanitize_mime(mime: &str) -> String {
    let trimmed = mime.trim();
    if trimmed.len() <= 80
        && trimmed.bytes().all(
            |b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'/' | b'.' | b'+' | b'-'),
        )
        && trimmed.contains('/')
    {
        trimmed.to_ascii_lowercase()
    } else {
        "application/octet-stream".to_string()
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

    #[test]
    fn put_blob_roundtrips_markdown_by_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let body = b"# review\n\npass\n";
        let (sha, size) = store.put_blob(body, "text/markdown").unwrap();
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha.len(), 64);
        let (mime, loaded) = store.load_bytes(&sha).unwrap();
        assert_eq!(mime, "text/markdown");
        assert_eq!(loaded, body);
    }

    #[test]
    fn load_bytes_refuses_path_traversal_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let err = store.load_bytes("../passwd").unwrap_err();
        assert!(matches!(err, MediaError::InvalidId));
        let err = store.load_bytes("abc").unwrap_err();
        assert!(matches!(err, MediaError::InvalidId));
    }

    #[test]
    fn missing_hash_is_not_found_not_an_io_path() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let sha = "ab".repeat(32);
        let err = store.load_bytes(&sha).unwrap_err();
        assert!(matches!(err, MediaError::NotFound));
    }

    fn live(hashes: &[&str]) -> HashSet<String> {
        hashes.iter().map(|hash| (*hash).to_owned()).collect()
    }

    fn age_file(path: &Path, age: Duration) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(SystemTime::now() - age))
            .unwrap();
    }

    fn policy(min_age_secs: u64, max_bytes: Option<u64>) -> SweepPolicy {
        SweepPolicy {
            max_bytes,
            min_age: Duration::from_secs(min_age_secs),
        }
    }

    #[test]
    fn sweep_removes_unreferenced_objects_after_grace_and_keeps_live_ones() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let kept = store.import_bytes(&png_bytes(2, 2)).unwrap();
        let removed = store.import_bytes(&png_bytes(3, 2)).unwrap();
        age_file(&kept.path, Duration::from_secs(3600));
        age_file(&removed.path, Duration::from_secs(3600));

        let report = store
            .sweep(
                &live(&[kept.sha256.as_str()]),
                SystemTime::now(),
                policy(60, None),
            )
            .unwrap();

        assert!(kept.path.exists());
        assert!(!removed.path.exists());
        assert_eq!(report.removed_objects, 1);
        assert_eq!(report.removed_bytes, removed.size_bytes);
        assert_eq!(report.live_bytes, kept.size_bytes);
        assert!(report.failures.is_empty());
        assert!(!report.over_budget);
    }

    #[test]
    fn sweep_keeps_unreferenced_objects_inside_the_grace_window() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let recent = store.import_bytes(&png_bytes(4, 4)).unwrap();

        let report = store
            .sweep(&HashSet::new(), SystemTime::now(), policy(3600, None))
            .unwrap();

        assert!(recent.path.exists());
        assert_eq!(report.removed_objects, 0);
        assert_eq!(report.unreferenced_kept_bytes, recent.size_bytes);
    }

    #[test]
    fn sweep_never_deletes_reachable_objects_to_meet_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let live_image = store.import_bytes(&png_bytes(5, 4)).unwrap();
        age_file(&live_image.path, Duration::from_secs(3600));

        let report = store
            .sweep(
                &live(&[live_image.sha256.as_str()]),
                SystemTime::now(),
                policy(60, Some(1)),
            )
            .unwrap();

        assert!(live_image.path.exists());
        assert_eq!(report.removed_objects, 0);
        assert!(
            report.over_budget,
            "the report must not hide an over-budget store"
        );
    }

    #[test]
    fn sweep_removes_an_orphaned_mime_companion() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let (sha, _) = store.put_blob(b"# note\n", "text/markdown").unwrap();
        let blob = dir.path().join(&sha);
        let mime = dir.path().join(format!("{sha}.mime"));
        fs::remove_file(&blob).unwrap();
        age_file(&mime, Duration::from_secs(3600));

        let report = store
            .sweep(&HashSet::new(), SystemTime::now(), policy(60, None))
            .unwrap();

        assert!(!mime.exists());
        assert_eq!(report.removed_objects, 1);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn sweep_leaves_unknown_files_and_symlinks_alone() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        fs::create_dir_all(dir.path()).unwrap();
        let notes = dir.path().join("notes.txt");
        fs::write(&notes, b"user data").unwrap();
        let outside = dir.path().join("outside.bin");
        fs::write(&outside, b"do not read through me").unwrap();
        let link = dir.path().join(format!("{}.png", "cd".repeat(32)));
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        let report = store
            .sweep(&HashSet::new(), SystemTime::now(), policy(0, None))
            .unwrap();

        assert!(notes.exists());
        #[cfg(unix)]
        {
            assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
            assert_eq!(fs::read(&outside).unwrap(), b"do not read through me");
            let err = store.load_bytes(&"cd".repeat(32)).unwrap_err();
            assert!(matches!(err, MediaError::NotFound));
        }
        assert!(report.unknown_files >= 1);
    }

    #[test]
    fn sweep_removes_its_own_stale_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let temp = dir
            .path()
            .join(format!(".{}.{}.{}.tmp", "ef".repeat(32), 123, 7));
        fs::write(&temp, b"partial").unwrap();
        age_file(&temp, Duration::from_secs(3600));

        let report = store
            .sweep(&HashSet::new(), SystemTime::now(), policy(60, None))
            .unwrap();

        assert!(!temp.exists());
        assert_eq!(report.removed_bytes, 7);
    }

    #[test]
    fn sweep_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let kept = store.import_bytes(&png_bytes(2, 2)).unwrap();
        let removed = store.import_bytes(&png_bytes(6, 4)).unwrap();
        age_file(&kept.path, Duration::from_secs(3600));
        age_file(&removed.path, Duration::from_secs(3600));
        let live = live(&[kept.sha256.as_str()]);
        let first = store
            .sweep(&live, SystemTime::now(), policy(60, None))
            .unwrap();
        let second = store
            .sweep(&live, SystemTime::now(), policy(60, None))
            .unwrap();

        assert_eq!(first.removed_objects, 1);
        assert_eq!(second.removed_objects, 0);
        assert_eq!(first.remaining_bytes, second.remaining_bytes);
    }

    #[test]
    fn import_repairs_a_truncated_object_under_the_same_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        let source = png_bytes(7, 5);
        let stored = store.import_bytes(&source).unwrap();
        fs::write(&stored.path, b"truncated").unwrap();

        let again = store.import_bytes(&source).unwrap();

        assert_eq!(again.sha256, stored.sha256);
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = MediaStore::new(fresh_dir.path())
            .import_bytes(&source)
            .unwrap();
        // The processed PNG is deterministic, so the repaired object must be
        // exactly the bytes a fresh import produces.
        assert_eq!(
            fs::read(&again.path).unwrap(),
            fs::read(&fresh.path).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn imported_images_and_their_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("media");
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
        let store = MediaStore::new(&root);
        let stored = store.import_bytes(&png_bytes(3, 3)).unwrap();

        assert_eq!(
            fs::metadata(&stored.path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn concurrent_imports_leave_one_complete_object() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("media");
        let store = std::sync::Arc::new(MediaStore::new(&root));
        let source = std::sync::Arc::new(png_bytes(9, 6));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let store = store.clone();
                let source = source.clone();
                std::thread::spawn(move || store.import_bytes(&source).unwrap())
            })
            .collect();
        let hashes: HashSet<String> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap().sha256)
            .collect();

        assert_eq!(hashes.len(), 1);
        let (_, bytes) = store.load_bytes(hashes.iter().next().unwrap()).unwrap();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let object_files = fs::read_dir(&root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".png"))
            .count();
        assert_eq!(object_files, 1);
    }
}
