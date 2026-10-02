//! Exact resource identities and capabilities shared by admission and storage.

use serde::{Deserialize, Serialize};

/// An operation whose permission never implies another capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Observe a repository.
    RepositoryRead,
    /// Change a repository.
    RepositoryMutate,
    /// Write repository metadata without destroying worktree data.
    RepositoryMetadataWrite,
    /// Change repository configuration.
    RepositoryConfigWrite,
    /// Destroy repository or worktree state.
    RepositoryDestroy,
    /// Read a configured remote.
    RemoteRead,
    /// Change a configured remote normally.
    RemoteMutate,
    /// Perform a destructive remote change.
    RemoteForce,
    /// Read a filesystem object.
    FilesystemRead,
    /// Write a filesystem object.
    FilesystemWrite,
    /// Delete filesystem objects.
    FilesystemDelete,
    /// Observe or wait for a runtime background task.
    BackgroundTaskObserve,
    /// Control a runtime background task.
    BackgroundTaskControl,
    /// Control an external process.
    ExternalProcessControl,
    /// Use a credential without exposing its value.
    CredentialUse,
    /// Read a credential's raw value.
    CredentialRawRead,
    /// Change a raw credential source.
    CredentialRawWrite,
}

/// A host-resolved identity. Model-supplied names alone cannot authorize it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceIdentity {
    /// Repository identity including its current object incarnation.
    Repository {
        /// Canonical repository identity.
        identity: String,
    },
    /// Effective configured remote snapshot.
    ConfiguredRemote {
        /// Repository identity.
        repository: String,
        /// Configured remote name.
        remote_name: String,
        /// Effective canonical URL, without credentials.
        canonical_url: String,
        /// Transport identity.
        transport: String,
    },
    /// Canonical path and object incarnation.
    FilesystemPath {
        /// Canonical path.
        canonical_path: String,
        /// Object or nearest existing parent identity.
        object_identity: String,
    },
    /// A task in its owning runtime.
    BackgroundTask {
        /// Runtime identity.
        runtime: String,
        /// Task identity.
        task_id: String,
        /// Owning session identity.
        owner: String,
        /// Process incarnation identity.
        process_identity: String,
    },
    /// External process identity resistant to PID reuse.
    ExternalProcess {
        /// Operating system process ID.
        pid: u32,
        /// Process start/boot identity.
        start_identity: String,
    },
    /// Credential metadata only; never a credential value.
    Credential {
        /// Project identity.
        project: String,
        /// Destination host.
        host: String,
        /// Credential source identity.
        identity: String,
        /// Transport identity.
        transport: String,
    },
}

/// One exact capability-resource association.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantBinding {
    /// Requested operation.
    pub capability: Capability,
    /// Host-resolved resource snapshot.
    pub resource: ResourceIdentity,
}

/// All bindings required for one action; partial coverage cannot authorize it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantRequest {
    /// Authoritative project identity.
    pub project_identity: String,
    /// Required capability-resource bindings.
    pub bindings: Vec<GrantBinding>,
}

/// How long an explicit resource approval may be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// This admitted call only; never persisted.
    Once,
    /// The exact persisted session, including restart/resume.
    Session,
    /// Future sessions in the same project.
    Project,
}
