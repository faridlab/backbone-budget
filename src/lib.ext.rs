//! The module's extension (hand-written; ADR-0031): what the module adds beside its
//! generated services. The generated module and builder carry `ModuleExt` and
//! `ModuleBuilderExt` and dereference to them, so their fields read as `module.field`.

#[allow(unused_imports)]
use super::*;

/// State the module adds; read through the generated module's `Deref`.
pub struct ModuleExt {
    pub(crate) budget_workflow_service: std::sync::Arc<
        application::service::budget_workflow_service::BudgetWorkflowService,
    >,
    pub(crate) budget_control_service: std::sync::Arc<
        application::service::budget_control_service::BudgetControlService,
    >,
}

/// State the builder adds.
pub struct ModuleBuilderExt {
}

impl Default for ModuleBuilderExt {
    fn default() -> Self {
        Self {
        }
    }
}

impl ModuleBuilderExt {
    /// Build the extension's state from what the generated build made.
    #[allow(unused_variables, clippy::redundant_clone)]
    pub(crate) fn build(self, parts: &ModuleParts<'_>) -> anyhow::Result<ModuleExt> {
        let db_pool = parts.db_pool.clone();
        let budget_workflow_service = std::sync::Arc::new(
            application::service::budget_workflow_service::BudgetWorkflowService::new(
                db_pool.clone(),
            ),
        );
        let budget_control_service = std::sync::Arc::new(
            application::service::budget_control_service::BudgetControlService::new(
                db_pool.clone(),
            ),
        );
        Ok(ModuleExt {
            budget_workflow_service,
            budget_control_service,
        })
    }
}

impl crate::ThisModule {
    /// The validated write path (lifecycle verbs + the guard matrix) — the
    /// service behind [`presentation::http::create_guarded_budget_routes`].
    pub fn budget_workflow_service(
        &self,
    ) -> &std::sync::Arc<application::service::budget_workflow_service::BudgetWorkflowService> {
        &self.budget_workflow_service
    }
    /// The control/read side: posting evaluation (behind accounting's
    /// host-wired `BudgetControlPort`), achievements, coverage. The composing
    /// app's seam adapter calls this — there is no Cargo edge between the two
    /// modules, so the host maps the types.
    pub fn budget_control_service(
        &self,
    ) -> &std::sync::Arc<application::service::budget_control_service::BudgetControlService> {
        &self.budget_control_service
    }
    /// Guarded composition: validated verbs + control reads + safe GETs, NO
    /// generic budget/budget-line mutation. Mount behind the host's
    /// org-auth middleware (it inserts the request `OrgContext` and binds the
    /// ambient org scope the services relay).
    pub fn guarded_routes(&self) -> Router {
        presentation::http::create_guarded_budget_routes(self)
    }
}
