-- Down: remove the company RLS fence for budget module

-- Reverse the company RLS fence for budget.budgets
DROP POLICY IF EXISTS budgets_company_isolation ON budget.budgets;
ALTER TABLE budget.budgets NO FORCE ROW LEVEL SECURITY;
ALTER TABLE budget.budgets DISABLE ROW LEVEL SECURITY;

-- Reverse the company RLS fence for budget.budget_lines
DROP POLICY IF EXISTS budget_lines_company_isolation ON budget.budget_lines;
ALTER TABLE budget.budget_lines NO FORCE ROW LEVEL SECURITY;
ALTER TABLE budget.budget_lines DISABLE ROW LEVEL SECURITY;

