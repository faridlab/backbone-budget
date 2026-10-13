-- Down: drop the declared foreign keys for budget module
ALTER TABLE IF EXISTS budget.budget_lines DROP CONSTRAINT IF EXISTS fk_budget_lines_budget_id;
