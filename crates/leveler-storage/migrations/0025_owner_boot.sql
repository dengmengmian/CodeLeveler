-- Which boot holds a task's ownership, and which boot runs a turn.
--
-- tasks.owner_runtime_id names a durable runtime, which several live
-- processes can share and which survives restarts; it cannot say whether the
-- owner is still executing. The boot can: another boot may take a task over,
-- or interrupt a running turn, only once that boot is proven to have ended.
--
-- tasks.owner_boot_id is written by the same ownership compare-and-swap that
-- advances owner_epoch. turns.owner_boot_id is written by the insert that
-- makes a turn `running`, and kept after the turn ends as its provenance.
--
-- NULL on rows written before these columns existed: no boot can be proven
-- dead for them, so such a running turn is never interrupted automatically.
ALTER TABLE tasks ADD COLUMN owner_boot_id TEXT;
ALTER TABLE turns ADD COLUMN owner_boot_id TEXT;
