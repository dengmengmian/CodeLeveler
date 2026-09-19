//! `/skills` — the resolved skill registry as transcript notes.
//!
//! The listing names every skill with its scope, source and status, and says
//! what is wrong with an unusable one. The detail shows where a skill came from
//! — including one installed for another tool — so a user can tell why a name
//! resolves the way it does.

use leveler_skills::{SkillEntry, SkillRegistry, SkillScope, SkillSource, SkillState};

fn scope(scope: SkillScope) -> &'static str {
    scope.as_str()
}

/// The origin label: CodeLeveler's own store, or the compatible ecosystem a
/// skill was installed for.
fn source(source: SkillSource) -> &'static str {
    source.label()
}

fn status_suffix(entry: &SkillEntry) -> String {
    match &entry.state {
        SkillState::Available => String::new(),
        SkillState::Invalid(reason) => format!(" · INVALID: {reason}"),
    }
}

pub fn listing_note(registry: &SkillRegistry, t: &crate::i18n::UiText) -> String {
    let available = registry.entries().len();
    let mut lines = vec![format!("{} ({})", t.skills_title, available)];
    for entry in registry.entries() {
        let mut line = format!(
            "  ${} — {} [{} · {}]{}",
            entry.name,
            entry.description.as_deref().unwrap_or("—"),
            scope(entry.scope),
            source(entry.source),
            status_suffix(entry),
        );
        for shadow in &entry.shadowed {
            line.push_str(&format!(
                " · shadows {} {}",
                scope(shadow.scope),
                source(shadow.source)
            ));
        }
        lines.push(line);
    }
    if !registry.problems().is_empty() {
        lines.push(format!(
            "{} ({})",
            t.skills_not_loaded,
            registry.problems().len()
        ));
        for problem in registry.problems() {
            lines.push(format!(
                "  {}: {}",
                problem.location.display(),
                problem.error
            ));
        }
    }
    lines.push(t.skills_hint.to_string());
    lines.join("\n")
}

pub fn detail_note(registry: &SkillRegistry, name: &str, t: &crate::i18n::UiText) -> String {
    let Some(entry) = registry.get(name.trim()) else {
        return format!(
            "{} {name}: no such skill. {}",
            t.skills_title, t.skills_hint
        );
    };
    let mut lines = vec![format!(
        "Skill ${} [{} · {}]{}",
        entry.name,
        scope(entry.scope),
        source(entry.source),
        status_suffix(entry)
    )];
    if let Some(description) = &entry.description {
        lines.push(format!("  {description}"));
    }
    if let Some(location) = &entry.location {
        lines.push(format!("  {}: {}", t.agents_location, location.display()));
    } else {
        lines.push(format!("  {}: (built-in)", t.agents_location));
    }
    // The bundled files come from the loader, so an invalid skill reports its
    // reason instead of a file list.
    match registry.load_skill(&entry.name) {
        Ok(detail) => {
            if !detail.scripts.is_empty() {
                lines.push(format!("  scripts: {}", detail.scripts.join(", ")));
            }
            if !detail.references.is_empty() {
                lines.push(format!("  references: {}", detail.references.join(", ")));
            }
            if !detail.other_files.is_empty() {
                lines.push(format!("  files: {}", detail.other_files.join(", ")));
            }
        }
        Err(error) => lines.push(format!("  {error}")),
    }
    for shadow in &entry.shadowed {
        lines.push(format!(
            "  {}: {} {}{}",
            t.agents_shadows,
            scope(shadow.scope),
            source(shadow.source),
            shadow
                .error
                .as_ref()
                .map(|e| format!(" ({e})"))
                .unwrap_or_default()
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_skills::{SkillRoot, SkillRoots, SkillScope as Scope, SkillSource as Source};

    fn fixture() -> (tempfile::TempDir, SkillRegistry) {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("repo/.leveler/skills");
        let user = tmp.path().join("home/.leveler/skills");
        for (base, name, desc, body) in [
            (&project, "deploy", "Ship safely.", "BODY"),
            (&user, "deploy", "User deploy.", "U"),
        ] {
            let dir = base.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: {desc}\n---\n\n{body}\n"),
            )
            .unwrap();
        }
        let broken = project.join("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("SKILL.md"), "---\nname: [oops\n---\nx\n").unwrap();
        let mut roots = SkillRoots::empty();
        roots.push(SkillRoot {
            scope: Scope::Project,
            source: Source::Native,
            dir: project,
            containing_root: None,
        });
        roots.push(SkillRoot {
            scope: Scope::User,
            source: Source::Native,
            dir: user,
            containing_root: None,
        });
        (tmp, SkillRegistry::load(&roots))
    }

    #[test]
    fn listing_names_scope_source_status_and_shadowing() {
        let (_tmp, registry) = fixture();
        let note = listing_note(&registry, crate::i18n::Locale::En.text());
        assert!(note.contains("$deploy"), "{note}");
        assert!(note.contains("project · CodeLeveler"), "{note}");
        assert!(note.contains("shadows user"), "{note}");
        assert!(note.contains("INVALID"), "{note}");
        assert!(
            note.contains("builtin") || note.contains("Built-in"),
            "{note}"
        );
    }

    #[test]
    fn a_detail_names_the_origin_and_an_unknown_name_says_so() {
        let (_tmp, registry) = fixture();
        let t = crate::i18n::Locale::En.text();
        let detail = detail_note(&registry, "deploy", t);
        assert!(detail.contains("Skill $deploy"), "{detail}");
        assert!(detail.contains("project · CodeLeveler"), "{detail}");
        let missing = detail_note(&registry, "nope", t);
        assert!(missing.contains("no such skill"), "{missing}");
    }
}
