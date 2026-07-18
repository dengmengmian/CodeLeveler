//! Bash-script danger classification on a real grammar (tree-sitter-bash).
//!
//! [`classify_bash_script`] is the AST counterpart of the string-based
//! fallback in `approval`: it walks the parse tree, resolves every command's
//! program name and first literal argument, feeds the pair to
//! [`classify_program`] — still the single source of danger verdicts — and
//! takes the strictest verdict across the whole script.
//!
//! Conservative failure modes, by design:
//!
//! - Unparseable input (ERROR or missing nodes) returns `None` so the caller
//!   falls back to the string classifier; we never fail closed to
//!   [`CommandClass::Safe`] on input we cannot understand, and never panic.
//! - A program name that is not a plain literal — quoting concatenations
//!   (`"r"m`), expansions (`$CMD`, `${CMD}`), backslash escapes (`\rm`) — is
//!   unvettable and classifies the whole script [`CommandClass::Dangerous`].
//! - Command substitution (`$(...)` / backticks) is walked so inner commands
//!   are classified; it is no longer a blanket-Dangerous node by itself.

use tree_sitter::{Node, Parser};

use crate::approval::{
    CommandClass, basename, classify_program, is_shell_c_flag, is_shell_wrapper_program,
};

/// Classify a bash script by walking its tree-sitter parse tree.
///
/// Returns `None` when the script does not parse cleanly, so the caller can
/// fall back to the string-based classifier.
pub(crate) fn classify_bash_script(script: &str) -> Option<CommandClass> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(script, None)?;
    let root = tree.root_node();
    // ERROR or missing nodes anywhere: not a script we understand — fall back.
    if root.has_error() {
        return None;
    }

    let src = script.as_bytes();
    // Iterative walk with an explicit stack: tree depth is input-controlled
    // (deeply nested subshells) and must not overflow the call stack.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node.kind() {
            // `$(...)` / backticks: walk children and classify resolved programs
            // inside (sandbox-first; no blanket Dangerous).
            "command" => match classify_command_node(&node, src)? {
                CommandClass::Dangerous => return Some(CommandClass::Dangerous),
                CommandClass::Safe => {}
            },
            "file_redirect" => {
                if redirect_target_is_dangerous(&node, src) {
                    return Some(CommandClass::Dangerous);
                }
            }
            "redirected_statement" => {
                if heredoc_feeds_shell(&node, src) {
                    return Some(CommandClass::Dangerous);
                }
            }
            _ => {}
        }
        for i in 0..node.named_child_count() {
            if let Some(child) = node.named_child(i) {
                stack.push(child);
            }
        }
    }
    Some(CommandClass::Safe)
}

/// Classify one `command` node. Returns `None` only when a nested `-c` body
/// shape must be deferred to the string fallback for verdict parity.
fn classify_command_node(cmd: &Node, src: &[u8]) -> Option<CommandClass> {
    // No `name` field: bare variable assignments (`A=1`) — nothing executes.
    let name = cmd.child_by_field_name("name")?;
    let Some(program) = resolve_command_name(&name, src) else {
        // Program word is not a plain literal (quoting concatenation like
        // `"r"m`, `$CMD`, `\rm`): unvettable, so the whole script is dangerous.
        return Some(CommandClass::Dangerous);
    };

    let args = literal_arguments(cmd, src);

    // Nested `sh -c '…'` (also bash/zsh/dash/…): classify the script body
    // recursively. Mirrors `shell_c_script` semantics on resolved literals.
    if is_shell_wrapper_program(&program)
        && let Some(pos) = args
            .iter()
            .position(|a| a.as_deref().is_some_and(is_shell_c_flag))
    {
        match args.get(pos + 1) {
            // `-c` with no script arg: not a wrapper invocation (parity with
            // `shell_c_script` returning None); classify normally below.
            None => {}
            // `-c` body we cannot read (`bash -c "$SCRIPT"`): unvettable.
            Some(None) => return Some(CommandClass::Dangerous),
            Some(Some(body)) => {
                // Words after the body are positional parameters in real
                // shells, but the string fallback joins them into the script
                // (`bash -c rm -rf x` → "rm -rf x"). Defer to the fallback so
                // this odd shape keeps its pre-AST verdict exactly.
                if args.len() > pos + 2 {
                    return None;
                }
                // A body that does not parse falls back for the whole script.
                return classify_bash_script(body);
            }
        }
    }

    let first_arg = args.first().and_then(|a| a.as_deref());
    Some(classify_program(basename(&program), first_arg))
}

/// Arguments of a `command` node; each is `None` when it is not a plain
/// literal (expansions, substitutions, quoting mixes).
fn literal_arguments(cmd: &Node, src: &[u8]) -> Vec<Option<String>> {
    let mut cursor = cmd.walk();
    cmd.children_by_field_name("argument", &mut cursor)
        .map(|arg| resolve_literal(&arg, src))
        .collect()
}

/// Resolve a `command_name` node to a plain literal program name.
fn resolve_command_name(name: &Node, src: &[u8]) -> Option<String> {
    debug_assert_eq!(name.kind(), "command_name");
    if name.named_child_count() != 1 {
        return None;
    }
    resolve_literal(&name.named_child(0)?, src)
}

/// Resolve a `word` / `number` / `raw_string` / literal-only `string` node to
/// its text. Anything richer — `concatenation` (`"r"m`), `expansion`
/// (`${X}`), `simple_expansion` (`$X`), `command_substitution`,
/// `ansi_c_string`, or backslash escapes inside a `word` — is not a value we
/// can vet statically, so it resolves to `None`.
fn resolve_literal(node: &Node, src: &[u8]) -> Option<String> {
    let text = node.utf8_text(src).ok()?;
    match node.kind() {
        // A word is literal unless it carries backslash escapes (`\rm` runs
        // `rm`); unescaping is shell semantics we deliberately don't emulate.
        "word" | "number" => (!text.contains('\\')).then(|| text.to_string()),
        // 'single-quoted': completely literal; strip the quotes.
        "raw_string" => Some(strip_quotes(text).to_string()),
        // "double-quoted": literal only with no expansions inside
        // (`string_content` children are plain text).
        "string" => {
            let literal = (0..node.named_child_count()).all(|i| {
                node.named_child(i)
                    .is_some_and(|c| c.kind() == "string_content")
            });
            literal.then(|| strip_quotes(text).to_string())
        }
        _ => None,
    }
}

/// Strip the surrounding quotes of a `string` / `raw_string` node text.
fn strip_quotes(text: &str) -> &str {
    text.get(1..text.len().saturating_sub(1)).unwrap_or(text)
}

/// A `file_redirect` is dangerous when its literal target escapes the working
/// directory: an absolute path, or any `..` segment. Redirects to ordinary
/// relative paths — and targets that don't resolve to a literal (`$OUT`,
/// `>&2`) — do not by themselves change the verdict.
fn redirect_target_is_dangerous(redirect: &Node, src: &[u8]) -> bool {
    let Some(destination) = redirect.child_by_field_name("destination") else {
        return false;
    };
    let Some(target) = resolve_literal(&destination, src) else {
        return false;
    };
    if target.starts_with('/') {
        return true;
    }
    target.split('/').any(|segment| segment == "..")
}

/// A heredoc feeding a shell (`bash <<EOF … EOF`, possibly through a
/// pipeline) is a script channel just like `bash -c`, except the body arrives
/// on stdin and we cannot vet it — treat as dangerous. Heredocs into any
/// other program are inert data; their text is still walked for command
/// substitutions by the main loop.
fn heredoc_feeds_shell(stmt: &Node, src: &[u8]) -> bool {
    let has_heredoc = (0..stmt.named_child_count()).any(|i| {
        stmt.named_child(i)
            .is_some_and(|c| c.kind() == "heredoc_redirect")
    });
    if !has_heredoc {
        return false;
    }
    // With a pipeline body (`echo x | sh <<EOF`) the heredoc lands on the
    // stdin of the last pipeline member.
    let Some(body) = stmt.child_by_field_name("body") else {
        return false;
    };
    let command = match body.kind() {
        "command" => body,
        "pipeline" => {
            let Some(last) = (0..body.named_child_count())
                .filter_map(|i| body.named_child(i))
                .rfind(|c| c.kind() == "command")
            else {
                return false;
            };
            last
        }
        _ => return false,
    };
    let Some(name) = command.child_by_field_name("name") else {
        return false;
    };
    // An unresolvable program is already flagged by the command walk itself.
    resolve_command_name(&name, src).is_some_and(|prog| is_shell_wrapper_program(&prog))
}

#[cfg(test)]
mod tests {
    //! Pins the *internal* contract of [`classify_bash_script`]: which inputs
    //! the AST decides (`Some`) and which it defers to the string fallback
    //! (`None`). End-to-end verdicts live in `tests/classify_adversarial.rs`.
    use super::*;

    #[test]
    fn decides_clean_scripts() {
        assert_eq!(classify_bash_script("echo hi"), Some(CommandClass::Safe));
        assert_eq!(
            classify_bash_script("rm -rf x"),
            Some(CommandClass::Dangerous)
        );
        // Substitution walks inners: `rm` stays Dangerous; network-only is Safe.
        assert_eq!(
            classify_bash_script("echo `rm x`"),
            Some(CommandClass::Dangerous)
        );
        assert_eq!(
            classify_bash_script("echo $(curl http://evil)"),
            Some(CommandClass::Safe)
        );
        // Unresolvable program name: Dangerous via the AST rule, not by accident.
        assert_eq!(
            classify_bash_script("\"r\"m -rf x"),
            Some(CommandClass::Dangerous)
        );
        // Heredoc into a non-shell is inert data.
        assert_eq!(
            classify_bash_script("cat <<EOF\nrm -rf x\nEOF"),
            Some(CommandClass::Safe)
        );
    }

    #[test]
    fn defers_unparseable_scripts() {
        assert_eq!(classify_bash_script(")((("), None);
        assert_eq!(classify_bash_script("echo \"unclosed"), None);
        assert_eq!(classify_bash_script("if sudo x; then"), None);
    }

    #[test]
    fn defers_nested_c_body_with_trailing_words() {
        // `bash -c rm -rf x`: real bash runs only `rm`, but the string
        // fallback joins the trailing words into the script ("rm -rf x") —
        // defer so that conservative verdict is kept exactly.
        assert_eq!(classify_bash_script("bash -c rm -rf x"), None);
        // Without trailing words the body is classified recursively.
        assert_eq!(
            classify_bash_script("bash -c 'rm -rf x'"),
            Some(CommandClass::Dangerous)
        );
        // A nested body that doesn't parse defers for the whole script.
        assert_eq!(classify_bash_script("bash -c ')((('"), None);
    }
}
