# TUI `/trace`

TUI entry to the Durable Runtime Observatory.

See [`DURABLE_RUNTIME_OBSERVATORY.md`](DURABLE_RUNTIME_OBSERVATORY.md) for
architecture, data sources, security, and CLI.

`/trace` is a TUI navigation command. It sends read-only
`QueryObservability` and renders `ObservabilityLoaded`. It does not change
runtime, tools, verification, or completion. Esc returns to Conversation.

The **Trace** tab is the bounded event window. The **Tools** tab is the
whole-session tool summary (not the current window). **Requests** are
whole-session `model_requests`. **Agents** remains window-local (copy says
窗口).
