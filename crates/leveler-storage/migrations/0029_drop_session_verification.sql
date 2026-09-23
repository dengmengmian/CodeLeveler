-- Completion no longer has a host-owned verification verdict. Tests, builds,
-- and linters remain ordinary commands and do not produce session state.
ALTER TABLE sessions DROP COLUMN verification;
