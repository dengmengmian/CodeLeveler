-- Permission Model V2: the tier names did not change, but their meanings did.
--
-- V1 rows keep their exact pre-upgrade semantics:
--   request_approval -> LegacyRestricted (strict)
--   assisted         -> LegacyAuto (old Auto)
--   full_access      -> Full (unchanged)
--
-- New rows must write an explicit contract. The column DEFAULT exists only to
-- give already-existing rows the legacy contract; it must not be relied on by
-- an insert path.
ALTER TABLE sessions ADD COLUMN permission_contract INTEGER NOT NULL DEFAULT 1;

-- Legacy wire names left by migration 0003's column DEFAULT. They are v1 by
-- definition. Do not rewrite any current name: 'assisted' is not evidence of
-- v2, and 'request_approval' is not evidence of v2 either.
UPDATE sessions SET mode = 'request_approval', permission_contract = 1
  WHERE mode = 'plan';
UPDATE sessions SET mode = 'assisted', permission_contract = 1
  WHERE mode = 'workspace_write';
