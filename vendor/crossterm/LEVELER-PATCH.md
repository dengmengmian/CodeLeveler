# Local terminal hangup patch

Upstream: crossterm 0.28.1, https://crates.io/crates/crossterm/0.28.1

Registry archive SHA256: 829d955a0bb380ef178a640b91779e3987da38c9aea133b20614cfed8cdea9c6

Source retained at the locked version under its MIT license. Only production changes:

- `src/event/source/unix/mio.rs`: EOF and non-retryable read errors return rather than spinning under the global input-reader lock.
- `src/event/stream.rs`: the background waiter wakes its consumer on a read error, rather than retrying forever.

No dependency version upgrade. Remove this patch when upstream provides equivalent hangup handling.
