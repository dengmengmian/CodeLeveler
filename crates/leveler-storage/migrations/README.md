# Migrations

SQLite migrations applied in filename order by sqlx at startup.

## Numbering gap: 0007–0011

The gap between `0006_session_axes.sql` and `0012_permission_profiles.sql` is
deliberate history, not missing files: versions 0007–0011 existed only in the
pre-publication private history and were squashed away before the public
tree was cut. **They were never part of any released binary**, so no user
database can contain them; sqlx's `VersionMissing` check cannot trigger for
the gap. Do not reuse the numbers — sqlx records applied versions by number,
and a reused number would collide with any long-lived dev database from the
private era.

## Rules

- Append-only: never edit an existing migration file; add a new one.
- Prefer `ALTER TABLE … ADD COLUMN` + data rewrites over table rebuilds.
- New indexes/columns must be backward-compatible with rows written by older
  binaries (see `0004_events_hardening.sql` for the versioning pattern).
