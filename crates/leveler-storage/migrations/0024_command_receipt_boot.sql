-- Which boot admitted a command, so an unsettled `dispatching` receipt can be
-- judged: the boot still holds its lease (it may yet settle the receipt), or
-- it has ended (nothing ever will). NULL on rows written before this column
-- existed — those cannot be attributed to any boot and stay unresolved.
--
-- New status value, no schema change needed (status is free TEXT):
--   'unresolvable' — admitted, but the boot responsible for the dispatch ended
--                    before settling it; the outcome cannot be recovered.
--                    Terminal: never re-dispatched, never marked completed.
ALTER TABLE command_receipts ADD COLUMN admitted_by_boot TEXT;
