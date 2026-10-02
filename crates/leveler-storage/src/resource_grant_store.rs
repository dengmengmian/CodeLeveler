//! Durable exact resource approvals. Legacy permission rules are not migrated.
use crate::{Database, StorageError};
use async_trait::async_trait;
use leveler_core::{GrantBinding, GrantScope};

/// Resource approval persistence port. Callers own mode and admission policy.
#[async_trait]
pub trait ResourceGrantStore: Send + Sync {
    /// True only when every binding is covered in this project and session.
    async fn covers(
        &self,
        project: &str,
        session: &str,
        bindings: &[GrantBinding],
    ) -> Result<bool, StorageError>;
    /// Atomically persist explicit Session or Project consent. Once is rejected.
    async fn grant(
        &self,
        project: &str,
        session: &str,
        scope: GrantScope,
        bindings: &[GrantBinding],
    ) -> Result<(), StorageError>;
}

const SCHEMA_VERSION: i64 = 1;

fn validate(project: &str, session: &str, bindings: &[GrantBinding]) -> Result<(), StorageError> {
    if project.is_empty() || session.is_empty() || bindings.is_empty() {
        return Err(StorageError::InvalidData(
            "resource grant requires project, session and bindings".into(),
        ));
    }
    // Identity fields are opaque, but missing incarnation data must never match.
    for binding in bindings {
        let value = serde_json::to_value(binding)
            .map_err(|_| StorageError::InvalidData("invalid resource binding".into()))?;
        let resource = value
            .get("resource")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| StorageError::InvalidData("invalid resource identity".into()))?;
        if resource
            .values()
            .any(|v| v.as_str().is_some_and(str::is_empty) || v.as_u64() == Some(0))
        {
            return Err(StorageError::InvalidData(
                "empty resource identity field".into(),
            ));
        }
    }
    Ok(())
}

fn decode(version: i64, payload: &str) -> Result<GrantBinding, StorageError> {
    if version != SCHEMA_VERSION {
        return Err(StorageError::InvalidData(
            "unsupported resource grant version".into(),
        ));
    }
    serde_json::from_str(payload)
        .map_err(|_| StorageError::InvalidData("invalid resource grant binding".into()))
}

#[async_trait]
impl ResourceGrantStore for Database {
    async fn covers(
        &self,
        project: &str,
        session: &str,
        bindings: &[GrantBinding],
    ) -> Result<bool, StorageError> {
        validate(project, session, bindings)?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT schema_version, binding_json FROM resource_grants WHERE project_identity = ? AND ((scope = 'session' AND session_identity = ?) OR (scope = 'project' AND session_identity = ''))"
        ).bind(project).bind(session).fetch_all(&self.pool).await?;
        let stored = rows
            .into_iter()
            .map(|(v, p)| decode(v, &p))
            .collect::<Result<Vec<_>, _>>()?;
        for binding in &stored {
            validate(project, session, std::slice::from_ref(binding))?;
        }
        Ok(bindings.iter().all(|b| stored.contains(b)))
    }
    async fn grant(
        &self,
        project: &str,
        session: &str,
        scope: GrantScope,
        bindings: &[GrantBinding],
    ) -> Result<(), StorageError> {
        validate(project, session, bindings)?;
        let (scope, session) = match scope {
            GrantScope::Once => {
                return Err(StorageError::InvalidData(
                    "Once resource consent cannot be persisted".into(),
                ));
            }
            GrantScope::Session => ("session", session),
            GrantScope::Project => ("project", ""),
        };
        let payloads = bindings
            .iter()
            .map(|b| {
                serde_json::to_string(b)
                    .map_err(|_| StorageError::InvalidData("invalid resource grant binding".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut tx = self.pool.begin().await?;
        for payload in payloads {
            sqlx::query("INSERT INTO resource_grants(project_identity, session_identity, scope, schema_version, binding_json) VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING")
                .bind(project).bind(session).bind(scope).bind(SCHEMA_VERSION).bind(payload).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

/// In-memory adapter for the same exact matching contract.
#[derive(Default)]
pub struct MemoryResourceGrantStore {
    rows: std::sync::Mutex<Vec<(String, String, GrantScope, GrantBinding)>>,
}

#[async_trait]
impl ResourceGrantStore for MemoryResourceGrantStore {
    async fn covers(
        &self,
        project: &str,
        session: &str,
        bindings: &[GrantBinding],
    ) -> Result<bool, StorageError> {
        validate(project, session, bindings)?;
        let rows = self
            .rows
            .lock()
            .map_err(|_| StorageError::InvalidData("resource grant lock poisoned".into()))?;
        Ok(bindings.iter().all(|binding| {
            rows.iter().any(|(p, s, scope, b)| {
                p == project && (*scope == GrantScope::Project || s == session) && b == binding
            })
        }))
    }
    async fn grant(
        &self,
        project: &str,
        session: &str,
        scope: GrantScope,
        bindings: &[GrantBinding],
    ) -> Result<(), StorageError> {
        validate(project, session, bindings)?;
        if scope == GrantScope::Once {
            return Err(StorageError::InvalidData(
                "Once resource consent cannot be persisted".into(),
            ));
        }
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| StorageError::InvalidData("resource grant lock poisoned".into()))?;
        let session = if scope == GrantScope::Project {
            ""
        } else {
            session
        };
        for binding in bindings {
            let row = (
                project.to_owned(),
                session.to_owned(),
                scope,
                binding.clone(),
            );
            if !rows.contains(&row) {
                rows.push(row);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use leveler_core::{Capability, ResourceIdentity};
    fn binding(name: &str) -> GrantBinding {
        GrantBinding {
            capability: Capability::RemoteRead,
            resource: ResourceIdentity::ConfiguredRemote {
                repository: "repo-epoch".into(),
                remote_name: name.into(),
                canonical_url: "https://example.com/repo.git".into(),
                transport: "https".into(),
            },
        }
    }
    #[tokio::test]
    async fn session_restart_and_project_isolation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("grants.db");
        let db = Database::connect(&path).await.unwrap();
        let bindings = vec![binding("origin")];
        db.grant("p", "s", GrantScope::Session, &bindings)
            .await
            .unwrap();
        assert!(db.covers("p", "s", &bindings).await.unwrap());
        db.pool.close().await;
        let db = Database::connect(&path).await.unwrap();
        assert!(db.covers("p", "s", &bindings).await.unwrap());
        assert!(!db.covers("p", "new", &bindings).await.unwrap());
        assert!(!db.covers("other", "s", &bindings).await.unwrap());
        assert!(!db.covers("p", "s", &[binding("different")]).await.unwrap());
        db.grant("p", "s", GrantScope::Project, &bindings)
            .await
            .unwrap();
        assert!(db.covers("p", "new", &bindings).await.unwrap());
    }
    #[tokio::test]
    async fn once_partial_and_capability_do_not_widen() {
        let db = Database::connect_in_memory().await.unwrap();
        let read = binding("origin");
        assert!(
            db.grant("p", "s", GrantScope::Once, std::slice::from_ref(&read))
                .await
                .is_err()
        );
        assert!(
            !db.covers("p", "s", std::slice::from_ref(&read))
                .await
                .unwrap()
        );
        db.grant("p", "s", GrantScope::Session, std::slice::from_ref(&read))
            .await
            .unwrap();
        assert!(
            !db.covers("p", "s", &[read.clone(), binding("other")])
                .await
                .unwrap()
        );
        let mut mutate = read;
        mutate.capability = Capability::RemoteMutate;
        assert!(!db.covers("p", "s", &[mutate]).await.unwrap());
    }
    #[tokio::test]
    async fn identity_changes_and_unknown_versions_fail_closed() {
        let db = Database::connect_in_memory().await.unwrap();
        let read = binding("origin");
        db.grant("p", "s", GrantScope::Project, std::slice::from_ref(&read))
            .await
            .unwrap();
        for field in ["repository", "remote_name", "canonical_url", "transport"] {
            let mut json = serde_json::to_value(&read).unwrap();
            json["resource"][field] = serde_json::Value::String("changed".into());
            let changed: GrantBinding = serde_json::from_value(json).unwrap();
            assert!(!db.covers("p", "s", &[changed]).await.unwrap(), "{field}");
        }
        sqlx::query("UPDATE resource_grants SET schema_version = 2")
            .execute(&db.pool)
            .await
            .unwrap();
        assert!(matches!(
            db.covers("p", "s", std::slice::from_ref(&read)).await,
            Err(StorageError::InvalidData(_))
        ));
        sqlx::query("UPDATE resource_grants SET schema_version = 1, binding_json = ?")
            .bind(r#"{"capability":"future_capability","resource":{"kind":"repository","identity":"x"}}"#)
            .execute(&db.pool).await.unwrap();
        assert!(matches!(
            db.covers("p", "s", &[read]).await,
            Err(StorageError::InvalidData(_))
        ));
    }
    #[tokio::test]
    async fn memory_adapter_matches_sqlite_and_invalid_batch_is_atomic() {
        let db = Database::connect_in_memory().await.unwrap();
        let memory = MemoryResourceGrantStore::default();
        let read = binding("origin");
        let invalid = GrantBinding {
            capability: Capability::RepositoryRead,
            resource: ResourceIdentity::Repository {
                identity: "".into(),
            },
        };
        for store in [&db as &dyn ResourceGrantStore, &memory] {
            assert!(
                store
                    .grant(
                        "p",
                        "s",
                        GrantScope::Session,
                        &[read.clone(), invalid.clone()]
                    )
                    .await
                    .is_err()
            );
            assert!(
                !store
                    .covers("p", "s", std::slice::from_ref(&read))
                    .await
                    .unwrap()
            );
            assert!(store.covers("p", "s", &[]).await.is_err());
            store
                .grant("p", "s", GrantScope::Session, std::slice::from_ref(&read))
                .await
                .unwrap();
            assert!(
                store
                    .covers("p", "s", std::slice::from_ref(&read))
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .covers("p", "new", std::slice::from_ref(&read))
                    .await
                    .unwrap()
            );
            store
                .grant("p", "s", GrantScope::Project, &[binding("other")])
                .await
                .unwrap();
            assert!(
                store
                    .covers("p", "s", &[read.clone(), binding("other")])
                    .await
                    .unwrap()
            );
            assert!(
                !store
                    .covers("other", "s", std::slice::from_ref(&read))
                    .await
                    .unwrap()
            );
        }
    }
    #[tokio::test]
    async fn database_failure_rolls_back_entire_binding_set() {
        let db = Database::connect_in_memory().await.unwrap();
        sqlx::query(r#"CREATE TRIGGER reject_other BEFORE INSERT ON resource_grants WHEN NEW.binding_json LIKE '%"remote_name":"other"%' BEGIN SELECT RAISE(ABORT, 'test write failure'); END"#)
            .execute(&db.pool).await.unwrap();
        assert!(matches!(
            db.grant(
                "p",
                "s",
                GrantScope::Project,
                &[binding("origin"), binding("other")]
            )
            .await,
            Err(StorageError::Sqlx(_))
        ));
        assert!(!db.covers("p", "s", &[binding("origin")]).await.unwrap());
    }
}
