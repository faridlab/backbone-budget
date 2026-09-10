//! Tenancy posture probe — what the module SHIPS vs what a composing
//! decorator installs (ADR-0029). Probed from BELOW, as a dedicated
//! non-superuser role (superusers bypass RLS even under FORCE):
//!
//! - the `company_id` column is GONE from both tables;
//! - the legacy `*_company_isolation` policies and company-leading indexes
//!   are GONE (a decorator's own org policy may exist and is not this
//!   probe's concern);
//! - row-level security is still ENABLED and FORCED on both tables — the
//!   module arms the flags; the decorator supplies the policies;
//! - default-deny holds: with rows present but no policy admitting the probe
//!   role, it counts zero rows on both tables.
//!
//! The workflow service seeds through the superuser pool (superusers bypass
//! RLS regardless of FORCE); the probe role holds only USAGE + SELECT grants.

use rust_decimal::Decimal;
use sqlx::{Acquire, PgPool};
use uuid::Uuid;

use backbone_budget::application::service::budget_workflow_service::{
    BudgetWorkflowService, NewBudget, NewBudgetLine,
};
use backbone_budget::domain::entity::BudgetEnforcement;

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_budget".into())
}

async fn pool() -> PgPool {
    PgPool::connect(&db_url()).await.unwrap()
}

/// A code unique to this run — the module-wide live-code rule makes reuse
/// across runs refuse.
fn unique_code(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

async fn seed_account(pool: &PgPool, company: Uuid, id: Uuid) {
    sqlx::query(
        r#"INSERT INTO accounting.accounts
            (id, company_id, account_number, account_code, name, account_type, account_subtype,
             normal_balance, is_detail, is_header, status)
           VALUES ($1,$2,'5000','5000','Expense','expense'::account_type,
                   'operating_expense'::account_subtype,'debit'::normal_balance,
                   TRUE, FALSE, 'active'::account_status)"#,
    )
    .bind(id)
    .bind(company)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_period(pool: &PgPool, company: Uuid, id: Uuid) {
    sqlx::query(
        r#"INSERT INTO accounting.fiscal_periods
            (id, company_id, period_code, name, period_type, start_date, end_date,
             fiscal_year, fiscal_month, status)
           VALUES ($1,$2,$3,$3,'monthly','2026-01-01','2026-01-31',2026,1,'open')"#,
    )
    .bind(id)
    .bind(company)
    .bind(unique_code("2026-01"))
    .execute(pool)
    .await
    .unwrap();
}

/// One confirmed budget + line, via the validated write path.
async fn seed_confirmed_budget(pool: &PgPool, account: Uuid, period: Uuid, code: &str) -> Uuid {
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(
            NewBudget {
                code: code.into(),
                name: code.into(),
                description: None,
                fiscal_year: 2026,
                date_from: "2026-01-01".parse().unwrap(),
                date_to: "2026-12-31".parse().unwrap(),
                enforcement: BudgetEnforcement::Warn,
                lines: vec![NewBudgetLine {
                    account_id: account,
                    cost_center_id: None,
                    fiscal_period_id: period,
                    planned_amount: Decimal::new(100, 0),
                    notes: None,
                }],
            },
            None,
        )
        .await
        .unwrap();
    svc.confirm(budget.id, None).await.unwrap();
    budget.id
}

#[tokio::test]
async fn tenancy_posture_probe() {
    let pool = pool().await;

    // Seed one live budget + line through the superuser pool so the
    // default-deny assertion below is meaningful (rows exist; the probe role
    // just cannot see them). Accounting is a separate, still company-scoped
    // module, so its seed rows keep a throwaway owner id.
    let owner = Uuid::new_v4();
    let account = Uuid::new_v4();
    let period = Uuid::new_v4();
    seed_account(&pool, owner, account).await;
    seed_period(&pool, owner, period).await;
    seed_confirmed_budget(&pool, account, period, &unique_code("RLS")).await;

    // ── schema posture: the tenant axis is gone, the fence flags stay armed ──

    for table in ["budgets", "budget_lines"] {
        let company_col: bool = sqlx::query_scalar(
            r#"SELECT EXISTS (
                   SELECT 1 FROM information_schema.columns
                   WHERE table_schema = 'budget' AND table_name = $1
                     AND column_name = 'company_id'
               )"#,
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!company_col, "budget.{table} must not carry a company_id column");

        let legacy_policies: i64 = sqlx::query_scalar(
            r#"SELECT count(*) FROM pg_policies
               WHERE schemaname = 'budget' AND tablename = $1
                 AND policyname LIKE '%company_isolation'"#,
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(legacy_policies, 0, "budget.{table} must not carry a legacy company-isolation policy");

        let company_indexes: i64 = sqlx::query_scalar(
            r#"SELECT count(*) FROM pg_indexes
               WHERE schemaname = 'budget' AND tablename = $1
                 AND indexname LIKE '%company_id%'"#,
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(company_indexes, 0, "budget.{table} must not carry company-leading indexes");

        let (rls_enabled, rls_forced): (bool, bool) = sqlx::query_as(
            r#"SELECT relrowsecurity, relforcerowsecurity
               FROM pg_class
               WHERE oid = to_regclass($1)"#,
        )
        .bind(format!("budget.{table}"))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(rls_enabled, "budget.{table} must keep row-level security ENABLED (the decorator owns the policies)");
        assert!(rls_forced, "budget.{table} must keep row-level security FORCED (the decorator owns the policies)");
    }

    // ── default-deny, probed from below ─────────────────────────────────────

    // The probe role: non-superuser, minimal grants, idempotent (NOLOGIN —
    // privileges from a prior run make DROP ROLE refuse, so the family
    // pattern creates-if-absent instead).
    sqlx::query(
        r#"DO $$ BEGIN
               IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'budget_probe_rls') THEN
                   CREATE ROLE budget_probe_rls NOLOGIN;
               END IF;
           END $$"#,
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("GRANT USAGE ON SCHEMA budget TO budget_probe_rls").execute(&pool).await.unwrap();
    sqlx::query("GRANT SELECT ON ALL TABLES IN SCHEMA budget TO budget_probe_rls")
        .execute(&pool)
        .await
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("SET ROLE budget_probe_rls").execute(&mut *conn).await.unwrap();

    // With no policy admitting it, the role sees nothing — even though the
    // superuser-seeded rows exist. Each phase runs inside ONE explicit
    // transaction; a SET/RESET pairing is session-level, but keeping the
    // reads transactional matches the family probe pattern.
    let mut tx = conn.begin().await.unwrap();
    let budgets: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budgets")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(budgets, 0, "a role no policy admits must see no budgets (default deny)");
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budget_lines")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(lines, 0, "a role no policy admits must see no budget lines (default deny)");
    tx.commit().await.unwrap();

    sqlx::query("RESET ROLE").execute(&mut *conn).await.unwrap();
    drop(conn);

    // The seeder (superuser) still sees its rows — the denial above is the
    // fence at work, not an empty database.
    let budgets: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budgets")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(budgets >= 1, "the owner pool must still see the seeded budget");
}
