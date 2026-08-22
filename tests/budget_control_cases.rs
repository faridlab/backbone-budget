//! Budget control cases (B3) — the posting evaluation behind accounting's
//! host-wired `BudgetControlPort`, plus the fail-closed posture when the
//! accounting schema is absent (B5).
//!
//! Breach rule: achieved (committed normal-direction movement on the exact
//! key through the posting date) + pending (the prospective posting's own
//! normal-direction contribution) > planned. Only confirmed budgets
//! participate. Exact keys: a NULL cost center matches only NULL positions.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_budget::application::service::budget_control_service::{
    BudgetControlError, BudgetControlLine, BudgetControlService,
};
use backbone_budget::application::service::budget_workflow_service::{
    BudgetWorkflowService, NewBudget, NewBudgetLine,
};
use backbone_budget::domain::entity::{BudgetEnforcement, BudgetStatus};

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_budget".into())
}

async fn pool() -> PgPool {
    PgPool::connect(&db_url()).await.unwrap()
}

// ── fixtures (mirror the achievement cases) ──────────────────────────────────

async fn seed_account(pool: &PgPool, company: Uuid, id: Uuid, code: &str, at: &str, st: &str, nb: &str) {
    sqlx::query(
        r#"INSERT INTO accounting.accounts
            (id, company_id, account_number, account_code, name, account_type, account_subtype,
             normal_balance, is_detail, is_header, status)
           VALUES ($1,$2,$3,$3,$4,$5::account_type,$6::account_subtype,$7::normal_balance,
                   TRUE, FALSE, 'active'::account_status)"#,
    )
    .bind(id)
    .bind(company)
    .bind(code)
    .bind(code)
    .bind(at)
    .bind(st)
    .bind(nb)
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

/// One committed ledger movement. The GL enforces parent foreign keys, so each
/// movement mints its own journal row first.
#[allow(clippy::too_many_arguments)]
async fn seed_ledger(
    pool: &PgPool,
    company: Uuid,
    account: Uuid,
    posting_date: NaiveDate,
    fiscal_period_id: Uuid,
    debit: Decimal,
    credit: Decimal,
    cost_center_id: Option<Uuid>,
) {
    let change = debit - credit;
    let journal = Uuid::new_v4();
    let journal_line = Uuid::new_v4();
    sqlx::query(
        r#"INSERT INTO accounting.journals
            (id, company_id, journal_number, transaction_date, description)
           VALUES ($1,$2,$3,$4,'seed')"#,
    )
    .bind(journal)
    .bind(company)
    .bind(format!("J-{journal}"))
    .bind(posting_date)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO accounting.journal_lines
            (id, journal_id, company_id, line_number, account_id, account_number,
             account_name, debit_amount, credit_amount, is_posted)
           VALUES ($1,$2,$3,1,$4,'5000','Expense',$5,$6,TRUE)"#,
    )
    .bind(journal_line)
    .bind(journal)
    .bind(company)
    .bind(account)
    .bind(debit)
    .bind(credit)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO accounting.ledgers
            (id, company_id, account_id, account_number, account_name, account_type,
             normal_balance, journal_id, journal_number, journal_line_id,
             transaction_date, posting_date, fiscal_period_id, fiscal_year, fiscal_month,
             description, balance_before, balance_after, balance_change, sequence_number,
             debit_amount, credit_amount, cost_center_id)
           VALUES ($1,$2,$3,'5000','Expense','expense'::account_type,
                   'debit'::normal_balance,$4,$5,$6,$7,$7,$8,2026,1,
                   'seed',0,$9,$9,1,$10,$11,$12)"#,
    )
    .bind(Uuid::new_v4())
    .bind(company)
    .bind(account)
    .bind(journal)
    .bind(format!("J-{journal}"))
    .bind(journal_line)
    .bind(posting_date)
    .bind(fiscal_period_id)
    .bind(change)
    .bind(debit)
    .bind(credit)
    .bind(cost_center_id)
    .execute(pool)
    .await
    .unwrap();
}

struct Fx {
    company: Uuid,
    expense: Uuid,
    jan: Uuid,
}

async fn fixture(pool: &PgPool) -> Fx {
    let company = Uuid::new_v4();
    let expense = Uuid::new_v4();
    let jan = Uuid::new_v4();
    seed_account(pool, company, expense, "5000", "expense", "operating_expense", "debit").await;
    seed_period(pool, company, jan).await;
    Fx { company, expense, jan }
}

async fn draft_budget(
    pool: &PgPool,
    fx: &Fx,
    code: &str,
    enforcement: BudgetEnforcement,
    planned: Decimal,
    cost_center: Option<Uuid>,
) -> Uuid {
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(
            fx.company,
            NewBudget {
                code: code.into(),
                name: code.into(),
                description: None,
                fiscal_year: 2026,
                date_from: "2026-01-01".parse().unwrap(),
                date_to: "2026-12-31".parse().unwrap(),
                enforcement,
                lines: vec![NewBudgetLine {
                    account_id: fx.expense,
                    cost_center_id: cost_center,
                    fiscal_period_id: fx.jan,
                    planned_amount: planned,
                    notes: None,
                }],
            },
            None,
        )
        .await
        .unwrap();
    budget.id
}

async fn confirm(pool: &PgPool, fx: &Fx, id: Uuid) {
    BudgetWorkflowService::new(pool.clone())
        .confirm(fx.company, id, None)
        .await
        .unwrap();
}

fn d(v: i64) -> Decimal {
    Decimal::new(v, 0)
}

fn jan(day: u8) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 1, u32::from(day)).unwrap()
}

fn pending_line(account: Uuid, debit: Decimal, credit: Decimal, cc: Option<Uuid>) -> BudgetControlLine {
    BudgetControlLine { account_id: account, debit, credit, cost_center_id: cc }
}

// ── the cases ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn block_budget_reports_the_breach() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = draft_budget(&pool, &fx, "CTL-1", BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, &fx, budget).await;
    seed_ledger(&pool, fx.company, fx.expense, jan(10), fx.jan, d(90), d(0), None).await;

    let breaches = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(20), d(0), None)])
        .await
        .unwrap();
    assert_eq!(breaches.len(), 1);
    let b = &breaches[0];
    assert_eq!(b.budget_id, budget);
    assert_eq!(b.planned_amount, d(100));
    assert_eq!(b.achieved_amount, d(90));
    assert_eq!(b.pending_amount, d(20));
    assert_eq!(b.enforcement, BudgetEnforcement::Block);
}

#[tokio::test]
async fn warn_budget_reports_with_warn_posture() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = draft_budget(&pool, &fx, "CTL-2", BudgetEnforcement::Warn, d(100), None).await;
    confirm(&pool, &fx, budget).await;
    seed_ledger(&pool, fx.company, fx.expense, jan(10), fx.jan, d(95), d(0), None).await;

    let breaches = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(10), d(0), None)])
        .await
        .unwrap();
    assert_eq!(breaches.len(), 1);
    assert_eq!(breaches[0].enforcement, BudgetEnforcement::Warn);
}

#[tokio::test]
async fn within_budget_reports_nothing() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = draft_budget(&pool, &fx, "CTL-3", BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, &fx, budget).await;
    seed_ledger(&pool, fx.company, fx.expense, jan(10), fx.jan, d(90), d(0), None).await;

    // 90 + 5 = 95 <= 100 — fits.
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(5), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Exactly on plan fits too (breach is strictly greater).
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(10), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn null_cost_center_keys_match_exactly() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO accounting.cost_centers (id, company_id, code, name) VALUES ($1,$2,'CC','CC')")
        .bind(cc)
        .bind(fx.company)
        .execute(&pool)
        .await
        .unwrap();

    // NULL-cc position, achieved 90 of 100.
    let budget = draft_budget(&pool, &fx, "CTL-4", BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, &fx, budget).await;
    seed_ledger(&pool, fx.company, fx.expense, jan(10), fx.jan, d(90), d(0), None).await;

    // A posting carrying a cost center is a DIFFERENT key — no breach.
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(50), d(0), Some(cc))])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Same posting without the cost center hits the NULL key — breach.
    let breaches = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(50), d(0), None)])
        .await
        .unwrap();
    assert_eq!(breaches.len(), 1);
}

#[tokio::test]
async fn draft_and_closed_budgets_are_inert() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());

    // Draft: plan exists, control ignores it.
    let draft = draft_budget(&pool, &fx, "CTL-5A", BudgetEnforcement::Block, d(10), None).await;
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(500), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Closed: same.
    svc.confirm(fx.company, draft, None).await.unwrap();
    svc.close(fx.company, draft, None).await.unwrap();
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(500), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // And the workflow service agrees about the status.
    let (header, _) = svc.budget_detail(fx.company, draft).await.unwrap();
    assert_eq!(header.status, BudgetStatus::Closed);
}

#[tokio::test]
async fn net_negative_pending_never_breaches() {
    // A reversal-shaped posting (credit on a debit-normal account) reduces
    // the key; it can never breach on its own.
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = draft_budget(&pool, &fx, "CTL-6", BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, &fx, budget).await;
    seed_ledger(&pool, fx.company, fx.expense, jan(10), fx.jan, d(90), d(0), None).await;

    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, jan(15), &[pending_line(fx.expense, d(0), d(200), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn cross_tenant_evaluation_sees_no_positions() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = draft_budget(&pool, &fx, "CTL-7", BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, &fx, budget).await;
    seed_ledger(&pool, fx.company, fx.expense, jan(10), fx.jan, d(90), d(0), None).await;

    // Another company evaluating the same shape sees neither the position nor
    // the movement — its own empty ledger and empty plan.
    let other = Uuid::new_v4();
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(other, jan(15), &[pending_line(fx.expense, d(50), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Coverage is fenced the same way.
    let ctrl = BudgetControlService::new(pool.clone());
    assert!(ctrl.coverage(other, fx.expense, None, fx.jan).await.unwrap().is_none());
    assert!(ctrl.coverage(fx.company, fx.expense, None, fx.jan).await.unwrap().is_some());
}

#[tokio::test]
async fn posting_date_without_a_period_passes() {
    // The GL stamps no fiscal period for such dates; no position can cover it.
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = draft_budget(&pool, &fx, "CTL-8", BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, &fx, budget).await;

    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.company, "2026-06-15".parse().unwrap(), &[pending_line(fx.expense, d(500), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn accounting_schema_absent_fails_closed() {
    // B5 — a budget module deployed without the accounting schema must
    // refuse to evaluate (or report achievements) rather than report
    // "everything within budget". The budget-only database carries no
    // accounting schema at all.
    let url = std::env::var("BUDGET_NOGL_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_budget_nogl".into());
    let pool = PgPool::connect(&url).await.unwrap();

    let err = BudgetControlService::new(pool.clone())
        .evaluate_posting(Uuid::new_v4(), jan(15), &[pending_line(Uuid::new_v4(), d(1), d(0), None)])
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetControlError::AccountingUnwired));
    assert_eq!(err.code(), "accounting_unwired");
    assert_eq!(err.http_status(), 503);

    let err = BudgetControlService::new(pool)
        .achievement(Uuid::new_v4(), Uuid::new_v4(), jan(31))
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetControlError::AccountingUnwired));
}
