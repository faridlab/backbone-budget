-- Down: drop budget.budgets table
DROP TABLE IF EXISTS budget.budgets CASCADE;
DROP FUNCTION IF EXISTS budget.budgets_audit_timestamp() CASCADE;
