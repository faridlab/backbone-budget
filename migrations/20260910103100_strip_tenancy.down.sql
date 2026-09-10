-- Hand-authored (user-owned). Not regenerated.
--
-- Best-effort restore sketch for the tenancy strip (ADR-0029). This is a breaking module
-- release against dev-stage databases: the down re-adds the company_id column as nullable
-- with its plain indexes and the company isolation policy shape, but restores NO data —
-- rows written after the strip (or after the decorator re-keyed them) carry org_unit_id
-- only. The composing service's tenancy decorator remains the live fence; treat this
-- down as a schema-shape sketch for archaeology, not a usable rollback.
--
-- The pre-strip partial uniques are not reproduced here: with no data restore they would
-- be either trivially satisfiable or instantly violated; the pre-strip shapes are
-- recorded in 20260426220001_create_budget_table.up.sql and
-- 20260426220002_create_budget_line_table.up.sql.

ALTER TABLE budget.budgets      ADD COLUMN IF NOT EXISTS company_id uuid;
ALTER TABLE budget.budget_lines ADD COLUMN IF NOT EXISTS company_id uuid;

CREATE INDEX IF NOT EXISTS idx_budgets_company_id_status
    ON budget.budgets (company_id, status);
CREATE INDEX IF NOT EXISTS idx_budgets_company_id_fiscal_year
    ON budget.budgets (company_id, fiscal_year);
CREATE INDEX IF NOT EXISTS idx_budget_lines_company_id_fiscal_period_id
    ON budget.budget_lines (company_id, fiscal_period_id);

CREATE POLICY budgets_company_isolation ON budget.budgets
    FOR ALL
    USING      (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
CREATE POLICY budget_lines_company_isolation ON budget.budget_lines
    FOR ALL
    USING      (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid)
    WITH CHECK (company_id = NULLIF(current_setting('app.company_id', true), '')::uuid);
