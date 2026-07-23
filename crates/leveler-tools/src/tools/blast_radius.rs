//! `blast_radius` — estimate the impact of changing a symbol by walking LSP
//! references outward, hop by hop. Built ON TOP of the language server (precise
//! `textDocument/references`), not a separate resident call-graph index: each
//! reference is mapped to its enclosing symbol (via document-symbol ranges), and
//! that symbol is expanded at the next hop. No index to build or keep fresh.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use leveler_execution::RiskLevel;
use leveler_lsp::SymbolSpan;

use super::symbols::{column_of, lsp_locate, relativize};
use crate::tool::{Tool, ToolContext, ToolError, ToolOutput};

const DEFAULT_DEPTH: usize = 2;
const MAX_DEPTH: usize = 5;

#[derive(Debug, Deserialize, JsonSchema)]
struct Input {
    /// The symbol whose change-impact to estimate.
    symbol: String,
    /// How many reference hops to follow (direct dependents = 1). Default 2.
    #[serde(default)]
    max_depth: Option<usize>,
}

pub struct BlastRadiusTool;

#[async_trait]
impl Tool for BlastRadiusTool {
    fn name(&self) -> &'static str {
        "blast_radius"
    }

    fn description(&self) -> &'static str {
        "Estimate the impact of changing a symbol: the files and functions that \
         (transitively) reference it, grouped by hop distance. Precise via a \
         language server (an ESTIMATE — static references miss dynamic dispatch). \
         Use before a refactor; use find_references for just the direct sites."
    }

    fn input_schema(&self) -> serde_json::Value {
        super::schema_of::<Input>()
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    fn supports_parallel(&self) -> bool {
        true
    }

    async fn execute(
        &self,
        input: serde_json::Value,
        context: ToolContext,
        _cancellation: CancellationToken,
    ) -> Result<ToolOutput, ToolError> {
        let input: Input = super::parse_input(self.name(), input)?;
        let max_depth = input.max_depth.unwrap_or(DEFAULT_DEPTH).clamp(1, MAX_DEPTH);
        let root = context.workspace.root().to_path_buf();

        let resolver = LspResolver {
            context: &context,
            root: &root,
        };
        let by_depth = compute_blast_radius(&input.symbol, max_depth, &resolver).await;

        if by_depth.iter().all(|(_, r)| r.is_empty()) {
            return Ok(ToolOutput::ok(format!(
                "(no impact found for `{}` — it has no references, or the file's \
                 language has no available language server; try find_references)\n",
                input.symbol
            )));
        }
        Ok(ToolOutput::ok(format_report(&input.symbol, &by_depth)))
    }
}

/// A referring site: the symbol that contains a reference, and its file.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct Referrer {
    file: String,
    symbol: String,
}

/// Resolves "which symbols reference `symbol`", as enclosing `(file, symbol)`.
#[async_trait]
trait ImpactResolver: Send + Sync {
    async fn referrers(&self, symbol: &str) -> Vec<Referrer>;
}

/// BFS from `seed` up to `max_depth` hops. A referrer is counted once per
/// `(file, symbol)` — a same-named function in another file is a distinct
/// impact, not a duplicate — while each enclosing symbol *name* is expanded at
/// most once (the resolver locates by name, so re-expanding a name only repeats
/// work). Cycles terminate because the expansion set only grows.
async fn compute_blast_radius(
    seed: &str,
    max_depth: usize,
    resolver: &dyn ImpactResolver,
) -> Vec<(usize, Vec<Referrer>)> {
    let mut counted: HashSet<Referrer> = HashSet::new();
    let mut expanded: HashSet<String> = HashSet::from([seed.to_string()]);
    let mut frontier = vec![seed.to_string()];
    let mut by_depth = Vec::new();

    for depth in 1..=max_depth {
        let mut found = Vec::new();
        let mut next = Vec::new();
        for sym in &frontier {
            for r in resolver.referrers(sym).await {
                if counted.insert(r.clone()) {
                    if expanded.insert(r.symbol.clone()) {
                        next.push(r.symbol.clone());
                    }
                    found.push(r);
                }
            }
        }
        if found.is_empty() {
            break;
        }
        by_depth.push((depth, found));
        frontier = next;
    }
    by_depth
}

fn format_report(seed: &str, by_depth: &[(usize, Vec<Referrer>)]) -> String {
    let mut all_files: HashSet<&str> = HashSet::new();
    let mut all_symbols = 0usize;
    let mut body = format!("Blast radius of `{seed}` (LSP references, estimate):\n");
    for (depth, referrers) in by_depth {
        let label = if *depth == 1 { "DIRECT" } else { "INDIRECT" };
        // Group this hop's referrers by file.
        let mut by_file: HashMap<&str, Vec<&str>> = HashMap::new();
        for r in referrers {
            by_file.entry(&r.file).or_default().push(&r.symbol);
            all_files.insert(&r.file);
            all_symbols += 1;
        }
        body.push_str(&format!(
            "\n{label} (hop {depth}): {} symbol(s) in {} file(s)\n",
            referrers.len(),
            by_file.len()
        ));
        let mut files: Vec<_> = by_file.into_iter().collect();
        files.sort_by(|a, b| a.0.cmp(b.0));
        for (file, mut syms) in files {
            syms.sort_unstable();
            syms.dedup();
            body.push_str(&format!("  {file}: {}\n", syms.join(", ")));
        }
    }
    body.push_str(&format!(
        "\nTOTAL: {all_symbols} impacted symbol(s) across {} file(s)\n",
        all_files.len()
    ));
    body
}

/// Innermost symbol whose body range contains `line` (0-based).
fn innermost_enclosing(spans: &[SymbolSpan], line: u64) -> Option<String> {
    spans
        .iter()
        .filter(|s| s.start_line <= line && line <= s.end_line)
        .min_by_key(|s| s.end_line - s.start_line)
        .map(|s| s.name.clone())
}

/// The production resolver: LSP `references` mapped to enclosing symbols.
struct LspResolver<'a> {
    context: &'a ToolContext,
    root: &'a Path,
}

#[async_trait]
impl ImpactResolver for LspResolver<'_> {
    async fn referrers(&self, symbol: &str) -> Vec<Referrer> {
        let Some((language, matches)) = lsp_locate(self.context, self.root, symbol).await else {
            return Vec::new();
        };
        let Some(def) = matches.first() else {
            return Vec::new();
        };
        let Some(spec) = leveler_lsp::server_for(language) else {
            return Vec::new();
        };
        // Clone the session Arc out and drop the lock before the LSP requests so
        // this BFS doesn't hold the global sessions mutex across many round-trips.
        let client = {
            let sessions = self.context.lsp_sessions.lock().await;
            let Some(client) = sessions.get(language.as_str()) else {
                return Vec::new();
            };
            client.clone()
        };

        let def_path = Path::new(&def.path);
        let line_text = std::fs::read_to_string(def_path)
            .ok()
            .and_then(|t| t.lines().nth(def.line as usize).map(String::from))
            .unwrap_or_default();
        let character = column_of(&line_text, symbol);
        let _ = client.open(def_path, &spec.language_id).await;
        let refs = client
            .references(def_path, def.line, character, false)
            .await
            .unwrap_or_default();

        let mut out = Vec::new();
        let mut span_cache: HashMap<String, Vec<SymbolSpan>> = HashMap::new();
        for r in refs {
            let spans = match span_cache.get(&r.path) {
                Some(spans) => spans.clone(),
                None => {
                    let path = Path::new(&r.path);
                    let _ = client.open(path, &spec.language_id).await;
                    let spans = client.document_symbol_spans(path).await.unwrap_or_default();
                    span_cache.insert(r.path.clone(), spans.clone());
                    spans
                }
            };
            let enclosing =
                innermost_enclosing(&spans, r.line).unwrap_or_else(|| "(file scope)".to_string());
            out.push(Referrer {
                file: relativize(&r.path, self.root),
                symbol: enclosing,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake reference graph keyed by symbol → its referrers.
    struct MockResolver(HashMap<&'static str, Vec<Referrer>>);

    #[async_trait]
    impl ImpactResolver for MockResolver {
        async fn referrers(&self, symbol: &str) -> Vec<Referrer> {
            self.0.get(symbol).cloned().unwrap_or_default()
        }
    }

    fn r(file: &str, symbol: &str) -> Referrer {
        Referrer {
            file: file.into(),
            symbol: symbol.into(),
        }
    }

    #[tokio::test]
    async fn bfs_walks_hops_and_dedups_symbols() {
        // target ← a (a.rs), b (b.rs);  a ← c (c.rs);  b ← a (already seen)
        let graph = HashMap::from([
            ("target", vec![r("a.rs", "a"), r("b.rs", "b")]),
            ("a", vec![r("c.rs", "c")]),
            ("b", vec![r("a.rs", "a")]), // 'a' already visited → not re-counted
        ]);
        let by_depth = compute_blast_radius("target", 3, &MockResolver(graph)).await;
        assert_eq!(by_depth.len(), 2, "{by_depth:?}");
        assert_eq!(by_depth[0].0, 1);
        assert_eq!(by_depth[0].1.len(), 2); // a, b
        assert_eq!(by_depth[1].0, 2);
        assert_eq!(by_depth[1].1, vec![r("c.rs", "c")]); // only c; a is a repeat
    }

    #[tokio::test]
    async fn same_name_symbol_in_two_files_counts_as_two_impacts() {
        // A function named `helper` in a.rs and another in b.rs both reference the
        // target. They are distinct impacts — keying dedup on the bare name would
        // silently drop one and under-report the blast radius.
        let graph = HashMap::from([(
            "target",
            vec![r("a.rs", "helper"), r("b.rs", "helper")],
        )]);
        let by_depth = compute_blast_radius("target", 2, &MockResolver(graph)).await;
        assert_eq!(by_depth.len(), 1);
        assert_eq!(
            by_depth[0].1.len(),
            2,
            "both files' `helper` must count: {by_depth:?}"
        );
    }

    #[tokio::test]
    async fn bfs_respects_the_depth_cap() {
        let graph = HashMap::from([
            ("target", vec![r("a.rs", "a")]),
            ("a", vec![r("b.rs", "b")]),
            ("b", vec![r("c.rs", "c")]),
        ]);
        let by_depth = compute_blast_radius("target", 1, &MockResolver(graph)).await;
        assert_eq!(by_depth.len(), 1);
        assert_eq!(by_depth[0].1, vec![r("a.rs", "a")]);
    }

    #[tokio::test]
    async fn bfs_stops_when_nothing_references_the_seed() {
        let by_depth = compute_blast_radius("orphan", 3, &MockResolver(HashMap::new())).await;
        assert!(by_depth.is_empty());
    }

    #[test]
    fn innermost_enclosing_prefers_the_tightest_span() {
        let spans = vec![
            SymbolSpan {
                name: "Service".into(),
                kind: 5,
                name_line: 10,
                name_character: 0,
                start_line: 10,
                end_line: 40,
            },
            SymbolSpan {
                name: "cancel".into(),
                kind: 6,
                name_line: 20,
                name_character: 4,
                start_line: 20,
                end_line: 25,
            },
        ];
        assert_eq!(innermost_enclosing(&spans, 22).as_deref(), Some("cancel"));
        assert_eq!(innermost_enclosing(&spans, 12).as_deref(), Some("Service"));
        assert_eq!(innermost_enclosing(&spans, 99), None);
    }

    #[test]
    fn report_groups_by_hop_and_file() {
        let by_depth = vec![
            (
                1,
                vec![
                    r("src/a.rs", "foo"),
                    r("src/a.rs", "bar"),
                    r("src/b.rs", "baz"),
                ],
            ),
            (2, vec![r("src/c.rs", "qux")]),
        ];
        let out = format_report("target", &by_depth);
        assert!(
            out.contains("DIRECT (hop 1): 3 symbol(s) in 2 file(s)"),
            "{out}"
        );
        assert!(out.contains("src/a.rs: bar, foo"), "{out}");
        assert!(
            out.contains("INDIRECT (hop 2): 1 symbol(s) in 1 file(s)"),
            "{out}"
        );
        assert!(
            out.contains("TOTAL: 4 impacted symbol(s) across 3 file(s)"),
            "{out}"
        );
    }
}
