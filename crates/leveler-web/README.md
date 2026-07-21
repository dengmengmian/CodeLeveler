# leveler-web

Browser WebUI server for CodeLeveler. It serves the single-page app and
bridges it to the runtime over the stable client protocol — the same
`LocalRuntimeService` seam the local daemon uses, so the server runs either
in-process (`leveler web`) or against a `leveler serve --tcp` daemon
(`leveler web --connect <addr> --token <token>`).

## Security posture

- The listener is **loopback-only**; `bind` refuses non-loopback addresses.
- Every endpoint (REST *and* the WebSocket upgrade) requires a 256-bit bearer
  token, compared in constant time. Present it as `?token=` or as an
  `Authorization: Bearer` header; failures get a bare `401`.
- The token is minted per process run, printed once in the startup URL
  (`http://127.0.0.1:PORT/?token=...`), and never persisted. An empty token
  refuses to start.

## REST

All under the token check above:

- `GET /api/health` → `{"ok":true}`
- `POST /api/sessions` — body is `CreateSessionRequest`
  (`{"goal":"...","model":null|{...},"mode":"assisted"}`); answers with the
  daemon's `SessionBootstrap` JSON.
- `GET /api/sessions/:id/snapshot` → `UiSessionSnapshot` JSON (`404` for an
  unknown session).

Everything else (`GET /` and any non-`/api`, non-`/ws` path) serves the SPA:
a real asset when the path names one, otherwise `index.html` (client-side
routing). When no frontend build is available the shell answers `503` with a
build hint.

## WebSocket

`GET /ws?session=<id>&token=<token>` — JSON text frames.

Upstream (browser → server), `tag="type"` snake_case:

```json
{"type":"deliver","command_id":"<uuid>","session_id":"<id>","command":<ClientCommand>}
{"type":"snapshot","session_id":"<id>"}
```

Downstream (server → browser):

```json
{"type":"event","event":<RuntimeEvent>}
{"type":"snapshot","session":<UiSessionSnapshot>}
{"type":"ack","command_id":"..."}
{"type":"error","code":"...","message":"...","command_id":null}
{"type":"resync_required","session_id":"..."}
```

Notes:

- On connect with `?session=`, the first frame is that session's `snapshot`
  (or an `error` frame if the session is unknown; the connection stays up).
- The server subscribes to the runtime's **global** event stream and forwards
  every event; session lists ride the normal command path
  (`request_session_list` → `session_list` event) and are filtered client-side.
- `deliver` wraps the command in a `CommandEnvelope` (idempotency key =
  `command_id`, no version check) and goes through `deliver_protocol`; success
  answers `ack` echoing `command_id`, failure answers `error` without closing.
- If the event subscription lags, the server sends `resync_required` and
  closes; the client should resync from a fresh snapshot and reconnect.

## Frontend assets

Build the SPA first — its output is what the server embeds:

```sh
cd crates/leveler-web/web
npm install
npm run build          # → web/dist (git-ignored)
```

The production build is embedded at compile time from `web/dist`
(`rust-embed`, `debug-embed`; a missing folder is tolerated so a backend-only
checkout compiles). Because assets are baked in even in debug builds, a server
compiled before `web/dist` existed keeps serving "not built" — rebuild
(`touch src/lib.rs && cargo build -p leveler-web`) after the frontend build
lands.

For frontend development against a live server, point the server at a build
output directory instead of recompiling:

```sh
LEVELER_WEB_DIST=crates/leveler-web/web/dist cargo run -p leveler-cli -- web
```

The env override takes precedence over the embedded assets for every request.
