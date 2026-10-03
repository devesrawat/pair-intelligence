-- Adapter reservations name the model they hold money for, so a settlement can be checked against
-- that model's registry price (tokens x price) instead of trusting the caller's reported cost.
-- Nullable: reservations made by the turn pipeline, the classifier and older builds carry no model,
-- and a reservation made without one is checked against the dearest model priced under its version.
-- Additive: a metadata-only ADD COLUMN, ignored by older builds.
ALTER TABLE budget_reservations ADD COLUMN model_id text;
