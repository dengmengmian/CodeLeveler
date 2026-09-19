//! `SKILL.md` frontmatter: parse it or say precisely why it is unusable.
//!
//! Deliberately not `unwrap_or_default`: a skill whose frontmatter failed to
//! parse is not "a skill with no description", and silently treating it as one
//! is how a broken package keeps working just enough to mislead.

use serde::Deserialize;

/// Frontmatter fields this runtime understands. Unknown keys (a compatible
/// tool's extension fields) are ignored, so a package written elsewhere still
/// loads.
#[derive(Debug, Default, Deserialize)]
pub struct Frontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
}

impl Frontmatter {
    /// The declared name, trimmed, when it is present and non-empty.
    pub fn name(&self) -> Option<&str> {
        self.name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// The declared description, with leading/trailing whitespace removed,
    /// when it is present and non-empty.
    pub fn description(&self) -> Option<String> {
        self.description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| d.to_string())
    }
}

/// A parsed `SKILL.md`: frontmatter plus the Markdown body.
#[derive(Debug)]
pub struct Parsed {
    pub frontmatter: Frontmatter,
    pub body: String,
}

/// Split the leading `---`-delimited YAML block from the body.
pub fn parse(content: &str) -> Result<Parsed, String> {
    let normalized = content.replace("\r\n", "\n");
    let rest = normalized.strip_prefix("---\n").ok_or_else(|| {
        "SKILL.md must start with a `---`-delimited YAML frontmatter block".to_string()
    })?;
    let end = rest
        .find("\n---")
        .ok_or_else(|| "SKILL.md frontmatter is not closed with `---`".to_string())?;
    let yaml = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\n', ' ']).to_string();
    let frontmatter: Frontmatter = serde_yaml::from_str(yaml)
        .map_err(|error| format!("SKILL.md frontmatter is not valid YAML: {error}"))?;
    Ok(Parsed { frontmatter, body })
}

/// Render a `SKILL.md` for a validated, single-line description and body.
pub fn render(name: &str, description: &str, body: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
        description.replace('\n', " ").trim(),
        body.trim()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let parsed =
            parse("---\nname: deploy\ndescription: Ship it.\nextra: x\n---\n\n# Body\n").unwrap();
        assert_eq!(parsed.frontmatter.name(), Some("deploy"));
        assert_eq!(
            parsed.frontmatter.description().as_deref(),
            Some("Ship it.")
        );
        assert_eq!(parsed.body, "# Body\n");
    }

    #[test]
    fn missing_frontmatter_is_an_error_not_a_default() {
        assert!(parse("# just markdown\n").is_err());
    }

    #[test]
    fn invalid_yaml_is_reported() {
        let error = parse("---\nname: [unterminated\n---\nbody\n").unwrap_err();
        assert!(error.contains("not valid YAML"), "{error}");
    }

    #[test]
    fn carriage_returns_are_normalized() {
        let parsed =
            parse("---\r\nname: deploy\r\ndescription: Ship it.\r\n---\r\n\r\nBody\r\n").unwrap();
        assert_eq!(parsed.frontmatter.name(), Some("deploy"));
        assert_eq!(parsed.body, "Body\n");
    }
}
