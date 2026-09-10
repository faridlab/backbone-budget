//! Budget workflow golden cases — the validated write path.
//!
//! The module is tenant-free (ADR-0029): no test passes a tenant key to any
//! verb. Each test mints fresh accounting masters (the cross-schema data the
//! guards read — accounting is a separate module that still scopes its own
//! rows), then drives `BudgetWorkflowService` verbs through the guard matrix:
//!
//! - BG1 budget_coverage_conflict — one live line per control key, across budgets
//! - BG2 budget_no_lines — confirm requires a plan
//! - BG3 budget_invalid_amount — planned > 0
//! - BG4 budget_account_missing — detail + active account
//! - BG5 budget_cost_center_invalid — leaf + active cost center
//! - BG6 budget_period_invalid — period fits the header's year and range
//! - BG7 budget_invalid_transition — CAS on confirm/close/cancel
//! - plus: draft-only line edits, enforcement's own edit rule, code uniqueness
//!   (service pre-check; the composing decorator's org-scoped partial uniques
//!   are the raced-insert backstop in a decorated deployment)

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use backbone_budget::application::service::budget_workflow_service::{
    BudgetPatch, BudgetWorkflowError, BudgetWorkflowService, NewBudget, NewBudgetLine,
};
use backbone_budget::domain::entity::{BudgetEnforcement, BudgetStatus};

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgresql://postgres:postgres@localhost:5433/backbone_budget".into())
}

async fn pool() -> PgPool {
    PgPool::connect(&db_url()).await.unwrap()
}

// ── accounting master fixtures (accounting is unstripped: its rows still
//    carry a company_id NOT NULL column, so the seeds keep providing one) ─────

async fn seed_account(
    pool: &PgPool,
    company: Uuid,
    id: Uuid,
    code: &str,
    at: &str,
    st: &str,
    nb: &str,
    detail: bool,
    status: &str,
) {
    sqlx::query(
        r#"INSERT INTO accounting.accounts
            (id, company_id, account_number, account_code, name, account_type, account_subtype,
             normal_balance, is_detail, is_header, status)
           VALUES ($1,$2,$3,$3,$4,$5::account_type,$6::account_subtype,$7::normal_balance,
                   $8, NOT $8, $9::account_status)"#,
    )
    .bind(id)
    .bind(company)
    .bind(code)
    .bind(code)
    .bind(at)
    .bind(st)
    .bind(nb)
    .bind(detail)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

/// `active=false` seeds an inactive leaf (the status enum replaced the old
/// is_active boolean in accounting's cost-center lifecycle migration).
async fn seed_cost_center(pool: &PgPool, company: Uuid, id: Uuid, code: &str, group: bool, active: bool) {
    sqlx::query(
        r#"INSERT INTO accounting.cost_centers (id, company_id, code, name, is_group, status)
           VALUES ($1,$2,$3,$4,$5,$6::cost_center_status)"#,
    )
    .bind(id)
    .bind(company)
    .bind(code)
    .bind(code)
    .bind(group)
    .bind(if active { "active" } else { "inactive" })
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_period(
    pool: &PgPool,
    company: Uuid,
    id: Uuid,
    code: &str,
    start: NaiveDate,
    end: NaiveDate,
    year: i32,
    month: i32,
) {
    sqlx::query(
        r#"INSERT INTO accounting.fiscal_periods
            (id, company_id, period_code, name, period_type, start_date, end_date,
             fiscal_year, fiscal_month, status)
           VALUES ($1,$2,$3,$3,'monthly',$4,$5,$6,$7,'open')"#,
    )
    .bind(id)
    .bind(company)
    .bind(code)
    .bind(start)
    .bind(end)
    .bind(year)
    .bind(month)
    .execute(pool)
    .await
    .unwrap();
}

/// One fresh set of accounting masters (a throwaway owner id fills the
/// accounting schema's own company column): an expense account, bank account,
/// a leaf cost center, a group cost center, an inactive cost center, and
/// January/February 2026 monthly periods.
struct Fixture {
    owner: Uuid,
    expense: Uuid,
    bank: Uuid,
    cc_leaf: Uuid,
    cc_group: Uuid,
    cc_inactive: Uuid,
    jan: Uuid,
    feb: Uuid,
    jan_2025: Uuid,
}

async fn fixture(pool: &PgPool) -> Fixture {
    let owner = Uuid::new_v4();
    let expense = Uuid::new_v4();
    let bank = Uuid::new_v4();
    let cc_leaf = Uuid::new_v4();
    let cc_group = Uuid::new_v4();
    let cc_inactive = Uuid::new_v4();
    let jan = Uuid::new_v4();
    let feb = Uuid::new_v4();
    let jan_2025 = Uuid::new_v4();

    seed_account(pool, owner, expense, "5000", "expense", "operating_expense", "debit", true, "active").await;
    seed_account(pool, owner, bank, "1100", "asset", "bank", "debit", true, "active").await;
    seed_cost_center(pool, owner, cc_leaf, "CC-1", false, true).await;
    seed_cost_center(pool, owner, cc_group, "CC-G", true, true).await;
    seed_cost_center(pool, owner, cc_inactive, "CC-X", false, false).await;
    seed_period(pool, owner, jan, "2026-01", "2026-01-01".parse().unwrap(), "2026-01-31".parse().unwrap(), 2026, 1).await;
    seed_period(pool, owner, feb, "2026-02", "2026-02-01".parse().unwrap(), "2026-02-28".parse().unwrap(), 2026, 2).await;
    seed_period(pool, owner, jan_2025, "2025-01", "2025-01-01".parse().unwrap(), "2025-01-31".parse().unwrap(), 2025, 1).await;

    Fixture {
        owner,
        expense,
        bank,
        cc_leaf,
        cc_group,
        cc_inactive,
        jan,
        feb,
        jan_2025,
    }
}

fn jan_line(fx: &Fixture, account: Uuid, amount: Decimal) -> NewBudgetLine {
    NewBudgetLine {
        account_id: account,
        cost_center_id: None,
        fiscal_period_id: fx.jan,
        planned_amount: amount,
        notes: None,
    }
}

fn new_budget(code: String, lines: Vec<NewBudgetLine>) -> NewBudget {
    let name = format!("{code} plan");
    NewBudget {
        code,
        name,
        description: None,
        fiscal_year: 2026,
        date_from: "2026-01-01".parse().unwrap(),
        date_to: "2026-12-31".parse().unwrap(),
        enforcement: BudgetEnforcement::Warn,
        lines,
    }
}

/// A code unique to this run — the module-wide live-code rule makes reuse
/// across runs refuse (under a decorated deployment the rule is per org
/// unit; the pre-check here is deliberately unscoped, matching the module's
/// tenant-free posture).
fn unique_code(prefix: &str) -> String {
    format!("{prefix}-{}", &Uuid::new_v4().simple().to_string()[..8])
}

fn code(e: &BudgetWorkflowError) -> &'static str {
    e.code()
}

// ── the cases ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_confirm_close_happy_path() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());

    let budget = svc
        .create_budget(
            new_budget(
                unique_code("B-2026"),
                vec![
                    jan_line(&fx, fx.expense, Decimal::new(1000, 0)),
                    NewBudgetLine {
                        account_id: fx.expense,
                        cost_center_id: Some(fx.cc_leaf),
                        fiscal_period_id: fx.feb,
                        planned_amount: Decimal::new(500, 0),
                        notes: None,
                    },
                ],
            ),
            None,
        )
        .await
        .unwrap();
    assert_eq!(budget.status, BudgetStatus::Draft);
    assert_eq!(budget.enforcement, BudgetEnforcement::Warn);

    let (header, lines) = svc.budget_detail(budget.id).await.unwrap();
    assert_eq!(header.id, budget.id);
    assert_eq!(lines.len(), 2);
    // The line denormalizes the period's own year/month.
    let jan = lines.iter().find(|l| l.fiscal_period_id == fx.jan).unwrap();
    assert_eq!(jan.fiscal_year, 2026);
    assert_eq!(jan.fiscal_month, Some(1));

    let confirmed = svc.confirm(budget.id, None).await.unwrap();
    assert_eq!(confirmed.status, BudgetStatus::Confirmed);

    let closed = svc.close(budget.id, None).await.unwrap();
    assert_eq!(closed.status, BudgetStatus::Closed);
}

#[tokio::test]
async fn zero_amount_line_refuses() {
    // BG3
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    let err = svc
        .create_budget(new_budget(unique_code("B-Z"), vec![jan_line(&fx, fx.expense, Decimal::ZERO)]), None)
        .await
        .unwrap_err();
    assert_eq!(code(&err), "budget_invalid_amount");
    assert_eq!(err.http_status(), 422);
}

#[tokio::test]
async fn non_detail_or_missing_account_refuses() {
    // BG4
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let header_account = Uuid::new_v4();
    seed_account(&pool, fx.owner, header_account, "1000", "asset", "non_current_asset", "debit", false, "active").await;
    let svc = BudgetWorkflowService::new(pool.clone());

    let unknown = svc
        .create_budget(new_budget(unique_code("B-U"), vec![jan_line(&fx, Uuid::new_v4(), Decimal::ONE)]), None)
        .await
        .unwrap_err();
    assert_eq!(code(&unknown), "budget_account_missing");
    assert_eq!(unknown.http_status(), 422);

    let header = svc
        .create_budget(new_budget(unique_code("B-H"), vec![jan_line(&fx, header_account, Decimal::ONE)]), None)
        .await
        .unwrap_err();
    assert_eq!(code(&header), "budget_account_missing");
}

#[tokio::test]
async fn group_or_inactive_cost_center_refuses() {
    // BG5
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    for (cc, label) in [(fx.cc_group, "group"), (fx.cc_inactive, "inactive")] {
        let err = svc
            .create_budget(
                new_budget(
                    unique_code(&format!("B-{label}")),
                    vec![NewBudgetLine {
                        account_id: fx.expense,
                        cost_center_id: Some(cc),
                        fiscal_period_id: fx.jan,
                        planned_amount: Decimal::ONE,
                        notes: None,
                    }],
                ),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(code(&err), "budget_cost_center_invalid");
        assert_eq!(err.http_status(), 422);
    }
}

#[tokio::test]
async fn period_outside_header_year_or_range_refuses() {
    // BG6
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());

    // A 2025 period can never fit a 2026 budget.
    let wrong_year = svc
        .create_budget(
            new_budget(
                unique_code("B-Y"),
                vec![NewBudgetLine {
                    account_id: fx.expense,
                    cost_center_id: None,
                    fiscal_period_id: fx.jan_2025,
                    planned_amount: Decimal::ONE,
                    notes: None,
                }],
            ),
            None,
        )
        .await;
    match wrong_year {
        Err(e) => {
            assert_eq!(code(&e), "budget_period_invalid");
            assert_eq!(e.http_status(), 422);
        }
        Ok(_) => panic!("2025 period must not fit a 2026 budget"),
    }

    // Header range narrower than the period: date_to before the period's end.
    let mut input = new_budget(unique_code("B-R"), vec![jan_line(&fx, fx.expense, Decimal::ONE)]);
    input.date_to = "2026-01-15".parse().unwrap();
    let err = svc.create_budget(input, None).await.unwrap_err();
    assert_eq!(code(&err), "budget_period_invalid");
}

#[tokio::test]
async fn duplicate_control_key_refuses_across_budgets() {
    // BG1 — one live line per key, whatever the budget. On this bare,
    // undecorated database the service's per-verb pre-check is the guard;
    // under a decorated deployment the decorator's org-scoped partial unique
    // backstops a raced insert with 23505.
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());

    svc.create_budget(
        new_budget(unique_code("B-1"), vec![jan_line(&fx, fx.expense, Decimal::new(100, 0))]),
        None,
    )
    .await
    .unwrap();

    // Same key in a second budget (draft or not) refuses.
    let err = svc
        .create_budget(
            new_budget(unique_code("B-2"), vec![jan_line(&fx, fx.expense, Decimal::new(50, 0))]),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "budget_coverage_conflict");
    assert_eq!(err.http_status(), 422);

    // add_line to an existing budget hits the same guard.
    let b2 = svc
        .create_budget(
            new_budget(unique_code("B-3"), vec![NewBudgetLine {
                account_id: fx.bank,
                cost_center_id: None,
                fiscal_period_id: fx.jan,
                planned_amount: Decimal::ONE,
                notes: None,
            }]),
            None,
        )
        .await
        .unwrap();
    let err = svc
        .add_line(b2.id, jan_line(&fx, fx.expense, Decimal::ONE), None)
        .await
        .unwrap_err();
    assert_eq!(code(&err), "budget_coverage_conflict");

    // A different cost center is a different key — allowed.
    svc.add_line(
        b2.id,
        NewBudgetLine {
            account_id: fx.expense,
            cost_center_id: Some(fx.cc_leaf),
            fiscal_period_id: fx.jan,
            planned_amount: Decimal::ONE,
            notes: None,
        },
        None,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn confirm_without_lines_refuses() {
    // BG2
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(new_budget(unique_code("B-E"), vec![]), None)
        .await
        .unwrap();
    let err = svc.confirm(budget.id, None).await.unwrap_err();
    assert_eq!(code(&err), "budget_no_lines");
    assert_eq!(err.http_status(), 422);
}

#[tokio::test]
async fn line_edits_are_draft_only() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(
            new_budget(unique_code("B-D"), vec![jan_line(&fx, fx.expense, Decimal::new(100, 0))]),
            None,
        )
        .await
        .unwrap();

    // Draft: edit + delete lines freely.
    let line = svc.budget_detail(budget.id).await.unwrap().1.remove(0);
    let updated = svc
        .update_line(budget.id, line.id, Decimal::new(250, 0), Some("raised".into()), None)
        .await
        .unwrap();
    assert_eq!(updated.planned_amount, Decimal::new(250, 0));
    svc.delete_line(budget.id, line.id, None).await.unwrap();
    let line2 = svc
        .add_line(budget.id, jan_line(&fx, fx.expense, Decimal::new(300, 0)), None)
        .await
        .unwrap();

    svc.confirm(budget.id, None).await.unwrap();

    // Confirmed: every line mutation refuses.
    assert_eq!(
        code(&svc.add_line(budget.id, jan_line(&fx, fx.bank, Decimal::ONE), None).await.unwrap_err()),
        "budget_not_draft"
    );
    assert_eq!(
        code(&svc.update_line(budget.id, line2.id, Decimal::ONE, None, None).await.unwrap_err()),
        "budget_not_draft"
    );
    assert_eq!(
        code(&svc.delete_line(budget.id, line2.id, None).await.unwrap_err()),
        "budget_not_draft"
    );
    // Non-enforcement header fields freeze too.
    assert_eq!(
        code(&svc
            .update_budget(budget.id, BudgetPatch { name: Some("x".into()), ..Default::default() }, None)
            .await
            .unwrap_err()),
        "budget_not_draft"
    );
}

#[tokio::test]
async fn enforcement_follows_its_own_edit_rule() {
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(
            new_budget(unique_code("B-F"), vec![jan_line(&fx, fx.expense, Decimal::ONE)]),
            None,
        )
        .await
        .unwrap();

    // Editable while draft.
    let flipped = svc
        .update_budget(
            budget.id,
            BudgetPatch { enforcement: Some(BudgetEnforcement::Block), ..Default::default() },
            None,
        )
        .await
        .unwrap();
    assert_eq!(flipped.enforcement, BudgetEnforcement::Block);

    // Still editable while confirmed — the day-to-day posture knob.
    svc.confirm(budget.id, None).await.unwrap();
    let flipped = svc
        .update_budget(
            budget.id,
            BudgetPatch { enforcement: Some(BudgetEnforcement::Warn), ..Default::default() },
            None,
        )
        .await
        .unwrap();
    assert_eq!(flipped.enforcement, BudgetEnforcement::Warn);

    // Frozen once closed.
    svc.close(budget.id, None).await.unwrap();
    let err = svc
        .update_budget(
            budget.id,
            BudgetPatch { enforcement: Some(BudgetEnforcement::Block), ..Default::default() },
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "budget_enforcement_locked");
}

#[tokio::test]
async fn transitions_are_cas() {
    // BG7
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    let budget = svc
        .create_budget(
            new_budget(unique_code("B-C"), vec![jan_line(&fx, fx.expense, Decimal::ONE)]),
            None,
        )
        .await
        .unwrap();

    // close from draft refuses (409)
    let err = svc.close(budget.id, None).await.unwrap_err();
    assert_eq!(code(&err), "budget_invalid_transition");
    assert_eq!(err.http_status(), 409);

    svc.confirm(budget.id, None).await.unwrap();
    // confirm twice refuses
    let err = svc.confirm(budget.id, None).await.unwrap_err();
    assert_eq!(code(&err), "budget_invalid_transition");

    // cancel from confirmed is allowed; every later verb refuses
    let cancelled = svc.cancel(budget.id, None).await.unwrap();
    assert_eq!(cancelled.status, BudgetStatus::Cancelled);
    for err in [
        svc.confirm(budget.id, None).await.unwrap_err(),
        svc.close(budget.id, None).await.unwrap_err(),
        svc.cancel(budget.id, None).await.unwrap_err(),
    ] {
        assert_eq!(code(&err), "budget_invalid_transition");
    }

    // Unknown budget 404s.
    assert_eq!(
        code(&svc.confirm(Uuid::new_v4(), None).await.unwrap_err()),
        "budget_not_found"
    );
}

#[tokio::test]
async fn repeated_code_refuses() {
    // The service's code_taken pre-check keeps the typed guard deterministic
    // on an undecorated database; in a decorated deployment the decorator's
    // (org_unit_id, code) partial unique is the raced-insert backstop.
    let pool = pool().await;
    let fx = fixture(&pool).await;
    let svc = BudgetWorkflowService::new(pool.clone());
    let shared = unique_code("SAME");
    svc.create_budget(
        new_budget(shared.clone(), vec![jan_line(&fx, fx.expense, Decimal::ONE)]),
        None,
    )
    .await
    .unwrap();
    let err = svc
        .create_budget(
            new_budget(shared, vec![NewBudgetLine {
                account_id: fx.bank,
                cost_center_id: None,
                fiscal_period_id: fx.feb,
                planned_amount: Decimal::ONE,
                notes: None,
            }]),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(code(&err), "budget_code_taken");
    assert_eq!(err.http_status(), 422);
}
