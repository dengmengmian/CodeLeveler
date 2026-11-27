//! `view_image` — load an image file from the workspace into the conversation
//! so a vision-capable model can see it.
//!
//! The bytes go through [`leveler_media::process_image`], the same pipeline
//! that backs the attachment store: the real type comes from the CONTENT (a
//! JPEG named `.png` is a JPEG), the pixel count and byte size are bounded
//! before the decoder allocates, and the re-encode to PNG is what strips EXIF.
//! This tool used to answer all three questions itself — MIME from the
//! extension, a byte cap and nothing else — which meant the model could be
//! shown a mislabelled image and the provider could be handed its GPS tags.

use async_trait::async_trait;
use base64::Engine;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Refuse source files larger than this before reading them. Base64 inflates
/// ~33% and providers cap the request size, so this is tighter than the
/// attachment store's own ceiling.
const MAX_BYTES: usize = 5 * 1024 * 1024;
/// A single call may load several pictures; each is bounded on its own.
const MAX_IMAGES: usize = 8;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to one image file, relative to the repository root.
    #[serde(default)]
    path: Option<String>,
    /// Paths to several image files, relative to the repository root. Use
    /// either this or `path`, not both.
    #[serde(default)]
    paths: Option<Vec<String>>,
}

pub struct ViewImageTool;

struct LoadedImage {
    path: String,
    png: Vec<u8>,
    width: u32,
    height: u32,
}

impl ViewImageTool {
    /// Read and normalize one source image.
    ///
    /// The bytes are opened under the workspace's confinement decision, so
    /// what is read is what was authorised even if the path is swapped after
    /// resolution. The size check happens before the read so a huge file is
    /// rejected rather than pulled into memory first.
    async fn load_one(&self, path: &str, context: &ToolContext) -> Result<LoadedImage, String> {
        if let Some(reason) = context.security_control_read_denied(path) {
            return Err(reason);
        }
        if let Some(reason) = context.skill_content_read_denied(path) {
            return Err(reason);
        }
        let workspace = match context.execution_workspace() {
            Ok(workspace) => workspace,
            Err(error) => return Err(format!("读取图片失败:{error}")),
        };
        let (_path, file) = match workspace.open_read(path) {
            Ok(opened) => opened,
            Err(leveler_execution::WorkspaceError::ReadOpen {
                kind: std::io::ErrorKind::NotFound,
                message,
                ..
            }) => {
                return Err(format!("读取图片失败:{message}"));
            }
            Err(error) => return Err(format!("读取图片失败:{error}")),
        };
        let file = tokio::fs::File::from_std(file);
        match file.metadata().await {
            Ok(meta) if meta.len() > MAX_BYTES as u64 => {
                return Err(format!(
                    "图片过大({} KB),上限 {} KB。",
                    meta.len() / 1024,
                    MAX_BYTES / 1024
                ));
            }
            Ok(_) => {}
            Err(e) => return Err(format!("读取图片失败:{e}")),
        }
        let bytes = {
            use tokio::io::AsyncReadExt;
            let mut bytes = Vec::new();
            let mut file = file;
            file.read_to_end(&mut bytes)
                .await
                .map_err(|e| format!("读取图片失败:{e}"))?;
            bytes
        };
        let processed = match leveler_media::process_image(&bytes) {
            Ok(processed) => processed,
            Err(leveler_media::MediaError::Unsupported(kind)) => {
                return Err(format!(
                    "不支持的图片格式({kind};支持 png/jpg/jpeg/gif/webp)。"
                ));
            }
            Err(error) => return Err(format!("读取图片失败:{error}")),
        };
        Ok(LoadedImage {
            path: path.to_string(),
            png: processed.png,
            width: processed.width,
            height: processed.height,
        })
    }
}

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &'static str {
        "view_image"
    }

    fn description(&self) -> &'static str {
        "Load image files from the workspace into the conversation so you can \
         see them (screenshots, diagrams, mockups). Provide `path` for one image \
         or `paths` for several, relative to the repository root. Supports \
         png/jpg/gif/webp."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    /// Pure in-process file read: no write, no subprocess, no language
    /// server, no network. Re-running it after a crash changes nothing.
    fn replay_is_side_effect_free(&self) -> bool {
        true
    }

    fn approval_reason(&self, input: &serde_json::Value, context: &ToolContext) -> Option<String> {
        let paths: Vec<&str> = input
            .get("paths")
            .and_then(serde_json::Value::as_array)
            .map(|list| list.iter().filter_map(serde_json::Value::as_str).collect())
            .unwrap_or_else(|| {
                input
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .into_iter()
                    .collect()
            });
        paths
            .into_iter()
            .find_map(|path| crate::workspace::path_approval_reason(context, path, false))
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let requested: Vec<String> = match (input.path, input.paths) {
            (Some(path), None) => vec![path],
            (None, Some(paths)) if !paths.is_empty() => paths,
            _ => {
                return Ok(ToolOutput::error(
                    "请提供 path(单个图片)或 paths(多个图片)之一。".to_string(),
                ));
            }
        };
        if requested.len() > MAX_IMAGES {
            return Ok(ToolOutput::error(format!(
                "一次最多加载 {MAX_IMAGES} 张图片。"
            )));
        }
        let mut loaded = Vec::with_capacity(requested.len());
        for path in &requested {
            match self.load_one(path, &context).await {
                Ok(image) => loaded.push(image),
                Err(reason) => return Ok(ToolOutput::error(reason)),
            }
        }
        let summary = loaded
            .iter()
            .map(|image| {
                format!(
                    "{} ({}×{}, {} KB)",
                    image.path,
                    image.width,
                    image.height,
                    image.png.len() / 1024
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        let mut metadata_images = Vec::with_capacity(loaded.len());
        for image in &loaded {
            metadata_images.push(serde_json::json!({
                "media_type": leveler_media::PROCESSED_MIME,
                "data": base64::engine::general_purpose::STANDARD.encode(&image.png),
            }));
        }
        let mut metadata = serde_json::json!({ "images": metadata_images });
        // Older consumers read the singular key. One image keeps both shapes;
        // several are only expressible as the list.
        if loaded.len() == 1 {
            metadata["image"] = metadata_images[0].clone();
        }
        Ok(ToolOutput::ok(format!("已加载图片 {summary}。")).with_metadata(metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_in(dir: &std::path::Path) -> ToolContext {
        let ws = leveler_execution::Workspace::new(dir).unwrap();
        ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval)
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "leveler-view-img-{tag}-{}",
            super::super::test_ordinal()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A one-pixel PNG, produced by the same encoder the tool normalizes with.
    fn tiny_png() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    #[tokio::test]
    async fn rejects_a_file_that_is_not_an_image() {
        let dir = scratch("unsupported");
        std::fs::write(dir.join("diagram.svg"), "<svg/>").unwrap();
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "diagram.svg"}),
                ctx_in(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("不支持的图片格式"), "{}", out.content);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The media type the model is told comes from the bytes, not the name: a
    /// PNG saved as `.jpg` is still declared as what it actually is.
    #[tokio::test]
    async fn the_media_type_comes_from_the_content_not_the_extension() {
        let dir = scratch("sniff");
        std::fs::write(dir.join("mislabelled.jpg"), tiny_png()).unwrap();
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "mislabelled.jpg"}),
                ctx_in(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(
            out.metadata
                .pointer("/image/media_type")
                .and_then(|v| v.as_str()),
            Some("image/png"),
            "the extension said jpeg; the bytes say png: {:?}",
            out.metadata
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Several pictures in one call are one result with an ordered list; the
    /// executor hands every one to the model and archives every one.
    #[tokio::test]
    async fn loads_several_images_in_one_call() {
        let dir = scratch("many");
        std::fs::write(dir.join("a.png"), tiny_png()).unwrap();
        std::fs::write(dir.join("b.png"), tiny_png()).unwrap();
        let out = ViewImageTool
            .execute(
                serde_json::json!({"paths": ["a.png", "b.png"]}),
                ctx_in(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("a.png") && out.content.contains("b.png"));
        let images = out
            .metadata
            .get("images")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(images.len(), 2);
        assert!(images[0].get("data").and_then(|v| v.as_str()).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn refuses_a_call_that_names_no_image() {
        let dir = scratch("empty");
        let out = ViewImageTool
            .execute(
                serde_json::json!({}),
                ctx_in(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("请提供"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let dir = scratch("missing");
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "missing.png"}),
                ctx_in(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("读取图片失败"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn rejects_oversized_image() {
        let dir = scratch("oversized");
        std::fs::write(dir.join("huge.png"), vec![0u8; MAX_BYTES + 1]).unwrap();
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "huge.png"}),
                ctx_in(&dir),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("图片过大"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
