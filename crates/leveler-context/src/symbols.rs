//! Lightweight, dependency-free symbol extraction (spec §26: a step toward the
//! phase-2 symbol index without a full LSP/tree-sitter platform yet).
//!
//! It scans for definition keywords and takes the following identifier. Crude
//! but effective for ranking: a file that *defines* the symbol a task mentions
//! is almost certainly the file to edit.

/// Keywords across common languages whose following token names a definition.
const DEFINITION_KEYWORDS: &[&str] = &[
    "fn",
    "func",
    "def",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "type",
    "function",
    "impl",
    "module",
    "package",
    "const",
    "static",
];

/// Extract the set of defined symbol names from source `content` (lowercased).
pub fn extract_symbols(content: &str) -> Vec<String> {
    let mut symbols = Vec::new();
    for line in content.lines() {
        let mut tokens = line
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .peekable();
        while let Some(tok) = tokens.next() {
            if DEFINITION_KEYWORDS.contains(&tok) {
                // The next non-empty token is the symbol name.
                for next in tokens.by_ref() {
                    if next.is_empty() {
                        continue;
                    }
                    let name = next.to_lowercase();
                    if name.len() >= 2 && !symbols.contains(&name) {
                        symbols.push(name);
                    }
                    break;
                }
            }
        }
    }
    symbols
}

/// Whether `content` defines a symbol named `keyword` (case-insensitive).
pub fn defines(content: &str, keyword: &str) -> bool {
    let kw = keyword.to_lowercase();
    extract_symbols(content).contains(&kw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_rust_symbols() {
        let src = "pub fn cancel_order() {}\nstruct OrderService;\ntrait Repo {}";
        let syms = extract_symbols(src);
        assert!(syms.contains(&"cancel_order".to_string()));
        assert!(syms.contains(&"orderservice".to_string()));
        assert!(syms.contains(&"repo".to_string()));
    }

    #[test]
    fn extracts_go_and_python() {
        assert!(defines("func Triple(x int) int { return x*3 }", "Triple"));
        assert!(defines(
            "class UserManager:\n    def disable(self): ...",
            "UserManager"
        ));
        assert!(defines("def disable_user(): ...", "disable_user"));
    }

    #[test]
    fn does_not_match_usage_only() {
        // `cancel_order` is called, not defined.
        assert!(!defines("let x = cancel_order();", "cancel_order"));
    }
}
