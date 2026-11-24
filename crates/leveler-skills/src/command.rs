//! Slash-command projection of the resolved skill registry.
//!
//! A Skill is invoked as `/name [args]` (or `/scope:name [args]` when the bare
//! name collides with a built-in command). This module only decides *whether*
//! that token addresses a usable Skill and which one; loading goes through
//! [`crate::SkillRegistry::load_skill`], so there is no second discovery path.
//!
//! Built-in command names are reserved. A Skill named `help` stays registered
//! and loadable as `$help`, but its slash identity is `project:help` (or
//! `user:help`), so `/help` can never be captured by a package.

use crate::audit::{SkillDeliveryFailure, lookup_failure};
use crate::name::{is_mentionable, validate_read_name};
use crate::registry::{
    SkillDetail, SkillEntry, SkillLookupError, SkillRegistry, SkillScope, SkillSource, SkillState,
};

/// One Skill addressable through the slash namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommand {
    /// The canonical registry name (directory name).
    pub name: String,
    pub scope: SkillScope,
    pub source: SkillSource,
    pub description: String,
    /// Display-only argument shape from `SKILL.md` frontmatter.
    pub argument_hint: Option<String>,
    /// What the completion menu offers: the bare name, or `scope:name` when
    /// the bare name is reserved by a built-in command.
    pub advertised: String,
    /// Always `scope:name`; unambiguous even when the advertised form is bare.
    pub canonical: String,
    /// Whether `$name` can spell this name. Dotted names are slash-only.
    pub mentionable: bool,
}

impl SkillCommand {
    /// Whether the advertised form is qualified because of a reserved name.
    pub fn is_qualified(&self) -> bool {
        self.advertised.contains(':')
    }
}

/// Every usable Skill, projected for the `/` completion menu.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCommandCatalog {
    pub commands: Vec<SkillCommand>,
}

impl SkillCommandCatalog {
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Find by advertised or canonical form, so both `/deploy` and
    /// `/project:deploy` resolve to the same row.
    pub fn get(&self, token: &str) -> Option<&SkillCommand> {
        let token = token.trim_start_matches('/');
        self.commands
            .iter()
            .find(|command| command.advertised == token || command.canonical == token)
    }
}

/// Why a slash token did not address a usable Skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillCommandError {
    /// No registered Skill owns the name. Callers that preflight user input
    /// should treat this as prose, not as a Skill error.
    NotFound { token: String },
    /// The bare name belongs to a built-in command.
    Reserved { token: String },
    /// The Skill exists but is disabled or invalid.
    Unavailable {
        token: String,
        name: String,
        scope: SkillScope,
        source: SkillSource,
        state: String,
    },
    /// The package sets `user-invocable: false`; only the model may load it.
    UserInvocationDisabled {
        token: String,
        name: String,
        scope: SkillScope,
        source: SkillSource,
    },
    /// No trusted entry owns the name, but an unapproved package does.
    Untrusted {
        token: String,
        name: String,
        scope: SkillScope,
        source: SkillSource,
        state: String,
    },
    /// The requested `scope:name` exists but a higher-precedence definition
    /// owns the name. The registry never falls back to the shadowed package.
    Shadowed {
        token: String,
        name: String,
        scope: SkillScope,
        source: SkillSource,
    },
}

impl std::fmt::Display for SkillCommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound { token } => write!(f, "no skill command named `/{token}`"),
            Self::Reserved { token } => write!(
                f,
                "`/{token}` is a built-in command; a skill with that name is invoked by its \
                 qualified form `/scope:{token}`"
            ),
            Self::Unavailable {
                token,
                name,
                scope,
                source,
                state,
            } => write!(
                f,
                "`/{token}` cannot run: skill `{name}` ({source} · {scope}) is {state}"
            ),
            Self::UserInvocationDisabled {
                token,
                name,
                scope,
                source,
            } => write!(
                f,
                "`/{token}` cannot run: skill `{name}` ({source} · {scope}) sets \
                 `user-invocable: false`; the model may still discover it"
            ),
            Self::Untrusted {
                token,
                name,
                scope,
                source,
                state,
            } => write!(
                f,
                "`/{token}` cannot run: skill `{name}` ({source} · {scope}) is present but not \
                 trusted ({state}); review it with /trust"
            ),
            Self::Shadowed {
                token,
                name,
                scope,
                source,
            } => write!(
                f,
                "`/{token}` cannot run: `{name}` in {scope} ({source}) is shadowed by the \
                 active definition; use /skills to inspect it"
            ),
        }
    }
}

impl std::error::Error for SkillCommandError {}

/// The first-line `/token` decision, including the loaded body when it worked.
///
/// `None` from [`SkillRegistry::resolve_command_request`] means "not a Skill
/// command at all" (built-in, path, prose, unknown). `Some` with an `error` is
/// a known Skill request that must not be sent as a normal message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillCommandResolution {
    /// The token as typed, with its leading slash.
    pub token: String,
    /// Resolved identity, present when the name addressed a registered entry.
    pub command: Option<SkillCommand>,
    /// The verified body, when loading succeeded.
    pub loaded: Option<SkillDetail>,
    /// The refusal, when the name addressed an entry that could not load.
    pub error: Option<String>,
    /// Stable delivery code for the refusal, when there is one. The message
    /// above is for humans and may name a host path; this code is what the
    /// durable audit records.
    pub failure: Option<SkillDeliveryFailure>,
}

impl SkillCommandResolution {
    pub fn is_loaded(&self) -> bool {
        self.loaded.is_some()
    }
}

impl SkillRegistry {
    /// Project every available entry into the slash namespace.
    ///
    /// `reserved` holds built-in command primary names and aliases. A collision
    /// moves the Skill to its qualified form; the built-in keeps the bare name.
    pub fn command_catalog(&self, reserved: &[&str]) -> SkillCommandCatalog {
        let reserved: Vec<&str> = reserved
            .iter()
            .map(|token| token.trim_start_matches('/'))
            .collect();
        let mut commands = Vec::new();
        for entry in self
            .entries()
            .iter()
            .filter(|entry| entry.is_available() && entry.user_invocable)
        {
            let Some(description) = entry.description.clone() else {
                continue;
            };
            let canonical = canonical_name(entry);
            let advertised = if reserved.contains(&entry.name.as_str()) {
                canonical.clone()
            } else {
                entry.name.clone()
            };
            commands.push(SkillCommand {
                name: entry.name.clone(),
                scope: entry.scope,
                source: entry.source,
                description,
                argument_hint: entry.argument_hint.clone(),
                advertised,
                canonical,
                mentionable: is_mentionable(&entry.name),
            });
        }
        commands.sort_by(|a, b| a.advertised.cmp(&b.advertised));
        SkillCommandCatalog { commands }
    }

    /// Resolve one slash token to a usable identity.
    ///
    /// The token may be bare (`deploy`), qualified (`project:deploy`), or carry
    /// a leading slash. Qualification only selects an entry scope; it never
    /// falls back to a shadowed definition.
    pub fn resolve_command(
        &self,
        token: &str,
        reserved: &[&str],
    ) -> Result<SkillCommand, SkillCommandError> {
        let token = token.trim().trim_start_matches('/');
        if token.is_empty() {
            return Err(SkillCommandError::NotFound {
                token: token.to_string(),
            });
        }
        if reserved
            .iter()
            .any(|name| name.trim_start_matches('/') == token)
        {
            return Err(SkillCommandError::Reserved {
                token: token.to_string(),
            });
        }
        if let Some((scope_text, name)) = token.split_once(':') {
            let Some(scope) = parse_scope(scope_text) else {
                return Err(SkillCommandError::NotFound {
                    token: token.to_string(),
                });
            };
            let Some(entry) = self.get(name) else {
                return Err(self.unknown_name(token, name));
            };
            if entry.scope != scope {
                let source = entry
                    .shadowed
                    .iter()
                    .find(|shadowed| shadowed.scope == scope)
                    .map(|shadowed| shadowed.source)
                    .unwrap_or(entry.source);
                return Err(SkillCommandError::Shadowed {
                    token: token.to_string(),
                    name: name.to_string(),
                    scope,
                    source,
                });
            }
            return command_for(entry, token);
        }
        let Some(entry) = self.get(token) else {
            return Err(self.unknown_name(token, token));
        };
        command_for(entry, token)
    }

    /// Decide a first-line slash request.
    ///
    /// Only a syntactically plausible Skill token is considered: an absolute
    /// path (`/Users/...`) or a built-in command returns `None`. An unknown
    /// plausible name also returns `None`, so the runtime never turns a typo
    /// into a Skill error; a *known* disabled/invalid/untrusted/shadowed name
    /// returns a resolution with `error` set.
    pub fn resolve_command_request(
        &self,
        text: &str,
        reserved: &[&str],
    ) -> Option<SkillCommandResolution> {
        let token = leading_skill_token(text)?;
        match self.resolve_command(&token, reserved) {
            Ok(command) => match self.load_skill_explicit(&command.name) {
                Ok(loaded) => Some(SkillCommandResolution {
                    token: format!("/{token}"),
                    command: Some(command),
                    loaded: Some(loaded),
                    error: None,
                    failure: None,
                }),
                Err(error) => Some(SkillCommandResolution {
                    token: format!("/{token}"),
                    command: Some(command),
                    loaded: None,
                    error: Some(error.to_string()),
                    failure: Some(lookup_failure(&error)),
                }),
            },
            Err(SkillCommandError::NotFound { .. } | SkillCommandError::Reserved { .. }) => None,
            Err(error) => Some(SkillCommandResolution {
                token: format!("/{token}"),
                command: None,
                loaded: None,
                error: Some(error.to_string()),
                failure: Some(command_failure(&error)),
            }),
        }
    }

    fn unknown_name(&self, token: &str, name: &str) -> SkillCommandError {
        if let Some(untrusted) = self.untrusted_skill(name) {
            return SkillCommandError::Untrusted {
                token: token.to_string(),
                name: untrusted.name.clone(),
                scope: untrusted.scope,
                source: untrusted.source,
                state: untrusted.state().to_string(),
            };
        }
        SkillCommandError::NotFound {
            token: token.to_string(),
        }
    }
}

/// The stable delivery code for a slash refusal.
fn command_failure(error: &SkillCommandError) -> SkillDeliveryFailure {
    match error {
        SkillCommandError::NotFound { .. } | SkillCommandError::Reserved { .. } => {
            SkillDeliveryFailure::NotFound
        }
        SkillCommandError::Unavailable { state, .. } => {
            if state.starts_with("disabled") {
                SkillDeliveryFailure::Disabled
            } else {
                SkillDeliveryFailure::Invalid
            }
        }
        SkillCommandError::UserInvocationDisabled { .. } => {
            SkillDeliveryFailure::UserInvocationDisabled
        }
        SkillCommandError::Untrusted { .. } => SkillDeliveryFailure::Untrusted,
        SkillCommandError::Shadowed { .. } => SkillDeliveryFailure::Shadowed,
    }
}

fn canonical_name(entry: &SkillEntry) -> String {
    format!("{}:{}", entry.scope.as_str(), entry.name)
}

fn command_for(entry: &SkillEntry, token: &str) -> Result<SkillCommand, SkillCommandError> {
    match &entry.state {
        SkillState::Available if !entry.user_invocable => {
            Err(SkillCommandError::UserInvocationDisabled {
                token: token.to_string(),
                name: entry.name.clone(),
                scope: entry.scope,
                source: entry.source,
            })
        }
        SkillState::Available => Ok(SkillCommand {
            name: entry.name.clone(),
            scope: entry.scope,
            source: entry.source,
            description: entry.description.clone().unwrap_or_default(),
            argument_hint: entry.argument_hint.clone(),
            advertised: if token.contains(':') {
                token.to_string()
            } else {
                entry.name.clone()
            },
            canonical: canonical_name(entry),
            mentionable: is_mentionable(&entry.name),
        }),
        SkillState::Disabled { reason } => Err(SkillCommandError::Unavailable {
            token: token.to_string(),
            name: entry.name.clone(),
            scope: entry.scope,
            source: entry.source,
            state: format!("disabled: {reason}"),
        }),
        SkillState::Invalid(reason) => Err(SkillCommandError::Unavailable {
            token: token.to_string(),
            name: entry.name.clone(),
            scope: entry.scope,
            source: entry.source,
            state: format!("invalid: {reason}"),
        }),
    }
}

fn parse_scope(text: &str) -> Option<SkillScope> {
    match text {
        "project" => Some(SkillScope::Project),
        "user" => Some(SkillScope::User),
        "builtin" => Some(SkillScope::Builtin),
        _ => None,
    }
}

/// The first-line `/token`, when it is shaped like a Skill name rather than a
/// path or prose. A trailing argument is allowed: `/deploy staging` yields
/// `deploy`.
///
/// Callers that only need to know whether a message *is* a Skill request
/// (for example, refusing one mid-turn) should use this rather than
/// [`SkillRegistry::resolve_command_request`], which also reads the package.
pub fn leading_skill_token(text: &str) -> Option<String> {
    let first_line = text.split('\n').next()?;
    let rest = first_line.trim_start();
    let token = rest
        .strip_prefix('/')?
        .split_whitespace()
        .next()
        .unwrap_or_default();
    if plausible_skill_token(token) {
        Some(token.to_string())
    } else {
        None
    }
}

fn plausible_skill_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if let Some((scope, name)) = token.split_once(':') {
        matches!(scope, "project" | "user" | "builtin") && validate_read_name(name).is_ok()
    } else {
        validate_read_name(token).is_ok()
    }
}

impl SkillCommandResolution {
    /// A refusal that came from a known registry entry (not a typo).
    pub fn rejection(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The registry error behind a failed load, for callers that need the
    /// typed variant rather than display text.
    pub fn lookup_error(&self, registry: &SkillRegistry) -> Option<SkillLookupError> {
        let name = self.command.as_ref()?.name.as_str();
        registry.load_skill(name).err()
    }
}
