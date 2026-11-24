//! Durable per-user skill activation preferences.
//!
//! Disabling a Skill keeps its definition on disk and keeps it visible in the
//! Skills surface, but removes it from the model index, explicit invocation and
//! `load_skill`. The file is user-global (`state/skills.toml`) because a Skill
//! identity does not depend on a workspace.
//!
//! Project-scope identities are project-local even though the file is global:
//! `disabled` holds `user:`/`builtin:` identities, while project disables live
//! under `[projects]` keyed by the canonical project root. Two projects with a
//! same-named `.leveler/skills/deploy` therefore do not affect each other.
//!
//! A malformed preferences file is a hard error, not an empty preference set:
//! the registry fails closed so a typo cannot silently re-enable procedures the
//! user meant to turn off.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::name::validate_read_name;
use crate::registry::SkillScope;

/// The stable name of a project in `state/skills.toml`.
///
/// The canonical path is used directly rather than a hash: it is already the
/// identity the user can recognize in the file, and TOML quoted keys carry any
/// path spelling. A moved or renamed project loses its project-scope disables,
/// which is preferable to silently applying them to a different checkout.
pub fn project_key(root: &Path) -> String {
    let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut key = canonical.to_string_lossy().replace('\\', "/");
    while key.len() > 1 && key.ends_with('/') {
        key.pop();
    }
    key
}

/// The identity a user sees for a disabled skill: `scope:name`.
///
/// Project scope is qualified by the project in the page, but the file stores
/// it under that project's table; this string is display/audit only.
pub fn disabled_key(scope: SkillScope, name: &str) -> String {
    format!("{}:{name}", scope.as_str())
}

/// On-disk preferences. Unknown keys are rejected so a misspelled field cannot
/// be silently ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkillPreferences {
    /// `user:name` and `builtin:name` identities. Project disables are stored
    /// under [`Self::projects`] so a same-named skill in another checkout is
    /// unaffected.
    #[serde(default)]
    pub disabled: BTreeSet<String>,
    /// Canonical project root -> disabled Skill names in that project.
    #[serde(default)]
    pub projects: BTreeMap<String, BTreeSet<String>>,
}

impl SkillPreferences {
    /// Whether `name` in `scope` is disabled. `project` must be the canonical
    /// project key when `scope` is `Project`; a missing key means the caller
    /// has no project and project disables do not apply.
    pub fn is_disabled(&self, scope: SkillScope, name: &str, project: Option<&str>) -> bool {
        match scope {
            SkillScope::Project => project
                .and_then(|project| self.projects.get(project))
                .is_some_and(|names| names.contains(name)),
            SkillScope::User | SkillScope::Builtin => {
                self.disabled.contains(&disabled_key(scope, name))
            }
        }
    }

    /// Toggle one identity. Returns whether it is now disabled.
    ///
    /// Project scope requires the project key: without it, a toggle cannot be
    /// scoped and must fail rather than write a global disable.
    pub fn set_disabled(
        &mut self,
        scope: SkillScope,
        name: &str,
        project: Option<&str>,
        disabled: bool,
    ) -> Result<bool, String> {
        match scope {
            SkillScope::Project => {
                let project = project
                    .filter(|project| !project.is_empty())
                    .ok_or_else(|| "project scope needs a project key".to_string())?;
                let names = self.projects.entry(project.to_string()).or_default();
                if disabled {
                    names.insert(name.to_string());
                } else {
                    names.remove(name);
                }
            }
            SkillScope::User | SkillScope::Builtin => {
                let key = disabled_key(scope, name);
                if disabled {
                    self.disabled.insert(key);
                } else {
                    self.disabled.remove(&key);
                }
            }
        }
        Ok(disabled)
    }

    /// Reject identities that could never address a registered Skill. Unknown
    /// *names* are allowed: a project may be missing today and return later.
    fn validate(&self) -> Result<(), String> {
        for key in &self.disabled {
            let Some((scope, name)) = key.split_once(':') else {
                return Err(format!("disabled entry `{key}` must use scope:name"));
            };
            if scope == "project" {
                return Err(format!(
                    "disabled entry `{key}` must use a project-scoped key under [projects]"
                ));
            }
            if !matches!(scope, "user" | "builtin") {
                return Err(format!(
                    "disabled entry `{key}` has unknown scope `{scope}`"
                ));
            }
            validate_read_name(name).map_err(|error| format!("disabled entry `{key}`: {error}"))?;
        }
        for (project, names) in &self.projects {
            if project.is_empty() {
                return Err("a [projects] table has an empty project key".to_string());
            }
            for name in names {
                validate_read_name(name)
                    .map_err(|error| format!("disabled entry for `{project}`: {error}"))?;
            }
        }
        Ok(())
    }

    /// Atomically publish the preferences file. The staged sibling plus rename
    /// means another process never observes a half-written file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            leveler_core::private_fs::restrict_dir(parent)?;
        }
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let staged = path.with_extension("toml.tmp");
        std::fs::write(&staged, text)?;
        leveler_core::private_fs::restrict_file(&staged)?;
        std::fs::rename(&staged, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&staged);
        })
    }
}

/// Loaded preferences or the reason they could not be read.
///
/// `Unavailable` is fail-closed: every Skill is treated as disabled until the
/// file is fixed, and callers render `error()` so the state is diagnosable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreferencesState {
    Loaded {
        preferences: SkillPreferences,
        project: Option<String>,
    },
    Unavailable {
        path: PathBuf,
        error: String,
        project: Option<String>,
    },
}

impl Default for PreferencesState {
    fn default() -> Self {
        Self::Loaded {
            preferences: SkillPreferences::default(),
            project: None,
        }
    }
}

impl PreferencesState {
    /// Load the user-global preferences, bound to one project.
    ///
    /// Pass the project root the registry is resolving; `None` is a registry
    /// with no project scope, where project disables are not applicable.
    pub fn load(home: &leveler_core::home::LevelerHome, project_root: Option<&Path>) -> Self {
        Self::load_path(&home.skills_preferences_file(), project_root)
    }

    /// Read and validate one preferences file. An absent file is the default
    /// (nothing disabled); every other failure is visible and fail-closed.
    pub fn load_path(path: &Path, project_root: Option<&Path>) -> Self {
        let project = project_root.map(project_key);
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Self::Loaded {
                    preferences: SkillPreferences::default(),
                    project,
                };
            }
            Err(error) => {
                return Self::Unavailable {
                    path: path.to_path_buf(),
                    error: error.to_string(),
                    project,
                };
            }
        };
        match toml::from_str::<SkillPreferences>(&text) {
            Ok(preferences) => match preferences.validate() {
                Ok(()) => Self::Loaded {
                    preferences,
                    project,
                },
                Err(error) => Self::Unavailable {
                    path: path.to_path_buf(),
                    error,
                    project,
                },
            },
            Err(error) => Self::Unavailable {
                path: path.to_path_buf(),
                error: error.to_string(),
                project,
            },
        }
    }

    pub fn is_disabled(&self, scope: SkillScope, name: &str) -> bool {
        match self {
            Self::Loaded {
                preferences,
                project,
            } => preferences.is_disabled(scope, name, project.as_deref()),
            // Fail closed: an unreadable preferences file must not enable
            // procedures the user may have disabled.
            Self::Unavailable { .. } => true,
        }
    }

    /// Human-readable reason for the `Disabled` state of one Skill.
    pub fn reason_for(&self, scope: SkillScope, name: &str) -> String {
        match self {
            Self::Loaded { .. } => {
                format!(
                    "`{}` is disabled in skill preferences",
                    disabled_key(scope, name)
                )
            }
            Self::Unavailable { path, error, .. } => format!(
                "skill preferences at {} could not be read: {error}",
                path.display()
            ),
        }
    }

    /// The file-level error, if preferences could not be read.
    pub fn error(&self) -> Option<&str> {
        match self {
            Self::Loaded { .. } => None,
            Self::Unavailable { error, .. } => Some(error),
        }
    }

    /// The file this state was loaded from, when known.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Loaded { .. } => None,
            Self::Unavailable { path, .. } => Some(path),
        }
    }

    /// The canonical project key this state is bound to.
    pub fn project(&self) -> Option<&str> {
        match self {
            Self::Loaded { project, .. } | Self::Unavailable { project, .. } => project.as_deref(),
        }
    }

    /// The loaded preferences, or `None` when fail-closed.
    pub fn preferences(&self) -> Option<&SkillPreferences> {
        match self {
            Self::Loaded { preferences, .. } => Some(preferences),
            Self::Unavailable { .. } => None,
        }
    }

    /// A mutable copy for a page toggle, or `None` when the file is unreadable.
    ///
    /// Callers save the clone and reload the registry only after the atomic
    /// write succeeds, so a failed write never leaves the UI showing a state
    /// that is not on disk.
    pub fn editable_preferences(&self) -> Option<SkillPreferences> {
        self.preferences().cloned()
    }
}
