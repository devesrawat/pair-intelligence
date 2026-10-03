-- Retry limit for interrupted runs and a way to tell an operator interrupt from a lapsed lease.
--
-- interrupt_count: how many times the sweeper has requeued the run after an interruption. A run
-- that keeps being interrupted (a step that crashes its worker every time) is failed after
-- MAX_INTERRUPTS instead of looping forever.
-- operator_interrupt_epoch: the lease_epoch at which an operator interrupted the run on purpose.
-- The sweeper never requeues a run whose current epoch equals this marker; a later claim bumps the
-- epoch, so an old marker cannot shield a run from the ordinary lease-lapse recovery.
-- Additive: two metadata-only ADD COLUMNs, ignored by older builds.
ALTER TABLE workflow_runs
    ADD COLUMN interrupt_count integer NOT NULL DEFAULT 0 CHECK (interrupt_count >= 0),
    ADD COLUMN operator_interrupt_epoch bigint;
