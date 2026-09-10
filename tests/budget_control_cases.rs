//! Budget control cases — the posting evaluation behind accounting's
//! host-wired `BudgetControlPort`, plus the fail-closed posture when the
//! accounting schema is absent.
//!
//! Breach rule: achieved (committed normal-direction movement on the exact
//! key through the posting date) + pending (the prospective posting's own
//! normal-direction contribution) > planned. Only confirmed budgets
//! participate. Exact keys: a NULL cost center matches only NULL positions.
//!
//! Test hygiene: the module is tenant-free (ADR-0029), so no test passes a
//! tenant key. Two consequences shape the fixtures:
//! - accounting is a separate, still company-scoped module — its seed rows
//!   keep carrying a throwaway owner id in their own company column;
//! - with no tenant scoping, each test seeds its fiscal period in its OWN
//!   month and posts inside that window, so `period_covering` resolves to
//!   exactly one row even though rows from sibling tests (and earlier runs)
//!   share the database. Budget codes are likewise suffixed per run: the
//!   module-wide code rule (the decorator re-installs it per org unit) would
//!   otherwise collide with rows a previous run left behind.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Wipe rows a PREVIOUS run of the suites left on the shared scratch database,
/// once per binary. The control cases resolve a posting date to a fiscal
/// period by smallest covering window — stale same-month periods from an
/// earlier run would tie with this run's and the winner would be arbitrary.
/// Every suite seeds its own masters, and the test targets run sequentially
/// (only threads within one binary overlap), so a sweep here cannot race the
/// other binaries; the flags order only this binary's own threads.
static SWEEP_DONE: AtomicBool = AtomicBool::new(false);
static SWEEP_RUNNING: AtomicBool = AtomicBool::new(false);

async fn ensure_clean_slate(pool: &PgPool) {
    if SWEEP_DONE.load(Ordering::SeqCst) {
        return;
    }
    if !SWEEP_RUNNING.swap(true, Ordering::SeqCst) {
        // This thread is the sweeper: FK-safe order, budgets first.
        sqlx::query("DELETE FROM budget.budget_lines").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM budget.budgets").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM accounting.ledgers").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM accounting.journal_lines").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM accounting.journals").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM accounting.fiscal_periods").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM accounting.cost_centers").execute(pool).await.unwrap();
        sqlx::query("DELETE FROM accounting.accounts").execute(pool).await.unwrap();
        SWEEP_DONE.store(true, Ordering::SeqCst);
    } else {
        // Another thread is sweeping; wait for it to finish before seeding.
        while !SWEEP_DONE.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

/// A code unique to this run — the module-wide live-code rule makes reuse
/// across runs refuse.
fn unique_code(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

// ── fixtures ──────────────────────────────────────────────────────────────────

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

/// `month` is 1-based in 2026. Each control test owns one month so the
/// date-to-period resolution is unambiguous on the shared database.
async fn seed_period(pool: &PgPool, company: Uuid, id: Uuid, month: u32) {
    let start = NaiveDate::from_ymd_opt(2026, month, 1).unwrap();
    let end = NaiveDate::from_ymd_opt(2026, month, 28).unwrap();
    sqlx::query(
        r#"INSERT INTO accounting.fiscal_periods
            (id, company_id, period_code, name, period_type, start_date, end_date,
             fiscal_year, fiscal_month, status)
           VALUES ($1,$2,$3,$3,'monthly',$4,$5,2026,$6,'open')"#,
    )
    .bind(id)
    .bind(company)
    .bind(format!("2026-{month:02}"))
    .bind(start)
    .bind(end)
    .bind(month as i32)
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
    owner: Uuid,
    expense: Uuid,
    /// The test's own fiscal period (its window covers `mid_month`).
    period: Uuid,
    /// A posting date inside `period`'s window — and inside no sibling's.
    mid_month: NaiveDate,
}

async fn fixture(pool: &PgPool, month: u32) -> Fx {
    let owner = Uuid::new_v4();
    let expense = Uuid::new_v4();
    let period = Uuid::new_v4();
    seed_account(pool, owner, expense, "5000", "expense", "operating_expense", "debit").await;
    seed_period(pool, owner, period, month).await;
    Fx {
        owner,
        expense,
        period,
        mid_month: NaiveDate::from_ymd_opt(2026, month, 15).unwrap(),
    }
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
                    fiscal_period_id: fx.period,
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

async fn confirm(pool: &PgPool, id: Uuid) {
    BudgetWorkflowService::new(pool.clone())
        .confirm(id, None)
        .await
        .unwrap();
}

fn d(v: i64) -> Decimal {
    Decimal::new(v, 0)
}

fn pending_line(account: Uuid, debit: Decimal, credit: Decimal, cc: Option<Uuid>) -> BudgetControlLine {
    BudgetControlLine { account_id: account, debit, credit, cost_center_id: cc }
}

// ── the cases ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn block_budget_reports_the_breach() {
    let pool = pool().await;
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,3).await; // March
    let budget = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, budget).await;
    seed_ledger(&pool, fx.owner, fx.expense, NaiveDate::from_ymd_opt(2026, 3, 10).unwrap(), fx.period, d(90), d(0), None).await;

    let breaches = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(20), d(0), None)])
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
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,4).await; // April
    let budget = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Warn, d(100), None).await;
    confirm(&pool, budget).await;
    seed_ledger(&pool, fx.owner, fx.expense, NaiveDate::from_ymd_opt(2026, 4, 10).unwrap(), fx.period, d(95), d(0), None).await;

    let breaches = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(10), d(0), None)])
        .await
        .unwrap();
    assert_eq!(breaches.len(), 1);
    assert_eq!(breaches[0].enforcement, BudgetEnforcement::Warn);
}

#[tokio::test]
async fn within_budget_reports_nothing() {
    let pool = pool().await;
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,5).await; // May
    let budget = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, budget).await;
    seed_ledger(&pool, fx.owner, fx.expense, NaiveDate::from_ymd_opt(2026, 5, 10).unwrap(), fx.period, d(90), d(0), None).await;

    // 90 + 5 = 95 <= 100 — fits.
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(5), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Exactly on plan fits too (breach is strictly greater).
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(10), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn null_cost_center_keys_match_exactly() {
    let pool = pool().await;
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,6).await; // June
    let cc = Uuid::new_v4();
    sqlx::query("INSERT INTO accounting.cost_centers (id, company_id, code, name) VALUES ($1,$2,'CC','CC')")
        .bind(cc)
        .bind(fx.owner)
        .execute(&pool)
        .await
        .unwrap();

    // NULL-cc position, achieved 90 of 100.
    let budget = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, budget).await;
    seed_ledger(&pool, fx.owner, fx.expense, NaiveDate::from_ymd_opt(2026, 6, 10).unwrap(), fx.period, d(90), d(0), None).await;

    // A posting carrying a cost center is a DIFFERENT key — no breach.
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(50), d(0), Some(cc))])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Same posting without the cost center hits the NULL key — breach.
    let breaches = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(50), d(0), None)])
        .await
        .unwrap();
    assert_eq!(breaches.len(), 1);
}

#[tokio::test]
async fn draft_and_closed_budgets_are_inert() {
    let pool = pool().await;
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,7).await; // July
    let svc = BudgetWorkflowService::new(pool.clone());

    // Draft: plan exists, control ignores it.
    let draft = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Block, d(10), None).await;
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(500), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // Closed: same.
    svc.confirm(draft, None).await.unwrap();
    svc.close(draft, None).await.unwrap();
    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(500), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());

    // And the workflow service agrees about the status.
    let (header, _) = svc.budget_detail(draft).await.unwrap();
    assert_eq!(header.status, BudgetStatus::Closed);
}

#[tokio::test]
async fn net_negative_pending_never_breaches() {
    // A reversal-shaped posting (credit on a debit-normal account) reduces
    // the key; it can never breach on its own.
    let pool = pool().await;
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,8).await; // August
    let budget = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, budget).await;
    seed_ledger(&pool, fx.owner, fx.expense, NaiveDate::from_ymd_opt(2026, 8, 10).unwrap(), fx.period, d(90), d(0), None).await;

    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting(fx.mid_month, &[pending_line(fx.expense, d(0), d(200), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn posting_date_without_a_period_passes() {
    // The GL stamps no fiscal period for such dates; no position can cover it.
    // 2027 is deliberately outside every period any suite seeds (2025/2026).
    let pool = pool().await;
    ensure_clean_slate(&pool).await;
    let fx = fixture(&pool,9).await; // September (its period is irrelevant here)
    let budget = draft_budget(&pool, &fx, &unique_code("CTL"), BudgetEnforcement::Block, d(100), None).await;
    confirm(&pool, budget).await;

    let empty = BudgetControlService::new(pool.clone())
        .evaluate_posting("2027-06-15".parse().unwrap(), &[pending_line(fx.expense, d(500), d(0), None)])
        .await
        .unwrap();
    assert!(empty.is_empty());
}

#[tokio::test]
async fn accounting_schema_absent_fails_closed() {
    // A budget module deployed without the accounting schema must refuse to
    // evaluate (or report achievements) rather than report "everything within
    // budget". The budget-only database carries no accounting schema at all.
    let url = std::env::var("BUDGET_NOGL_DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_budget_nogl".into());
    let pool = PgPool::connect(&url).await.unwrap();

    let err = BudgetControlService::new(pool.clone())
        .evaluate_posting(jan(15), &[pending_line(Uuid::new_v4(), d(1), d(0), None)])
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetControlError::AccountingUnwired));
    assert_eq!(err.code(), "accounting_unwired");
    assert_eq!(err.http_status(), 503);

    let err = BudgetControlService::new(pool)
        .achievement(Uuid::new_v4(), jan(31))
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetControlError::AccountingUnwired));
}

fn jan(day: u8) -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 1, u32::from(day)).unwrap()
}
