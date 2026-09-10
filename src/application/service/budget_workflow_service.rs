//! `BudgetWorkflowService` — the validated budget write path (hand-authored,
//! user-owned; see `metaphor.codegen.yaml`).
//!
//! Mirrors the family shape (expenses / attendance): a concrete service, an
//! error enum carrying `code()`/`http_status()`, transaction-per-verb with
//! row-truth state guards — every verb is a compare-and-set on the row's own
//! `status`, so a raced verb matches zero rows and surfaces as 409, never a
//! corrupt state.
//!
//! Tenancy: none, by design (ADR-0029). The module is tenant-agnostic — no
//! tenant key on any write, no scope parameter on any verb. Every transaction
//! relays the AMBIENT request org scope, when the composing service bound one
//! (`backbone_orm::org_scope::bind_org_scope_on`): the decorator-installed
//! row-level fences govern which rows a verb can see and write. A deployment
//! that runs unfenced gets an unfenced module.
//!
//! Lifecycle: draft → confirmed (control active) → closed (frozen) |
//! cancelled. Line edits are DRAFT-ONLY; a confirmed budget changes by
//! cancel + recreate. The enforcement posture is the one field still editable
//! while confirmed (warn ⇄ block); closed/cancelled budgets refuse everything.
//!
//! Guard matrix (typed refusals; the DB backstops what raw writers could do):
//! - BG1 budget_coverage_conflict 422 — another live line already holds the
//!   control key (account, cost_center, fiscal_period). Checked here per
//!   verb; under a decorated deployment the decorator's org-scoped partial
//!   unique is the raced-insert backstop (23505).
//! - BG2 budget_no_lines 422 — confirm requires ≥ 1 line.
//! - BG3 budget_invalid_amount 422 — planned_amount strictly > 0 (the DB
//!   CHECK >= 0 is the raw-writer backstop).
//! - BG4 budget_account_missing 422 — account exists in accounting.accounts,
//!   is a detail account, active. Cross-schema; accounting schema absent ⇒
//!   fail-closed (accounting_unwired).
//! - BG5 budget_cost_center_invalid 422 — cost center exists, leaf, active.
//! - BG6 budget_period_invalid 422 — fiscal period exists, fiscal year
//!   matches the header, period dates inside the header range.
//! - BG7 budget_invalid_transition 409 — CAS on confirm/close/cancel.
//! - code uniqueness — one live budget per code per org unit. Checked here
//!   per verb; the decorator's org-scoped partial unique is the raced-insert
//!   backstop (23505 → budget_code_taken).

use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

use crate::domain::entity::{Budget, BudgetEnforcement, BudgetLine, BudgetStatus};
use crate::infrastructure::persistence::BudgetReadRepository;

// ─── error surface ────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum BudgetWorkflowError {
    #[error("budget not found")]
    NotFound,
    #[error("budget line not found")]
    LineNotFound,
    #[error("budget is not a draft — lines and header fields are frozen; cancel and recreate instead")]
    NotDraft,
    #[error("budget state does not allow this transition")]
    InvalidTransition { current: BudgetStatus },
    #[error("a budget needs at least one line before it can control postings")]
    NoLines,
    #[error("another live budget line already covers this account/cost-center/period — one position per control key")]
    CoverageConflict,
    #[error("planned amount must be strictly greater than zero")]
    InvalidAmount,
    #[error("account is missing, not a detail account, or inactive in the chart of accounts")]
    AccountMissing(Uuid),
    #[error("cost center is missing, a group, or inactive")]
    CostCenterInvalid(Uuid),
    #[error("fiscal period is missing or does not fit the budget's year and date range")]
    PeriodInvalid(Uuid),
    #[error("date_from must be on or before date_to")]
    BadDateRange,
    #[error("enforcement is fixed once a budget is closed or cancelled")]
    EnforcementLocked,
    #[error("budget code is already taken by a live budget in this org unit")]
    CodeTaken,
    #[error("the accounting schema is unwired — budget validation needs the chart of accounts and fiscal periods")]
    AccountingUnwired,
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl BudgetWorkflowError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "budget_not_found",
            Self::LineNotFound => "budget_line_not_found",
            Self::NotDraft => "budget_not_draft",
            Self::InvalidTransition { .. } => "budget_invalid_transition",
            Self::NoLines => "budget_no_lines",
            Self::CoverageConflict => "budget_coverage_conflict",
            Self::InvalidAmount => "budget_invalid_amount",
            Self::AccountMissing(_) => "budget_account_missing",
            Self::CostCenterInvalid(_) => "budget_cost_center_invalid",
            Self::PeriodInvalid(_) => "budget_period_invalid",
            Self::BadDateRange => "budget_bad_date_range",
            Self::EnforcementLocked => "budget_enforcement_locked",
            Self::CodeTaken => "budget_code_taken",
            Self::AccountingUnwired => "accounting_unwired",
            Self::Internal(_) => "internal_error",
            Self::Db(_) => "database_error",
        }
    }

    pub fn http_status(&self) -> u16 {
        match self {
            Self::NotFound | Self::LineNotFound => 404,
            Self::InvalidTransition { .. } => 409,
            Self::AccountingUnwired => 503,
            Self::Internal(_) | Self::Db(_) => 500,
            _ => 422,
        }
    }
}

fn internal(e: sqlx::Error) -> BudgetWorkflowError {
    if matches!(e, sqlx::Error::Configuration(_)) {
        return BudgetWorkflowError::AccountingUnwired;
    }
    if let sqlx::Error::Database(db) = &e {
        // Under a decorated deployment the decorator's org-scoped partial
        // uniques speak last: a raced key or code insert surfaces as 23505,
        // mapped to the typed guard codes.
        if db.code().as_deref() == Some("23505") {
            let msg = db.message();
            if msg.contains("budget_lines") {
                return BudgetWorkflowError::CoverageConflict;
            }
            if msg.contains("budgets") {
                return BudgetWorkflowError::CodeTaken;
            }
        }
    }
    BudgetWorkflowError::Internal(e.to_string())
}

// ─── inputs ───────────────────────────────────────────────────────────────────

/// One plan position to create (inline at budget create, or added later).
#[derive(Debug, Clone)]
pub struct NewBudgetLine {
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub planned_amount: Decimal,
    pub notes: Option<String>,
}

/// The create input; lines are validated (BG3–BG6) and inserted as drafts.
#[derive(Debug)]
pub struct NewBudget {
    pub code: String,
    pub name: String,
    pub description: Option<String>,
    pub fiscal_year: i32,
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
    pub enforcement: BudgetEnforcement,
    pub lines: Vec<NewBudgetLine>,
}

/// Partial header update. `enforcement` follows its own rule (editable while
/// draft or confirmed); every other field is draft-only.
#[derive(Debug, Default)]
pub struct BudgetPatch {
    pub name: Option<String>,
    pub description: Option<Option<String>>,
    pub date_from: Option<NaiveDate>,
    pub date_to: Option<NaiveDate>,
    pub enforcement: Option<BudgetEnforcement>,
}

// ─── the service ──────────────────────────────────────────────────────────────

pub struct BudgetWorkflowService {
    pool: PgPool,
    reads: BudgetReadRepository,
}

impl BudgetWorkflowService {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            reads: BudgetReadRepository,
        }
    }

    /// Open a transaction carrying the AMBIENT request org scope, when the
    /// composing service bound one — relayed verbatim so the decorator's
    /// row-level fences evaluate for every statement of the verb. Transaction-
    /// local (`set_config(..., true)`): nothing leaks onto a pooled
    /// connection reused by the next request. Unfenced deployments have no
    /// ambient scope and skip this entirely.
    async fn scoped_tx(&self) -> Result<sqlx::Transaction<'static, sqlx::Postgres>, BudgetWorkflowError> {
        let mut tx = self.pool.begin().await?;
        if let Some(scope) = backbone_orm::org_scope::current_org_scope() {
            backbone_orm::org_scope::bind_org_scope_on(&mut tx, &scope)
                .await
                .map_err(internal)?;
        }
        Ok(tx)
    }

    // ── master-data guards (BG3–BG6) ──────────────────────────────────────

    /// Validate one prospective line against the GL masters and the header.
    /// Runs on the verb's transaction so the whole verb sees one consistent
    /// snapshot.
    async fn validate_line(
        &self,
        conn: &mut sqlx::PgConnection,
        header: &Budget,
        line: &NewBudgetLine,
    ) -> Result<(), BudgetWorkflowError> {
        if line.planned_amount <= Decimal::ZERO {
            return Err(BudgetWorkflowError::InvalidAmount); // BG3
        }
        if !self
            .reads
            .account_postable(conn, line.account_id)
            .await
            .map_err(internal)?
        {
            return Err(BudgetWorkflowError::AccountMissing(line.account_id)); // BG4
        }
        if let Some(cc) = line.cost_center_id {
            if !self.reads.cost_center_usable(conn, cc).await.map_err(internal)? {
                return Err(BudgetWorkflowError::CostCenterInvalid(cc)); // BG5
            }
        }
        let Some(period) = self
            .reads
            .find_period(conn, line.fiscal_period_id)
            .await
            .map_err(internal)?
        else {
            return Err(BudgetWorkflowError::PeriodInvalid(line.fiscal_period_id)); // BG6
        };
        if period.fiscal_year != header.fiscal_year
            || period.start_date < header.date_from
            || period.end_date > header.date_to
        {
            return Err(BudgetWorkflowError::PeriodInvalid(line.fiscal_period_id)); // BG6
        }
        Ok(())
    }

    /// BG1: does any OTHER live line (any budget) already hold this key?
    async fn key_taken(
        &self,
        conn: &mut sqlx::PgConnection,
        account_id: Uuid,
        cost_center_id: Option<Uuid>,
        fiscal_period_id: Uuid,
        own_budget_id: Uuid,
    ) -> Result<bool, BudgetWorkflowError> {
        let hit: Option<i32> = sqlx::query_scalar(
            r#"SELECT 1 FROM budget.budget_lines
               WHERE account_id = $1
                 AND cost_center_id IS NOT DISTINCT FROM $2
                 AND fiscal_period_id = $3
                 AND budget_id <> $4
                 AND (metadata->>'deleted_at') IS NULL
               LIMIT 1"#,
        )
        .bind(account_id)
        .bind(cost_center_id)
        .bind(fiscal_period_id)
        .bind(own_budget_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(internal)?;
        Ok(hit.is_some())
    }

    /// Does another live budget already carry this code? The per-unit code
    /// unique is the composing decorator's posture; this pre-check keeps the
    /// typed guard deterministic on unfenced databases too.
    async fn code_taken(
        &self,
        conn: &mut sqlx::PgConnection,
        code: &str,
        own_budget_id: Uuid,
    ) -> Result<bool, BudgetWorkflowError> {
        let hit: Option<i32> = sqlx::query_scalar(
            r#"SELECT 1 FROM budget.budgets
               WHERE code = $1
                 AND id <> $2
                 AND (metadata->>'deleted_at') IS NULL
               LIMIT 1"#,
        )
        .bind(code)
        .bind(own_budget_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(internal)?;
        Ok(hit.is_some())
    }

    /// Insert a line (minting `fiscal_year`/`fiscal_month` from the period).
    async fn insert_line(
        &self,
        conn: &mut sqlx::PgConnection,
        budget_id: Uuid,
        period_id: Uuid,
        line: &NewBudgetLine,
        actor: Option<Uuid>,
        now: chrono::DateTime<Utc>,
    ) -> Result<BudgetLine, BudgetWorkflowError> {
        let period = self
            .reads
            .find_period(conn, period_id)
            .await
            .map_err(internal)?
            .ok_or(BudgetWorkflowError::PeriodInvalid(period_id))?;
        let id = Uuid::new_v4();
        let row = sqlx::query_as::<_, BudgetLine>(
            r#"INSERT INTO budget.budget_lines
                   (id, budget_id, account_id, cost_center_id,
                    fiscal_period_id, fiscal_year, fiscal_month, planned_amount, notes, metadata)
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,
                       jsonb_build_object('created_by', to_jsonb($10::uuid),
                                          'created_at', to_jsonb($11::timestamptz)))
               RETURNING *"#,
        )
        .bind(id)
        .bind(budget_id)
        .bind(line.account_id)
        .bind(line.cost_center_id)
        .bind(line.fiscal_period_id)
        .bind(period.fiscal_year)
        .bind(period.fiscal_month)
        .bind(line.planned_amount)
        .bind(&line.notes)
        .bind(actor)
        .bind(now)
        .fetch_one(&mut *conn)
        .await
        .map_err(internal)?;
        Ok(row)
    }

    async fn get_budget_row(
        conn: &mut sqlx::PgConnection,
        budget_id: Uuid,
    ) -> Result<Option<Budget>, BudgetWorkflowError> {
        sqlx::query_as::<_, Budget>(
            r#"SELECT * FROM budget.budgets
               WHERE id = $1
                 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(budget_id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(internal)
    }

    // ── reads backing the detail endpoint ──────────────────────────────────

    /// One live budget header + its live lines (any status) — the detail
    /// endpoint's projection.
    pub async fn budget_detail(
        &self,
        budget_id: Uuid,
    ) -> Result<(Budget, Vec<BudgetLine>), BudgetWorkflowError> {
        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;
        let lines = self
            .reads
            .budget_lines(&mut tx, budget_id)
            .await?;
        tx.commit().await?;
        Ok((budget, lines))
    }

    // ── create ─────────────────────────────────────────────────────────────

    /// Create a draft budget with its lines inline. Every line is validated
    /// (BG3–BG6) and key-checked (BG1); a repeated code refuses (code_taken).
    pub async fn create_budget(
        &self,
        input: NewBudget,
        actor: Option<Uuid>,
    ) -> Result<Budget, BudgetWorkflowError> {
        if input.date_from > input.date_to {
            return Err(BudgetWorkflowError::BadDateRange);
        }
        let now = Utc::now();
        let mut tx = self.scoped_tx().await?;

        let header_draft = Budget {
            id: Uuid::new_v4(),
            code: input.code.clone(),
            name: input.name.clone(),
            description: input.description.clone(),
            fiscal_year: input.fiscal_year,
            date_from: input.date_from,
            date_to: input.date_to,
            status: BudgetStatus::Draft,
            enforcement: input.enforcement,
            metadata: Default::default(),
        };

        if self
            .code_taken(&mut tx, &input.code, header_draft.id)
            .await?
        {
            return Err(BudgetWorkflowError::CodeTaken);
        }

        for line in &input.lines {
            self.validate_line(&mut tx, &header_draft, line).await?;
            if self
                .key_taken(&mut tx, line.account_id, line.cost_center_id, line.fiscal_period_id, header_draft.id)
                .await?
            {
                return Err(BudgetWorkflowError::CoverageConflict); // BG1
            }
        }

        let budget = sqlx::query_as::<_, Budget>(
            r#"INSERT INTO budget.budgets
                   (id, code, name, description, fiscal_year,
                    date_from, date_to, status, enforcement, metadata)
               VALUES ($1,$2,$3,$4,$5,$6,$7,'draft'::budget_status,$8::budget_enforcement,
                       jsonb_build_object('created_by', to_jsonb($9::uuid),
                                          'created_at', to_jsonb($10::timestamptz)))
               RETURNING *"#,
        )
        .bind(header_draft.id)
        .bind(&input.code)
        .bind(&input.name)
        .bind(&input.description)
        .bind(input.fiscal_year)
        .bind(input.date_from)
        .bind(input.date_to)
        .bind(&input.enforcement)
        .bind(actor)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;

        for line in &input.lines {
            self.insert_line(&mut tx, budget.id, line.fiscal_period_id, line, actor, now)
                .await?;
        }
        tx.commit().await?;
        Ok(budget)
    }

    // ── header update (draft-only fields; enforcement has its own rule) ────

    /// Patch header fields. Non-enforcement fields are draft-only; enforcement
    /// is editable while draft or confirmed and refused on closed/cancelled.
    pub async fn update_budget(
        &self,
        budget_id: Uuid,
        patch: BudgetPatch,
        actor: Option<Uuid>,
    ) -> Result<Budget, BudgetWorkflowError> {
        let wants_frozen_fields = patch.name.is_some()
            || patch.description.is_some()
            || patch.date_from.is_some()
            || patch.date_to.is_some();

        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;

        if wants_frozen_fields && budget.status != BudgetStatus::Draft {
            return Err(BudgetWorkflowError::NotDraft);
        }
        if let Some(enforcement) = patch.enforcement {
            match budget.status {
                BudgetStatus::Draft | BudgetStatus::Confirmed => {
                    let updated = sqlx::query_as::<_, Budget>(
                        r#"UPDATE budget.budgets
                           SET enforcement = $3::budget_enforcement,
                               metadata = jsonb_set(
                                   jsonb_set(metadata, '{updated_by}',
                                             COALESCE(to_jsonb($4::uuid), 'null'::jsonb)),
                                   '{updated_at}', to_jsonb($5::timestamptz))
                           WHERE id = $1 AND id = $2
                             AND (metadata->>'deleted_at') IS NULL
                           RETURNING *"#,
                    )
                    .bind(budget_id)
                    .bind(budget_id)
                    .bind(enforcement)
                    .bind(actor)
                    .bind(Utc::now())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(internal)?;
                    tx.commit().await?;
                    return Ok(updated);
                }
                _ => return Err(BudgetWorkflowError::EnforcementLocked),
            }
        }

        let new_from = patch.date_from.unwrap_or(budget.date_from);
        let new_to = patch.date_to.unwrap_or(budget.date_to);
        if new_from > new_to {
            return Err(BudgetWorkflowError::BadDateRange);
        }

        let updated = sqlx::query_as::<_, Budget>(
            r#"UPDATE budget.budgets
               SET name = COALESCE($2, name),
                   description = COALESCE($3, description),
                   date_from = $4,
                   date_to = $5,
                   metadata = jsonb_set(
                       jsonb_set(metadata, '{updated_by}',
                                 COALESCE(to_jsonb($6::uuid), 'null'::jsonb)),
                       '{updated_at}', to_jsonb($7::timestamptz))
               WHERE id = $1
                 AND status = 'draft'
                 AND (metadata->>'deleted_at') IS NULL
               RETURNING *"#,
        )
        .bind(budget_id)
        .bind(patch.name.as_deref().map(|s| s.to_string()))
        .bind(patch.description.clone().flatten())
        .bind(new_from)
        .bind(new_to)
        .bind(actor)
        .bind(Utc::now())
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await?;
        Ok(updated)
    }

    // ── line verbs (draft only) ────────────────────────────────────────────

    /// Add one validated line to a DRAFT budget (BG1/BG3–BG6).
    pub async fn add_line(
        &self,
        budget_id: Uuid,
        line: NewBudgetLine,
        actor: Option<Uuid>,
    ) -> Result<BudgetLine, BudgetWorkflowError> {
        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;
        if budget.status != BudgetStatus::Draft {
            return Err(BudgetWorkflowError::NotDraft);
        }
        self.validate_line(&mut tx, &budget, &line).await?;
        if self
            .key_taken(&mut tx, line.account_id, line.cost_center_id, line.fiscal_period_id, budget_id)
            .await?
        {
            return Err(BudgetWorkflowError::CoverageConflict);
        }
        let created = self
            .insert_line(&mut tx, budget_id, line.fiscal_period_id, &line, actor, Utc::now())
            .await?;
        tx.commit().await?;
        Ok(created)
    }

    /// Update a line's planned amount/notes (draft only). The control key
    /// (account/cost center/period) is immutable — cancel+recreate changes
    /// coverage; amount edits are the day-to-day knob.
    pub async fn update_line(
        &self,
        budget_id: Uuid,
        line_id: Uuid,
        planned_amount: Decimal,
        notes: Option<String>,
        actor: Option<Uuid>,
    ) -> Result<BudgetLine, BudgetWorkflowError> {
        if planned_amount <= Decimal::ZERO {
            return Err(BudgetWorkflowError::InvalidAmount);
        }
        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;
        if budget.status != BudgetStatus::Draft {
            return Err(BudgetWorkflowError::NotDraft);
        }
        // Belt-and-braces: the line must belong to this budget.
        let updated = sqlx::query_as::<_, BudgetLine>(
            r#"UPDATE budget.budget_lines
               SET planned_amount = $3, notes = $4,
                   metadata = jsonb_set(
                       jsonb_set(metadata, '{updated_by}',
                                 COALESCE(to_jsonb($5::uuid), 'null'::jsonb)),
                       '{updated_at}', to_jsonb($6::timestamptz))
               WHERE budget_id = $1 AND id = $2
                 AND (metadata->>'deleted_at') IS NULL
               RETURNING *"#,
        )
        .bind(budget_id)
        .bind(line_id)
        .bind(planned_amount)
        .bind(notes)
        .bind(actor)
        .bind(Utc::now())
        .fetch_optional(&mut *tx)
        .await
        .map_err(internal)?
        .ok_or(BudgetWorkflowError::LineNotFound)?;
        tx.commit().await?;
        Ok(updated)
    }

    /// Soft-delete a line (draft only).
    pub async fn delete_line(
        &self,
        budget_id: Uuid,
        line_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<(), BudgetWorkflowError> {
        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;
        if budget.status != BudgetStatus::Draft {
            return Err(BudgetWorkflowError::NotDraft);
        }
        let deleted = sqlx::query(
            r#"UPDATE budget.budget_lines
               SET metadata = jsonb_set(
                       jsonb_set(metadata, '{deleted_by}',
                                 COALESCE(to_jsonb($3::uuid), 'null'::jsonb)),
                       '{deleted_at}', to_jsonb($4::timestamptz))
               WHERE budget_id = $1 AND id = $2
                 AND (metadata->>'deleted_at') IS NULL"#,
        )
        .bind(budget_id)
        .bind(line_id)
        .bind(actor)
        .bind(Utc::now())
        .execute(&mut *tx)
        .await
        .map_err(internal)?;
        if deleted.rows_affected() == 0 {
            return Err(BudgetWorkflowError::LineNotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    // ── lifecycle verbs (CAS, BG7) ─────────────────────────────────────────

    /// draft → confirmed. Runs the full guard matrix: ≥ 1 live line (BG2),
    /// every line's key free elsewhere (BG1), masters still valid (BG4–BG6).
    /// The row-truth CAS makes a raced confirm match zero rows → 409.
    pub async fn confirm(
        &self,
        budget_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<Budget, BudgetWorkflowError> {
        let now = Utc::now();
        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;
        if budget.status != BudgetStatus::Draft {
            return Err(BudgetWorkflowError::InvalidTransition {
                current: budget.status,
            });
        }

        let lines = self.reads.budget_lines(&mut tx, budget_id).await?;
        if lines.is_empty() {
            return Err(BudgetWorkflowError::NoLines); // BG2
        }
        for line in &lines {
            // Re-run the master guards at confirm time: accounts may have
            // been deactivated since the line was written.
            let candidate = NewBudgetLine {
                account_id: line.account_id,
                cost_center_id: line.cost_center_id,
                fiscal_period_id: line.fiscal_period_id,
                planned_amount: line.planned_amount,
                notes: line.notes.clone(),
            };
            self.validate_line(&mut tx, &budget, &candidate).await?;
            if self
                .key_taken(&mut tx, line.account_id, line.cost_center_id, line.fiscal_period_id, budget_id)
                .await?
            {
                return Err(BudgetWorkflowError::CoverageConflict); // BG1
            }
        }

        let confirmed = sqlx::query_as::<_, Budget>(
            r#"UPDATE budget.budgets
               SET status = 'confirmed'::budget_status,
                   metadata = jsonb_set(
                       jsonb_set(metadata, '{updated_by}',
                                 COALESCE(to_jsonb($3::uuid), 'null'::jsonb)),
                       '{updated_at}', to_jsonb($4::timestamptz))
               WHERE id = $1
                 AND status = $2::budget_status
                 AND (metadata->>'deleted_at') IS NULL
               RETURNING *"#,
        )
        .bind(budget_id)
        .bind(BudgetStatus::Draft)
        .bind(actor)
        .bind(now)
        .fetch_one(&mut *tx)
        .await
        .map_err(internal)?;
        tx.commit().await?;
        Ok(confirmed)
    }

    /// confirmed → closed (frozen; historical).
    pub async fn close(
        &self,
        budget_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<Budget, BudgetWorkflowError> {
        self.transition(budget_id, &[BudgetStatus::Confirmed], "closed", actor)
            .await
    }

    /// draft | confirmed → cancelled.
    pub async fn cancel(
        &self,
        budget_id: Uuid,
        actor: Option<Uuid>,
    ) -> Result<Budget, BudgetWorkflowError> {
        self.transition(
            budget_id,
            &[BudgetStatus::Draft, BudgetStatus::Confirmed],
            "cancelled",
            actor,
        )
        .await
    }

    /// Shared CAS verb: read the row, refuse unless its status is allowed,
    /// then flip guarded by `status = <read status>` so a raced verb matches
    /// zero rows and surfaces as 409 with the status actually observed.
    async fn transition(
        &self,
        budget_id: Uuid,
        allowed_from: &[BudgetStatus],
        to: &str,
        actor: Option<Uuid>,
    ) -> Result<Budget, BudgetWorkflowError> {
        let mut tx = self.scoped_tx().await?;
        let budget = Self::get_budget_row(&mut tx, budget_id)
            .await?
            .ok_or(BudgetWorkflowError::NotFound)?;
        if !allowed_from.contains(&budget.status) {
            return Err(BudgetWorkflowError::InvalidTransition {
                current: budget.status,
            });
        }
        let sql = format!(
            r#"UPDATE budget.budgets
               SET status = '{to}'::budget_status,
                   metadata = jsonb_set(
                       jsonb_set(metadata, '{{updated_by}}',
                                 COALESCE(to_jsonb($3::uuid), 'null'::jsonb)),
                       '{{updated_at}}', to_jsonb($4::timestamptz))
               WHERE id = $1
                 AND status = $2::budget_status
                 AND (metadata->>'deleted_at') IS NULL
               RETURNING *"#,
        );
        let updated = sqlx::query_as::<_, Budget>(&sql)
            .bind(budget_id)
            .bind(budget.status)
            .bind(actor)
            .bind(Utc::now())
            .fetch_optional(&mut *tx)
            .await
            .map_err(internal)?
            .ok_or(BudgetWorkflowError::InvalidTransition {
                current: budget.status,
            })?;
        tx.commit().await?;
        Ok(updated)
    }
}
