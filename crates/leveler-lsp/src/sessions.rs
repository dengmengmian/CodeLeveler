//! The language-server session pool: the one owner of `LspClient` lifetime.
//!
//! Starting a server costs seconds and it must index the workspace once, so
//! clients are cached per language and reused across calls. A crashed or
//! timed-out server is evicted so the NEXT call restarts it — leaving a corpse
//! in the map pins that language into permanent degradation.
//!
//! This pool used to live in `ToolContext.services` (two `HashMap`s every tool
//! could reach) with the start/locate logic spread across the tool files, so
//! `find_symbol` carried a second copy of it. Both moved here, to the crate
//! that owns the client.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use tokio::sync::Mutex;

use leveler_project::Language;

use crate::client::{LspClient, SymbolLocation};
use crate::registry::{ServerSpec, server_available_with_environment, server_for};

/// How long to keep asking a freshly started server, which may still be
/// indexing, before treating the empty answer as final.
const INDEXING_RETRIES: usize = 6;

/// A pool of live language-server sessions, keyed by language.
pub struct LspSessions {
    environment: Arc<leveler_core::EnvSnapshot>,
    clients: Mutex<HashMap<String, Arc<LspClient>>>,
    /// Per-language startup locks. Starting a server may take seconds; these
    /// prevent duplicate starts without holding the global clients mutex.
    start_locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl LspSessions {
    pub fn new(environment: Arc<leveler_core::EnvSnapshot>) -> Self {
        Self {
            environment,
            clients: Mutex::new(HashMap::new()),
            start_locks: Mutex::new(HashMap::new()),
        }
    }

    /// The environment these sessions resolve server programs against.
    pub fn environment(&self) -> &Arc<leveler_core::EnvSnapshot> {
        &self.environment
    }

    /// Whether this language's server program is installed.
    pub fn server_available(&self, language: Language) -> bool {
        server_available_with_environment(language, &self.environment)
    }

    /// The cached client for `language`, starting the server if needed.
    ///
    /// Only this language's startup lock is held across the process launch, so
    /// other languages and already-running sessions stay available.
    pub async fn get_or_start(
        &self,
        language: Language,
        spec: &ServerSpec,
        root: &Path,
    ) -> Result<Arc<LspClient>, String> {
        let key = language.as_str().to_string();
        if let Some(client) = self.clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }
        let start_lock = {
            let mut locks = self.start_locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _starting = start_lock.lock().await;
        if let Some(client) = self.clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }
        let client = Arc::new(
            LspClient::start(&spec.program, &spec.args, root)
                .await
                .map_err(|error| error.to_string())?,
        );
        self.clients.lock().await.insert(key, client.clone());
        Ok(client)
    }

    /// Drop a dead client, but only if it is still the one in the map: a
    /// concurrent call may already have restarted the server, and evicting
    /// that fresh client would restart it again for nothing.
    pub async fn evict_if_current(&self, language: Language, expected: &Arc<LspClient>) {
        remove_if_same(&mut *self.clients.lock().await, language.as_str(), expected);
    }

    /// Locate a symbol's definitions through a language server.
    ///
    /// Returns the server that answered, the live client (so a follow-up
    /// `references` / `document_symbol` query reuses the same session), and the
    /// exact-name matches. `None` when no server is installed for any language
    /// in `root`, none of them answered, or the symbol is not defined — "no
    /// language server answer" and nothing more. The caller decides what to
    /// say about that; this never substitutes a textual guess for an answer.
    pub async fn locate(&self, root: &Path, symbol: &str) -> Option<Located> {
        for language in leveler_project::detect_languages(root) {
            if !self.server_available(language) {
                continue;
            }
            let Some(spec) = server_for(language) else {
                continue;
            };
            let Ok(client) = self.get_or_start(language, &spec, root).await else {
                continue;
            };

            let mut located = Vec::new();
            let mut server_died = false;
            for _ in 0..INDEXING_RETRIES {
                match client.workspace_symbols(symbol).await {
                    Ok(found) if !found.is_empty() => {
                        located = found;
                        break;
                    }
                    // Still indexing on first use; ask again shortly.
                    Ok(_) => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
                    Err(_) => {
                        server_died = true;
                        break;
                    }
                }
            }
            if server_died {
                self.evict_if_current(language, &client).await;
            }
            let matches: Vec<_> = located
                .into_iter()
                .filter(|s| s.name.eq_ignore_ascii_case(symbol))
                .collect();
            if !matches.is_empty() {
                return Some(Located {
                    spec,
                    client,
                    definitions: matches,
                });
            }
        }
        None
    }
}

/// What a language server answered about a symbol.
pub struct Located {
    /// The server that answered, for naming it in the result and for the
    /// `languageId` a follow-up `open` needs.
    pub spec: ServerSpec,
    /// The live session, so a follow-up query does not restart anything.
    pub client: Arc<LspClient>,
    /// Definition sites whose name matches exactly.
    pub definitions: Vec<SymbolLocation>,
}

/// Remove `key` only when it still maps to `expected`. Generic so the
/// identity rule can be exercised without a live language server.
fn remove_if_same<T>(map: &mut HashMap<String, Arc<T>>, key: &str, expected: &Arc<T>) {
    if map
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, expected))
    {
        map.remove(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_never_removes_a_concurrently_restarted_session() {
        let stale = Arc::new(1_u8);
        let restarted = Arc::new(2_u8);
        let mut map = HashMap::new();
        map.insert("rust".to_string(), restarted.clone());

        remove_if_same(&mut map, "rust", &stale);
        assert!(
            Arc::ptr_eq(map.get("rust").unwrap(), &restarted),
            "a restarted session must survive eviction of the stale one"
        );

        remove_if_same(&mut map, "rust", &restarted);
        assert!(
            !map.contains_key("rust"),
            "the current session is evictable"
        );
    }
}
