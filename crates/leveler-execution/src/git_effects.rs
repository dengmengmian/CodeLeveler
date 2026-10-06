//! What a Git invocation mechanically does, resolved from argv.
//!
//! Permission for Git is about EFFECTS, not about the word `git`. A command
//! line is parsed into `(program, subcommand, args)` and mapped to the set of
//! mechanical facts it will produce: repository metadata read/write, working
//! tree mutation, remote read/write, configuration write, irreversible
//! history loss. The permission layer then reasons about those facts:
//!
//! - `git status` reads metadata and nothing else;
//! - `git fetch` writes repository metadata and reads a remote, and cannot
//!   move the working tree or rewrite repository identity;
//! - `git push` has a REMOTE side effect, so it never rides on the fetch
//!   decision;
//! - `git remote set-url` rewrites repository identity (`.git/config`).
//!
//! This is deliberately the ONLY place that knows Git's subcommand vocabulary.
//! Callers must never branch on a command string: they ask for effects.
//!
//! Fail-closed rules, because a permission verdict that guesses is worse than
//! one that asks:
//!
//! - an unknown subcommand, or a global option that injects configuration or
//!   relocates the program Git executes (`-c`, `--config-env`, `--exec-path`),
//!   marks the invocation unresolved, so it is gated and never auto-widened;
//! - a global option that only moves the TARGET (`-C`, `--git-dir`,
//!   `--work-tree`, `--namespace`) is tracked separately: the subcommand's
//!   effects still hold, so a read stays allowed while a write can never be
//!   bound to a repository this host knows;
//! - a remote named by URL or path (`git fetch https://…`) is not the
//!   repository's own configured remote, so a sync command cannot be turned
//!   into an arbitrary fetch target on this path.

/// The set of mechanical facts one Git invocation produces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitEffects {
    /// Reads repository objects, refs, or the index. Touches nothing.
    pub metadata_read: bool,
    /// Writes repository metadata: objects, refs, index, `FETCH_HEAD`, HEAD.
    pub metadata_write: bool,
    /// Changes the checkout's STATE: tracked files, HEAD, index, or stash.
    /// Not "any write under the workspace" — creating new files (a patch
    /// export, a build artifact) is the authority the profile already grants.
    pub workspace_mutation: bool,
    /// Contacts a remote without changing it.
    pub remote_read: bool,
    /// Changes state on a remote, or a local record of one after the remote
    /// already accepted it. Irreversible from this machine.
    pub remote_write: bool,
    /// Rewrites repository identity (`.git/config`): remotes, upstreams.
    pub config_write: bool,
    /// Discards committed or uncommitted work that this machine cannot
    /// reconstruct.
    pub irreversible: bool,
}

/// Stable capability ids, in a fixed order, for the approval prompt and for
/// logs. These are the vocabulary a permission question is asked in.
pub const CAP_METADATA_READ: &str = "git_metadata.read";
pub const CAP_METADATA_WRITE: &str = "git_metadata.write";
pub const CAP_WORKSPACE_WRITE: &str = "git_workspace.write";
pub const CAP_REMOTE_READ: &str = "git_remote.read";
pub const CAP_REMOTE_WRITE: &str = "git_remote.write";
pub const CAP_CONFIG_WRITE: &str = "git_config.write";
pub const CAP_HISTORY_DESTROY: &str = "git_history.destroy";
pub const CAP_ARGV_UNRESOLVED: &str = "git_argv.unresolved";
pub const CAP_TARGET_RELOCATED: &str = "git_target.relocated";

impl GitEffects {
    pub fn is_empty(self) -> bool {
        self == Self::default()
    }

    pub fn merge(&mut self, other: Self) {
        self.metadata_read |= other.metadata_read;
        self.metadata_write |= other.metadata_write;
        self.workspace_mutation |= other.workspace_mutation;
        self.remote_read |= other.remote_read;
        self.remote_write |= other.remote_write;
        self.config_write |= other.config_write;
        self.irreversible |= other.irreversible;
    }

    /// Whether the invocation writes inside the repository's Git metadata
    /// directory, which the workspace sandbox seals by default.
    pub fn needs_repository_write(self) -> bool {
        self.metadata_write || self.config_write
    }

    /// The capability ids these effects ask for, in a stable order.
    pub fn capabilities(self) -> Vec<&'static str> {
        let mut ids = Vec::new();
        if self.metadata_read {
            ids.push(CAP_METADATA_READ);
        }
        if self.metadata_write {
            ids.push(CAP_METADATA_WRITE);
        }
        if self.workspace_mutation {
            ids.push(CAP_WORKSPACE_WRITE);
        }
        if self.remote_read {
            ids.push(CAP_REMOTE_READ);
        }
        if self.remote_write {
            ids.push(CAP_REMOTE_WRITE);
        }
        if self.config_write {
            ids.push(CAP_CONFIG_WRITE);
        }
        if self.irreversible {
            ids.push(CAP_HISTORY_DESTROY);
        }
        ids
    }
}

/// One `git …` invocation, resolved as far as static reading allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitCommandEffects {
    pub effects: GitEffects,
    /// The subcommand is one this module knows AND its deciding arguments were
    /// readable. `false` means the verdict is a guess, so it must be asked.
    pub resolved: bool,
    /// A global option moved the invocation's TARGET (the working directory,
    /// the metadata directory, the ref namespace) somewhere this module cannot
    /// bind to the repository it knows. The effects above are still exactly
    /// what the subcommand produces — only the target is unbound — so an
    /// effect-derived WRITE capability is never granted, while a read-only
    /// subcommand is still a read and must not become a prompt.
    pub relocated: bool,
    /// The invocation names a remote by URL or path rather than by the name of
    /// a remote the repository itself configures.
    pub explicit_remote: bool,
}

impl GitCommandEffects {
    fn resolved(effects: GitEffects) -> Self {
        Self {
            effects,
            resolved: true,
            relocated: false,
            explicit_remote: false,
        }
    }

    fn unresolved(effects: GitEffects) -> Self {
        Self {
            effects,
            resolved: false,
            relocated: false,
            explicit_remote: false,
        }
    }
}

/// Everything a tool call will execute, as Git effects.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallGitEffects {
    pub effects: GitEffects,
    /// A Git command appears in the call.
    pub any_git: bool,
    /// EVERY command the call runs is a resolved Git invocation. Only then may
    /// a Git capability be granted without asking: a script that mixes Git
    /// with anything else must not inherit Git's narrow allowance.
    pub fully_git: bool,
    /// Some Git invocation in the call was not fully readable.
    pub unresolved: bool,
    /// Some Git invocation relocated its target with a global option.
    pub relocated: bool,
    /// A Git invocation named a remote by URL or path.
    pub explicit_remote: bool,
}

impl CallGitEffects {
    /// Whether the call may be granted the repository-metadata WRITE
    /// capability from its effects alone (no prompt). Narrow on purpose: the
    /// invocation adds nothing to the working tree, nothing to repository
    /// identity, and nothing to a remote.
    pub fn grants_metadata_write_by_effect(&self) -> bool {
        self.fully_git
            && !self.unresolved
            && !self.relocated
            && self.effects.needs_repository_write()
            && !self.effects.workspace_mutation
            && !self.effects.remote_write
            && !self.effects.config_write
            && !self.effects.irreversible
    }

    /// An effect that must be approved before it runs, whatever the profile:
    /// a change to the checkout's state or to repository identity, a remote
    /// side effect, irreversible history loss, an unreadable invocation, or a
    /// sync against a remote the repository does not configure (the model
    /// would otherwise pick the target).
    ///
    /// A pure repository-metadata write that changes neither (`git fetch`,
    /// `git add`, `git commit`) is deliberately NOT gated: it is the shape
    /// the profile already permits, and the metadata scope it runs with comes
    /// from [`Self::needs_repository_write_scope`].
    ///
    /// Relocation is not a reason to ask by itself. `git -C <dir> status` and
    /// `git --git-dir=<dir> log` move the TARGET of a read that the profile
    /// already permits anywhere; treating that as unreadable argv asked for
    /// confirmation on the most ordinary read-only Git there is. It gates only
    /// when the unbound target could receive a write or reach a remote — the
    /// two things the effect-derived capability has to be bound for.
    pub fn gated(&self) -> bool {
        if !self.any_git {
            return false;
        }
        self.unresolved
            || self.effects.workspace_mutation
            || self.effects.remote_write
            || self.effects.config_write
            || self.effects.irreversible
            || (self.explicit_remote && (self.effects.remote_read || self.effects.metadata_write))
            || (self.relocated
                && (self.effects.needs_repository_write() || self.effects.remote_read))
    }

    /// Whether the call must write repository metadata at all, once it is
    /// authorized. True for every resolved Git invocation that writes, so an
    /// APPROVED destructive or remote command can still write the refs it
    /// needs — the capability follows the effect, not the decision. A relocated
    /// invocation is excluded: the repository whose `.git` this scope would
    /// unseal is not the one the command was pointed at.
    pub fn needs_repository_write_scope(&self) -> bool {
        self.fully_git
            && !self.unresolved
            && !self.relocated
            && self.effects.needs_repository_write()
    }

    /// The capability ids a prompt or a log should name.
    pub fn capabilities(&self) -> Vec<&'static str> {
        let mut ids = self.effects.capabilities();
        if self.unresolved {
            ids.push(CAP_ARGV_UNRESOLVED);
        }
        if self.relocated {
            ids.push(CAP_TARGET_RELOCATED);
        }
        ids
    }
}

/// Union the effects of every command a call has been shown to execute.
///
/// `commands` is the resolved argv of each executed command (program word
/// first). `complete` says whether that list is the WHOLE list; an incomplete
/// list can still gate (a `git push` seen is a `git push` seen) but can never
/// grant a capability.
pub fn call_git_effects(commands: &[Vec<String>], complete: bool) -> CallGitEffects {
    let mut out = CallGitEffects::default();
    let mut all_git = !commands.is_empty();
    for command in commands {
        match git_command_effects(command) {
            Some(git) => {
                out.any_git = true;
                out.effects.merge(git.effects);
                out.unresolved |= !git.resolved;
                out.relocated |= git.relocated;
                out.explicit_remote |= git.explicit_remote;
            }
            None => all_git = false,
        }
    }
    out.fully_git = all_git && complete;
    // A visible read cannot establish that the unreadable remainder of the
    // invocation is harmless (for example `git status; git clean $FLAGS`).
    out.unresolved |= out.any_git && !complete;
    out
}

/// Arguments that name a program Git will EXECUTE. A transport command is
/// otherwise a data transfer; with one of these it is arbitrary code, so it can
/// never ride an effect-derived grant. Also covers the legacy `--exec` alias.
fn names_transport_exec_override(args: &[String]) -> bool {
    const FLAGS: [&str; 3] = ["--upload-pack", "--receive-pack", "--exec"];
    args.iter().any(|arg| {
        FLAGS
            .iter()
            .any(|flag| arg == flag || arg.starts_with(&format!("{flag}=")))
    })
}

/// Resolve one invocation (`args[0]` is the program word) into Git effects.
/// `None` when this is not a Git invocation.
pub fn git_command_effects(args: &[String]) -> Option<GitCommandEffects> {
    let program = args.first()?;
    if basename(program) != "git" {
        return None;
    }
    let Some(parsed) = split_subcommand(&args[1..]) else {
        // `git` with no subcommand at all: usage output, nothing executes.
        return Some(GitCommandEffects::resolved(GitEffects {
            metadata_read: true,
            ..GitEffects::default()
        }));
    };
    let mut effects = classify_subcommand(&parsed.subcommand, &parsed.args);
    if parsed.rule_override {
        // `git -c key=value …` / `--config-env` / `--exec-path` changes the
        // rules the invocation runs under — configuration that can name a
        // program Git will execute. The effect is no longer readable, so never
        // auto-widen on it.
        effects.resolved = false;
    }
    if parsed.target_relocation {
        // `git -C <dir> …` / `--git-dir` / `--work-tree` / `--namespace`
        // changes only WHERE the invocation points. The subcommand still does
        // exactly what the effects say, so a read stays a read; a write can no
        // longer be bound to a repository this host knows, so it never earns
        // the metadata scope and is gated by [`CallGitEffects::gated`].
        effects.relocated = true;
    }
    if names_transport_exec_override(&parsed.args) {
        // `git fetch --upload-pack=<program> …` runs that program locally.
        effects.resolved = false;
    }
    Some(effects)
}

/// A reusable grant target resolved by the same argv parser as Git effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitGrantSelection {
    pub effects: GitEffects,
    pub remote_name: Option<String>,
}

pub fn git_grant_selection(args: &[String]) -> Option<GitGrantSelection> {
    // A caller-selected executable named `git` is not proof it is Git.
    // PATH/environment overrides are additionally rejected by host admission.
    if args.first()?.as_str() != "git" {
        return None;
    }
    let effects = git_command_effects(args)?;
    if !effects.resolved || effects.relocated || effects.explicit_remote {
        return None;
    }
    let parsed = split_subcommand(&args[1..])?;
    if !effects.effects.remote_read && !effects.effects.remote_write {
        return Some(GitGrantSelection {
            effects: effects.effects,
            remote_name: None,
        });
    }
    if !matches!(parsed.subcommand.as_str(), "fetch" | "ls-remote" | "push") {
        return None;
    }
    let transport = transport_arguments(&parsed.subcommand, &parsed.args);
    if !transport.resolved
        || transport.multiple
        || parsed.args.iter().any(|arg| {
            matches!(arg.as_str(), "--all" | "--stdin") || arg.starts_with("--recurse-submodules")
        })
    {
        return None;
    }
    let name = transport.positional.first()?;
    if name.is_empty() || names_explicit_remote(&[name]) {
        return None;
    }
    Some(GitGrantSelection {
        effects: effects.effects,
        remote_name: Some((*name).clone()),
    })
}

struct ParsedInvocation {
    subcommand: String,
    args: Vec<String>,
    /// A global option injects configuration or changes where Git looks for its
    /// own subcommands: the invocation's EFFECTS are no longer readable.
    rule_override: bool,
    /// A global option moves the invocation's TARGET: the effects are still
    /// readable, but they no longer belong to the repository this host knows.
    target_relocation: bool,
}

/// Split global options from the subcommand. `None` when no subcommand follows.
fn split_subcommand(args: &[String]) -> Option<ParsedInvocation> {
    let mut i = 0;
    let mut rule_override = false;
    let mut target_relocation = false;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--" {
            i += 1;
            break;
        }
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        // Options that move the invocation's target without changing what the
        // subcommand does. Resolved as target relocation, not as unreadable
        // argv: `git -C <dir> status` is the same read pointed somewhere else,
        // and the profile permits reads anywhere. Both spellings
        // (`--git-dir X`, `--git-dir=X`) count.
        const RELOCATIONS: [&str; 4] = ["-C", "--git-dir", "--work-tree", "--namespace"];
        if let Some(name) = RELOCATIONS.iter().find(|name| {
            arg == *name || name.starts_with("--") && arg.starts_with(&format!("{name}="))
        }) {
            target_relocation = true;
            // A long option in `=` form carries its value in the same word.
            i += if *name == arg { 2 } else { 1 };
            continue;
        }
        // Options that inject configuration or change where Git looks for its
        // own subcommands. They can name a program Git executes, so the effect
        // is no longer readable: reported as overrides and never
        // auto-widened. Both spellings (`--config-env X`, `--config-env=X`)
        // count.
        const OVERRIDES: [&str; 4] = ["-c", "--config-env", "--exec-path", "--super-prefix"];
        if let Some(name) = OVERRIDES.iter().find(|name| {
            arg == *name || name.starts_with("--") && arg.starts_with(&format!("{name}="))
        }) {
            rule_override = true;
            // A long option in `=` form carries its value in the same word.
            i += if *name == arg { 2 } else { 1 };
            continue;
        }
        if arg == "--shallow-file" || arg.starts_with("--shallow-file=") {
            rule_override = true;
            i += if arg == "--shallow-file" { 2 } else { 1 };
            continue;
        }
        if arg.starts_with("-C") && !arg.starts_with("--") && arg.len() > 2 {
            // `-C<dir>`: the short option with its value attached.
            target_relocation = true;
            i += 1;
            continue;
        }
        if arg.starts_with("-c") && !arg.starts_with("--") && arg.len() > 2 {
            // `-c<key>=<value>`: the short option with its value attached.
            rule_override = true;
            i += 1;
            continue;
        }
        // Only options whose effects are understood may disappear from argv.
        // In particular `--bare` relocates metadata to the current directory.
        if !matches!(
            arg.as_str(),
            "--no-pager"
                | "--no-optional-locks"
                | "--no-replace-objects"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
                | "--no-lazy-fetch"
                | "--version"
        ) {
            rule_override = true;
        }
        i += 1;
    }
    args.get(i).map(|subcommand| ParsedInvocation {
        subcommand: subcommand.clone(),
        args: args[i + 1..].to_vec(),
        rule_override,
        target_relocation,
    })
}

/// The one place Git's subcommand vocabulary is written down.
fn classify_subcommand(subcommand: &str, args: &[String]) -> GitCommandEffects {
    let read = GitEffects {
        metadata_read: true,
        ..GitEffects::default()
    };
    let metadata = GitEffects {
        metadata_write: true,
        ..GitEffects::default()
    };
    let workspace = GitEffects {
        metadata_write: true,
        workspace_mutation: true,
        ..GitEffects::default()
    };
    let identity = GitEffects {
        metadata_read: true,
        config_write: true,
        ..GitEffects::default()
    };
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let any_prefix = |prefix: &str| args.iter().any(|a| a.starts_with(prefix));
    let positional: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();

    match subcommand {
        // ---- reads --------------------------------------------------------
        "status" | "diff" | "log" | "show" | "rev-parse" | "rev-list" | "ls-files" | "ls-tree"
        | "cat-file" | "describe" | "blame" | "annotate" | "shortlog" | "whatchanged" | "grep"
        | "merge-base" | "cherry" | "name-rev" | "show-ref" | "for-each-ref" | "verify-commit"
        | "verify-tag" | "check-ignore" | "check-attr" | "check-ref-format" | "count-objects"
        | "var" | "version" | "help" | "range-diff" | "diff-tree" | "diff-index" | "diff-files"
        | "verify-pack" | "merge-tree" | "get-tar-commit-id" | "fsck" => {
            GitCommandEffects::resolved(read)
        }

        // `reflog` reads; only its rewriting subcommands are writes.
        "reflog" => match args.first().map(String::as_str) {
            Some("expire") | Some("delete") => GitCommandEffects::resolved(metadata),
            _ => GitCommandEffects::resolved(read),
        },

        // `symbolic-ref` reads; `-d`/`-m` write.
        "symbolic-ref" => {
            if has("-d") || has("--delete") || has("-m") {
                GitCommandEffects::resolved(metadata)
            } else {
                GitCommandEffects::resolved(read)
            }
        }

        // `config` reads only with an explicit query flag.
        "config" => {
            let reads = has("--get")
                || has("--get-all")
                || has("--get-regexp")
                || has("--get-urlmatch")
                || has("--list")
                || has("-l");
            if reads {
                GitCommandEffects::resolved(read)
            } else {
                GitCommandEffects::resolved(identity)
            }
        }

        // `remote`: listing/querying reads, everything else rewrites identity.
        "remote" => {
            let rest = args
                .iter()
                .position(|arg| !matches!(arg.as_str(), "-v" | "--verbose"))
                .map(|index| &args[index..])
                .unwrap_or_default();
            match rest.first().map(String::as_str) {
                None | Some("get-url") => GitCommandEffects::resolved(read),
                Some("show") => {
                    let mut out = GitCommandEffects::resolved(GitEffects {
                        remote_read: !has("-n"),
                        ..read
                    });
                    out.explicit_remote = rest[1..]
                        .iter()
                        .filter(|arg| !arg.starts_with('-'))
                        .any(|arg| names_explicit_remote(&[arg]));
                    out
                }
                Some(_) => GitCommandEffects::resolved(identity),
            }
        }

        // `branch`: listing reads; creating writes metadata; setting an
        // upstream rewrites identity; deleting destroys a ref.
        "branch" => {
            let listing = has("-l")
                || has("--list")
                || has("--show-current")
                || any_prefix("--contains")
                || any_prefix("--merged")
                || any_prefix("--no-merged")
                || any_prefix("--points-at")
                || any_prefix("--format")
                || has("-a")
                || has("--all")
                || has("-r")
                || has("--remotes")
                || has("-v")
                || has("--verbose")
                || args.is_empty();
            let sets_upstream = has("-u")
                || has("--set-upstream")
                || any_prefix("--set-upstream-to")
                || has("--unset-upstream")
                || has("--edit-description");
            let deletes = has("-d") || has("-D") || any_prefix("--delete");
            if sets_upstream {
                GitCommandEffects::resolved(identity)
            } else if deletes {
                GitCommandEffects::resolved(GitEffects {
                    metadata_write: true,
                    irreversible: true,
                    ..GitEffects::default()
                })
            } else if listing {
                GitCommandEffects::resolved(read)
            } else {
                GitCommandEffects::resolved(metadata)
            }
        }

        // `tag`: listing reads; creating writes metadata; deleting destroys.
        "tag" => {
            let listing = has("-l")
                || has("--list")
                || any_prefix("--contains")
                || any_prefix("--points-at")
                || any_prefix("--format")
                || args.is_empty();
            let deletes = has("-d") || any_prefix("--delete");
            if deletes {
                GitCommandEffects::resolved(GitEffects {
                    metadata_write: true,
                    irreversible: true,
                    ..GitEffects::default()
                })
            } else if listing {
                GitCommandEffects::resolved(read)
            } else {
                GitCommandEffects::resolved(metadata)
            }
        }

        // `stash` without a subcommand IS the saving form (`git stash` ==
        // `git stash push`); only explicit inspection reads.
        "stash" => match args.first().map(String::as_str) {
            Some("list") | Some("show") => GitCommandEffects::resolved(read),
            Some("drop") | Some("clear") => GitCommandEffects::resolved(GitEffects {
                metadata_write: true,
                workspace_mutation: true,
                irreversible: true,
                ..GitEffects::default()
            }),
            _ => GitCommandEffects::resolved(workspace),
        },

        // `worktree` manages sibling checkouts of the same repository.
        "worktree" => match args.first().map(String::as_str) {
            None | Some("list") => GitCommandEffects::resolved(read),
            Some("remove") | Some("prune") if has("-f") || has("--force") => {
                GitCommandEffects::resolved(GitEffects {
                    metadata_write: true,
                    workspace_mutation: true,
                    irreversible: true,
                    ..GitEffects::default()
                })
            }
            Some(_) => GitCommandEffects::resolved(workspace),
        },

        // `submodule` records subproject URLs in `.git/config`.
        "submodule" => match args.first().map(String::as_str) {
            None | Some("status") | Some("summary") => GitCommandEffects::resolved(read),
            Some(_) => GitCommandEffects::resolved(identity),
        },

        // `notes` reads when listing/showing.
        "notes" => match args.first().map(String::as_str) {
            Some("list") | Some("show") => GitCommandEffects::resolved(read),
            _ => GitCommandEffects::resolved(metadata),
        },

        // ---- metadata writes ---------------------------------------------
        "add" | "commit" | "update-index" | "hash-object" | "mktree" | "write-tree"
        | "commit-tree" | "pack-refs" | "update-server-info" | "pack-objects" | "index-pack"
        | "unpack-objects" | "commit-graph" | "multi-pack-index" | "repack" | "gc"
        | "maintenance" | "rerere" | "replace" => GitCommandEffects::resolved(metadata),

        // `update-ref` rewrites a ref; deleting one destroys it.
        "update-ref" => {
            let deletes = has("-d") || has("--delete");
            GitCommandEffects::resolved(GitEffects {
                metadata_write: true,
                irreversible: deletes,
                ..GitEffects::default()
            })
        }

        // Changing the sparse cone rewrites which files are checked out.
        "sparse-checkout" => GitCommandEffects::resolved(workspace),

        // `send-email` publishes outside this machine; `archive` and
        // `format-patch` only write new files under the workspace the profile
        // already permits.
        "send-email" => GitCommandEffects::resolved(GitEffects {
            metadata_read: true,
            remote_write: true,
            ..GitEffects::default()
        }),
        "archive" => {
            let parsed = transport_arguments("archive", args);
            let mut out = GitCommandEffects::resolved(GitEffects {
                remote_read: parsed.archive_remote.is_some(),
                ..read
            });
            out.resolved = parsed.resolved;
            if let Some(target) = parsed.archive_remote {
                let target = target.to_string();
                out.explicit_remote = names_explicit_remote(&[&target]);
            }
            out
        }
        "format-patch" => GitCommandEffects::resolved(read),

        // Object pruning deletes unreachable history outright.
        "prune" => GitCommandEffects::resolved(GitEffects {
            metadata_write: true,
            irreversible: true,
            ..GitEffects::default()
        }),

        // ---- Git transport ------------------------------------------------
        // A read of the remote that writes only locally fetched metadata.
        "fetch" => {
            let sets_upstream =
                has("-u") || has("--set-upstream") || any_prefix("--set-upstream-to");
            let mut out = GitCommandEffects::resolved(GitEffects {
                metadata_write: true,
                remote_read: true,
                config_write: sets_upstream,
                ..GitEffects::default()
            });
            let parsed = transport_arguments("fetch", args);
            out.resolved = parsed.resolved;
            out.explicit_remote = if parsed.multiple {
                parsed
                    .positional
                    .iter()
                    .any(|target| names_explicit_remote(&[target]))
            } else {
                names_explicit_remote(&parsed.positional)
            };
            out
        }
        "ls-remote" => {
            let mut out = GitCommandEffects::resolved(GitEffects {
                metadata_read: true,
                remote_read: true,
                ..GitEffects::default()
            });
            let parsed = transport_arguments("ls-remote", args);
            out.resolved = parsed.resolved;
            out.explicit_remote = if parsed.multiple {
                parsed
                    .positional
                    .iter()
                    .any(|target| names_explicit_remote(&[target]))
            } else {
                names_explicit_remote(&parsed.positional)
            };
            out
        }
        // `pull` is fetch PLUS a working-tree merge/rebase. It must never ride
        // on the fetch decision.
        "pull" => {
            let sets_upstream = has("-u") || has("--set-upstream");
            let mut out = GitCommandEffects::resolved(GitEffects {
                metadata_write: true,
                workspace_mutation: true,
                remote_read: true,
                config_write: sets_upstream,
                ..GitEffects::default()
            });
            let parsed = transport_arguments("pull", args);
            out.resolved = parsed.resolved;
            out.explicit_remote = if parsed.multiple {
                parsed
                    .positional
                    .iter()
                    .any(|target| names_explicit_remote(&[target]))
            } else {
                names_explicit_remote(&parsed.positional)
            };
            out
        }
        "push" => {
            let force = has("-f")
                || has("--force")
                || any_prefix("--force-with-lease")
                || any_prefix("--force-if-includes")
                || has("--mirror")
                || has("--delete")
                || has("-d")
                || has("--prune")
                || positional
                    .iter()
                    .any(|a| a.starts_with('+') || a.starts_with(':'));
            let sets_upstream = has("-u") || has("--set-upstream");
            let mut out = GitCommandEffects::resolved(GitEffects {
                metadata_read: true,
                metadata_write: true,
                remote_write: true,
                config_write: sets_upstream,
                irreversible: force,
                ..GitEffects::default()
            });
            let parsed = transport_arguments("push", args);
            out.resolved = parsed.resolved;
            out.explicit_remote = names_explicit_remote(&parsed.positional);
            out
        }
        // `clone` creates a repository BESIDE the current one; it never writes
        // the current repository's metadata, so there is no configured remote
        // to prefer and no metadata capability to widen. Its own `.git` is new
        // and never sealed.
        "clone" => {
            let mut out = GitCommandEffects::resolved(GitEffects {
                metadata_write: true,
                workspace_mutation: true,
                remote_read: true,
                ..GitEffects::default()
            });
            out.explicit_remote = true;
            out
        }

        // ---- working-tree mutations --------------------------------------
        "merge" | "rebase" | "cherry-pick" | "revert" | "am" | "apply" | "bisect" => {
            GitCommandEffects::resolved(workspace)
        }
        "checkout" | "switch" => {
            let forced = has("-f") || has("--force") || args.iter().any(|a| a == "--");
            GitCommandEffects::resolved(GitEffects {
                irreversible: forced,
                ..workspace
            })
        }
        // `reset` without `--hard` rewrites the index; `--hard` also discards
        // uncommitted working-tree changes.
        "reset" => {
            let hard = has("--hard") || has("--merge");
            GitCommandEffects::resolved(GitEffects {
                irreversible: hard,
                ..workspace
            })
        }
        // `restore` exists to overwrite working-tree files.
        "restore" => GitCommandEffects::resolved(GitEffects {
            irreversible: true,
            ..workspace
        }),
        "clean" => {
            let forced = args.iter().any(|a| a.starts_with("-f") || a == "--force");
            GitCommandEffects::resolved(GitEffects {
                workspace_mutation: true,
                irreversible: forced,
                ..GitEffects::default()
            })
        }
        "rm" | "mv" => GitCommandEffects::resolved(GitEffects {
            irreversible: subcommand == "rm",
            ..workspace
        }),

        // `init` writes repository identity wherever it lands.
        "init" => GitCommandEffects::resolved(identity),

        // Anything else: we do not know what it does, so we do not guess.
        _ => GitCommandEffects::unresolved(read),
    }
}

/// Consume Git transport options before choosing repository operands. Values
/// such as a depth or sort key are not repository identities. Unknown options
/// and malformed values cannot earn an automatic metadata-write capability.
struct TransportArguments<'a> {
    positional: Vec<&'a String>,
    archive_remote: Option<&'a str>,
    multiple: bool,
    resolved: bool,
}

#[derive(Clone, Copy)]
enum TransportOption {
    Flag,
    Value,
    OptionalValue,
}

fn transport_arguments<'a>(subcommand: &str, args: &'a [String]) -> TransportArguments<'a> {
    let mut out = TransportArguments {
        positional: Vec::new(),
        archive_remote: None,
        multiple: false,
        resolved: true,
    };
    let mut index = 0;
    let mut options = true;
    while let Some(arg) = args.get(index) {
        index += 1;
        if options && arg == "--" {
            options = false;
            continue;
        }
        if !options || !arg.starts_with('-') || arg == "-" {
            out.positional.push(arg);
            continue;
        }
        let (name, inline_value) = if arg.starts_with("--") {
            arg.split_once('=')
                .map_or((arg.as_str(), None), |(name, value)| (name, Some(value)))
        } else if arg.len() > 2 {
            // Git accepts attached short-option values (-j2, -Xours). Do not
            // guess at combined flag clusters: unsupported forms ask instead.
            let Some(name) = arg.get(..2) else {
                out.resolved = false;
                continue;
            };
            (name, arg.get(2..))
        } else {
            (arg.as_str(), None)
        };
        let Some(kind) = transport_option(subcommand, name) else {
            out.resolved = false;
            continue;
        };
        let value = match kind {
            TransportOption::Flag => {
                if inline_value.is_some() {
                    out.resolved = false;
                }
                None
            }
            TransportOption::OptionalValue => inline_value,
            TransportOption::Value => {
                if let Some(value) = inline_value {
                    Some(value)
                } else {
                    let value = args.get(index);
                    index += usize::from(value.is_some());
                    value.map(String::as_str)
                }
            }
        };
        if matches!(kind, TransportOption::Value) && value.is_none_or(str::is_empty) {
            out.resolved = false;
        }
        if matches!(
            name,
            "--upload-pack" | "--receive-pack" | "--exec" | "--repo"
        ) {
            // A caller-selected executable is a separate authority expansion,
            // even when its value and the repository are mechanically readable.
            out.resolved = false;
        }
        if matches!(name, "--multiple" | "-m") {
            out.multiple = true;
        }
        if subcommand == "archive" && name == "--remote" {
            // Git parse-options overwrites this scalar on repetition: the
            // final target is the one upload-archive actually contacts.
            out.archive_remote = value;
        }
    }
    out
}

/// Option arity from Git's fetch, ls-remote, pull and archive documentation.
/// Kept beside effects so every transport target uses the same interpretation.
fn transport_option(subcommand: &str, name: &str) -> Option<TransportOption> {
    use TransportOption::{Flag, OptionalValue, Value};
    if let Some(positive) = name.strip_prefix("--no-") {
        let positive = format!("--{positive}");
        return transport_option(subcommand, &positive).map(|_| Flag);
    }
    match (subcommand, name) {
        ("push", "--repo" | "--receive-pack" | "--exec" | "--push-option" | "-o") => Some(Value),
        ("push", "--force-with-lease" | "--recurse-submodules" | "--signed") => Some(OptionalValue),
        (
            "push",
            "--all"
            | "--branches"
            | "--prune"
            | "--mirror"
            | "--tags"
            | "--follow-tags"
            | "--dry-run"
            | "-n"
            | "--porcelain"
            | "--delete"
            | "-d"
            | "--force"
            | "-f"
            | "--force-if-includes"
            | "--set-upstream"
            | "-u"
            | "--thin"
            | "--atomic"
            | "--verbose"
            | "-v"
            | "--quiet"
            | "-q"
            | "--progress"
            | "--ipv4"
            | "-4"
            | "--ipv6"
            | "-6"
            | "--verify",
        ) => Some(Flag),
        (
            "fetch" | "pull",
            "--depth" | "--deepen" | "--shallow-since" | "--shallow-exclude" | "--refmap"
            | "--server-option" | "-o" | "--negotiation-tip" | "--upload-pack",
        ) => Some(Value),
        ("fetch", "--jobs" | "-j" | "--filter") => Some(Value),
        ("pull", "--jobs") => Some(OptionalValue),
        ("pull", "-j" | "--cleanup" | "--strategy" | "-s" | "--strategy-option" | "-X") => {
            Some(Value)
        }
        ("fetch" | "pull", "--recurse-submodules") => Some(OptionalValue),
        ("pull", "--rebase" | "--log" | "--signoff" | "--gpg-sign" | "-S") => Some(OptionalValue),
        (
            "fetch" | "pull",
            "--all"
            | "--append"
            | "-a"
            | "--force"
            | "-f"
            | "--tags"
            | "-t"
            | "-n"
            | "--prune"
            | "-p"
            | "--dry-run"
            | "--keep"
            | "-k"
            | "--progress"
            | "--unshallow"
            | "--update-shallow"
            | "--ipv4"
            | "-4"
            | "--ipv6"
            | "-6"
            | "--show-forced-updates"
            | "--set-upstream"
            | "--verbose"
            | "-v"
            | "--quiet"
            | "-q",
        ) => Some(Flag),
        (
            "fetch",
            "--atomic"
            | "--multiple"
            | "-m"
            | "--prefetch"
            | "--prune-tags"
            | "-P"
            | "--porcelain"
            | "--write-fetch-head"
            | "--update-head-ok"
            | "-u"
            | "--refetch"
            | "--negotiate-only"
            | "--auto-maintenance"
            | "--auto-gc"
            | "--write-commit-graph"
            | "--stdin",
        ) => Some(Flag),
        (
            "pull",
            "-r"
            | "--stat"
            | "--squash"
            | "--commit"
            | "--edit"
            | "-e"
            | "--ff"
            | "--ff-only"
            | "--verify"
            | "--verify-signatures"
            | "--autostash"
            | "--allow-unrelated-histories",
        ) => Some(Flag),
        ("ls-remote", "--upload-pack" | "--sort" | "--server-option" | "-o") => Some(Value),
        (
            "ls-remote",
            "--quiet" | "-q" | "--tags" | "-t" | "--heads" | "-h" | "--branches" | "-b" | "--refs"
            | "--get-url" | "--exit-code" | "--symref",
        ) => Some(Flag),
        (
            "archive",
            "--format" | "--prefix" | "--add-file" | "--add-virtual-file" | "--output" | "-o"
            | "--mtime" | "--remote" | "--exec",
        ) => Some(Value),
        (
            "archive",
            "--worktree-attributes"
            | "--verbose"
            | "-v"
            | "--list"
            | "-l"
            | "-0"
            | "-1"
            | "-2"
            | "-3"
            | "-4"
            | "-5"
            | "-6"
            | "-7"
            | "-8"
            | "-9",
        ) => Some(Flag),
        _ => None,
    }
}

/// Whether a positional argument names a remote by URL or path instead of by
/// the name of a remote the repository configures.
///
/// This is the whole "trusted remote" test available to a sandbox: a name is
/// resolved by Git from the repository's own configuration, while a URL or
/// path is chosen by whoever wrote the command line.
fn names_explicit_remote(positional: &[&String]) -> bool {
    let Some(first) = positional.first() else {
        return false;
    };
    let value = first.as_str();
    value.contains(':')
        || value.contains('/')
        || value.contains('\\')
        || value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
        || std::path::Path::new(value).is_absolute()
}

fn basename(program: &str) -> &str {
    std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
}

#[cfg(test)]
mod grant_selection_tests {
    use super::*;
    fn selection(command: &[&str]) -> Option<GitGrantSelection> {
        git_grant_selection(
            &command
                .iter()
                .map(|word| word.to_string())
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn named_transport_targets_and_force_have_distinct_effects() {
        let read = selection(&["git", "fetch", "--depth", "1", "origin"]).unwrap();
        assert_eq!(read.remote_name.as_deref(), Some("origin"));
        assert!(read.effects.remote_read && !read.effects.remote_write);
        let push = selection(&["git", "push", "origin", "main"]).unwrap();
        let force = selection(&["git", "push", "--force-with-lease", "origin", "main"]).unwrap();
        assert_eq!(push.remote_name.as_deref(), Some("origin"));
        assert!(!push.effects.irreversible);
        assert!(force.effects.irreversible);
        for command in [
            vec!["git", "push", "origin", ":main"],
            vec!["git", "push", "-d", "origin", "main"],
            vec!["git", "push", "--prune", "origin", "main"],
        ] {
            assert!(selection(&command).unwrap().effects.irreversible);
        }
    }
    #[test]
    fn unknown_targets_overrides_and_multiple_transports_are_not_reusable() {
        for command in [
            vec!["git", "fetch", "https://example.com/repo"],
            vec!["git", "-c", "x=y", "fetch", "origin"],
            vec!["git", "fetch", "--multiple", "origin", "other"],
            vec!["git", "fetch", "--recurse-submodules", "origin"],
            vec!["git", "push", "--receive-pack=evil", "origin"],
            vec!["/tmp/selected/git", "push", "origin"],
        ] {
            assert!(selection(&command).is_none(), "{command:?}");
        }
    }
}
