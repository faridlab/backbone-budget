# Budget control — the flow and its oracle

Actors: a company planner (owns budgets), any posting producer (orders, expenses,
billing, manual journals — whoever posts to the GL), and the composing app (wires the
two modules together).

## Plan a budget (planner)

Preconditions: the company has a chart of accounts (detail accounts), optionally cost
centers, and fiscal periods; the accounting module shares this database.

Main path: create a draft budget with its positions inline — each position names an
account, optionally a cost center (NULL = postings carrying no cost center), a fiscal
period, and a strictly positive planned amount. Lines can be added, re-planned, and
removed while the budget is draft. Confirm runs the full guard matrix and flips the
budget to `confirmed` — from then on its positions control postings. `close` freezes
it (fiscal year ended); `cancel` abandons it; both are compare-and-set.

Business rules (the BG guard matrix):

- one live position per control key (company, account, cost_center, fiscal_period) —
  across all budgets, drafts included (BG1);
- confirm requires at least one line (BG2); amounts strictly > 0 (BG3);
- account must be a live detail account (BG4); cost center a live leaf (BG5);
- period must belong to the company and fit the header's fiscal year and date range
  (BG6); transitions are CAS (BG7); a line's company must equal its budget's (BG8).

Failure paths: typed 4xx refusals with the codes above; the partial unique indexes
backstop the key and code rules against raw writers.

Postconditions: a confirmed budget participates in posting control; a draft, closed,
or cancelled one never does.

## Post against a budget (any producer)

Preconditions: the composing app registered its adapter for accounting's
`BudgetControlPort` (unwired = no check, fail-open — hosts without a budget module
are unaffected).

Main path: the producer posts to the GL; after the pure double-entry rules pass, the
chokepoint asks the port to evaluate the posting. The evaluation resolves the fiscal
period covering the posting date, orients each line by its account's normal balance,
aggregates the prospective contribution per exact key, adds the committed movement on
the same keys through the posting date, and reports every position where
achieved + pending > planned.

- warn-budget breach: a structured warning is logged and the posting commits;
- block-budget breach: the posting is refused (422 `budget_exceeded`), audited
  through the standard failed-post path (row + event), no ledger rows written;
- a block breach anywhere refuses the whole posting even when other breaches were
  warn-only;
- reversal-shaped legs (a credit on a debit-normal account) reduce their key and
  never breach alone.

Alternate paths: no period covers the posting date → nothing can be planned for it →
pass; the accounting schema is absent → the evaluation refuses (fail-closed,
`accounting_unwired`) rather than report "everything within budget"; the port itself
errors → the posting is refused (a broken budget module must not silently disable
enforcement).

Postconditions: within-plan and warn-breach postings commit; block breaches leave no
journal or ledger rows.

## Watch achievement (planner)

Per position: planned, achieved (committed normal-direction movement through a chosen
date), remaining, utilization — plus a coverage pre-check (`GET /budgets/coverage`)
answering whether an exact control key is planned at all, for UI validation before a
line is even written.

## The executable oracle

- `tests/budget_workflow_cases.rs` — the guard matrix and lifecycle;
- `tests/budget_achievement_cases.rs` — plan vs achieved, orientation, key matching;
- `tests/budget_control_cases.rs` — evaluation (within / warn / block, exact keys,
  inert statuses, cross-tenant, fail-closed without the GL);
- `tests/rls_probe.rs` — the row-level company fence;
- backbone-accounting `tests/posting_budget_control_cases.rs` — the chokepoint side:
  fail-open unwired, block/warn golden, block-dominates, fail-closed broken port,
  idempotent reuse skipping the consult.
