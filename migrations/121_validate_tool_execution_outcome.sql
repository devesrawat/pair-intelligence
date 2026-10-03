-- Separate transaction from 120 on purpose: VALIDATE scans existing rows without blocking writes.
ALTER TABLE tool_executions VALIDATE CONSTRAINT tool_executions_outcome_check;
