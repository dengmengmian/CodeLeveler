-- A stable handle for each persisted resource grant.
--
-- The table's natural key is the full binding JSON, which is not something a
-- person can retype. The id lets a human revoke exactly one stored grant
-- without reconstructing its resource identity. Existing rows get a random id;
-- `revoke` deletes by (project_identity, grant_id), so no cross-project handle
-- can reach another project's grant.
ALTER TABLE resource_grants ADD COLUMN grant_id TEXT;
UPDATE resource_grants SET grant_id = lower(hex(randomblob(16)));
CREATE UNIQUE INDEX resource_grants_by_id ON resource_grants(project_identity, grant_id);
