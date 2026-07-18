//! The context package and compiler (spec §26): assemble a bounded, relevant
//! slice of the repository for the planner and executor.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::repo_map::RepositoryMap;
use crate::rules::{ProjectInstruction, load_rules_for_paths, render_instructions};

const MAX_CANDIDATES: usize = 10;
const MAX_SCAN_FILES: usize = 400;
const MAX_FILE_SCAN_BYTES: usize = 64 * 1024;

/// The compiled context handed to planning and execution (spec §26.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPackage {
    pub task_summary: String,
    pub project_summary: String,
    pub instructions: Vec<ProjectInstruction>,
    pub candidate_files: Vec<String>,
    pub related_tests: Vec<String>,
    pub repo_map: String,
    pub estimated_tokens: u32,
    /// Available skills (name + description only; body is loaded on demand).
    #[serde(default)]
    pub skills: Vec<leveler_skills::SkillSummary>,
}

impl ContextPackage {
    /// Render the package as a compact prompt block for the model.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("Project: {}\n", self.project_summary));
        if !self.instructions.is_empty() {
            s.push_str("\nProject rules:\n");
            s.push_str(&render_instructions(&self.instructions));
        }
        if !self.skills.is_empty() {
            s.push('\n');
            s.push_str(&leveler_skills::render_index(&self.skills));
        }
        if !self.candidate_files.is_empty() {
            s.push_str("\nLikely relevant files:\n");
            for f in &self.candidate_files {
                s.push_str(&format!("- {f}\n"));
            }
        }
        if !self.related_tests.is_empty() {
            s.push_str("\nRelated tests:\n");
            for f in &self.related_tests {
                s.push_str(&format!("- {f}\n"));
            }
        }
        s.push_str("\nRepository files:\n");
        s.push_str(&self.repo_map);
        s
    }
}

/// Builds a [`ContextPackage`] for a task, using only cheap filesystem scans
/// (spec §26.3 first-stage strategy: no AST index yet).
pub struct ContextCompiler;

impl ContextCompiler {
    /// Compile context for `goal`, rooted at `root`.
    pub fn compile(root: &Path, goal: &str) -> ContextPackage {
        let repo_map = RepositoryMap::build(root);
        let skills = leveler_skills::discover(root);

        let terms = GoalTerms::from_goal(goal);
        let candidate_files = select_candidates(root, &repo_map, &terms);
        let related_tests = related_tests(root, &repo_map, &candidate_files, &terms);
        let instructions = load_rules_for_paths(root, &candidate_files);

        let project_summary = format!(
            "{} file(s), languages: {}",
            repo_map.files.len(),
            if repo_map.languages.is_empty() {
                "unknown".to_string()
            } else {
                repo_map
                    .languages
                    .iter()
                    .map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        );

        let rendered_map = repo_map.render();
        let estimated_tokens = estimate_tokens(&rendered_map, &instructions);

        ContextPackage {
            task_summary: goal.to_string(),
            project_summary,
            instructions,
            candidate_files,
            related_tests,
            repo_map: rendered_map,
            estimated_tokens,
            skills,
        }
    }
}

/// Extract lowercase keywords from a goal, dropping short/common words.
fn keywords(goal: &str) -> Vec<String> {
    const STOP: &[&str] = &[
        "the", "and", "for", "with", "that", "this", "add", "fix", "make", "use", "into", "from",
        "when", "then", "should", "must", "not", "function", "method", "file", "code", "please",
        "a", "an", "to", "of", "in", "is", "it", "on",
    ];
    let mut seen = Vec::new();
    for raw in goal.split(|c: char| !c.is_alphanumeric() && c != '_') {
        let w = raw.to_lowercase();
        if w.len() >= 3 && !STOP.contains(&w.as_str()) && !seen.contains(&w) {
            seen.push(w);
        }
    }
    seen
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GoalTerms {
    keywords: Vec<String>,
    symbol_terms: Vec<String>,
}

impl GoalTerms {
    fn from_goal(goal: &str) -> Self {
        let keywords = keywords(goal);
        let mut symbol_terms = keywords.clone();

        for raw in goal.split(|c: char| !c.is_alphanumeric() && c != '_') {
            let lower = raw.to_lowercase();
            if !keywords.contains(&lower) {
                continue;
            }
            for variant in identifier_variants(raw) {
                if variant.len() >= 3 && !symbol_terms.contains(&variant) {
                    symbol_terms.push(variant);
                }
            }
        }

        // Users often say "order service" while code defines `OrderService`.
        // Add adjacent compounds for definition matching only; do not use these
        // for broad content hit counts, or noisy prose would get even noisier.
        for width in 2..=3 {
            for window in keywords.windows(width) {
                let joined = window.join("");
                if joined.len() >= 4 && !symbol_terms.contains(&joined) {
                    symbol_terms.push(joined);
                }
                let snake = window.join("_");
                if snake.len() >= 4 && !symbol_terms.contains(&snake) {
                    symbol_terms.push(snake);
                }
            }
        }

        Self {
            keywords,
            symbol_terms,
        }
    }
}

fn identifier_variants(raw: &str) -> Vec<String> {
    let parts = identifier_parts(raw);
    let mut variants = Vec::new();
    if parts.is_empty() {
        return variants;
    }
    push_unique(&mut variants, parts.join(""));
    push_unique(&mut variants, parts.join("_"));
    for part in parts {
        push_unique(&mut variants, part);
    }
    variants
}

fn identifier_parts(raw: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut prev_lower_or_digit = false;

    for ch in raw.chars() {
        if ch == '_' {
            if !current.is_empty() {
                parts.push(current.to_lowercase());
                current.clear();
            }
            prev_lower_or_digit = false;
            continue;
        }
        if ch.is_uppercase() && prev_lower_or_digit && !current.is_empty() {
            parts.push(current.to_lowercase());
            current.clear();
        }
        prev_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
        current.push(ch);
    }
    if !current.is_empty() {
        parts.push(current.to_lowercase());
    }
    parts
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.contains(&value) {
        values.push(value);
    }
}

/// Rank source files by keyword hits in their path and content.
fn select_candidates(root: &Path, map: &RepositoryMap, terms: &GoalTerms) -> Vec<String> {
    if terms.keywords.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(u32, String)> = Vec::new();
    for (scanned, file) in map.source_files().enumerate() {
        if scanned >= MAX_SCAN_FILES {
            break;
        }
        let mut score = 0u32;
        let path_lower = file.to_lowercase();
        for kw in &terms.keywords {
            if path_lower.contains(kw) {
                score += 5; // a filename match is a strong signal
            }
        }
        if let Ok(content) = std::fs::read(root.join(file)) {
            let slice = &content[..content.len().min(MAX_FILE_SCAN_BYTES)];
            let text = String::from_utf8_lossy(slice).to_lowercase();
            for kw in &terms.keywords {
                score += text.matches(kw.as_str()).count().min(10) as u32;
            }
            // Strong signal: this file *defines* a symbol the task names.
            let symbols = crate::symbols::extract_symbols(&text);
            for term in &terms.symbol_terms {
                if symbols.contains(term) {
                    score += 40;
                }
            }
        }
        if score > 0 {
            scored.push((score, file.clone()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, f)| f)
        .collect()
}

/// Find test files related to the candidate files by shared base name.
fn related_tests(
    root: &Path,
    map: &RepositoryMap,
    candidates: &[String],
    terms: &GoalTerms,
) -> Vec<String> {
    let bases: Vec<String> = candidates.iter().filter_map(|c| base_name(c)).collect();
    let mut scored = Vec::new();
    for test in map.test_files() {
        let mut score = 0u32;
        if let Some(tb) = base_name(test) {
            let tb = tb.trim_end_matches("_test").trim_end_matches(".test");
            if bases.iter().any(|b| b == tb || tb.contains(b.as_str())) {
                score += 20;
            }
        }

        let path_lower = test.to_lowercase();
        for term in &terms.symbol_terms {
            if path_lower.contains(term) {
                score += 8;
            }
        }
        for kw in &terms.keywords {
            if path_lower.contains(kw) {
                score += 3;
            }
        }

        if let Ok(content) = std::fs::read(root.join(test)) {
            let slice = &content[..content.len().min(MAX_FILE_SCAN_BYTES)];
            let text = String::from_utf8_lossy(slice).to_lowercase();
            let symbols = crate::symbols::extract_symbols(&text);
            for term in &terms.symbol_terms {
                if symbols
                    .iter()
                    .any(|symbol| symbol == term || symbol.contains(term))
                {
                    score += 20;
                } else if text.contains(term) {
                    score += 5;
                }
            }
        }

        if score > 0 {
            scored.push((score, test.clone()));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(MAX_CANDIDATES)
        .map(|(_, f)| f)
        .collect()
}

fn base_name(path: &str) -> Option<String> {
    let file = path.rsplit('/').next()?;
    let stem = file.split('.').next()?;
    Some(stem.to_string())
}

fn estimate_tokens(map: &str, instructions: &[ProjectInstruction]) -> u32 {
    let chars: usize = map.len() + instructions.iter().map(|i| i.content.len()).sum::<usize>();
    // ~4 chars per token.
    (chars / 4) as u32
}

/// Estimate tokens for an arbitrary string (~4 chars/token).
pub fn estimate_text_tokens(text: &str) -> u32 {
    (text.len() / 4) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        // Unique per call so concurrent tests don't share one dir (a test's
        // remove_dir_all would otherwise delete another's fixture mid-run).
        let dir = std::env::temp_dir().join(format!(
            "leveler-ctx-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/order.rs"),
            "pub struct OrderService;\nimpl OrderService { pub fn cancel(&self) {} }\n",
        )
        .unwrap();
        std::fs::write(dir.join("src/user.rs"), "pub struct User;\n").unwrap();
        std::fs::write(dir.join("src/order_test.rs"), "// tests for order\n").unwrap();
        dir
    }

    #[test]
    fn keywords_drop_stopwords() {
        let kw = keywords("Add a cancel method to the OrderService");
        assert!(kw.contains(&"cancel".to_string()));
        assert!(kw.contains(&"orderservice".to_string()));
        assert!(!kw.contains(&"the".to_string()));
        assert!(!kw.contains(&"add".to_string()));
    }

    #[test]
    fn goal_terms_include_adjacent_symbol_compounds() {
        let terms = GoalTerms::from_goal("fix order service timeout handling");
        assert!(terms.keywords.contains(&"order".to_string()));
        assert!(terms.keywords.contains(&"service".to_string()));
        assert!(terms.symbol_terms.contains(&"orderservice".to_string()));
    }

    #[test]
    fn goal_terms_match_camel_to_snake_symbols() {
        let terms = GoalTerms::from_goal("fix OrderServiceTimeout");
        assert!(
            terms
                .symbol_terms
                .contains(&"orderservicetimeout".to_string())
        );
        assert!(
            terms
                .symbol_terms
                .contains(&"order_service_timeout".to_string())
        );
    }

    #[test]
    fn selects_relevant_candidate() {
        let dir = setup();
        let pkg = ContextCompiler::compile(&dir, "cancel an order in OrderService");
        assert_eq!(
            pkg.candidate_files.first().map(String::as_str),
            Some("src/order.rs")
        );
        assert!(pkg.related_tests.iter().any(|t| t.contains("order_test")));
        assert!(pkg.estimated_tokens > 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ranks_compound_symbol_definition_above_keyword_noise() {
        let dir = setup();
        std::fs::write(
            dir.join("src/noisy_order.rs"),
            "order service timeout order service timeout order service timeout\n\
             order service timeout order service timeout order service timeout\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/service.rs"),
            "pub struct OrderService;\nimpl OrderService { pub fn configure_timeout(&self) {} }\n",
        )
        .unwrap();

        let pkg = ContextCompiler::compile(&dir, "fix order service timeout handling");

        assert_eq!(
            pkg.candidate_files.first().map(String::as_str),
            Some("src/service.rs")
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn related_tests_use_symbol_terms_not_only_file_basename() {
        let dir = setup();
        std::fs::write(
            dir.join("src/service.rs"),
            "pub fn order_service_timeout() {}\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(
            dir.join("tests/timeout_flow.rs"),
            "fn order_service_timeout_handles_retry() {}\n",
        )
        .unwrap();

        let pkg = ContextCompiler::compile(&dir, "fix OrderServiceTimeout");

        assert!(
            pkg.related_tests
                .iter()
                .any(|path| path == "tests/timeout_flow.rs"),
            "{:?}",
            pkg.related_tests
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compiles_and_renders_skill_index() {
        let dir = setup();
        let skill = dir.join(".leveler/skills/deploy");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: deploy\ndescription: Ship the service.\n---\n\nRun make deploy.",
        )
        .unwrap();

        let pkg = ContextCompiler::compile(&dir, "deploy the service");
        assert_eq!(pkg.skills.len(), 1);
        assert_eq!(pkg.skills[0].name, "deploy");

        let rendered = pkg.render();
        assert!(rendered.contains("Available skills"), "{rendered}");
        assert!(
            rendered.contains("deploy") && rendered.contains("Ship the service."),
            "{rendered}"
        );
        assert!(
            rendered.contains("progressive disclosure") || rendered.contains("How to use skills"),
            "how-to-use rules must appear: {rendered}"
        );
        // The body stays out of context until load_skill / $name injection.
        assert!(!rendered.contains("Run make deploy."), "{rendered}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
