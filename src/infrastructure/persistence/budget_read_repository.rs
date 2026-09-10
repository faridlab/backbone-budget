//! `BudgetReadRepository` — the cross-schema reads behind budget control and
//! achievements (hand-authored, user-owned; see `metaphor.codegen.yaml`).
//!
//! Budgets are plans; their achieved amounts live in the general ledger
//! (`accounting.ledgers`), another module's schema. There is deliberately NO
//! Cargo edge into backbone-accounting (the family's zero-edge rule): these
//! reads speak plain SQL against the shared database, mirroring the ledger
//! query shapes accounting's own reporting repository uses. The composing app
//! guarantees both modules share one database.
//!
//! Fail-closed on the GL: every read first probes `to_regclass` for the
//! accounting tables. A budget module deployed without the accounting schema
//! cannot compute achievements or control postings, so those reads refuse
//! (`accounting_unwired`) instead of silently reporting zero spend.
//!
//! Tenancy: none, by design (ADR-0029). No query here carries a tenant
//! predicate — the tables this module reads are scoped by the COMPOSING
//! service's fence, whatever shape each owner's schema carries. Every read
//! runs on a caller-managed connection, and the services open their
//! transactions by relaying the ambient request org scope
//! (`backbone_orm::org_scope::bind_org_scope_on`), so in a decorated
//! deployment the row-level fences govern what these queries can see: a
//! cross-tenant id simply matches zero rows. Unfenced deployments get an
//! unfenced module — that is the module being agnostic and reusable.

use chrono::NaiveDate;
use rust_decimal::Decimal;
use uuid::Uuid;

use crate::domain::entity::{Budget, BudgetLine};

/// One achievement row: what a plan position has absorbed vs its plan.
#[derive(Debug, serde::Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct AchievementRow {
    pub budget_id: Uuid,
    pub budget_line_id: Uuid,
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub planned_amount: Decimal,
    /// Net normal-direction ledger movement through `through` (committed).
    pub achieved_amount: Decimal,
    pub remaining_amount: Decimal,
    /// achieved / planned, 0 when nothing is planned.
    pub utilization: Decimal,
}

/// A confirmed plan position + the enforcement posture of its budget, as the
/// control read sees it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ControlPosition {
    pub budget_id: Uuid,
    pub budget_line_id: Uuid,
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub planned_amount: Decimal,
    pub enforcement: crate::domain::entity::BudgetEnforcement,
}

/// Net normal-direction movement already committed on a control key.
#[derive(Debug, sqlx::FromRow)]
pub struct AchievedRow {
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Option<Uuid>,
    /// debit-normal: Σdebit − Σcredit; credit-normal: Σcredit − Σdebit.
    pub achieved: Decimal,
}

/// An account's posting orientation (stamped on every ledger row at post time).
#[derive(Debug, sqlx::FromRow)]
pub struct NormalBalanceRow {
    pub id: Uuid,
    pub normal_balance: String,
}

/// One fiscal period row as the control resolves it from a posting date.
#[derive(Debug, sqlx::FromRow)]
pub struct PeriodRow {
    pub id: Uuid,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub fiscal_year: i32,
    pub fiscal_month: Option<i32>,
}

pub struct BudgetReadRepository;

impl BudgetReadRepository {
    // ── schema guards ────────────────────────────────────────────────────────

    /// Fail-closed probe: refuses when the accounting tables this module reads
    /// do not exist (budgets are meaningless without the GL).
    pub async fn require_gl(
        conn: &mut sqlx::PgConnection,
    ) -> Result<(), sqlx::Error> {
        // to_regclass yields regclass (NULL when absent) — no uuid cast exists,
        // so probe with a plain boolean.
        let present: bool =
            sqlx::query_scalar("SELECT to_regclass('accounting.ledgers') IS NOT NULL")
                .fetch_one(&mut *conn)
                .await?;
        if !present {
            return Err(sqlx::Error::Configuration(
                "accounting schema unwired: accounting.ledgers is absent — budget \
                 achievements and control need the general ledger"
                    .into(),
            ));
        }
        Ok(())
    }

    // ── master-data validation reads (guards BG4/BG5/BG6) ───────────────────

    /// BG4: the account exists, is a detail account, and is active.
    pub async fn account_postable(
        &self,
        conn: &mut sqlx::PgConnection,
        account_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let ok: Option<bool> = sqlx::query_scalar(
            r#"SELECT TRUE FROM accounting.accounts
               WHERE id = $1
                 AND is_detail AND NOT is_header
                 AND status = 'active'
                 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(account_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(ok.unwrap_or(false))
    }

    /// BG5: the cost center exists, is a leaf (not a group), and is active.
    /// (`is_active` became the `status` enum — 'active'/'inactive' — in the
    /// accounting module's lifecycle migration; this reads the current shape.)
    pub async fn cost_center_usable(
        &self,
        conn: &mut sqlx::PgConnection,
        cost_center_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let ok: Option<bool> = sqlx::query_scalar(
            r#"SELECT TRUE FROM accounting.cost_centers
               WHERE id = $1
                 AND NOT is_group AND status = 'active'
                 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(cost_center_id)
        .fetch_optional(&mut *conn)
        .await?;
        Ok(ok.unwrap_or(false))
    }

    /// BG6: the fiscal period exists and is a live row. Under a decorated
    /// deployment the composing scope's fence limits the visible periods to
    /// the caller's own; with no fence mounted this is a plain id lookup.
    pub async fn find_period(
        &self,
        conn: &mut sqlx::PgConnection,
        fiscal_period_id: Uuid,
    ) -> Result<Option<PeriodRow>, sqlx::Error> {
        sqlx::query_as::<_, PeriodRow>(
            r#"SELECT id, start_date, end_date, fiscal_year, fiscal_month
               FROM accounting.fiscal_periods
               WHERE id = $1
                 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(fiscal_period_id)
        .fetch_optional(&mut *conn)
        .await
    }

    /// The fiscal period covering a posting date (smallest covering window
    /// wins, mirroring how the GL stamps posts). Under a decorated deployment
    /// the composing scope's fence limits the candidates to the caller's own
    /// calendar; with no fence mounted, every period in the database is a
    /// candidate.
    pub async fn period_covering(
        &self,
        conn: &mut sqlx::PgConnection,
        posting_date: NaiveDate,
    ) -> Result<Option<PeriodRow>, sqlx::Error> {
        sqlx::query_as::<_, PeriodRow>(
            r#"SELECT id, start_date, end_date, fiscal_year, fiscal_month
               FROM accounting.fiscal_periods
               WHERE start_date <= $1 AND end_date >= $1
                 AND (metadata->>'deleted_at') IS NULL
               ORDER BY (end_date - start_date) ASC LIMIT 1"#,
        )
        .bind(posting_date)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Normal-balance orientation for a set of accounts (used to orient
    /// prospective posting contributions; the ledger rows carry theirs).
    pub async fn normal_balances(
        &self,
        conn: &mut sqlx::PgConnection,
        account_ids: &[Uuid],
    ) -> Result<Vec<NormalBalanceRow>, sqlx::Error> {
        sqlx::query_as::<_, NormalBalanceRow>(
            r#"SELECT id, normal_balance::text AS normal_balance
               FROM accounting.accounts WHERE id = ANY($1)"#,
        )
        .bind(account_ids)
        .fetch_all(&mut *conn)
        .await
    }

    // ── the control read ─────────────────────────────────────────────────────

    /// Confirmed-budget positions matching a set of exact control keys
    /// (account x cost-center-including-NULL x fiscal period). Draft, closed,
    /// and cancelled budgets are invisible to control.
    pub async fn confirmed_positions_on_keys(
        &self,
        conn: &mut sqlx::PgConnection,
        account_ids: &[Uuid],
        fiscal_period_id: Uuid,
    ) -> Result<Vec<ControlPosition>, sqlx::Error> {
        sqlx::query_as::<_, ControlPosition>(
            r#"SELECT l.budget_id, l.id AS budget_line_id, l.account_id,
                      l.cost_center_id, l.fiscal_period_id, l.planned_amount,
                      b.enforcement::text::budget_enforcement AS enforcement
               FROM budget.budget_lines l
               JOIN budget.budgets b ON b.id = l.budget_id
               WHERE l.account_id = ANY($1)
                 AND l.fiscal_period_id = $2
                 AND b.status = 'confirmed'
                 AND (l.metadata->>'deleted_at') IS NULL
                 AND (b.metadata->>'deleted_at') IS NULL"#,
        )
        .bind(account_ids)
        .bind(fiscal_period_id)
        .fetch_all(&mut *conn)
        .await
    }

    /// Committed normal-direction movement per control key in one fiscal
    /// period, through `through` (posting date inclusive). Orientation comes
    /// from the ledger rows' own post-time normal_balance stamp.
    pub async fn achieved_on_keys(
        &self,
        conn: &mut sqlx::PgConnection,
        account_ids: &[Uuid],
        fiscal_period_id: Uuid,
        through: NaiveDate,
    ) -> Result<Vec<AchievedRow>, sqlx::Error> {
        sqlx::query_as::<_, AchievedRow>(
            r#"SELECT account_id, cost_center_id, fiscal_period_id,
                      SUM(CASE WHEN normal_balance = 'debit'::normal_balance
                               THEN debit_amount - credit_amount
                               ELSE credit_amount - debit_amount END) AS achieved
               FROM accounting.ledgers
               WHERE account_id = ANY($1)
                 AND fiscal_period_id = $2
                 AND posting_date <= $3
                 AND (metadata->>'deleted_at') IS NULL
               GROUP BY account_id, cost_center_id, fiscal_period_id"#,
        )
        .bind(account_ids)
        .bind(fiscal_period_id)
        .bind(through)
        .fetch_all(&mut *conn)
        .await
    }

    // ── achievements (per budget) ────────────────────────────────────────────

    /// The live budget header (any status) — the achievement read's anchor.
    pub async fn get_budget(
        &self,
        conn: &mut sqlx::PgConnection,
        budget_id: Uuid,
    ) -> Result<Option<Budget>, sqlx::Error> {
        sqlx::query_as::<_, Budget>(
            r#"SELECT * FROM budget.budgets
               WHERE id = $1
                 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(budget_id)
        .fetch_optional(&mut *conn)
        .await
    }

    /// Live lines of one budget, ordered stably.
    pub async fn budget_lines(
        &self,
        conn: &mut sqlx::PgConnection,
        budget_id: Uuid,
    ) -> Result<Vec<BudgetLine>, sqlx::Error> {
        sqlx::query_as::<_, BudgetLine>(
            r#"SELECT * FROM budget.budget_lines
               WHERE budget_id = $1
                 AND (metadata->>'deleted_at') IS NULL
               ORDER BY fiscal_period_id, account_id"#,
        )
        .bind(budget_id)
        .fetch_all(&mut *conn)
        .await
    }

    /// Committed normal-direction movement for a set of exact control keys
    /// (achievement read: one fiscal period per line, keys resolved in the
    /// service). `through` bounds the posting date, inclusive.
    pub async fn achieved_per_period(
        &self,
        conn: &mut sqlx::PgConnection,
        account_ids: &[Uuid],
        fiscal_period_ids: &[Uuid],
        through: NaiveDate,
    ) -> Result<Vec<AchievedRow>, sqlx::Error> {
        sqlx::query_as::<_, AchievedRow>(
            r#"SELECT account_id, cost_center_id, fiscal_period_id,
                      SUM(CASE WHEN normal_balance = 'debit'::normal_balance
                               THEN debit_amount - credit_amount
                               ELSE credit_amount - debit_amount END) AS achieved
               FROM accounting.ledgers
               WHERE account_id = ANY($1)
                 AND fiscal_period_id = ANY($2)
                 AND posting_date <= $3
                 AND (metadata->>'deleted_at') IS NULL
               GROUP BY account_id, cost_center_id, fiscal_period_id"#,
        )
        .bind(account_ids)
        .bind(fiscal_period_ids)
        .bind(through)
        .fetch_all(&mut *conn)
        .await
    }

    // ── coverage pre-check (UI) ─────────────────────────────────────────────

    /// The single live position covering an exact control key, whatever the
    /// budget status (coverage answers "is this key planned at all").
    pub async fn coverage(
        &self,
        conn: &mut sqlx::PgConnection,
        account_id: Uuid,
        cost_center_id: Option<Uuid>,
        fiscal_period_id: Uuid,
    ) -> Result<Option<ControlPosition>, sqlx::Error> {
        sqlx::query_as::<_, ControlPosition>(
            r#"SELECT l.budget_id, l.id AS budget_line_id, l.account_id,
                      l.cost_center_id, l.fiscal_period_id, l.planned_amount,
                      b.enforcement::text::budget_enforcement AS enforcement
               FROM budget.budget_lines l
               JOIN budget.budgets b ON b.id = l.budget_id
               WHERE l.account_id = $1
                 AND l.cost_center_id IS NOT DISTINCT FROM $2
                 AND l.fiscal_period_id = $3
                 AND (l.metadata->>'deleted_at') IS NULL
                 AND (b.metadata->>'deleted_at') IS NULL"#,
        )
        .bind(account_id)
        .bind(cost_center_id)
        .bind(fiscal_period_id)
        .fetch_optional(&mut *conn)
        .await
    }
}
