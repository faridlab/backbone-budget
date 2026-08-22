//! Guarded route composition — the RECOMMENDED way to mount the budget module.
//!
//! Hand-authored (user-owned; see `metaphor.codegen.yaml`). Deliberately does
//! NOT mount [`crate::BudgetModule::all_crud_routes`]: the generated 12-endpoint
//! CRUD surface would let a well-formed request write `status='confirmed'`
//! directly (skipping BG1–BG7), move a line's control key under a live budget,
//! or soft-delete a line out from under an enforcement decision. Instead:
//!
//! - **Reads**: the generated GET-only routers for budgets and budget lines.
//! - **Writes**: every mutation flows through [`BudgetWorkflowService`] verbs
//!   (draft-only line edits, CAS confirm/close/cancel, the BG1–BG8 guard
//!   matrix) — never generic CRUD.
//! - **Control reads**: achievement + coverage projections from
//!   [`BudgetControlService`].
//!
//! The tenant comes from the [`CompanyContext`] the host's `company_auth`
//! middleware inserts — never from the body. Mount this behind the
//! authenticated tree; the read routers ride the DB fence (strict RLS +
//! request-scoped `app.company_id` binding), and every verb's SQL carries its
//! own company predicate besides.
//!
//! Route map (relative to the mount point):
//!
//! | Method | Path | Handler |
//! |---|---|---|
//! | GET | /budgets | generated list (paginated) |
//! | GET | /budgets/:id | generated read |
//! | GET | /budgets/:id/detail | header + nested live lines |
//! | POST | /budgets | create draft (lines inline) |
//! | PATCH | /budgets/:id | header patch (enforcement per its own rule) |
//! | POST | /budgets/:id/lines | add a line (draft only) |
//! | PATCH | /budgets/:id/lines/:line_id | re-plan a line (draft only) |
//! | DELETE | /budgets/:id/lines/:line_id | soft-delete a line (draft only) |
//! | POST | /budgets/:id/confirm | draft → confirmed (CAS) |
//! | POST | /budgets/:id/close | confirmed → closed (CAS) |
//! | POST | /budgets/:id/cancel | draft|confirmed → cancelled (CAS) |
//! | GET | /budgets/:id/achievement | plan vs achieved per line |
//! | GET | /budgets/coverage | does a control key have a live position |
//! | GET | /budget_lines… | generated line reads |

use std::sync::Arc;

use axum::{
    routing::{get, patch, post},
    Router,
};

use crate::application::service::budget_control_service::BudgetControlService;
use crate::application::service::budget_workflow_service::BudgetWorkflowService;
use crate::BudgetModule;

use super::budget_ops_handler as ops;
use super::{create_budget_line_read_routes, create_budget_read_routes};

/// Build the guarded budget router: validated verbs + control reads + safe
/// GETs, NO generic budget/budget-line mutation. Mount under the host's
/// authenticated (`company_auth`) tree.
pub fn create_guarded_budget_routes(m: &BudgetModule) -> Router {
    let workflow = m.budget_workflow_service.clone();
    let control = m.budget_control_service.clone();

    let verbs = Router::new()
        .route("/budgets", post(ops::create_budget))
        .route("/budgets/:id", patch(ops::update_budget))
        .route("/budgets/:id/detail", get(ops::budget_detail))
        .route("/budgets/:id/lines", post(ops::add_line))
        .route(
            "/budgets/:id/lines/:line_id",
            patch(ops::update_line).delete(ops::delete_line),
        )
        .route("/budgets/:id/confirm", post(ops::confirm_budget))
        .route("/budgets/:id/close", post(ops::close_budget))
        .route("/budgets/:id/cancel", post(ops::cancel_budget))
        .with_state(workflow);

    // The coverage/achievement handlers need the control service; mount them
    // on their own state, then merge with the workflow-verb router.
    let control_reads = Router::new()
        .route("/budgets/coverage", get(ops::coverage))
        .route("/budgets/:id/achievement", get(ops::achievement))
        .with_state(control);

    Router::new()
        .merge(create_budget_read_routes(m.budget_service.clone()))
        .merge(create_budget_line_read_routes(m.budget_line_service.clone()))
        .merge(verbs)
        .merge(control_reads)
}
