//! Budget achievement cases (B2) — plan vs achieved per line, where achieved
//! is the committed normal-direction ledger movement on the exact control key.
//!
//! Ledger rows are seeded directly (the GL is another module's writer; these
//! tests verify the budget module's reads, not posting). Orientation comes
//! from the ledger row's own `normal_balance` stamp, exactly as the control
//! read resolves it.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_budget::application::service::budget_control_service::BudgetControlService;
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

async fn seed_cost_center(pool: &PgPool, company: Uuid, id: Uuid, code: &str) {
    sqlx::query(
        "INSERT INTO accounting.cost_centers (id, company_id, code, name) VALUES ($1,$2,$3,$3)",
    )
    .bind(id)
    .bind(company)
    .bind(code)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_period(pool: &PgPool, company: Uuid, id: Uuid, start: NaiveDate, end: NaiveDate, month: i32) {
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
    .bind(month)
    .execute(pool)
    .await
    .unwrap();
}

/// One committed ledger movement on an exact key. `normal_balance` stamps the
/// row the way the GL's poster would. The GL enforces parent foreign keys, so
/// each movement mints its own journal row first.
#[allow(clippy::too_many_arguments)]
async fn seed_ledger(
    pool: &PgPool,
    company: Uuid,
    account: Uuid,
    account_number: &str,
    account_type: &str,
    normal_balance: &str,
    posting_date: NaiveDate,
    fiscal_period_id: Uuid,
    debit: Decimal,
    credit: Decimal,
    cost_center_id: Option<Uuid>,
) {
    let change = if normal_balance == "debit" {
        debit - credit
    } else {
        credit - debit
    };
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
           VALUES ($1,$2,$3,1,$4,$5,$5,$6,$7,TRUE)"#,
    )
    .bind(journal_line)
    .bind(journal)
    .bind(company)
    .bind(account)
    .bind(account_number)
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
           VALUES ($1,$2,$3,$4,$4,$5::account_type,$6::normal_balance,$7,$8,$9,
                   $10,$10,$11,2026,1,'seed',0,$12,$12,1,$13,$14,$15)"#,
    )
    .bind(Uuid::new_v4())
    .bind(company)
    .bind(account)
    .bind(account_number)
    .bind(account_type)
    .bind(normal_balance)
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
    revenue: Uuid,
    cc: Uuid,
    jan: Uuid,
    feb: Uuid,
}

async fn fixture(pool: &PgPool) -> Fx {
    let company = Uuid::new_v4();
    let expense = Uuid::new_v4();
    let revenue = Uuid::new_v4();
    let cc = Uuid::new_v4();
    let jan = Uuid::new_v4();
    let feb = Uuid::new_v4();
    seed_account(pool, company, expense, "5000", "expense", "operating_expense", "debit").await;
    seed_account(pool, company, revenue, "4000", "revenue", "operating_revenue", "credit").await;
    seed_cost_center(pool, company, cc, "CC-1").await;
    seed_period(pool, company, jan, "2026-01-01".parse().unwrap(), "2026-01-31".parse().unwrap(), 1).await;
    seed_period(pool, company, feb, "2026-02-01".parse().unwrap(), "2026-02-28".parse().unwrap(), 2).await;
    Fx { company, expense, revenue, cc, jan, feb }
}

async fn confirmed_budget(pool: &PgPool, fx: &Fx, code: &str, lines: Vec<NewBudgetLine>) -> Uuid {
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
                enforcement: BudgetEnforcement::Warn,
                lines,
            },
            None,
        )
        .await
        .unwrap();
    svc.confirm(fx.company, budget.id, None).await.unwrap();
    budget.id
}

fn d(v: i64) -> Decimal {
    Decimal::new(v, 0)
}

// ── the cases ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn debit_normal_achievement_sums_net_movement() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = confirmed_budget(
        &pool,
        &fx,
        "ACH-1",
        vec![NewBudgetLine {
            account_id: fx.expense,
            cost_center_id: None,
            fiscal_period_id: fx.jan,
            planned_amount: d(1000),
            notes: None,
        }],
    )
    .await;

    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-10".parse().unwrap(), fx.jan, d(400), d(0), None).await;
    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-15".parse().unwrap(), fx.jan, d(300), d(0), None).await;
    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-20".parse().unwrap(), fx.jan, d(0), d(100), None).await;

    let rows = BudgetControlService::new(pool.clone())
        .achievement(fx.company, budget, "2026-01-31".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.planned_amount, d(1000));
    assert_eq!(r.achieved_amount, d(600)); // 400 + 300 − 100
    assert_eq!(r.remaining_amount, d(400));
    assert_eq!(r.utilization, Decimal::new(6, 1)); // 0.6
}

#[tokio::test]
async fn through_date_bounds_the_window() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = confirmed_budget(
        &pool,
        &fx,
        "ACH-2",
        vec![NewBudgetLine {
            account_id: fx.expense,
            cost_center_id: None,
            fiscal_period_id: fx.jan,
            planned_amount: d(1000),
            notes: None,
        }],
    )
    .await;

    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-10".parse().unwrap(), fx.jan, d(100), d(0), None).await;
    // March posting shares the January period stamp? No — it would carry its
    // own period; here the same January period but a later posting date.
    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-25".parse().unwrap(), fx.jan, d(50), d(0), None).await;

    let ctrl = BudgetControlService::new(pool.clone());
    let through_jan_20: NaiveDate = "2026-01-20".parse().unwrap();
    let rows = ctrl.achievement(fx.company, budget, through_jan_20).await.unwrap();
    assert_eq!(rows[0].achieved_amount, d(100)); // the 25th is beyond the window
    let through_end: NaiveDate = "2026-01-31".parse().unwrap();
    let rows = ctrl.achievement(fx.company, budget, through_end).await.unwrap();
    assert_eq!(rows[0].achieved_amount, d(150));
}

#[tokio::test]
async fn credit_normal_orients_by_the_row_stamp() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = confirmed_budget(
        &pool,
        &fx,
        "ACH-3",
        vec![NewBudgetLine {
            account_id: fx.revenue,
            cost_center_id: None,
            fiscal_period_id: fx.jan,
            planned_amount: d(5000),
            notes: None,
        }],
    )
    .await;

    seed_ledger(&pool, fx.company, fx.revenue, "4000", "revenue", "credit", "2026-01-05".parse().unwrap(), fx.jan, d(0), d(500), None).await;
    seed_ledger(&pool, fx.company, fx.revenue, "4000", "revenue", "credit", "2026-01-06".parse().unwrap(), fx.jan, d(200), d(0), None).await;

    let rows = BudgetControlService::new(pool.clone())
        .achievement(fx.company, budget, "2026-01-31".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(rows[0].achieved_amount, d(300)); // 500 − 200
}

#[tokio::test]
async fn keys_match_exactly_including_null_cost_center() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = confirmed_budget(
        &pool,
        &fx,
        "ACH-4",
        vec![
            NewBudgetLine {
                account_id: fx.expense,
                cost_center_id: None,
                fiscal_period_id: fx.jan,
                planned_amount: d(100),
                notes: None,
            },
            NewBudgetLine {
                account_id: fx.expense,
                cost_center_id: Some(fx.cc),
                fiscal_period_id: fx.jan,
                planned_amount: d(100),
                notes: None,
            },
        ],
    )
    .await;

    // Movements on three keys: NULL-cc, cc, and a foreign cc (a real cost
    // center row — the GL enforces that foreign key too).
    let foreign = Uuid::new_v4();
    seed_cost_center(&pool, fx.company, foreign, "CC-F").await;
    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-05".parse().unwrap(), fx.jan, d(10), d(0), None).await;
    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-06".parse().unwrap(), fx.jan, d(20), d(0), Some(fx.cc)).await;
    seed_ledger(&pool, fx.company, fx.expense, "5000", "expense", "debit", "2026-01-07".parse().unwrap(), fx.jan, d(40), d(0), Some(foreign)).await;

    let rows = BudgetControlService::new(pool.clone())
        .achievement(fx.company, budget, "2026-01-31".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    for r in &rows {
        match r.cost_center_id {
            None => assert_eq!(r.achieved_amount, d(10)),
            Some(cc) if cc == fx.cc => assert_eq!(r.achieved_amount, d(20)),
            _ => panic!("unexpected key"),
        }
    }
}

#[tokio::test]
async fn budget_without_movements_reads_zero() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let budget = confirmed_budget(
        &pool,
        &fx,
        "ACH-5",
        vec![NewBudgetLine {
            account_id: fx.expense,
            cost_center_id: None,
            fiscal_period_id: fx.feb,
            planned_amount: d(100),
            notes: None,
        }],
    )
    .await;

    let rows = BudgetControlService::new(pool.clone())
        .achievement(fx.company, budget, "2026-02-28".parse().unwrap())
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].achieved_amount, d(0));
    assert_eq!(rows[0].remaining_amount, d(100));
    assert_eq!(rows[0].utilization, d(0));
}
