-- Structured review verdict: coverage, limitations and conclusion.
--
-- The reviewer declares these through `report_review`; the runtime persists
-- them beside the report instead of parsing the reviewer's prose. NULL means
-- the reviewer declared nothing, which is not the same as "no issues".
ALTER TABLE reviews ADD COLUMN verdict_json TEXT;
