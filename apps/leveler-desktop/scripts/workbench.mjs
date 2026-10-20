// SUPERSEDED: the `#workbench-*` shell this script targeted no longer exists.
// The current right dock's split, float, fullscreen, width, restart and draft
// behavior is covered by files-and-tabs.mjs. Importing that acceptance keeps
// this entry honest instead of reporting a stale selector failure or a hollow
// pass.
await import('./files-and-tabs.mjs');
