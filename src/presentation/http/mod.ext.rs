// This directory's extension (hand-written; ADR-0031). The generated mod.rs (lib.rs for
// src/) pulls it in with include!, so its lines resolve against this directory.

pub mod budget_ops_handler;
pub mod guarded_routes;
pub use guarded_routes::create_guarded_budget_routes;
