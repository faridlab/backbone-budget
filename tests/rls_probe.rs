//! RLS fence probe (B4) — walks `budget.budgets` and `budget.budget_lines`
//! as a dedicated non-superuser role (superusers bypass RLS even under
//! FORCE, so the fence is probed from below):
//!
//! - unbound (`app.company_id` unset): zero rows on both tables;
//! - bound to company A: exactly A's rows, never B's;
//! - a write with a mismatched `app.company_id` is rejected by WITH CHECK.
//!
//! The workflow service seeds through the superuser pool (migrations/owners
//! bypass the fence); the probe role holds only USAGE + table grants.

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
           VALUES ($1,$2,'2026-01','Jan','monthly','2026-01-01','2026-01-31',2026,1,'open')"#,
    )
    .bind(id)
    .bind(company)
    .execute(pool)
    .await
    .unwrap();
}

/// One confirmed budget + line for `company`, via the validated write path.
async fn seed_confirmed_budget(pool: &PgPool, company: Uuid, account: Uuid, period: Uuid, code: &str) -> Uuid {
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(
            company,
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
    svc.confirm(company, budget.id, None).await.unwrap();
    budget.id
}

#[tokio::test]
async fn company_fence_walk() {
    let pool = pool().await;

    // Two tenants, one confirmed budget each.
    let a = Uuid::new_v4();
    let b = Uuid::new_v4();
    let a_account = Uuid::new_v4();
    let a_period = Uuid::new_v4();
    seed_account(&pool, a, a_account).await;
    seed_period(&pool, a, a_period).await;
    seed_confirmed_budget(&pool, a, a_account, a_period, "RLS-A").await;
    let b_account = Uuid::new_v4();
    let b_period = Uuid::new_v4();
    seed_account(&pool, b, b_account).await;
    seed_period(&pool, b, b_period).await;
    seed_confirmed_budget(&pool, b, b_account, b_period, "RLS-B").await;

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
    sqlx::query("GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA budget TO budget_probe_rls")
        .execute(&pool)
        .await
        .unwrap();

    let mut conn = pool.acquire().await.unwrap();

    // The binding is transaction-local, so each phase runs inside ONE explicit
    // transaction on this connection — separate statements on a pooled
    // connection would each see the variable evaporate (it is scoped to the
    // surrounding transaction, and an autocommit statement is its own).
    sqlx::query("SET ROLE budget_probe_rls").execute(&mut *conn).await.unwrap();

    // 1. Unbound: zero rows everywhere.
    let mut tx = conn.begin().await.unwrap();
    let budgets: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budgets")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(budgets, 0, "unbound role must see no budgets");
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budget_lines")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(lines, 0, "unbound role must see no budget lines");
    tx.commit().await.unwrap();

    // 2. Bound to A: exactly A's budget and lines.
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let budgets: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budgets")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(budgets, 1);
    let codes: Vec<String> = sqlx::query_scalar("SELECT code FROM budget.budgets ORDER BY code")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(codes, vec!["RLS-A".to_string()]);
    let lines: i64 = sqlx::query_scalar("SELECT count(*) FROM budget.budget_lines")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(lines, 1);
    tx.commit().await.unwrap();

    // 3. WITH CHECK rejects writing another tenant's row while bound to A.
    // The refused INSERT aborts its own transaction; the role reset below is
    // session-level and survives it.
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.company_id', $1, true)")
        .bind(a.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let cross = sqlx::query(
        r#"INSERT INTO budget.budgets
            (id, company_id, code, name, fiscal_year, date_from, date_to)
           VALUES ($1,$2,'RLS-EVIL','evil',2026,'2026-01-01','2026-12-31')"#,
    )
    .bind(Uuid::new_v4())
    .bind(b)
    .execute(&mut *tx)
    .await;
    assert!(cross.is_err(), "cross-tenant insert must violate the policy");
    // End the aborted transaction (drop would roll back too, but an explicit
    // rollback releases the connection borrow deterministically).
    tx.rollback().await.unwrap();

    sqlx::query("RESET ROLE").execute(&mut *conn).await.unwrap();
    drop(conn);
}
