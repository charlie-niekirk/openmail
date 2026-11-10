-- The per-minute quota budget the Gmail client learned from rate limiting, and
-- the per-minute quota it was learned under, so the next sync starts at it.
ALTER TABLE sync_state ADD COLUMN quota_budget INTEGER;
ALTER TABLE sync_state ADD COLUMN quota_budget_basis INTEGER;
