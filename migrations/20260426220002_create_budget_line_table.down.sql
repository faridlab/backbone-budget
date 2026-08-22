-- Down: drop budget.budget_lines table
DROP TABLE IF EXISTS budget.budget_lines CASCADE;
DROP FUNCTION IF EXISTS budget.budget_lines_audit_timestamp() CASCADE;
