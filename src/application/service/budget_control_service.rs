//! `BudgetControlService` — the read/control side of budgeting (hand-authored,
//! user-owned; see `metaphor.codegen.yaml`).
//!
//! Three operations over the shared database:
//! - `evaluate_posting`: given a prospective posting's lines (budget's own
//!   DTO) and a posting date, return which confirmed plan positions the
//!   posting would push over plan — the computation behind accounting's
//!   host-side `BudgetControlPort` adapter. No Cargo edge either way: the
//!   composing app maps the types.
//! - `achievement`: per-line {planned, achieved, remaining, utilization} for
//!   one budget — the management view.
//! - `coverage`: does an exact control key have a live position at all — the
//!   UI pre-check.
//!
//! Control semantics (module contract):
//! - key = (account, cost_center, fiscal_period); EXACT matching — a NULL
//!   cost center matches only positions whose cost center is NULL, never an
//!   aggregate rollup (no double counting). Tenant scoping rides underneath:
//!   the rows a read can see are whatever the composing service's fence
//!   admits, so "the position on this key" means "in the caller's org".
//! - only `confirmed` budgets participate — draft/closed/cancelled are inert;
//! - achieved = net normal-direction ledger movement in the period on the key
//!   through the posting date (debit-normal: Σdebit−Σcredit; credit-normal:
//!   Σcredit−Σdebit, oriented by the ledger row's post-time normal_balance);
//! - pending = the prospective posting's own normal-direction contribution
//!   per key (reversal-shaped legs reduce it; a posting can never breach via
//!   a net-negative key);
//! - breach when achieved + pending > planned; enforcement rides the budget
//!   header (warn default, block override).
//!
//! Tenancy: none, by design (ADR-0029). No scope parameter on any operation.
//! Self-owned transactions relay the AMBIENT request org scope, when the
//! composing service bound one (`backbone_orm::org_scope::bind_org_scope_on`),
//! so the decorator's row-level fences govern every read. The `*_on` variants
//! run on a caller-managed connection and inherit whatever scope THAT
//! connection already carries — the host adapter binds its own.
//!
//! The GL reads are fail-closed on the accounting schema: without it the
//! evaluation refuses instead of reporting "everything within budget".

use chrono::NaiveDate;
use rust_decimal::Decimal;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;

use crate::domain::entity::BudgetEnforcement;
use crate::infrastructure::persistence::{
    AchievementRow, BudgetReadRepository, ControlPosition,
};

/// One prospective posting line, as the host adapter translates it from the
/// posting contract. Exactly one of `debit`/`credit` is > 0.
#[derive(Debug, Clone)]
pub struct BudgetControlLine {
    pub account_id: Uuid,
    pub debit: Decimal,
    pub credit: Decimal,
    pub cost_center_id: Option<Uuid>,
}

/// A position the prospective posting would push over plan — the shape the
/// host adapter maps onto accounting's `BudgetBreach`.
#[derive(Debug, Clone)]
pub struct BudgetBreachInfo {
    pub budget_id: Uuid,
    pub budget_line_id: Uuid,
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub planned_amount: Decimal,
    pub achieved_amount: Decimal,
    pub pending_amount: Decimal,
    pub enforcement: BudgetEnforcement,
}

/// Typed control-read failure.
#[derive(Debug, thiserror::Error)]
pub enum BudgetControlError {
    #[error("the accounting schema is unwired — budget control needs the general ledger")]
    AccountingUnwired,
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl BudgetControlError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::AccountingUnwired => "accounting_unwired",
            Self::Internal(_) => "internal_error",
            Self::Db(_) => "database_error",
        }
    }
    pub fn http_status(&self) -> u16 {
        match self {
            Self::AccountingUnwired => 503,
            Self::Internal(_) | Self::Db(_) => 500,
        }
    }
}

fn internal(e: sqlx::Error) -> BudgetControlError {
    if matches!(e, sqlx::Error::Configuration(_)) {
        return BudgetControlError::AccountingUnwired;
    }
    BudgetControlError::Internal(e.to_string())
}

/// The normal-direction contribution of one prospective line for an account
/// of the given orientation: debit-normal accounts count debits minus credits
/// (a credit leg reduces the key — reversals never breach alone).
fn normal_direction(normal_balance: &str, debit: Decimal, credit: Decimal) -> Decimal {
    if normal_balance == "debit" {
        debit - credit
    } else {
        credit - debit
    }
}

pub struct BudgetControlService {
    pool: PgPool,
    repo: BudgetReadRepository,
}

impl BudgetControlService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            repo: BudgetReadRepository,
        }
    }

    /// Open a transaction carrying the AMBIENT request org scope, when the
    /// composing service bound one — relayed verbatim so the decorator's
    /// row-level fences evaluate for every read of the operation.
    /// Transaction-local (`set_config(..., true)`): nothing leaks onto a
    /// pooled connection reused by the next request. Unfenced deployments
    /// have no ambient scope and skip this entirely.
    async fn scoped_tx(
        &self,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, BudgetControlError> {
        let mut tx = self.pool.begin().await?;
        if let Some(scope) = backbone_orm::org_scope::current_org_scope() {
            backbone_orm::org_scope::bind_org_scope_on(&mut tx, &scope)
                .await
                .map_err(internal)?;
        }
        Ok(tx)
    }

    /// Which confirmed positions would this posting exceed? Empty = within
    /// budget or no coverage. Reads run inside one scope-relayed transaction
    /// (a consistent snapshot of plan + achieved for the whole decision).
    pub async fn evaluate_posting(
        &self,
        posting_date: NaiveDate,
        lines: &[BudgetControlLine],
    ) -> Result<Vec<BudgetBreachInfo>, BudgetControlError> {
        if lines.is_empty() {
            return Ok(vec![]);
        }
        let mut tx = self.scoped_tx().await?;
        let breaches = self
            .evaluate_posting_on(&mut tx, posting_date, lines)
            .await?;
        tx.commit().await?;
        Ok(breaches)
    }

    /// The same evaluation on a caller-managed connection — the seam adapter
    /// may reuse its own transaction; the caller's connection-bound scope (if
    /// any) governs what these reads can see.
    pub async fn evaluate_posting_on(
        &self,
        conn: &mut sqlx::PgConnection,
        posting_date: NaiveDate,
        lines: &[BudgetControlLine],
    ) -> Result<Vec<BudgetBreachInfo>, BudgetControlError> {
        BudgetReadRepository::require_gl(conn).await.map_err(internal)?;

        // 1. Resolve the fiscal period the posting lands in. No defined
        //    period => no position can cover it => within budget (the GL
        //    itself stamps no period on such posts).
        let Some(period) = self
            .repo
            .period_covering(conn, posting_date)
            .await
            .map_err(internal)?
        else {
            return Ok(vec![]);
        };

        // 2. Orient each line by its account's normal balance, then aggregate
        //    the prospective contribution per exact control key.
        let account_ids: Vec<Uuid> = {
            let mut v: Vec<Uuid> = lines.iter().map(|l| l.account_id).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        let orientations: HashMap<Uuid, String> = self
            .repo
            .normal_balances(conn, &account_ids)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|r| (r.id, r.normal_balance))
            .collect();

        let mut pending: HashMap<(Uuid, Option<Uuid>), Decimal> = HashMap::new();
        for line in lines {
            let Some(nb) = orientations.get(&line.account_id) else {
                // Unknown account: the posting contract rejects it long
                // before budget control; nothing to plan against.
                continue;
            };
            let contribution = normal_direction(nb, line.debit, line.credit);
            let key = (line.account_id, line.cost_center_id);
            *pending.entry(key).or_insert(Decimal::ZERO) += contribution;
        }
        if pending.is_empty() {
            return Ok(vec![]);
        }

        // 3. Load confirmed positions on these accounts in that period, then
        //    the committed movement on the same keys through the posting date.
        let positions = self
            .repo
            .confirmed_positions_on_keys(conn, &account_ids, period.id)
            .await
            .map_err(internal)?;
        let accounts_for_read: Vec<Uuid> =
            positions.iter().map(|p| p.account_id).collect();
        let achieved = self
            .repo
            .achieved_on_keys(conn, &accounts_for_read, period.id, posting_date)
            .await
            .map_err(internal)?;
        let achieved_map: HashMap<(Uuid, Option<Uuid>), Decimal> = achieved
            .into_iter()
            .filter_map(|r| {
                r.fiscal_period_id
                    .map(|_| ((r.account_id, r.cost_center_id), r.achieved))
            })
            .collect();

        // 4. Exact-key comparison; only net-positive keys can breach.
        let mut breaches = Vec::new();
        for pos in &positions {
            let key = (pos.account_id, pos.cost_center_id);
            let Some(p) = pending.get(&key) else {
                continue;
            };
            if *p <= Decimal::ZERO {
                continue;
            }
            let achieved = achieved_map
                .get(&key)
                .copied()
                .unwrap_or(Decimal::ZERO);
            if achieved + *p > pos.planned_amount {
                breaches.push(BudgetBreachInfo {
                    budget_id: pos.budget_id,
                    budget_line_id: pos.budget_line_id,
                    account_id: pos.account_id,
                    cost_center_id: pos.cost_center_id,
                    fiscal_period_id: pos.fiscal_period_id,
                    planned_amount: pos.planned_amount,
                    achieved_amount: achieved,
                    pending_amount: *p,
                    enforcement: pos.enforcement,
                });
            }
        }
        Ok(breaches)
    }

    /// Per-line achievement for one budget through a date (default: today's
    /// date at call time — pass an explicit date for reproducible reports).
    pub async fn achievement(
        &self,
        budget_id: Uuid,
        through: NaiveDate,
    ) -> Result<Vec<AchievementRow>, BudgetControlError> {
        let mut tx = self.scoped_tx().await?;
        let rows = self.achievement_on(&mut tx, budget_id, through).await?;
        tx.commit().await?;
        Ok(rows)
    }

    async fn achievement_on(
        &self,
        conn: &mut sqlx::PgConnection,
        budget_id: Uuid,
        through: NaiveDate,
    ) -> Result<Vec<AchievementRow>, BudgetControlError> {
        BudgetReadRepository::require_gl(conn).await.map_err(internal)?;
        let budget = self
            .repo
            .get_budget(conn, budget_id)
            .await
            .map_err(internal)?
            .ok_or(BudgetControlError::Internal(format!(
                "budget {budget_id} not found"
            )))?;
        let _ = budget; // anchor exists; rows below carry the plan truth
        let lines = self
            .repo
            .budget_lines(conn, budget_id)
            .await
            .map_err(internal)?;

        let account_ids: Vec<Uuid> = lines.iter().map(|l| l.account_id).collect();
        let period_ids: Vec<Uuid> = lines.iter().map(|l| l.fiscal_period_id).collect();
        let achieved = self
            .repo
            .achieved_per_period(conn, &account_ids, &period_ids, through)
            .await
            .map_err(internal)?;
        let achieved_map: HashMap<(Uuid, Option<Uuid>, Uuid), Decimal> = achieved
            .into_iter()
            .filter_map(|r| {
                r.fiscal_period_id
                    .map(|p| ((r.account_id, r.cost_center_id, p), r.achieved))
            })
            .collect();

        let mut rows = Vec::with_capacity(lines.len());
        for l in lines {
            let achieved = achieved_map
                .get(&(l.account_id, l.cost_center_id, l.fiscal_period_id))
                .copied()
                .unwrap_or(Decimal::ZERO);
            let remaining = l.planned_amount - achieved;
            let utilization = if l.planned_amount != Decimal::ZERO {
                achieved / l.planned_amount
            } else {
                Decimal::ZERO
            };
            rows.push(AchievementRow {
                budget_id: l.budget_id,
                budget_line_id: l.id,
                account_id: l.account_id,
                cost_center_id: l.cost_center_id,
                fiscal_period_id: l.fiscal_period_id,
                planned_amount: l.planned_amount,
                achieved_amount: achieved,
                remaining_amount: remaining,
                utilization,
            });
        }
        Ok(rows)
    }

    /// Does an exact control key have a live position (any budget status)?
    /// The UI pre-check — control itself only consults confirmed budgets.
    pub async fn coverage(
        &self,
        account_id: Uuid,
        cost_center_id: Option<Uuid>,
        fiscal_period_id: Uuid,
    ) -> Result<Option<ControlPosition>, BudgetControlError> {
        let mut tx = self.scoped_tx().await?;
        BudgetReadRepository::require_gl(&mut tx).await.map_err(internal)?;
        let covered = self
            .repo
            .coverage(&mut tx, account_id, cost_center_id, fiscal_period_id)
            .await
            .map_err(internal)?;
        tx.commit().await?;
        Ok(covered)
    }
}
