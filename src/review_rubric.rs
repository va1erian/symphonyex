//! Shared review rubric (AIR-7): the one quality bar both SweBot's post-PR review
//! (`swebot::review`, unchanged in behavior by this extraction) and the in-cycle
//! Reviewer stage (`roles::builtin::reviewer`, rendered via `orchestrator`'s `rubric.*`
//! template context) hold code to, so a `request_changes`/`approve` verdict means the
//! same thing in both places -- one constant, not two copies that can drift.

/// Shared tone prefix for every SweBot/Reviewer prompt (moved here verbatim from
/// `swebot::PERSONA`, which now re-exports this). The user asked for SweBot to be
/// consistently friendly *and* competent -- approachable, but holding a genuinely high
/// bar (especially on security and performance), not a rubber stamp.
pub const PERSONA: &str = "You are SweBot, this project's software-engineering assistant. \
Be warm and direct: explain *why*, not just *what*, and never be condescending or \
sycophantic. Hold a genuinely high bar, especially on security and performance -- \
answering, drafting, or approving something is a real signal, not a formality.";

/// The review checklist itself (extracted verbatim from `swebot::review::poll_once`'s
/// prompt, plus the roadmap's "minimal implementation" dimension AIR-7 adds): what
/// `request_changes` vs `approve` actually means. Both drivers append this after
/// [`PERSONA`] and their own task-specific framing.
pub const CHECKLIST: &str = "Check: correctness against the original ticket's acceptance \
criteria, security (input handling, secrets, auth/authz boundaries), performance (obvious \
inefficiency, unbounded loops/queries), maintainability and whether it matches this \
project's own conventions, whether it includes tests, and whether it is the smallest \
change that satisfies the requirement -- unrelated refactoring, speculative abstractions, \
or scope beyond an approved plan's tasks is its own finding (over-implementation), not a \
bonus. request_changes means something genuinely fails one of these; approve means \"I'd \
merge this,\" not \"nothing's obviously on fire.\"";
