//! The Skill registry through the client protocol.
//!
//! A client lists and inspects the same registry the runtime's model index,
//! `$mention`, `load_skill` and explicit `/name` use. Untrusted packages are
//! listed with their trust state and a bounded preview for human inspection;
//! nothing here is admitted to a turn.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use leveler_app::{Application, InProcessRuntimeClient};
use leveler_client_protocol::{
    ClientCommand, CommandId, InteractiveRuntimeClient, RuntimeEvent, SessionId, UiSkillScope,
    UiSkillSource, UiSkillStatus,
};
use leveler_definition_trust::{
    ApprovalSurface, DefinitionKind, DiscoveryRoots, approve, discover,
};
use leveler_execution::PermissionProfile;
use leveler_model::ModelRef;
use leveler_project::Layout;
use leveler_skills::{SkillPreferences, SkillScope};

/// One process-wide home: the installed environment is first-install-wins.
///
/// `Application::assemble` snapshots the PROCESS environment, so the user-home
/// roots are redirected too. Without this, a fixture would discover the real
/// user's `~/.codex`, `~/.agents` and `~/.claude` skills.
fn home() -> &'static Path {
    static HOME: OnceLock<tempfile::TempDir> = OnceLock::new();
    HOME.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let user_home = dir.path().join("user-home");
        std::fs::create_dir_all(&user_home).unwrap();
        let _ = leveler_core::install_environment(leveler_core::EnvSnapshot::new(
            [(
                std::ffi::OsString::from("LEVELER_HOME"),
                dir.path().as_os_str().to_os_string(),
            )],
            std::env::temp_dir(),
            std::env::temp_dir(),
        ));
        unsafe {
            std::env::set_var("LEVELER_HOME", dir.path());
            std::env::set_var("HOME", &user_home);
            std::env::remove_var("USERPROFILE");
        }
        dir
    })
    .path()
}

struct Fixture {
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    client: Arc<InProcessRuntimeClient>,
    session: SessionId,
}

impl Fixture {
    async fn new() -> Self {
        home();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let layout =
            Layout::from_parts(repo.clone(), repo.join("configs"), tmp.path().join("state"));
        let app = Arc::new(Application::assemble(layout).expect("assemble"));
        let model = ModelRef::parse("deepseek/test-model").unwrap();
        let session = app.create_session(&model, "skills-session").await.unwrap();
        let client = Arc::new(InProcessRuntimeClient::new(
            app,
            model,
            PermissionProfile::Assisted,
            false,
        ));
        Self {
            _tmp: tmp,
            repo,
            client,
            session,
        }
    }

    fn skill(&self, name: &str, body: &str) {
        self.skill_with_description(name, &format!("Fixture skill {name}."), body);
    }

    fn skill_with_description(&self, name: &str, description: &str, body: &str) {
        let dir = self.repo.join(".leveler/skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    async fn ask<T>(&self, command: ClientCommand, pick: impl Fn(RuntimeEvent) -> Option<T>) -> T {
        let mut rx = self.client.subscribe();
        self.client.send(command).await.unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Ok(event)) => {
                    if let Some(found) = pick(event) {
                        return found;
                    }
                }
                other => panic!("no answer: {other:?}"),
            }
        }
    }
}

/// Approve exactly the named skills, leaving every other package untrusted.
fn approve_skills(repo: &Path, names: &[&str]) {
    let roots = DiscoveryRoots {
        project_root: Some(repo.to_path_buf()),
        leveler_home: home().to_path_buf(),
        user_home: None,
    };
    let requests: Vec<_> = discover(&roots)
        .iter()
        .filter(|candidate| candidate.identity.kind == DefinitionKind::Skill)
        .filter(|candidate| {
            let directory = candidate
                .identity
                .display
                .rsplit('/')
                .next()
                .unwrap_or(&candidate.identity.display);
            names.iter().any(|name| directory == *name)
        })
        .filter_map(|candidate| candidate.approval_request().ok())
        .collect();
    assert_eq!(requests.len(), names.len(), "every fixture skill approves");
    approve(home(), repo, &requests, ApprovalSurface::Cli).expect("approve fixture");
}

fn disable_project_skill(repo: &Path, name: &str) {
    let mut preferences = SkillPreferences::default();
    let project = leveler_skills::project_key(repo);
    preferences
        .set_disabled(SkillScope::Project, name, Some(&project), true)
        .unwrap();
    preferences
        .save(&leveler_core::LevelerHome::from_root(home().to_path_buf()).skills_preferences_file())
        .unwrap();
}

async fn list(
    fx: &Fixture,
) -> (
    Vec<leveler_client_protocol::UiSkillEntry>,
    Vec<leveler_client_protocol::UiSkillProblem>,
) {
    let q = CommandId::generate();
    let want = q.clone();
    fx.ask(
        ClientCommand::ListSkills {
            session_id: fx.session.clone(),
            query_id: Some(q),
        },
        move |event| match event {
            RuntimeEvent::SkillsLoaded {
                query_id,
                skills,
                problems,
            } if query_id.as_ref() == Some(&want) => Some((skills, problems)),
            _ => None,
        },
    )
    .await
}

async fn detail(
    fx: &Fixture,
    name: &str,
) -> Result<leveler_client_protocol::UiSkillDetail, String> {
    let q = CommandId::generate();
    let want = q.clone();
    let name = name.to_string();
    fx.ask(
        ClientCommand::GetSkill {
            session_id: fx.session.clone(),
            name: name.clone(),
            query_id: Some(q),
        },
        move |event| match event {
            RuntimeEvent::SkillLoaded {
                query_id,
                name: answered,
                skill,
                error,
            } if query_id.as_ref() == Some(&want) && answered == name => Some(match skill {
                Some(detail) => Ok(detail),
                None => Err(error.unwrap_or_default()),
            }),
            _ => None,
        },
    )
    .await
}

#[tokio::test]
async fn listing_separates_available_untrusted_disabled_and_invalid() {
    let fx = Fixture::new().await;
    fx.skill("trusted-skill", "TRUSTED_BODY_MARKER");
    fx.skill("untrusted-skill", "UNTRUSTED_BODY_MARKER");
    fx.skill_with_description("broken-skill", "", "BROKEN_BODY_MARKER");
    fx.skill("disabled-skill", "DISABLED_BODY_MARKER");
    approve_skills(
        &fx.repo,
        &["trusted-skill", "broken-skill", "disabled-skill"],
    );
    disable_project_skill(&fx.repo, "disabled-skill");

    let (skills, problems) = list(&fx).await;
    let entry = |name: &str| {
        skills
            .iter()
            .find(|skill| skill.name == name)
            .unwrap_or_else(|| panic!("{name} missing from {skills:?}"))
    };

    let trusted = entry("trusted-skill");
    assert_eq!(trusted.status, UiSkillStatus::Available);
    assert_eq!(trusted.scope, UiSkillScope::Project);
    assert_eq!(trusted.source, UiSkillSource::Native);
    assert_eq!(
        trusted.description.as_deref(),
        Some("Fixture skill trusted-skill.")
    );
    assert!(trusted.user_invocable);
    assert!(
        trusted
            .location
            .as_deref()
            .unwrap()
            .ends_with("trusted-skill")
    );

    let untrusted = entry("untrusted-skill");
    assert_eq!(untrusted.status, UiSkillStatus::Untrusted);
    assert_eq!(untrusted.reason.as_deref(), Some("unapproved"));
    assert!(
        untrusted.description.is_none(),
        "unapproved metadata is not usable description"
    );
    assert_eq!(untrusted.scope, UiSkillScope::Project);

    let broken = entry("broken-skill");
    assert_eq!(broken.status, UiSkillStatus::Invalid);
    assert!(broken.reason.as_deref().unwrap().contains("description"));

    let disabled = entry("disabled-skill");
    assert_eq!(disabled.status, UiSkillStatus::Disabled);
    assert!(disabled.reason.as_deref().unwrap().contains("disabled"));

    // The built-in creator ships with every registry and is never shadowed by
    // a project package of another name.
    assert_eq!(entry("skill-creator").scope, UiSkillScope::Builtin);

    // A refused package is not also reported as a scan problem.
    assert!(
        !problems
            .iter()
            .any(|problem| problem.location.ends_with("untrusted-skill")),
        "untrusted packages have their own state: {problems:?}"
    );
}

#[tokio::test]
async fn detail_returns_the_trusted_body_and_an_untrusted_preview() {
    let fx = Fixture::new().await;
    fx.skill("trusted-skill", "TRUSTED_BODY_MARKER");
    fx.skill("untrusted-skill", "UNTRUSTED_BODY_MARKER");
    approve_skills(&fx.repo, &["trusted-skill"]);

    let trusted = detail(&fx, "trusted-skill").await.expect("trusted detail");
    assert_eq!(trusted.entry.status, UiSkillStatus::Available);
    assert!(
        trusted
            .body
            .as_deref()
            .unwrap()
            .contains("TRUSTED_BODY_MARKER")
    );
    assert!(!trusted.body_truncated);

    let untrusted = detail(&fx, "untrusted-skill")
        .await
        .expect("untrusted package is inspectable");
    assert_eq!(untrusted.entry.status, UiSkillStatus::Untrusted);
    assert!(
        untrusted
            .body
            .as_deref()
            .unwrap()
            .contains("UNTRUSTED_BODY_MARKER")
    );
    assert!(!untrusted.body_truncated);

    let missing = detail(&fx, "no-such-skill").await;
    assert!(missing.unwrap_err().contains("not found"));
}

#[tokio::test]
async fn a_changed_definition_is_listed_as_untrusted_again() {
    let fx = Fixture::new().await;
    fx.skill("changing-skill", "BEFORE_APPROVAL");
    approve_skills(&fx.repo, &["changing-skill"]);
    assert_eq!(
        detail(&fx, "changing-skill")
            .await
            .expect("trusted")
            .entry
            .status,
        UiSkillStatus::Available
    );

    // The persistent approval binds the exact package digest. Editing the
    // body invalidates it; the list must say so instead of keeping a stale
    // "available" row.
    fx.skill("changing-skill", "AFTER_APPROVAL");
    let (skills, _) = list(&fx).await;
    let changed = skills
        .iter()
        .find(|skill| skill.name == "changing-skill")
        .expect("still discovered");
    assert_eq!(changed.status, UiSkillStatus::Untrusted);
    assert_eq!(changed.reason.as_deref(), Some("changed"));
    assert!(changed.description.is_none());
    let detail = detail(&fx, "changing-skill").await.expect("inspectable");
    assert!(detail.body.as_deref().unwrap().contains("AFTER_APPROVAL"));
}
