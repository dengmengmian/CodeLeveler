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

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// Path to the image file, relative to the repository root.
    path: String,
}

pub struct ViewImageTool;

#[async_trait]
impl Tool for ViewImageTool {
    fn name(&self) -> &'static str {
        "view_image"
    }

    fn description(&self) -> &'static str {
        "Load an image file from the workspace into the conversation so you can \
         see it (screenshots, diagrams, mockups). Provide a path relative to the \
         repository root. Supports png/jpg/gif/webp."
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

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let path = context.execution.workspace.resolve_for_read(&input.path)?;
        // Check the size before reading, so a huge file is rejected instead of
        // pulled fully into memory first.
        match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.len() > MAX_BYTES as u64 => {
                return Ok(ToolOutput::error(format!(
                    "图片过大({} KB),上限 {} KB。",
                    meta.len() / 1024,
                    MAX_BYTES / 1024
                )));
            }
            Ok(_) => {}
            Err(e) => return Ok(ToolOutput::error(format!("读取图片失败:{e}"))),
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::error(format!("读取图片失败:{e}"))),
        };
        let processed = match leveler_media::process_image(&bytes) {
            Ok(processed) => processed,
            Err(leveler_media::MediaError::Unsupported(kind)) => {
                return Ok(ToolOutput::error(format!(
                    "不支持的图片格式({kind};支持 png/jpg/jpeg/gif/webp)。"
                )));
            }
            Err(error) => return Ok(ToolOutput::error(format!("读取图片失败:{error}"))),
        };
        let data = base64::engine::general_purpose::STANDARD.encode(&processed.png);
        Ok(ToolOutput::ok(format!(
            "已加载图片 {} ({}×{}, {} KB)。",
            input.path,
            processed.width,
            processed.height,
            processed.png.len() / 1024
        ))
        .with_metadata(serde_json::json!({
            "image": {
                "media_type": leveler_media::PROCESSED_MIME,
                "data": data,
            }
        })))
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
