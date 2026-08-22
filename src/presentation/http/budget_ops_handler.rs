//! Budget verbs + control reads over HTTP (hand-authored, user-owned; see
//! `metaphor.codegen.yaml`). These handlers are thin: parse, delegate to
//! [`BudgetWorkflowService`] / [`BudgetControlService`], map the typed error
//! to its code + status. All tenant truth comes from the [`CompanyContext`]
//! the host's `company_auth` middleware inserts — never from the body.
//!
//! Fence posture mirrors the family: reads ride the DB fence (strict RLS +
//! `app.company_id` request binding) and every verb's SQL carries its own
//! company predicate, so a cross-tenant id simply matches zero rows → 404.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use backbone_auth::company::CompanyContext;
use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::application::service::budget_control_service::{
    BudgetControlError, BudgetControlService,
};
use crate::application::service::budget_workflow_service::{
    BudgetPatch, BudgetWorkflowError, BudgetWorkflowService, NewBudget, NewBudgetLine,
};
use crate::domain::entity::{Budget, BudgetEnforcement, BudgetLine};
use crate::infrastructure::persistence::{AchievementRow, ControlPosition};

// ─── error shape ──────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
    message: String,
}

pub fn workflow_err(e: BudgetWorkflowError) -> axum::response::Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(ErrorBody {
            error: e.code(),
            message: e.to_string(),
        }),
    )
        .into_response()
}

pub fn control_err(e: BudgetControlError) -> axum::response::Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (
        status,
        Json(ErrorBody {
            error: e.code(),
            message: e.to_string(),
        }),
    )
        .into_response()
}

// ─── response bodies ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetBody<'a> {
    pub id: Uuid,
    pub company_id: Uuid,
    pub code: &'a str,
    pub name: &'a str,
    pub description: &'a Option<String>,
    pub fiscal_year: i32,
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
    pub status: &'a str,
    pub enforcement: &'a str,
}

impl<'a> From<&'a Budget> for BudgetBody<'a> {
    fn from(b: &'a Budget) -> Self {
        Self {
            id: b.id,
            company_id: b.company_id,
            code: &b.code,
            name: &b.name,
            description: &b.description,
            fiscal_year: b.fiscal_year,
            date_from: b.date_from,
            date_to: b.date_to,
            status: match b.status {
                crate::domain::entity::BudgetStatus::Draft => "draft",
                crate::domain::entity::BudgetStatus::Confirmed => "confirmed",
                crate::domain::entity::BudgetStatus::Closed => "closed",
                crate::domain::entity::BudgetStatus::Cancelled => "cancelled",
            },
            enforcement: match b.enforcement {
                BudgetEnforcement::Warn => "warn",
                BudgetEnforcement::Block => "block",
            },
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BudgetLineBody {
    pub id: Uuid,
    pub budget_id: Uuid,
    pub account_id: Uuid,
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub fiscal_year: i32,
    pub fiscal_month: Option<i32>,
    pub planned_amount: Decimal,
    pub notes: Option<String>,
}

impl From<BudgetLine> for BudgetLineBody {
    fn from(l: BudgetLine) -> Self {
        Self {
            id: l.id,
            budget_id: l.budget_id,
            account_id: l.account_id,
            cost_center_id: l.cost_center_id,
            fiscal_period_id: l.fiscal_period_id,
            fiscal_year: l.fiscal_year,
            fiscal_month: l.fiscal_month,
            planned_amount: l.planned_amount,
            notes: l.notes,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct BudgetDetailBody<'a> {
    budget: BudgetBody<'a>,
    lines: Vec<BudgetLineBody>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AchievementBody {
    budget_id: Uuid,
    through: NaiveDate,
    rows: Vec<AchievementRow>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CoverageBody {
    account_id: Uuid,
    cost_center_id: Option<Uuid>,
    fiscal_period_id: Uuid,
    covered: bool,
    budget_id: Option<Uuid>,
    planned_amount: Option<Decimal>,
    enforcement: Option<&'static str>,
}

impl CoverageBody {
    fn uncovered(account_id: Uuid, cost_center_id: Option<Uuid>, fiscal_period_id: Uuid) -> Self {
        Self {
            account_id,
            cost_center_id,
            fiscal_period_id,
            covered: false,
            budget_id: None,
            planned_amount: None,
            enforcement: None,
        }
    }

    fn covered_by(p: &ControlPosition) -> Self {
        Self {
            account_id: p.account_id,
            cost_center_id: p.cost_center_id,
            fiscal_period_id: p.fiscal_period_id,
            covered: true,
            budget_id: Some(p.budget_id),
            planned_amount: Some(p.planned_amount),
            enforcement: Some(match p.enforcement {
                BudgetEnforcement::Warn => "warn",
                BudgetEnforcement::Block => "block",
            }),
        }
    }
}

// ─── request bodies ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateBudgetBody {
    pub code: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub fiscal_year: i32,
    pub date_from: NaiveDate,
    pub date_to: NaiveDate,
    /// "warn" (default) | "block"
    #[serde(default)]
    pub enforcement: Option<String>,
    #[serde(default)]
    pub lines: Vec<CreateLineBody>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateLineBody {
    pub account_id: Uuid,
    #[serde(default)]
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
    pub planned_amount: Decimal,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchBudgetBody {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_option")]
    pub description: Option<Option<String>>,
    #[serde(default)]
    pub date_from: Option<NaiveDate>,
    #[serde(default)]
    pub date_to: Option<NaiveDate>,
    /// "warn" | "block" — editable while draft or confirmed
    #[serde(default)]
    pub enforcement: Option<String>,
}

/// `description: null` clears the field; absent leaves it untouched.
fn deserialize_opt_option<'de, D>(de: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::deserialize(de)?))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinePatchBody {
    pub planned_amount: Decimal,
    #[serde(default)]
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementQuery {
    /// Achievements are measured through this date (posting dates <= it);
    /// defaults to today.
    #[serde(default)]
    pub through: Option<NaiveDate>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CoverageQuery {
    pub account_id: Uuid,
    #[serde(default)]
    pub cost_center_id: Option<Uuid>,
    pub fiscal_period_id: Uuid,
}

fn parse_enforcement(s: Option<&str>) -> Option<BudgetEnforcement> {
    match s {
        None | Some("warn") => Some(BudgetEnforcement::Warn),
        Some("block") => Some(BudgetEnforcement::Block),
        Some(_) => None,
    }
}

/// The acting principal as a uuid actor stamp, when the token's `sub` parses
/// as one.
fn actor(t: &CompanyContext) -> Option<Uuid> {
    Uuid::parse_str(&t.user_id).ok()
}

// ─── handlers ─────────────────────────────────────────────────────────────────

pub async fn create_budget(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Json(b): Json<CreateBudgetBody>,
) -> axum::response::Response {
    let Some(enforcement) = parse_enforcement(b.enforcement.as_deref()) else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ErrorBody {
                error: "budget_bad_enforcement",
                message: "enforcement must be \"warn\" or \"block\"".into(),
            }),
        )
            .into_response();
    };
    let lines: Vec<NewBudgetLine> = b
        .lines
        .into_iter()
        .map(|l| NewBudgetLine {
            account_id: l.account_id,
            cost_center_id: l.cost_center_id,
            fiscal_period_id: l.fiscal_period_id,
            planned_amount: l.planned_amount,
            notes: l.notes,
        })
        .collect();
    let input = NewBudget {
        code: b.code,
        name: b.name,
        description: b.description,
        fiscal_year: b.fiscal_year,
        date_from: b.date_from,
        date_to: b.date_to,
        enforcement,
        lines,
    };
    match svc
        .create_budget(tenant.company_id, input, actor(&tenant))
        .await
    {
        Ok(budget) => (StatusCode::CREATED, Json(BudgetBody::from(&budget))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn update_budget(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
    Json(b): Json<PatchBudgetBody>,
) -> axum::response::Response {
    let enforcement = match b.enforcement.as_deref() {
        None => None,
        Some(s) => match parse_enforcement(Some(s)) {
            Some(e) => Some(e),
            None => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(ErrorBody {
                        error: "budget_bad_enforcement",
                        message: "enforcement must be \"warn\" or \"block\"".into(),
                    }),
                )
                    .into_response()
            }
        },
    };
    let patch = BudgetPatch {
        name: b.name,
        description: b.description,
        date_from: b.date_from,
        date_to: b.date_to,
        enforcement,
    };
    match svc
        .update_budget(tenant.company_id, budget_id, patch, actor(&tenant))
        .await
    {
        Ok(budget) => (StatusCode::OK, Json(BudgetBody::from(&budget))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn budget_detail(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
) -> axum::response::Response {
    match svc
        .budget_detail(tenant.company_id, budget_id)
        .await
    {
        Ok((budget, lines)) => (
            StatusCode::OK,
            Json(BudgetDetailBody {
                budget: BudgetBody::from(&budget),
                lines: lines.into_iter().map(BudgetLineBody::from).collect(),
            }),
        )
            .into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn add_line(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
    Json(b): Json<CreateLineBody>,
) -> axum::response::Response {
    match svc
        .add_line(
            tenant.company_id,
            budget_id,
            NewBudgetLine {
                account_id: b.account_id,
                cost_center_id: b.cost_center_id,
                fiscal_period_id: b.fiscal_period_id,
                planned_amount: b.planned_amount,
                notes: b.notes,
            },
            actor(&tenant),
        )
        .await
    {
        Ok(line) => (StatusCode::CREATED, Json(BudgetLineBody::from(line))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn update_line(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path((budget_id, line_id)): Path<(Uuid, Uuid)>,
    Json(b): Json<LinePatchBody>,
) -> axum::response::Response {
    match svc
        .update_line(
            tenant.company_id,
            budget_id,
            line_id,
            b.planned_amount,
            b.notes,
            actor(&tenant),
        )
        .await
    {
        Ok(line) => (StatusCode::OK, Json(BudgetLineBody::from(line))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn delete_line(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path((budget_id, line_id)): Path<(Uuid, Uuid)>,
) -> axum::response::Response {
    match svc
        .delete_line(tenant.company_id, budget_id, line_id, actor(&tenant))
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn confirm_budget(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
) -> axum::response::Response {
    match svc
        .confirm(tenant.company_id, budget_id, actor(&tenant))
        .await
    {
        Ok(budget) => (StatusCode::OK, Json(BudgetBody::from(&budget))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn close_budget(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
) -> axum::response::Response {
    match svc.close(tenant.company_id, budget_id, actor(&tenant)).await {
        Ok(budget) => (StatusCode::OK, Json(BudgetBody::from(&budget))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn cancel_budget(
    State(svc): State<Arc<BudgetWorkflowService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
) -> axum::response::Response {
    match svc
        .cancel(tenant.company_id, budget_id, actor(&tenant))
        .await
    {
        Ok(budget) => (StatusCode::OK, Json(BudgetBody::from(&budget))).into_response(),
        Err(e) => workflow_err(e),
    }
}

pub async fn achievement(
    State(svc): State<Arc<BudgetControlService>>,
    tenant: CompanyContext,
    Path(budget_id): Path<Uuid>,
    Query(q): Query<AchievementQuery>,
) -> axum::response::Response {
    let through = q.through.unwrap_or_else(|| Utc::now().date_naive());
    match svc.achievement(tenant.company_id, budget_id, through).await {
        Ok(rows) => (
            StatusCode::OK,
            Json(AchievementBody {
                budget_id,
                through,
                rows,
            }),
        )
            .into_response(),
        Err(e) => control_err(e),
    }
}

pub async fn coverage(
    State(svc): State<Arc<BudgetControlService>>,
    tenant: CompanyContext,
    Query(q): Query<CoverageQuery>,
) -> axum::response::Response {
    match svc
        .coverage(
            tenant.company_id,
            q.account_id,
            q.cost_center_id,
            q.fiscal_period_id,
        )
        .await
    {
        Ok(None) => (
            StatusCode::OK,
            Json(CoverageBody::uncovered(
                q.account_id,
                q.cost_center_id,
                q.fiscal_period_id,
            )),
        )
            .into_response(),
        Ok(Some(p)) => (StatusCode::OK, Json(CoverageBody::covered_by(&p))).into_response(),
        Err(e) => control_err(e),
    }
}
