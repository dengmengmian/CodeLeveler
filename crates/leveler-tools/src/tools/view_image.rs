//! `view_image` — load an image file from the workspace into the conversation
//! so a vision-capable model can see it. The tool base64-encodes the file and
//! hands it back through `metadata.image`; the executor turns that into an
//! `ContentPart::Image` in the next request.

use async_trait::async_trait;
use base64::Engine;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;

use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

/// Refuse images larger than this (base64 inflates ~33%, and providers cap the
/// request size).
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

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let Some(media_type) = media_type_for(&input.path) else {
            return Ok(ToolOutput::error(
                "不支持的图片格式(支持 png/jpg/jpeg/gif/webp)。",
            ));
        };
        let path = context.workspace.resolve_read(&input.path)?;
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
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        Ok(ToolOutput::ok(format!(
            "已加载图片 {} ({} KB)。",
            input.path,
            bytes.len() / 1024
        ))
        .with_metadata(serde_json::json!({
            "image": { "media_type": media_type, "data": data }
        })))
    }
}

/// The MIME type for a supported image extension, or `None` if unsupported.
fn media_type_for(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    match ext.as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_extensions() {
        assert_eq!(media_type_for("a/b.png"), Some("image/png"));
        assert_eq!(media_type_for("shot.JPG"), Some("image/jpeg"));
        assert_eq!(media_type_for("x.webp"), Some("image/webp"));
        assert_eq!(media_type_for("notes.txt"), None);
        assert_eq!(media_type_for("noext"), None);
    }

    #[tokio::test]
    async fn rejects_unsupported_extension() {
        let dir =
            std::env::temp_dir().join(format!("leveler-view-img-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "diagram.svg"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("不支持的图片格式"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn reports_missing_file() {
        let dir =
            std::env::temp_dir().join(format!("leveler-view-img-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "missing.png"}),
                ctx,
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
        let dir =
            std::env::temp_dir().join(format!("leveler-view-img-{}", super::super::test_ordinal()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("huge.png"), vec![0u8; MAX_BYTES + 1]).unwrap();
        let ws = leveler_execution::Workspace::new(&dir).unwrap();
        let ctx = ToolContext::new(ws, leveler_execution::PermissionProfile::RequestApproval);
        let out = ViewImageTool
            .execute(
                serde_json::json!({"path": "huge.png"}),
                ctx,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(out.is_error);
        assert!(out.content.contains("图片过大"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
