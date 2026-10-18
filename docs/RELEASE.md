# CodeLeveler 1.0.18

Chinese: [`RELEASE.zh-CN.md`](RELEASE.zh-CN.md)

Web search is now a capability of its own: the search backend is selected
independently of the conversation model, so the model that answers and the
service that searches are two separate choices. This patch also fixes how the
first result is shown and makes a missing search credential diagnosable.

## Web search

- `[web.search]` in `~/.leveler/config.toml` is the only switch for `web_search`. The legacy `LEVELER_SEARCH_API_KEY` fallback is removed: without this section, search stays unavailable.
- Two backends: the DeepSeek official Search API (`POST https://api.deepseek.com/v1/tools/search`) and Tavily.
- The backend is independent of `[providers.*]` and `[models.*]`. Switching the conversation model, including to another vendor, does not change it, and a model provider's `base_url` is never reused as the search endpoint.
- The credential is read only from the configured `api_key_env`. A model gateway key is never reused for search, the variable is scrubbed from child processes, and its value never reaches logs or tool output.
- Each call is one HTTP request. It honors the configured timeout, does not retry, does not fall back to another provider, and does not start a model request.
- Results carry title, URL, snippet, and publication date. The TUI shows the first result instead of an external-data marker.

## Configuration

```toml
[web.search]
provider = "deepseek"                     # deepseek | tavily
api_key_env = "LEVELER_OFFICIAL_SEARCH_KEY"
base_url = "https://api.deepseek.com/v1/tools"
max_results = 5
timeout_ms = 15000
```

Export `LEVELER_OFFICIAL_SEARCH_KEY` in the environment the app starts from.
Keep the key out of the repository and out of the TOML file. If the variable is
unset, `web_search` stays unavailable, the model and every other feature keep
working, and `~/.leveler/leveler.log` records a warning naming the variable.

## Updates

- A Homebrew-managed install no longer replaces itself in the Cellar. The start-up updater and `leveler update` defer to `brew upgrade leveler`, so the formula's record and the installed file stay in agreement; `leveler update --check` remains a read-only check. Installations from the install script or a manual copy keep their existing self-update behaviour.

## Qualification

- The Dogfood coverage audit now persists this run's layer artifacts before taking the pre-RC verdict. A test that L1 passed is no longer reported as "L1 did not run for this candidate".
- Coverage evidence is bound to the candidate named on the audit command line: a run directory that declares another candidate is refused instead of credited.

## Fixes

- The TUI renders the first search result instead of the external-data marker.
- A configured search whose credential variable is unset is logged at warn level (it was debug, which the default filter never wrote).
