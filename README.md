# backbone-budget

Budget control for the finance family (pillar-finance F-6). Odoo community ships no
budget source (`account_budget` is Enterprise), so this module owns its spec.

A **Budget** is the plan header (identity, fiscal coverage, lifecycle, enforcement
posture); each **BudgetLine** is one plan position keyed
`(company, account, cost_center, fiscal_period)` — the shared analytic dimension.
Achieved amounts are read from the general ledger (`accounting.ledgers`, plain
cross-schema SQL — no Cargo edge into backbone-accounting).

## Control semantics

- key = account x cost center x fiscal period; **exact-key matching** — a NULL cost
  center matches only positions whose cost center is NULL, never an aggregate rollup;
- only `status = confirmed` budgets participate (draft / closed / cancelled are inert);
- achieved = net normal-direction ledger movement in that period on that key through
  the posting date (debit-normal: Σdebit−Σcredit; credit-normal: Σcredit−Σdebit,
  oriented by the ledger row's post-time `normal_balance` stamp);
- breach when achieved + pending (the prospective posting's own contribution) >
  planned; reversal-shaped legs reduce the key and never breach alone;
- enforcement rides the budget header: **warn by default** (structured log, posting
  proceeds), **block per budget** (refuse with 422 `budget_exceeded`).

## Lifecycle

`draft → confirmed → closed | cancelled` (compare-and-set verbs; a raced verb matches
zero rows and surfaces 409). Line edits are draft-only; a confirmed budget changes by
cancel + recreate. `enforcement` is the one field still editable while confirmed —
the day-to-day posture knob — and frozen once closed/cancelled.

## The posting seam (zero Cargo edges, both directions)

accounting owns the chokepoint trait `BudgetControlPort`
(`src/domain/repositories/budget_control.rs` in backbone-accounting), consulted in
`PostingService::validate_lines` after the pure double-entry rules. The deployed app
implements that port by delegating to this module's `BudgetControlService`
(`evaluate_posting` — plain methods in budget's own DTOs; the host adapter maps the
types). Unwired (`None`) means no budget check — accounting keeps working for hosts
without a budget module. A wired-but-erroring port refuses the posting (fail-closed).

## Surface

`BudgetModule::guarded_routes()` — validated verbs + control reads + safe GETs, no
generic mutation: `POST /budgets` (draft, lines inline), `PATCH /budgets/:id`,
line add/update/delete (draft only), `confirm` / `close` / `cancel` (CAS),
`GET /budgets/:id/achievement`, `GET /budgets/coverage`. Tenant truth comes from the
`CompanyContext` the host's `company_auth` middleware inserts — never the body.

## Guard matrix

| Code | Meaning |
|---|---|
| `budget_coverage_conflict` 422 | another live line already holds the control key (partial unique index backstops) |
| `budget_no_lines` 422 | confirm requires ≥ 1 line |
| `budget_invalid_amount` 422 | planned_amount strictly > 0 |
| `budget_account_missing` 422 | account exists, is a detail account, active |
| `budget_cost_center_invalid` 422 | cost center exists, leaf, active |
| `budget_period_invalid` 422 | period exists, same company, fits the header's year/range |
| `budget_invalid_transition` 409 | CAS on confirm/close/cancel |
| `budget_line_company_mismatch` 422 | a line's company must equal its budget's |

Cross-schema guards and reads fail closed when the accounting schema is absent
(`accounting_unwired` 503) — budgets are meaningless without the GL.

## Testing

`cargo test` — DB via `DATABASE_URL` or the default
`postgresql://postgres:postgres@localhost:5433/backbone_budget` (needs both modules'
migrations; `backbone_budget_nogl`, budget migrations only, backs the fail-closed
case). Every test mints a fresh company. Golden suites: workflow guards
(`tests/budget_workflow_cases.rs`), achievements (`tests/budget_achievement_cases.rs`),
posting evaluation (`tests/budget_control_cases.rs`), RLS fence probe
(`tests/rls_probe.rs`).
