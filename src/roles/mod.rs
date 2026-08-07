//! Built-in role rubrics: static prompt content appended to a pipeline stage's own
//! turn prompt when `pipeline.stages[].role` names a built-in role. Per-role prompts,
//! backends and tool restrictions in general are AIR-2's scope; this only carries the
//! one rubric AIR-8 needs (`security`), kept as its own module so the next role
//! (AIR-4's requirements rubric, AIR-6's test rubric, ...) has an obvious place to add
//! its own `builtin/<role>.md` beside it instead of growing a new mechanism.

/// OWASP Top 10 (2021) checklist rubric for the `security` pipeline stage
/// (`orchestrator::run_pipeline`, `security::SecurityFindings`).
pub const SECURITY_RUBRIC: &str = include_str!("builtin/security.md");
