//! Token and cost budgets with enforced stopping conditions (Roadmap §7/§4, AIR-11).
//!
//! Symphony already *measures* tokens (`src/eventlog.rs`, `/usage`); this module is
//! what turns that measurement into an enforced ceiling. Three pieces, all
//! deliberately thin:
//!
//! - **Pricing**: a `<backend>/<model>` -> price-per-million-tokens table
//!   ([`PriceEntry`]/[`PricingTable`]), resolved once at config time
//!   (`config::resolve`) and never hardcoded inside a backend module. An unpriced
//!   model computes to [`Cost::Unknown`], never a silent `0`.
//! - **Enforcement** ([`check_after_turn`]): after every turn, compares the running
//!   totals at each of the four scopes (stage/cycle/application/platform) against
//!   `config::BudgetsConfig`, using `eventlog::usage_summary_since` for the
//!   window-rollup scopes (application/platform) -- the event log is the one source
//!   of truth for consumption, so this is a filtered replay of it, not a second
//!   counter. Stage/cycle usage is tracked by the caller (`orchestrator::run_turn_loop`)
//!   from each turn's own reported [`TokenUsage`], since those scopes reset on a
//!   boundary (a new stage, a new cycle) the event log has no notion of.
//! - **Stopping conditions** ([`ProgressTracker`]): `stop_conditions.no_progress_turns`
//!   and `stop_conditions.repeated_error`, the other half of "an agent loops to
//!   `max_turns` producing nothing" that isn't a budget at all.
//!
//! Accuracy caveat (see AGENTS.md): usage lands only on a turn's final `result` event;
//! a preempted turn (killed by a stall timeout, a crash) reports no usage at all. Every
//! function here treats "no usage reported" as *zero consumed this turn*, not as an
//! error -- a budget check is a best-effort ceiling against what's actually been
//! observed, not a guarantee no tokens were spent. Platform-level enforcement is
//! likewise scoped per-process: under `symphony serve` each project's own orchestrator
//! enforces `budgets.platform` against its own event log (correct for the common
//! single-project case, and for a project running standalone); true cross-project
//! aggregation would need a shared accounting service, out of scope for this ticket's
//! "keep it small" constraint.

use crate::config::{self, BudgetAction, BudgetLimits, BudgetWindow, StopConditionsConfig};
use crate::eventlog;
use chrono::{DateTime, Datelike, Utc};
use std::collections::HashMap;
use std::path::Path;

use crate::agent::TokenUsage;

/// Price per million tokens for one `<backend>/<model>` pricing-table entry.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceEntry {
    pub input_per_million: f64,
    pub output_per_million: f64,
    /// Not yet consumed: `TokenUsage` doesn't break out cache tokens separately today.
    /// Carried on the table now so a project can already price it, ready for whichever
    /// backend starts reporting cache tokens distinctly.
    pub cache_per_million: Option<f64>,
}

pub type PricingTable = HashMap<String, PriceEntry>;

/// `<backend>/<model>`, lowercased -- the pricing table's key shape. `model: None`
/// (a project that never set `claude.model`/`opencode.model`, i.e. "whatever the CLI's
/// own default is") deliberately resolves to a key that won't match any real pricing
/// entry, so cost comes back `Unknown` rather than silently `0`.
pub fn model_key(backend: &str, model: Option<&str>) -> String {
    format!("{backend}/{}", model.unwrap_or("default")).to_lowercase()
}

/// Illustrative starting table for the handful of models Symphony ships backends for.
/// Meant to be overridden per-project via `pricing:` in `WORKFLOW.md` (`config::resolve`
/// merges overrides on top, override wins) -- never treat these numbers as current list
/// prices.
pub fn default_pricing() -> PricingTable {
    let mut table = PricingTable::new();
    table.insert(
        "claude/claude-sonnet-5".to_string(),
        PriceEntry {
            input_per_million: 3.0,
            output_per_million: 15.0,
            cache_per_million: Some(0.30),
        },
    );
    table.insert(
        "claude/claude-opus-5".to_string(),
        PriceEntry {
            input_per_million: 15.0,
            output_per_million: 75.0,
            cache_per_million: Some(1.50),
        },
    );
    table.insert(
        "claude/claude-haiku-4-5-20251001".to_string(),
        PriceEntry {
            input_per_million: 1.0,
            output_per_million: 5.0,
            cache_per_million: Some(0.10),
        },
    );
    table
}

/// Merge a project's own `pricing:` entries on top of [`default_pricing`] -- override
/// wins per-key, everything else keeps the built-in default.
pub fn resolve_pricing(overrides: &PricingTable) -> PricingTable {
    let mut table = default_pricing();
    for (key, entry) in overrides {
        table.insert(key.to_lowercase(), *entry);
    }
    table
}

/// Cost in `budgets.currency`. `Unknown` (not `0`) whenever `model_key` isn't in
/// `pricing` -- an unpriced model must be surfaced as such, per the ticket's
/// acceptance criteria.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cost {
    Known(f64),
    Unknown,
}

pub fn compute_cost(usage: &TokenUsage, model_key: &str, pricing: &PricingTable) -> Cost {
    match pricing.get(&model_key.to_lowercase()) {
        Some(price) => Cost::Known(
            (usage.input_tokens as f64 / 1_000_000.0) * price.input_per_million
                + (usage.output_tokens as f64 / 1_000_000.0) * price.output_per_million,
        ),
        None => Cost::Unknown,
    }
}

fn usage_from_summary(summary: &eventlog::UsageSummary) -> TokenUsage {
    TokenUsage {
        input_tokens: summary.input_tokens.max(0) as u64,
        output_tokens: summary.output_tokens.max(0) as u64,
        total_tokens: summary.total_tokens.max(0) as u64,
    }
}

/// Start of the current `window` containing `now` -- UTC, weekly starts Monday.
pub fn window_start(window: BudgetWindow, now: DateTime<Utc>) -> DateTime<Utc> {
    let start_of_day = |d: chrono::NaiveDate| d.and_hms_opt(0, 0, 0).unwrap().and_utc();
    match window {
        BudgetWindow::Daily => start_of_day(now.date_naive()),
        BudgetWindow::Weekly => {
            let back = now.weekday().num_days_from_monday() as i64;
            start_of_day(now.date_naive() - chrono::Duration::days(back))
        }
        BudgetWindow::Monthly => {
            start_of_day(chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap())
        }
    }
}

/// Which of the four scopes an enforcement decision came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Stage,
    Cycle,
    Application,
    Platform,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Stage => "stage",
            Scope::Cycle => "cycle",
            Scope::Application => "application",
            Scope::Platform => "platform",
        }
    }
}

/// A budget exceeded at `scope`, with the human-readable rule that fired
/// (`status.rs`/`/events` show this verbatim -- the explanation surface constraint #2
/// requires).
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub scope: Scope,
    pub action: BudgetAction,
    pub reason: String,
}

/// Everything [`check_after_turn`] needs, bundled so `run_turn_loop`'s signature stays
/// readable -- one new struct rather than five more positional arguments.
pub struct BudgetContext<'a> {
    pub db_path: &'a Path,
    pub budgets: &'a config::BudgetsConfig,
    pub pricing: &'a PricingTable,
    pub model_key: &'a str,
    /// This stage's own `pipeline.stages[].budget` override, if set; falls back to
    /// `budgets.stage` when `None`.
    pub stage_override: Option<&'a BudgetLimits>,
}

fn exceeded_reason(
    usage: &TokenUsage,
    limits: &BudgetLimits,
    model_key: &str,
    pricing: &PricingTable,
    currency: &str,
) -> Option<String> {
    if let Some(limit) = limits.tokens
        && usage.total_tokens > limit
    {
        return Some(format!(
            "{} tokens used, budget is {limit}",
            usage.total_tokens
        ));
    }
    if let Some(limit) = limits.cost
        && let Cost::Known(spent) = compute_cost(usage, model_key, pricing)
        && spent > limit
    {
        return Some(format!(
            "{spent:.2} {currency} spent, budget is {limit:.2} {currency}"
        ));
    }
    None
}

/// Checked after every turn (Section AIR-11's "Enforcement"). Priority order
/// stage -> cycle -> application -> platform: the first scope found over budget is
/// returned (all four share one `on_exceeded` action, so there's nothing to gain by
/// collecting every scope that's over at once). `None` means every configured scope is
/// within budget (including the common case of no budgets configured at all, since an
/// absent `BudgetLimits` never trips).
pub fn check_after_turn(
    ctx: &BudgetContext<'_>,
    stage_usage: &TokenUsage,
    cycle_usage: &TokenUsage,
) -> Option<Verdict> {
    let currency = &ctx.budgets.currency;

    let stage_limits = ctx.stage_override.or(ctx.budgets.stage.as_ref());
    if let Some(limits) = stage_limits
        && let Some(reason) =
            exceeded_reason(stage_usage, limits, ctx.model_key, ctx.pricing, currency)
    {
        return Some(Verdict {
            scope: Scope::Stage,
            action: ctx.budgets.on_exceeded,
            reason,
        });
    }

    if let Some(limits) = &ctx.budgets.cycle
        && let Some(reason) =
            exceeded_reason(cycle_usage, limits, ctx.model_key, ctx.pricing, currency)
    {
        return Some(Verdict {
            scope: Scope::Cycle,
            action: ctx.budgets.on_exceeded,
            reason,
        });
    }

    if ctx.budgets.application.is_none() && ctx.budgets.platform.is_none() {
        return None;
    }
    let since = window_start(ctx.budgets.window, Utc::now()).to_rfc3339();
    let Ok(summary) = eventlog::usage_summary_since(ctx.db_path, &since) else {
        return None;
    };
    let window_usage = usage_from_summary(&summary);

    if let Some(limits) = &ctx.budgets.application
        && let Some(reason) =
            exceeded_reason(&window_usage, limits, ctx.model_key, ctx.pricing, currency)
    {
        return Some(Verdict {
            scope: Scope::Application,
            action: ctx.budgets.on_exceeded,
            reason,
        });
    }
    if let Some(limits) = &ctx.budgets.platform
        && let Some(reason) =
            exceeded_reason(&window_usage, limits, ctx.model_key, ctx.pricing, currency)
    {
        return Some(Verdict {
            scope: Scope::Platform,
            action: ctx.budgets.on_exceeded,
            reason,
        });
    }
    None
}

/// `git status --porcelain` inside `path`. `None` when it can't be determined (not a
/// git repo, `git` not on `PATH`, ...) rather than `false` -- a caller that only has a
/// tool-call signal to fall back on needs to tell "confirmed clean" apart from
/// "unknown."
pub async fn workspace_touched(path: &Path) -> Option<bool> {
    let output = tokio::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(path)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.is_empty())
}

/// A stopping condition that fired (`stop_conditions.*`), independent of any budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSignal {
    None,
    NoProgress(u32),
    RepeatedError(u32),
}

/// Per-cycle state for `stop_conditions.no_progress_turns`/`repeated_error` -- reset
/// once per cycle (`orchestrator::run_pipeline`/`run_attempt_body` each create a fresh
/// one), fed one turn at a time via [`ProgressTracker::observe_turn`].
#[derive(Debug, Default)]
pub struct ProgressTracker {
    no_progress_streak: u32,
    last_error_signature: Option<String>,
    repeated_error_streak: u32,
}

impl ProgressTracker {
    /// `made_progress`: this turn made at least one tool call or left a workspace
    /// diff. `error_signature`: `Some(text)` when the turn ended in error (its
    /// normalized reason), `None` when it completed cleanly -- an error signature
    /// identical to the previous turn's extends the repeated-error streak; anything
    /// else (including a clean turn) resets it.
    pub fn observe_turn(
        &mut self,
        made_progress: bool,
        error_signature: Option<&str>,
        conditions: &StopConditionsConfig,
    ) -> StopSignal {
        if made_progress {
            self.no_progress_streak = 0;
        } else {
            self.no_progress_streak += 1;
        }

        match error_signature {
            Some(sig) if self.last_error_signature.as_deref() == Some(sig) => {
                self.repeated_error_streak += 1;
            }
            Some(sig) => {
                self.last_error_signature = Some(sig.to_string());
                self.repeated_error_streak = 1;
            }
            None => {
                self.last_error_signature = None;
                self.repeated_error_streak = 0;
            }
        }

        if let Some(n) = conditions.repeated_error
            && n > 0
            && self.repeated_error_streak >= n
        {
            return StopSignal::RepeatedError(self.repeated_error_streak);
        }
        if let Some(n) = conditions.no_progress_turns
            && n > 0
            && self.no_progress_streak >= n
        {
            return StopSignal::NoProgress(self.no_progress_streak);
        }
        StopSignal::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64) -> TokenUsage {
        TokenUsage {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
        }
    }

    #[test]
    fn compute_cost_is_unknown_for_an_unpriced_model_never_zero() {
        let pricing = PricingTable::new();
        let cost = compute_cost(
            &usage(1_000_000, 1_000_000),
            "claude/some-future-model",
            &pricing,
        );
        assert_eq!(cost, Cost::Unknown);
    }

    #[test]
    fn compute_cost_is_known_and_correct_for_a_priced_model() {
        let mut pricing = PricingTable::new();
        pricing.insert(
            "claude/test-model".to_string(),
            PriceEntry {
                input_per_million: 3.0,
                output_per_million: 15.0,
                cache_per_million: None,
            },
        );
        let cost = compute_cost(&usage(1_000_000, 1_000_000), "claude/test-model", &pricing);
        assert_eq!(cost, Cost::Known(18.0));
    }

    #[test]
    fn resolve_pricing_lets_project_override_win_over_default() {
        let mut overrides = PricingTable::new();
        overrides.insert(
            "claude/claude-sonnet-5".to_string(),
            PriceEntry {
                input_per_million: 1.0,
                output_per_million: 1.0,
                cache_per_million: None,
            },
        );
        let table = resolve_pricing(&overrides);
        let entry = table.get("claude/claude-sonnet-5").unwrap();
        assert_eq!(entry.input_per_million, 1.0);
    }

    fn budgets_with(f: impl FnOnce(&mut config::BudgetsConfig)) -> config::BudgetsConfig {
        let mut b = config::BudgetsConfig {
            currency: "USD".to_string(),
            ..Default::default()
        };
        f(&mut b);
        b
    }

    #[test]
    fn check_after_turn_is_none_when_no_budgets_configured() {
        let budgets = config::BudgetsConfig::default();
        let pricing = PricingTable::new();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let ctx = BudgetContext {
            db_path: &db,
            budgets: &budgets,
            pricing: &pricing,
            model_key: "claude/default",
            stage_override: None,
        };
        let verdict = check_after_turn(&ctx, &usage(0, 0), &usage(10_000_000, 10_000_000));
        assert!(verdict.is_none());
    }

    #[test]
    fn check_after_turn_flags_cycle_token_budget_exceeded() {
        let budgets = budgets_with(|b| {
            b.cycle = Some(BudgetLimits {
                tokens: Some(1_000),
                cost: None,
            });
        });
        let pricing = PricingTable::new();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let ctx = BudgetContext {
            db_path: &db,
            budgets: &budgets,
            pricing: &pricing,
            model_key: "claude/default",
            stage_override: None,
        };
        let verdict = check_after_turn(&ctx, &usage(0, 0), &usage(600, 600)).unwrap();
        assert_eq!(verdict.scope, Scope::Cycle);
        assert_eq!(verdict.action, BudgetAction::Stop);
        assert!(verdict.reason.contains("1200"));
    }

    #[test]
    fn check_after_turn_prefers_stage_override_over_default_stage_budget() {
        let budgets = budgets_with(|b| {
            b.stage = Some(BudgetLimits {
                tokens: Some(1_000_000),
                cost: None,
            });
        });
        let pricing = PricingTable::new();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");
        let stage_override = BudgetLimits {
            tokens: Some(100),
            cost: None,
        };
        let ctx = BudgetContext {
            db_path: &db,
            budgets: &budgets,
            pricing: &pricing,
            model_key: "claude/default",
            stage_override: Some(&stage_override),
        };
        let verdict = check_after_turn(&ctx, &usage(60, 60), &usage(60, 60)).unwrap();
        assert_eq!(verdict.scope, Scope::Stage);
    }

    #[test]
    fn check_after_turn_flags_cost_exceeded_only_when_model_is_priced() {
        let budgets = budgets_with(|b| {
            b.cycle = Some(BudgetLimits {
                tokens: None,
                cost: Some(1.0),
            });
        });
        let mut pricing = PricingTable::new();
        pricing.insert(
            "claude/priced".to_string(),
            PriceEntry {
                input_per_million: 10.0,
                output_per_million: 10.0,
                cache_per_million: None,
            },
        );
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("events.db");

        // Unpriced model: cost is Unknown, so a cost-only budget never trips (avoids
        // stopping a cycle over a limit it has no way to actually measure).
        let ctx_unpriced = BudgetContext {
            db_path: &db,
            budgets: &budgets,
            pricing: &pricing,
            model_key: "claude/unpriced",
            stage_override: None,
        };
        assert!(check_after_turn(&ctx_unpriced, &usage(0, 0), &usage(1_000_000, 0)).is_none());

        // Priced model over the cost limit: flagged.
        let ctx_priced = BudgetContext {
            db_path: &db,
            budgets: &budgets,
            pricing: &pricing,
            model_key: "claude/priced",
            stage_override: None,
        };
        let verdict = check_after_turn(&ctx_priced, &usage(0, 0), &usage(200_000, 0)).unwrap();
        assert_eq!(verdict.scope, Scope::Cycle);
    }

    #[test]
    fn window_start_monthly_is_first_of_month_midnight_utc() {
        let now = DateTime::parse_from_rfc3339("2026-08-07T15:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let start = window_start(BudgetWindow::Monthly, now);
        assert_eq!(start.to_rfc3339(), "2026-08-01T00:00:00+00:00");
    }

    #[test]
    fn window_start_weekly_is_monday_midnight_utc() {
        // 2026-08-07 is a Friday.
        let now = DateTime::parse_from_rfc3339("2026-08-07T15:30:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let start = window_start(BudgetWindow::Weekly, now);
        assert_eq!(start.to_rfc3339(), "2026-08-03T00:00:00+00:00");
    }

    #[test]
    fn progress_tracker_stops_on_no_progress_streak() {
        let conditions = StopConditionsConfig {
            no_progress_turns: Some(3),
            repeated_error: None,
        };
        let mut tracker = ProgressTracker::default();
        assert_eq!(
            tracker.observe_turn(false, None, &conditions),
            StopSignal::None
        );
        assert_eq!(
            tracker.observe_turn(false, None, &conditions),
            StopSignal::None
        );
        assert_eq!(
            tracker.observe_turn(false, None, &conditions),
            StopSignal::NoProgress(3)
        );
    }

    #[test]
    fn progress_tracker_resets_no_progress_streak_on_a_productive_turn() {
        let conditions = StopConditionsConfig {
            no_progress_turns: Some(2),
            repeated_error: None,
        };
        let mut tracker = ProgressTracker::default();
        tracker.observe_turn(false, None, &conditions);
        assert_eq!(
            tracker.observe_turn(true, None, &conditions),
            StopSignal::None
        );
        assert_eq!(
            tracker.observe_turn(false, None, &conditions),
            StopSignal::None,
            "streak should have reset after the productive turn"
        );
    }

    #[test]
    fn progress_tracker_stops_on_repeated_identical_error() {
        let conditions = StopConditionsConfig {
            no_progress_turns: None,
            repeated_error: Some(3),
        };
        let mut tracker = ProgressTracker::default();
        assert_eq!(
            tracker.observe_turn(true, Some("compile error: E0308"), &conditions),
            StopSignal::None
        );
        assert_eq!(
            tracker.observe_turn(true, Some("compile error: E0308"), &conditions),
            StopSignal::None
        );
        assert_eq!(
            tracker.observe_turn(true, Some("compile error: E0308"), &conditions),
            StopSignal::RepeatedError(3)
        );
    }

    #[test]
    fn progress_tracker_does_not_stop_when_error_signature_changes() {
        let conditions = StopConditionsConfig {
            no_progress_turns: None,
            repeated_error: Some(2),
        };
        let mut tracker = ProgressTracker::default();
        tracker.observe_turn(true, Some("error A"), &conditions);
        assert_eq!(
            tracker.observe_turn(true, Some("error B"), &conditions),
            StopSignal::None
        );
    }

    #[test]
    fn progress_tracker_disabled_conditions_never_fire() {
        let conditions = StopConditionsConfig::default();
        let mut tracker = ProgressTracker::default();
        for _ in 0..50 {
            assert_eq!(
                tracker.observe_turn(false, Some("same error"), &conditions),
                StopSignal::None
            );
        }
    }
}
