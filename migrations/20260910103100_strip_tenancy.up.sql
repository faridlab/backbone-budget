-- Hand-authored (user-owned). Not regenerated.
--
-- Strip every company-fence artifact from the budget tables (ADR-0029): the module is
-- tenant-agnostic; org scoping is installed by the COMPOSING service's tenancy decorator,
-- never by the module. Dropped here, per table: the company-leading indexes, the
-- <table>_company_isolation RLS policy, and the company_id column itself.
--
-- Tables: budgets, budget_lines.
--
-- Posture notes for the composing service's decorator. Two of the dropped indexes are
-- POSTURE (per-unit rules) and must be re-created org-scoped, org_unit_id-leading, same
-- predicates, at composition time — they are intentionally NOT restored here:
--   1. (org_unit_id, code) WHERE deleted-at IS NULL          — was idx_budgets_company_id_code
--   2. (org_unit_id, account_id, cost_center_id, fiscal_period_id)
--      WHERE deleted-at IS NULL — THE CONTROL KEY            — was idx_budget_lines_company_id_...
-- Without the decorator's control-key unique, the service's key_taken() pre-check is the
-- only coverage-ambiguity guard, and a raced insert surfaces as Internal instead of 23505.
-- The lifecycle / per-year listing indexes on budgets and the per-period control read on
-- budget_lines are optional worklist indexes — the decorator may re-create them org-scoped
-- if its deployments want them; their absence changes nothing semantically.
--
-- Ordering guard (the decorator must run FIRST on any database with data): the module
-- never moves tenancy data. A table is safe to strip when EITHER
--   a) it carries org_unit_id with no NULLs — the decorator backfilled it from company_id —
--      or b) it is empty (a fresh database: the earlier chain files created it empty).
-- Otherwise the strip RAISEs, naming the decorator step, rather than dropping a column
-- that still holds the only tenancy key. The file is re-runnable (every drop is IF EXISTS
-- and the tracker has no checksums), so a failed run retries cleanly after the decorator
-- lands.
--
-- RLS enable/force flags are deliberately NOT touched: the decorator owns those now.

DO $$
DECLARE
    t text;
    has_org boolean;
    org_nulls bigint;
    total bigint;
    offenders text := '';
BEGIN
    FOREACH t IN ARRAY ARRAY['budgets', 'budget_lines']
    LOOP
        IF to_regclass(format('budget.%I', t)) IS NULL THEN
            CONTINUE; -- chain not fully applied on this database; nothing to strip
        END IF;

        SELECT EXISTS (
                   SELECT 1 FROM information_schema.columns
                   WHERE table_schema = 'budget' AND table_name = t AND column_name = 'org_unit_id'
               )
        INTO has_org;

        EXECUTE format('SELECT count(*) FROM budget.%I', t) INTO total;

        IF has_org THEN
            EXECUTE format(
                'SELECT count(*) FROM budget.%I WHERE org_unit_id IS NULL', t)
            INTO org_nulls;
        ELSE
            org_nulls := total; -- no org column: every row's only tenancy key is company_id
        END IF;

        IF has_org AND org_nulls = 0 THEN
            CONTINUE; -- decorator backfilled: safe
        END IF;
        IF total = 0 THEN
            CONTINUE; -- empty table (fresh database): safe
        END IF;
        offenders := offenders || format(' budget.%s (%s rows, %s rows not covered by org_unit_id);', t, total, org_nulls);
    END LOOP;

    IF offenders <> '' THEN
        RAISE EXCEPTION 'refusing to strip company_id — these tables are not yet covered by the tenancy decorator:%. Apply the composing service''s tenancy decorator (it backfills org_unit_id from company_id) and re-run; it is the only step that moves tenancy data.', offenders;
    END IF;
END $$;

-- ── budgets ───────────────────────────────────────────────────────────────────
DROP INDEX IF EXISTS budget.idx_budgets_company_id_code;
DROP INDEX IF EXISTS budget.idx_budgets_company_id_status;
DROP INDEX IF EXISTS budget.idx_budgets_company_id_fiscal_year;
DROP POLICY IF EXISTS budgets_company_isolation ON budget.budgets;
ALTER TABLE budget.budgets DROP COLUMN IF EXISTS company_id;

-- ── budget_lines ──────────────────────────────────────────────────────────────
DROP INDEX IF EXISTS budget.idx_budget_lines_company_id_account_id_cost_center_id_fiscal_period_id;
DROP INDEX IF EXISTS budget.idx_budget_lines_company_id_fiscal_period_id;
DROP POLICY IF EXISTS budget_lines_company_isolation ON budget.budget_lines;
ALTER TABLE budget.budget_lines DROP COLUMN IF EXISTS company_id;
