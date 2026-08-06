# Symphony — reference

Deep reference material: configuration, architecture rationale, and operational
details. For a quick overview and how to get started, see [README.md](README.md).

This is a Rust implementation of the [Symphony service specification](https://github.com/openai/symphony/blob/main/SPEC.md):
a daemon that polls an issue tracker, creates a per-issue workspace, and runs a coding
agent inside it. Section references below (e.g. "Section 8.5") refer to that spec.

## Coding-agent backends

Set `agent.backend: claude` (default), `agent.backend: codex`, or `agent.backend:
opencode` in `WORKFLOW.md`.

- **`claude`** (default, the "make it Claude-compatible" path): launches the `claude`
  CLI in headless mode (`claude -p <prompt> --output-format stream-json --resume
  <session_id>`), one subprocess per turn, `--permission-mode bypassPermissions` by
  default (auto-approves edits *and* commands — a high-trust posture, see below).
  Note this is **not** `acceptEdits`, which only covers file-edit tools
  (Write/Edit/NotebookEdit) — Bash/PowerShell calls still hit the normal approval path
  under `acceptEdits`, and with no human present in a headless run those get
  auto-denied, silently preventing agents from ever running their own build/tests.
  Learned that the hard way running this against a real multi-ticket pipeline. This is
  the well-exercised path in this implementation.
- **`codex`**: launches `codex app-server` and speaks JSON-RPC 2.0 over stdio. The spec
  (Section 10) is explicit that the installed Codex build's own schema
  (`codex app-server generate-json-schema`) is the source of truth for method names and
  payload shapes, not this document. This module implements the transport contract
  correctly (framing, timeouts, separate stderr) but guesses at method names
  (`initialize`, `thread/start`, `turn/start`) and uses substring matching on
  notification methods to detect turn completion. Treat it as a skeleton to adjust
  against your installed Codex version, not a verified client — see
  `src/agent/codex.rs` for the exact caveat.
- **`opencode`** (the pluggable-model-provider path): launches the open-source,
  provider-agnostic [`opencode`](https://opencode.ai) CLI in headless mode (`opencode
  run --format json --model <provider/model> --auto --session <session_id> <prompt>`),
  one subprocess per turn, same `--auto`-by-default high-trust posture as `claude`'s
  `bypassPermissions` and the same reasoning (no human present to approve tool calls in
  a headless run). This is how Fireworks AI (or any of `opencode`'s ~75 other
  providers) plugs in: `opencode` itself owns the provider connection and
  credentials — configure a provider once via `opencode`'s own `/connect` flow or a
  static `opencode.json` provider block (e.g. pointing `FIREWORKS_API_KEY` at a
  `fireworks` provider), then set `opencode.model: fireworks/<model-id>` here. Symphony
  never sees or manages that credential directly, but `opencode.api_key: $VAR_NAME` (a
  `$VAR_NAME` reference, mirroring `claude.api_key`) tells Symphony which env var to
  forward into a Docker-mode container so the baked provider config's `{env:VAR_NAME}`
  actually resolves at runtime — see "Docker mode" below. Like the Codex backend,
  `opencode run --format json`'s exact per-event schema isn't fully documented
  publicly; this module implements the subprocess/NDJSON transport solidly and guesses
  at event field names leniently — see `src/agent/opencode.rs` for the exact caveat.

Per-backend settings live under the `claude:`, `codex:`, and `opencode:` front-matter
keys (mirroring each other where it makes sense); none of these are part of the
normative schema, they're all spec extensions.

## Live status dashboard

`symphony ./WORKFLOW.md --port 7777` additionally serves a small dashboard at
`http://127.0.0.1:7777` (loopback only; see `src/status.rs`), off by default and only
starting when `--port` is passed:

- **`/`** — currently-running agents as cards (identifier, session id, elapsed time,
  turn count, last event) and the retry queue. Live-updates in place via a small
  inline `EventSource` subscribed to `/fragment-stream` (Server-Sent Events, pushed
  directly off the same `watch::Receiver` the dashboard reads, so an update lands as
  soon as something changes rather than on a fixed poll interval) — not a `<meta>`
  full-page refresh (the original mechanism, and genuinely janky: every tick was a
  real browser navigation, resetting scroll position and any open selection). The new
  mechanism has none of that; nothing outside the one data container ever reloads.
  `/fragment` (a single non-streaming render of the same fragment) stays mounted
  alongside it for a one-shot fetch instead of a stream.
- **`/events`** — filterable, paginated browse of every dispatch/turn/tool-call/exit
  event ever recorded, backed by a persistent SQLite log (`<workflow_dir>/symphony.db`,
  `src/eventlog.rs`) that survives a restart — unlike `/`, which is live state and
  correctly shows nothing running right after a restart. High-frequency
  low-information events (`other_message`, Claude's own intermediate streaming
  chunks) are hidden by default; a "Show all" link brings them back.
- **`/usage`** — token/turn/tool-call consumption, globally and per-issue, queried
  live from the same SQLite log. Independent of, and does not replace, the
  always-on `--report` HTML file below (both exist; this one is browsable/filterable
  and survives restarts by design, the other is a single continuously-rewritten
  snapshot file).

Useful for watching dispatch/concurrency behavior — e.g. bump
`agent.max_concurrent_agents` and you'll see multiple cards running at once on `/`, or
look back at exactly what happened on `/events` afterward.

`src/status.rs` exposes this as a plain `Router` (`status::router(status_rx, db_path,
base_path)`) with no bind/serve attached, so it can be reused unmodified either at the
root (the single-project CLI path above) or nested under a path prefix (the
multi-project service below) — `base_path` is what makes every link/asset URL this
router renders come out correctly prefixed either way.

## Usage report

Always on: Symphony writes a cumulative HTML usage report to
`<workflow_dir>/symphony-report.html` (override with `--report <path>`), rewritten
after every dispatch/turn/tool-call/exit for the life of the process — see
`src/metrics.rs`. It tracks, both globally and per-issue: agents spawned (dispatches),
turns (each is a fresh `claude` subprocess launch, since the Claude backend runs one
process per turn), tool calls (by name, extracted from `tool_use` content blocks —
Claude backend only, Codex backend doesn't detect these yet), and input/output/total
tokens. Since it's rewritten continuously rather than only at shutdown, the report is
current even if the process is killed rather than stopped cleanly.

Known accuracy caveat: token counts are only recorded when a turn's final `result`
event arrives from the `claude` subprocess. If reconciliation reclaims a worker right
after it marks its issue done (Section 8.5 — this can preempt mid-turn) but before
that event arrives, turns/tool-calls for that dispatch are still counted correctly but
its tokens show as 0. Narrow race, mostly visible on short poll intervals against
trivial/fast tasks; not expected to matter much at realistic poll intervals against
real work.

That same reconciliation-preempts-mid-turn race used to be worse than a metrics gap:
`after_run` (and therefore an agent's `git commit`/`push`, if that's what the hook
does) never ran at all when a worker was aborted by reconciliation rather than exiting
on its own — a fully-verified, ticket-complete attempt could get its workspace deleted
before its work was ever persisted anywhere. Fixed: reconciliation now `.abort()`s
*and awaits* the worker's `JoinHandle` (not just an `AbortHandle`, which only requests
cancellation) before running `after_run` or touching the workspace, so the aborted
task's `Drop` — which `kill_on_drop`s the agent subprocess — has actually finished
first, and `after_run` runs every time a running attempt ends, matching Section 9.4
("success, failure, timeout, or cancellation"). Found by running the real
bsky-archiver pipeline: a ticket showed `done` with fully verified work, but its
branch never appeared upstream.

## Plan usage-limit handling (`claude` backend)

The Claude Code CLI reports its own plan/account usage limit as an ordinary turn
failure — observed live, verbatim: `"You've hit your session limit · resets
12:30am (Europe/Paris)"`. Treating that identically to a transient error (the normal
exponential backoff, capped at `agent.max_retry_backoff_ms` — minutes) would spend the
rest of the plan's multi-hour reset window relaunching the `claude` subprocess every
few minutes, each attempt failing immediately.

`orchestrator::is_plan_rate_limited` recognizes the phrase (`"session limit"` /
`"usage limit"`, not a generic `"rate limit"` substring — that could also describe an
unrelated transient 429 that should still use the normal short backoff) in a failed
turn's error text. When matched:

- The affected issue's retry is scheduled after a fixed `agent.rate_limit_pause_ms`
  (default 30 minutes) instead of the exponential curve, and its attempt counter does
  not escalate — this isn't the ticket's fault.
- Every other issue's *new* dispatch is paused too (`on_tick` checks
  `OrchestratorState::rate_limited_until`) for the same window, since all concurrently
  running issues share one account and would hit the same wall immediately. Already-
  running turns are left alone; only new dispatch is gated.
- A `pipeline.stages[].blocking: true` stage does **not** park the issue over this
  specific failure (see the `is_plan_rate_limited` check in `run_pipeline`) — a plan
  limit isn't a judged exit-criteria failure, so it retries instead of blocking.

Deliberately does **not** parse an exact reset instant out of the message: the CLI
names a wall-clock local time in an arbitrary timezone, which isn't reliably
convertible into an instant without a timezone database this crate doesn't otherwise
need. Instead it waits the fixed interval and re-checks, surfacing the CLI's own
(freshest) message each time via the existing retry-queue "Last error" column on the
dashboard — no new UI needed for this.

## Docker mode

Off by default; opt in per-project with `workspace.docker` in `WORKFLOW.md`:

```yaml
workspace:
  root: ./.workspaces
  docker:
    enabled: true
    image: my-project-agent:latest   # FROM this repo's own Dockerfile, see below
    network: bridge                    # default; "none" if a project needs no egress
    mem_limit: 4g                       # optional
    cpus: "2"                            # optional
```

**Why this exists.** Workspace hooks (`hooks.rs`) run their scripts via WSL's `bash`,
while the coding agent's own `Bash` tool — invoked by `claude`/`opencode` running
natively on the host — resolves paths via Git Bash/MSYS. These disagree about how to
spell a Windows path for the same directory (`/mnt/c/...` vs `/c/...`), and there is no
single spelling that satisfies both; hit this twice in production (once via a
hardcoded `git clone` URL, once via an agent "fixing" a remote URL to match its own
environment and breaking the hooks'). Docker mode runs **both** the hooks and the
coding-agent process inside the same Linux container, bind-mounted once to the whole
project directory (`workflow_dir` — covers the workspace, the tracker's `issues/`, and
a mainline repo like bsky-archiver's `app/` in one mount) at a fixed path (`/project`,
`container::CONTAINER_PROJECT_ROOT`). Inside the container there is exactly one
filesystem and one path convention, so this class of bug becomes structurally
impossible rather than just less likely. It's also the practical fix for a second,
unrelated problem on Windows specifically: `opencode` itself has no first-class
Windows installer, so running it in a container (`opencode` is baked into this repo's
own base image) sidesteps installing it on the host at all.

As a side effect this also isolates concurrent agents from each other and the host:
each ticket gets its own persistent container (created alongside its workspace, torn
down alongside it), so a runaway agent is capped by `mem_limit`/`cpus` and can't touch
anything outside the bind mount.

**Building an image.** This repo's own `Dockerfile` is the base every project image
extends: `debian:bookworm-slim` + `bash`/`git`/the `claude` CLI/the `opencode` CLI + a
Linux build of `symphony` itself (for the MCP tool-server subcommand,
`container::CONTAINER_SYMPHONY_BIN` at `/usr/local/bin/symphony`) — built inside a
multi-stage Docker build, so no cross-compilation toolchain is needed on the host, just
`docker build` itself:

```bash
docker build -t symphony-base:latest .
```

A project adds whatever toolchain its tickets need on top:

```dockerfile
FROM symphony-base:latest
RUN apt-get update && apt-get install -y --no-install-recommends <your toolchain> \
    && rm -rf /var/lib/apt/lists/*
```

**Supported for `claude` and `opencode`, not yet `codex`** — Codex's `start_session`
accepts and ignores the container parameter for now; `validate_for_dispatch` rejects
`workspace.docker.enabled: true` with `agent.backend: codex` rather than silently
no-op'ing the isolation.

**Fireworks-via-`opencode` in Docker mode, concretely.** The base image already bakes
in a global `opencode` provider config (`/home/agent/.config/opencode/opencode.json`)
declaring `fireworks` as an OpenAI-compatible provider whose key is read from the
container's own `FIREWORKS_API_KEY` env var at request time — nothing to run
interactively inside the container. A project's `WORKFLOW.md` just needs:

```yaml
agent:
  backend: opencode
workspace:
  docker:
    enabled: true
    image: my-project-agent:latest
    user: "1000:1000"
opencode:
  model: fireworks/accounts/fireworks/models/kimi-k2p7-code
  api_key: $FIREWORKS_API_KEY   # names the env var to forward; the value comes from
                                  # Symphony's own process environment, never the config file
```

Fireworks' serverless catalog turns over fast (model ids get retired) -- verify
the exact id via `GET https://api.fireworks.ai/inference/v1/models` before assuming
any specific slug (including the one above) is still live.

`FIREWORKS_API_KEY` must be set in the environment Symphony itself runs in (same
convention as `repo.token`/`claude.api_key`) — `envsub::collect_var_refs` forwards
exactly that name into each per-ticket container via `docker run -e`.

**Gotcha: env vars only reach a container at its first creation.**
`container::ensure_running` is idempotent by *name* (`symphony-<hash>-<ticket>`) — an
already-existing container (from an earlier run, possibly before a secret was even
set) is reused as-is, env vars and all; it is never recreated just because
`WORKFLOW.md` or the process environment changed. Hit this for real testing the
`opencode`/Fireworks path: a container created before `FIREWORKS_API_KEY` was set kept
silently missing it across several `symphony` restarts, surfacing as `opencode`
reporting "Model not found, inaccessible, and/or not deployed" (its generic error for
an unauthenticated request) even once the key really was set in the shell launching
`symphony`. Fix is `docker rm -f symphony-<hash>-<ticket>` (or blow away the whole
`.workspaces/` dir) after changing anything that only takes effect at container
creation (env var *values* -- not just which names get forwarded, which
`docker inspect <name> --format '{{.Config.Env}}'` shows without printing this
process's actual env -- `mem_limit`, `cpus`, `user`, etc.), then let the next dispatch
recreate it fresh.

**Prerequisite**: Docker Desktop (or an equivalent daemon) running, `docker` on `PATH`.
Symphony checks this at startup when `docker.enabled: true` and fails fast with a clear
message rather than surfacing "docker: command not found" buried in a hook failure.

**Security posture — what this does and does not fix.** Real improvement: the agent's
auto-approved shell (`bypassPermissions` for `claude`, `--auto` for `opencode`) can now
only reach what's bind-mounted, not the whole host filesystem, plus resource limits and
process isolation that don't exist at all today. What stays exactly as risky: that
auto-approval itself is unchanged (the agent still has an unrestricted shell, just a
smaller reachable filesystem); the bind mount
is full read-write across the *whole* project, so a bad agent on one ticket can still
corrupt another ticket's workspace or the mainline repo; `WORKFLOW.md` hooks remain a
trusted-script trust boundary, and running Docker at all typically requires
host-privileged access anyway; there's no image signing/scanning, and this is stock
`docker run` (namespaces + cgroups), not a hardened runtime (gVisor/Firecracker). This
is a real hardening step for a single trusted operator running their own
`WORKFLOW.md` locally — **not** sufficient for multi-tenant or untrusted-input
production use.

## Git repo as first-class input

A project can hand-write `hooks.after_create`/`before_run`/`after_run` to clone/pull/
commit/push itself (see `WORKFLOW.md`'s own hooks in this repo's demo, or
bsky-archiver's original hand-written version). A `repo:` block does it for you:

```yaml
repo:
  url: https://github.com/owner/name.git
  default_branch: main
  token: $GITHUB_TOKEN   # name of an env var holding a git credential; optional
```

When `repo.url` is set and a project hasn't supplied its own `hooks.after_create` /
`before_run` / `after_run` (per-hook, not all-or-nothing -- an explicit `hooks.*`
entry always wins), Symphony synthesizes them: always a fresh per-ticket branch
(`issue-<workspace-key>`, never pushed straight to `default_branch` -- a generic
daemon can't know in advance whether tickets are safely sequential the way a
hand-written WORKFLOW.md can), a loud failure on push (not swallowed), and an
`is-inside-work-tree` guard so a silently-failed `after_create` fails loudly on the
next hook instead of quietly no-op'ing forever.

**Keeping a long-running ticket rebased on `default_branch`**: every `before_run` (so
every turn, not just the first) fetches `default_branch` and rebases the ticket
branch onto it. Real drift over a multi-turn ticket is exactly what this catches --
without it, a ticket that runs for a while while other work merges to main only
discovers the conflict at PR-open time, as a merge conflict a human then has to sort
out by hand. Two outcomes:
- **Clean rebase**: silent, nothing for the agent to do differently this turn.
- **Real conflict**: the hook does *not* fail -- resolving a conflict needs the
  agent's own understanding of the code, not a script, so it leaves the workspace
  genuinely mid-rebase (`.git/rebase-merge`/`.git/rebase-apply` present, files with
  `<<<<<<<` markers) and prints a clear `MERGE CONFLICT:` line to stderr instead. A
  project's own prompt needs to tell the agent what to do when it finds itself in
  that state (see bsky-archiver's `WORKFLOW.md` for a template instruction) --
  Symphony can detect and surface the conflict, but resolving it is inherently a
  content decision only the coding agent (or a human) can make.
  `after_run` refuses to commit/push while still mid-rebase (a partial/conflicted
  tree must never reach the shared repo), and once a rebase *did* happen, pushes with
  `--force-with-lease` instead of a plain push -- a rebase rewrites the ticket
  branch's own commit history, so a plain push would be rejected as non-fast-forward
  against whatever was pushed there before.
- A rebase failing for a reason *other* than a conflict (network, corrupt state,
  etc.) gets `git rebase --abort`ed automatically, leaving the branch exactly as it
  was; a `WARNING:` line notes it, and the next turn's `before_run` just tries again.

**Credentials**: `repo.token` names an env var (not a literal value) holding a git
credential; the synthesized hooks reference it by name in a generated `git config
credential.helper`, so the secret's actual value never gets embedded in the hook
script text or the git remote URL. In **Docker mode**, that named var also needs to
actually reach the per-ticket container's own environment -- `docker run` doesn't
inherit the host environment into a container the way a plain child process would, so
Symphony scans the whole resolved `WORKFLOW.md` config for every `$VAR`-shaped
reference (`repo.token`, the tracker's own token, anything else) and forwards exactly
those via `docker run -e VAR_NAME` (name only, no `=value` -- Docker reads the value
from *its own* invoking process's environment, so the secret never appears in the
`docker run`/`docker exec` command line itself). The same list gets forwarded into a
daemonized Symphony's own container too (see below), since the orchestrator process
running inside it needs these just as much as a per-ticket container does.

**Pull/merge requests instead of a silently pushed branch**: by default the
synthesized `after_run` hook pushes each ticket's branch and leaves it there for a
human to notice on their own. Set `repo.pull_request: true` (requires `repo.url` to
parse for the configured `repo.provider` and `repo.token` to be set) to expose a
second agent tool, `open_pull_request(title, body)`, alongside whatever the tracker
itself exposes (see "Provider-native tracker tool" below) — the agent calls it once
it's pushed and happy with the change, supplying its own title and rationale. Calling
it again (a retry, or more work landing on the same branch) updates the existing
PR/MR in place rather than creating a duplicate.

This changes how issues close: put `Closes #<issue-number>` in the PR/MR body and the
code host closes the tracker issue automatically **when a human merges it**, not when
the agent decides it's done. The existing tracker adapters already treat a closed
issue as done regardless of how it got closed, so nothing else needs to change for
the polling loop to pick this up.

**GitLab support**: set `repo.provider: gitlab` (default is `github`, so every
existing `repo:` block with no `provider:` key is unaffected). `repo.url` can be any
GitLab host, including self-managed instances — unlike GitHub detection, which only
recognizes `github.com`, GitLab detection accepts any host once `provider: gitlab` is
explicit. The GitLab REST API root is derived from `repo.url`'s own scheme+host plus
`/api/v4` (covering the common self-managed case with zero extra config); override it
explicitly with `repo.api_base_url` when the API is reachable somewhere else (already
including the `/api/v4` suffix — nothing is appended onto an explicit value):

```yaml
repo:
  provider: gitlab
  url: https://gitlab.example.com/group/subgroup/name.git
  api_base_url: https://gitlab.example.com/api/v4   # optional, derived from url otherwise
  default_branch: main
  token: $GITLAB_TOKEN
  pull_request: true
```

The synthesized clone hooks also pick GitLab's documented HTTPS credential-helper
username (`oauth2`) instead of GitHub's (`x-access-token`) when `provider: gitlab` —
deploy tokens, which need their own configured username, aren't supported by this
synthesized default, same "extend, don't configure around" posture as GitHub
Enterprise not being supported by URL-sniffed detection.

**Important operational detail, found running this live**: the agent must *not* call
`update_issue_state` with `"done"` in this mode (that decision belongs to whoever
reviews the PR) -- but it still needs to call `update_issue_state` with some
*non-terminal, non-active* state (e.g. `"in review"`, added to the project's own
`active_state_labels` but deliberately left out of `active_states`) once it's opened
the PR. Without that, the issue stays in an active state indefinitely, and Symphony's
own dispatcher keeps redispatching it forever with nothing new to do -- there's no
mechanism to leave a ticket "paused, waiting on an external event" otherwise. This
needs no Symphony code changes (an issue whose normalized state matches neither
`active_states` nor `terminal_states` is already never dispatched, and an already-running
one in that state is already cleanly stopped by `reconcile`'s "no longer
active/routable" path) -- it's purely a `tracker.provider.active_state_labels` +
prompt convention a project sets up itself, documented here so the next project
enabling `repo.pull_request` doesn't rediscover this the same way.

Off by default: this is a real behavior change (a live PR gets opened on the real
repo, and "done" no longer means what it used to), so a project opts in
deliberately, same posture as `workspace.docker.mount_claude_credentials`.

## Evidence collection

`repo.evidence: true` (requires `repo.pull_request: true` -- there has to be a PR/MR
to attach evidence to) exposes a second agent tool alongside `open_pull_request`:
`attach_evidence({image_path, caption})`. The coding agent produces an image itself
somewhere inside its own workspace -- typically a screenshot of the app it just built
or changed, taken with whatever headless-browser tooling the project's own agent
Docker image provides (nothing built into Symphony launches a browser; that's the
project's job, same as `cargo test`/`npm test` already is) -- and calls the tool with
the image's path (relative to the workspace root) and a short caption. The tool
uploads the file to the ticket branch via the code host's contents API
(`repo_host::github::GithubRepoHost::attach_evidence` /
`repo_host::gitlab::GitlabRepoHost::attach_evidence`) at a content-hashed path under
`.symphony/evidence/` (hashed, not the agent's own filename, so a same-named retry
never collides with an existing blob and never needs that blob's `sha`/lock token to
update it -- a plain create always succeeds) and returns a ready-to-paste markdown
image snippet pointing at the committed file (`raw.githubusercontent.com` on GitHub, a
`-/raw/<branch>/<path>` URL on GitLab). The agent pastes that snippet into
`open_pull_request`'s `body` so a reviewer sees the working app directly instead of
taking the agent's word for it.

**Resolving the image path**: `attach_evidence` needs to read a file from whatever
filesystem the `__mcp_tool_server` subprocess itself is running on -- the in-container
path in Docker mode (since `claude`/`opencode` and their MCP subprocess all run inside
the container there), the host path otherwise. This is threaded through as a new
`--workspace-dir` argument to `__mcp_tool_server` (`main.rs`), computed with the same
container-aware mapping `--workflow-dir` already uses
(`claude::write_mcp_config`/`opencode::mcp_config_env`), and passed down through
`mcp::run_stdio_server` into `RepoHost::execute_agent_tool`'s new `workspace_dir`
parameter -- every tool but `attach_evidence` ignores it.

Off by default, same posture as `repo.pull_request` itself: this commits a real file
to the real repo. A project enabling this should also tell its agent, in its own
`WORKFLOW.md` prompt, how to actually get a screenshot in the first place (start the
app, point a headless browser at it, save a PNG) -- Symphony only handles getting that
file from the workspace onto the PR/MR, not producing it.

## SweBot

A software-engineering assistant with three capabilities, working against either
GitHub or GitLab (`repo.provider`), no webhook receiver — just another poll loop,
like everything else here:

1. **Q&A**: answers questions asked in the repo's `Q&A`-category GitHub Discussions,
   or, on GitLab (which has no Discussions object), Issues carrying the
   `swebot.qa.label` label (default `"swebot::question"`).
2. **Ticket drafting**: turns a rough idea posted in the `Ideas`-category Discussions
   (GitHub) or carrying the `swebot.drafting.label` label (GitLab, default
   `"swebot::idea"`) into a properly scoped issue through a clarifying dialogue, then
   creates a **new** issue via the tracker (the source thread stays the messy
   conversational space; the tracker's Issues stays the clean, actionable backlog
   Symphony's own dispatch loop watches — on GitLab, where the source thread is
   itself a tracker Issue, the source issue is closed once promoted to keep that
   separation from collapsing).
3. **PR/MR review**: reviews the pull/merge requests Symphony's own coding agents
   open (branch name matching `issue-<identifier>`, not every PR/MR a human might
   open by hand) and posts an approve/request-changes/comment verdict. **Never
   merges** — a human always does that.

Q&A and ticket drafting run through **chat mode's** GitHub connector
(`src/swebot/chat/github.rs`) — the same store/worker pipeline that backs the chat UI
(see "SweBot chat mode"); PR review is polled directly by `swebot::run`. Both read
the same `repo:` block and share the one restricted backend.

```yaml
swebot:
  enabled: true
  token: $SWEBOT_GITHUB_TOKEN         # optional; see "Two identities" below
  qa:
    discussion_category: "Q&A"        # default, GitHub
    label: "swebot::question"         # default, GitLab
  drafting:
    discussion_category: "Ideas"      # default, GitHub
    label: "swebot::idea"             # default, GitLab
  review:
    enabled: true                     # defaults to `swebot.enabled`'s own value
```

Off by default, same posture as `repo.pull_request`: this posts real comments and
reviews on a real repo. Requires `repo.url` to parse for the configured
`repo.provider` and either `repo.token` or `swebot.token` to be set — SweBot keys off
`repo:` (the same source of truth `repo.pull_request` uses) rather than
`tracker.provider.repo`, so it works the same regardless of `tracker.kind`.

**Two identities — why `swebot.token` exists**: by default SweBot authenticates with
the same `repo.token` the coding agent uses to push branches and open PRs/MRs.
That's fine for Q&A and drafting, but it breaks approving specifically: GitHub's API
rejects an `APPROVE`/`REQUEST_CHANGES` review from the same account that authored the
pull request (422 "Can not approve your own pull request") — a blanket restriction —
and since Symphony's coding agent is always the PR's author, single-identity SweBot
can *never* actually approve one. GitLab's equivalent ("prevent approval by author")
is narrower: it only blocks the `/approve` endpoint, not plain discussion notes, so
single-identity SweBot on GitLab can genuinely post a request-changes or comment
verdict even without a second identity — only *approving* needs one. Either way,
`swebot/review.rs`'s driver falls back to posting a plain comment instead so a poll
cycle doesn't loop forever retrying a rejected approval, but a genuine approval never
lands until a second identity is configured. Set `swebot.token` to a second
credential (a separate bot account/App installation, e.g. "codebot" for the coding
agent's own `repo.token` and "swebot" for this one) to give SweBot a distinct
identity from the PR/MR author, and approvals go through for real. `swebot.token`
follows the same `$VAR_NAME`-env-var-reference convention as `repo.token` (see
`config::RepoConfig::token_env`'s doc comment) — the value itself is never embedded in
config, only the env var's name.

**Persona and quality bar**: every SweBot prompt shares one tone/rubric prefix
(`swebot::PERSONA`) — friendly and direct, but holding a genuinely high bar
(correctness against the original ticket's acceptance criteria, security, performance,
matching the project's own conventions) rather than a rubber stamp. `request_changes`
means something genuinely fails one of those; `approve` means "I'd merge this."

**Why GitHub Discussions, not issue comments — and what GitLab uses instead**:
Discussions' built-in `Q&A`/`Ideas` categories are a much closer semantic fit for
open-ended conversation than Issues, which are meant to be scoped, actionable work
items. The trade-off: Discussions has no REST API, only GraphQL
(`repo_host::github::GithubRepoHost`'s own `graphql` helper, alongside the plain-REST
calls everything else here uses) — see `src/repo_host/github.rs`'s own module doc
comment. GitLab has no Discussions object at all, so its Q&A/Ideas conversational
surface is instead a plain label on regular Issues — a real trade-off (the
conversational and the actionable share one Issues list, distinguished only by
label), but GitLab's Issue Notes API already supports threaded comments, so the
thread+comment shape SweBot's marker-scanning logic consumes barely changes between
the two hosts (see `src/repo_host/gitlab.rs`'s own module doc comment for the
provider-specific wire-format details, all handled entirely inside that module).

**No local persistence (until chat)**: which PR/MR commits SweBot has already reviewed
is derived from a hidden HTML-comment marker (`<!-- swebot:reviewed:<sha> -->`) embedded
in SweBot's own review text, scanned out of the host's own stored data on every poll;
review never needs a database. The chat side (`swebot.chat.enabled`) does land in
`symphony-chat.db` (which also keeps mid-flight turn state and read receipts), but its
*dedupe* is still host-native: the `<!-- swebot:answered:<id> -->` markers in posted
replies are what tell a poll (and a fresh chat ingest) that a message was already
answered, so even a reset chat store picks back up exactly where the markers say.
Ticket drafting doesn't use `--resume` across poll cycles either (a human's next reply
may come hours later, well past a restart) -- each turn reconstructs the transcript
from the thread's own comment history and sends it fresh, which the model already
needs context for anyway.

**Isolation from the coding agent's own trust boundary**: SweBot's sessions run with
file-mutating tools explicitly disallowed, on top of the same high-trust mode ticket
dispatch uses (auto-approve Bash/read tools so SweBot can still explore the code and
run tests during review). The restriction is backend-specific rather than a hardcoded
CLI flag: for `claude` it's `--disallowedTools Edit,Write,NotebookEdit`; for `opencode`
it's a `permission` config of `{"edit":"deny"}` (the umbrella rule covering
`edit`/`write`/`patch`) passed as `OPENCODE_PERMISSION`, which enforces the deny even
under `--auto`. The backend SweBot runs on is `swebot.backend` when set, else
`agent.backend` (see "Coding-agent backends") — so SweBot can answer/draft/review on
`opencode` (e.g. Fireworks) while tickets stay on `claude`. `codex` is not supported
for SweBot yet; it refuses to start rather than silently running an unrestricted
skeleton. SweBot answers, drafts, and reviews — it never edits the repo directly. That
stays the coding agent's job, gated by the normal ticket-dispatch flow this whole
document is otherwise about.

## SweBot chat mode

A unified, collaborative Q&A + ticket-drafting conversation surface. Active under
`swebot.chat.enabled`; when it's on for a GitHub repo, chat's GitHub connector owns that
repo's Discussions (`src/swebot/chat/github.rs`) and the standalone Q&A/drafting loop
skips GitHub -- GitLab keeps the standalone loop, and when chat is off the standalone
loop handles GitHub too, exactly as before; the bundled browser chat UI is served
alongside. Same
restricted backend as Q&A/drafting/review, same `swebot.backend`-or-`agent.backend`
rule, but turned toward interactivity:

```yaml
swebot:
  enabled: true
  backend: opencode
  chat:
    enabled: true
    connectors: [web]              # interactive connectors: 'web' is the chat UI
    poll_interval_ms: 5000         # how often the worker answers pending messages (local only)
    remote_poll_interval_ms: 30000 # how often each remote connector (github) polls its own API
    max_concurrent_replies: 2      # answers per processing cycle (1-2 is plenty)
    auto_create_issue: true        # file a finished draft immediately (default)
    first_text_deadline_ms: 2000
```

`poll_interval_ms` and `remote_poll_interval_ms` are deliberately separate: answering a
pending message is a local SQLite read plus a model turn, so polling for it often costs
nothing external -- but a remote connector's `ingest`/`deliver` (GitHub Discussions'
GraphQL API today) burns real rate-limit budget every tick, so it defaults to a much
slower cadence. `web` has no remote platform to poll (`ingest`/`deliver` are no-ops
there), so `remote_poll_interval_ms` only matters once a remote connector is active.

**How it's structured** (`src/swebot/chat/`): a SQLite store (`symphony-chat.db` next to
`symphony.db`, `store.rs`) holds conversations and messages; a connector-agnostic
worker (`worker.rs`) claims `pending` user messages and answers them; connectors
(`connector.rs`) bridge the store to each platform:

- `github.rs` — when chat is on for a GitHub repo, the Discussions Q&A/drafting surface
  (and the standalone loop skips GitHub). It ingests each human comment as a `pending`
  user message and `deliver`s sent replies back as discussion comments carrying the
  `swebot:answered` marker; a slow turn's "still working" notice is delivered as a
  marker-less comment so the thread isn't marked answered by a placeholder.
- `web.rs` (when `swebot.chat.enabled`) — the bundled browser chat UI (server-rendered
  page + tiny JSON API: `/send`, `/messages`, `/read`), mounted at `/chat` under the
  status dashboard, and per-project at `/projects/<id>/chat` under `symphony serve`.
- `teams.rs` (`--features teams`) — a compile-checked MS Teams skeleton that shows
  exactly what a new connector implements; see "Adding a chat connector" below.

The chat pipeline runs once per project under `swebot.chat.enabled`; its `web`
connector's UI is always part of that. The `github` connector is a GitHub-only
surface -- GitLab Q&A/drafting stays on the standalone loop.

**Adding a chat connector** (e.g. MS Teams): implement the two-method `ChatConnector`
trait (`ingest` pulls platform messages in, `deliver` pushes replies out) — the store
already gives you idempotent ingestion (`upsert_remote_user_message` keyed by
`remote_message_id`), eventual delivery (`undelivered_*` + `mark_delivered`), and
read-receipt state (`read`/`read_at`). Construct your connector in `chat::start`
alongside the existing ones. That's the whole API; `teams.rs` compiles as the template.

**Latency: streaming + the must-notify rule.** Chat answers stream: text chunks are
flushed into the store as the turn produces them, so the UI shows the reply appearing
in place (and the `read`/`processing` indicators move with it) rather than a blank
window. If the first text hasn't arrived within `first_text_deadline_ms` the worker
inserts a system notice ("still working — checking the code") *before* the slow turn
finishes, instead of leaving the user staring at a spinner; the notice resolves once
the reply lands. The prompt also asks the model to front-load a one-line commitment
whenever a turn needs real research, but the deadline is the structural backstop, not
a model-behavior hope.

**Status vocabulary** (rendered by the web UI): user messages `pending → processing →
processed | failed`; assistant messages `streaming → sent → read` (the `read` tick is
a genuine read receipt — the browser POSTs `/read` once it has actually displayed
them); system rows `notice-active → notice-done`. Everything survives restarts in the
same SQLite file; a message a crashed run left mid-claim is requeued at startup.

**Collaborative drafting**: when the model finishes a scoped draft, `auto_create_issue:
true` creates it via the tracker immediately (the reply carries the issue link) — or,
with `auto_create_issue: false`, SweBot stashes the draft and asks the user to reply
"create it" to file it, so nothing lands in the tracker without a human's explicit go.
Clarifying questions come back as ordinary messages; the conversation keeps going
until either the ticket exists or the user drops it. Chat requires `swebot.enabled`
(validation rejects `chat.enabled` otherwise) and shares SweBot's `repo:`/token
requirements.

## Daemonizing Symphony (single project)

Everything above assumes a human launches `symphony` and watches it. `symphony
daemon` runs Symphony itself as a long-lived, auto-restarting, single-instance Docker
container instead -- point it at a git repo, a GitHub issues board (or the local
tracker), and a `WORKFLOW.md`, and it keeps working unattended, surviving crashes and
reboots without a terminal session to babysit it.

```bash
symphony daemon start ./WORKFLOW.md --port 7777
symphony daemon status ./WORKFLOW.md
symphony daemon logs ./WORKFLOW.md --follow
symphony daemon stop ./WORKFLOW.md
```

**Requires `workspace.docker.enabled: true`** (see "Docker mode" above) -- the daemon
container spawns per-ticket sibling containers by reaching the *host's* Docker daemon
through a mounted socket (Docker-outside-of-Docker), so per-ticket Docker mode has to
already be configured; `daemon start` refuses to run otherwise.

**Under the hood**: `daemon start` runs the same base image `docker build -t
symphony-base .` produces, as `docker run -d --name symphony-daemon-<hash> --restart
unless-stopped -v <named-volume>:/project -v /var/run/docker.sock:/var/run/docker.sock
-e SYMPHONY_DAEMON_VOLUME=<named-volume> ... symphony <workflow-path> --port <port>`.
Three things worth knowing:

- **The project lives in a named Docker volume, not a host bind-mount.** Per-ticket
  sibling containers are created by the *host's* Docker daemon, not nested inside the
  daemon's own container -- a bind-mount naming the daemon's own in-container path
  (e.g. `/project`) would resolve to nothing on the host, where those sibling
  containers actually run. A named volume is referenced by name and resolves
  correctly regardless of which container asked for it. The first `daemon start` for
  a project seeds a fresh volume by copying whatever's currently on the host at
  `workflow_dir`; later starts reuse the same volume as-is (so the daemon's own
  commits/pushes since then aren't overwritten by a stale host copy).
- **`docker run --name` is the single-instance guard.** A second `daemon start` for
  the same project fails immediately with a clear error instead of silently racing --
  directly closing the failure mode from an incident that motivated daemonizing this
  in the first place: a manually-relaunched orchestrator accidentally left two
  instances dispatching the same ticket concurrently.
- **The status dashboard binds `0.0.0.0` instead of `127.0.0.1` in daemon mode**
  (detected via the `SYMPHONY_DAEMON_VOLUME` env var `daemon start` sets) -- loopback
  in a container's own network namespace isn't reachable through `docker run -p` at
  all, so the usual host-mode "loopback only" default would make `--port` silently
  useless. The container boundary is the isolation mechanism here instead;
  reachability is already gated by whether `-p`/`--port` was passed.

**Crash-restart** is Docker's own `--restart unless-stopped` policy: it restarts on an
internal crash or a Docker daemon restart, but deliberately *not* after an explicit
`docker stop`/`kill` (`symphony daemon stop` included) -- that's Docker's standard
"stopped on purpose, stay stopped" semantics, not a Symphony behavior. **Reboot
survival** additionally depends on Docker Desktop itself starting on login --
something to configure in Docker Desktop's own settings, not in Symphony.
`daemon start` against a project whose daemon was `stop`ped (container still exists,
just not running) resumes that same container rather than erroring on a name
conflict; it comes back with whatever port/settings it was originally started with,
not new flags passed to the later `start` call -- `docker rm` the old container first
to pick up a changed image or port.

**Security note**: mounting the host's Docker socket into the daemon container is
**effectively root-equivalent host access** -- anything that can reach the socket can
launch a container with an arbitrary bind-mount of the entire host filesystem. This is
a materially bigger trust concession than per-ticket Docker mode alone (which only
ever bind-mounts one project directory into an otherwise-isolated container), and
applies on top of that section's own caveats, not instead of them. Same bottom line,
stated more strongly here: reasonable for a single trusted operator running their own
project unattended on their own machine, not a step toward multi-tenant safety.

## Long-running multi-repo service

`symphony serve` is a different way to run Symphony unattended: instead of one process
per repo (`symphony daemon`, above), one process manages **any number of repos**,
registered through a browser instead of a local `WORKFLOW.md` file.

```bash
SYMPHONY_ADMIN_TOKEN=<a-shared-secret> symphony serve --port 8080 --data-dir ./symphony-data
```

Requires `SYMPHONY_ADMIN_TOKEN` — the web UI can register/remove repos, so it refuses
to start unprotected rather than defaulting to open (see "Auth" below). Open
`http://localhost:8080`, log in with that token, and register a repo: its GitHub URL,
branch, the path to its `WORKFLOW.md` inside that repo (defaults to `WORKFLOW.md` at
the root), and — for private repos — the *name* of an env var already set in this
service's own environment holding a GitHub token (never a literal token pasted into
the form; see "Tokens" below).

**What happens on registration** (`src/service.rs`): Symphony fetches that
`WORKFLOW.md` straight from GitHub's contents API (`repo_host::fetch_file`), writes it
to `<data-dir>/projects/<slug>/WORKFLOW.md`, and spawns a headless
`orchestrator::run_managed` task for it — the exact same tracker-polling/SweBot logic
`symphony`/`symphony daemon` already run for one project, just started
programmatically instead of from the CLI, and stoppable from the outside (a `oneshot`
shutdown signal) without killing the rest of the service. Everything is persisted to
a SQLite registry (`<data-dir>/registry.db`, `src/registry.rs`) so registered repos
resume automatically the next time `symphony serve` starts.

**Staying in sync with GitHub**: every ~5 minutes, a background task re-fetches each
registered repo's `WORKFLOW.md` and overwrites the local copy if it changed. No new
reload logic needed for this — `orchestrator.rs`'s existing hot-reload
(`maybe_reload`) already mtime-watches that same local file every poll tick; the
refresh task's only job is to be the thing that changes it, from the GitHub side.

**Per-project dashboards**: each registered project's own status/events/usage pages
(the same ones described in "Live status dashboard" above) are nested at
`/projects/<slug>/...`, reusing `status::router` unmodified — `status.rs` renders
every link/asset URL relative to a `base_path` specifically so this works regardless
of whether it's mounted at the root (single-project CLI mode) or under a prefix (here).

**Tokens**: exactly like `repo.token`/`swebot.token`/`claude.api_key` everywhere else
in this codebase, the registration form only ever accepts an env var *name*, never a
literal secret — Symphony resolves it to a value solely at the point of an outbound
GitHub call, and never stores or displays the resolved value. This does mean a
private repo's token must already be set in `symphony serve`'s own process
environment *before* you register that repo; there's no way to hand Symphony a
brand-new secret purely through the browser, by design.

**Auth**: a single shared admin token (`SYMPHONY_ADMIN_TOKEN`) gates `/register` and
`/projects/<id>/remove` only — logging in sets an `HttpOnly` cookie; a non-browser
client can instead send `Authorization: Bearer <token>`. The read-only dashboard
(`/` and every nested per-project page) stays open, same posture the single-project
dashboard (`status.rs`) already has. This is one operator-held bearer token, not a
user-account system — appropriate for the same "single trusted operator" scope as the
rest of this project (see "Trust and safety posture" below), not multi-tenant access
control.

**Running it unattended**: no changes were needed to `Dockerfile` for this — it
already produces a `symphony` binary capable of running any subcommand. Run it the
same way you'd run any other long-lived container:

```bash
docker run -d --restart unless-stopped -p 8080:8080 \
  -e SYMPHONY_ADMIN_TOKEN=... -e GITHUB_TOKEN=... \
  -v symphony-service-data:/data \
  symphony-base symphony serve --port 8080 --data-dir /data
```

This is deliberately not wired through `symphony daemon` — that command's
container/volume naming is keyed off a single project's workflow directory, which
doesn't apply to a service managing many repos at once. A plain `docker run --restart
unless-stopped` gets the same "keeps running, survives crashes" property directly.

## Delivery pipeline (AI Roadmap 2026, Step 1 -- AIR-1)

Off by default. `pipeline.enabled: true` in `WORKFLOW.md` turns a ticket's single
undifferentiated agent run into an ordered sequence of stages executed within the
*same* per-issue workspace and agent session (a stage boundary is not a workspace
boundary):

```yaml
pipeline:
  enabled: true
  blocked_state: blocked      # default; must sit outside both active_states and
                               # terminal_states so a parked issue is never redispatched
  stages:
    - id: requirements
      role: requirements       # not yet resolved to a distinct prompt/backend --
      max_turns: 4             # see "not yet implemented" below
      on_failure: escalate     # escalate (default) | retry | skip
    - id: implement
      role: developer
      max_turns: 20
    - id: review
      role: reviewer
      max_turns: 6
      blocking: true           # a failed exit criterion parks the issue instead of
                                # falling back to the normal whole-attempt retry
```

`pipeline:` absent (the default) leaves every code path byte-identical to before this
feature existed — `orchestrator::run_attempt_body` runs its original single loop over
`agent.max_turns`, no stage events, no config validation for a block that isn't there.

**What a stage is, today.** Each stage runs the project's one `WORKFLOW.md` prompt for
up to its own `max_turns`, exactly the same per-turn loop (render prompt, run turn,
refresh tracker state, check still-active/routable) the legacy single-stage path always
ran — factored into `orchestrator::run_turn_loop` so both paths share it. A stage
"succeeds" when it exhausts its own turn budget without an agent-turn error; it "ends
the whole cycle" if the issue leaves the active/routable tracker state mid-stage (e.g.
the agent called `update_issue_state` to a terminal state) -- exactly what ended the
legacy single-stage run.

**`on_failure`** governs what happens when a turn inside a stage errors out:
- `escalate` (default): stop the cycle. A `blocking: true` stage parks the issue in
  `pipeline.blocked_state` (via `TrackerAdapter::set_issue_state`, host-side — the
  orchestrator's own decision, not something asked of the just-failed agent); a
  non-blocking stage falls back to the orchestrator's existing whole-attempt retry
  backoff, the same path any turn failure already took before pipelines existed.
- `retry`: re-run the stage's own turn budget once more before falling back to
  `escalate`'s handling — a bounded retry, not unbounded looping.
- `skip`: record the failure and move on to the next stage anyway.

**A non-blocking `escalate`/exhausted-`retry` failure falls back to the whole-attempt
retry the legacy path already had** -- there is no per-stage checkpoint yet (that's
AIR-13's durable cycle state), so the next dispatch of that issue re-runs the pipeline
from its first stage, same as a legacy single-stage attempt always retried the whole
attempt on error.

**Not yet implemented** (later AIR tickets, see `issues/AI-ROADMAP-PLAN.md`): per-stage
roles resolving to distinct prompts/backends/models and backend-native tool
restrictions (AIR-2), a structured artifact store so one stage's output feeds the next
(AIR-3), and the actual Requirements/Planner/Test/Reviewer/Security/Release/
Observability agents themselves (AIR-4 … AIR-10) — today every stage runs the same
prompt and the only failure signal is a turn erroring out, not a judgement about
whether the work actually meets any exit criteria.

**Observability.** Per-stage progress is always visible without extra config: the
live dashboard's running card shows the current stage (`status::RunningRow::stage`),
and every stage boundary is a `stage_started`/`stage_finished` event on `/events`,
filterable by issue like any other event — no new page, reusing the existing
dashboard exactly as it already works for turns and tool calls.

## The human approval gate (AI Roadmap 2026, Step 1 -- AIR-5)

Roadmap §3: "Human approval is required for architectural, business-critical and
high-risk decisions." Any pipeline stage can require one:

```yaml
pipeline:
  enabled: true
  blocked_state: blocked
  awaiting_approval_state: awaiting approval   # default; same "outside active/terminal
                                                # states" convention as blocked_state
  approval:
    auto_approve_when:            # absent (the default) -- never auto-approve
      risk: low                   # must match the stage's own reported `risk`
      impacted_components: [src/status.rs, src/web.rs]   # allowlist; every reported
                                                           # component must be in it
      estimate_turns_max: 4       # reported `estimate_turns` must be <= this
  stages:
    - id: plan
      role: planner
      max_turns: 6
      requires_approval: true
    - id: implement
      role: developer
      max_turns: 20
```

When a `requires_approval: true` stage completes successfully, `orchestrator::
handle_stage_approval` looks for a fenced ` ```json ` block in the stage's last turn
message (the same convention `swebot`'s `qa`/`drafting`/`review` drivers already use to
get structured output from free text — reused via `swebot::extract_json_block`) and
evaluates `pipeline.approval.auto_approve_when` against it. A match moves straight to
the next stage, no human involved, and records an `approval_auto_approved` event. No
match (including no structured output at all — missing information never satisfies a
configured condition) parks the cycle exactly like a `blocking` stage's failure does:
the issue moves to `pipeline.awaiting_approval_state` (host-side, via
`TrackerAdapter::set_issue_state`) and a pending row is recorded in `symphony.db`
(`src/approvals.rs`) — SQLite, so it survives a daemon restart.

**Two channels resolve a pending approval**, both ending up as a call to
`approvals::resolve` (which only *records* the decision — see below for why):

- **Dashboard** — `/approvals` (`status.rs`) lists every pending request with its
  stage's captured output and Approve / Request changes / Reject buttons, each a POST
  form gated by the same `SYMPHONY_ADMIN_TOKEN` (Bearer header or the
  `symphony_admin` cookie `symphony serve`'s own `/login` sets) `symphony serve` already
  uses — never a state-changing GET link.
- **Issue comment** — `orchestrator::poll_approval_comments`, run every tick alongside
  dispatch, scans every pending approval's issue thread (`TrackerAdapter::
  fetch_issue_comments`, implemented for `local`/`github`; unsupported adapters just
  never surface a comment) for `/approve`, `/changes <reason>` or `/reject [reason]`
  past whatever was already scanned.

**Applying a decision is the orchestrator's job alone** (`orchestrator::
apply_resolved_approvals`, called every tick): the one thing with standing authority to
mutate tracker state moves the issue to `active_states[0]` (approve/changes) or
`pipeline.blocked_state` (reject), records an `approval_decided` event (actor,
timestamp, outcome, comment — the roadmap §4 "decision traceability" bar), and leaves a
resume point. `run_pipeline` consumes that resume point (`approvals::take_resume`,
handed out at most once) at the start of its next cycle: "approve" resumes at the
stage *after* the approved one; "request changes" re-runs the *same* stage with the
reviewer's comment appended to its first prompt.

Neither the dashboard handler nor the comment poller mutates tracker state directly —
see `approvals.rs`'s module doc comment for why that split exists.

## Provider-native tracker tool (Section 10.5)

The coding agent only ever runs inside the per-issue workspace — it has no visibility
into `issues/` (or wherever the tracker actually stores state). Without a way to write
back, an issue never leaves its active state, so the orchestrator just keeps
redispatching it forever (continuation retries) once the agent thinks it's done.

`LocalTrackerAdapter` fixes this by exposing one provider-native tool,
`update_issue_state({state})`. The wiring (`claude` and `opencode` backends):

1. `TrackerAdapter` has two default (opt-in) methods, `agent_tool_specs()` and
   `execute_agent_tool()` — an adapter with nothing to expose just doesn't override
   them (Section 10.5's `agent_tool_specs()` / `execute_agent_tool()` hooks).
2. If the active adapter returns any specs, `start_session` wires an MCP server
   pointing back at **this same `symphony` binary**, run as
   `symphony __mcp_tool_server --tracker-kind ... --issue-id ...` (a hidden
   subcommand; see `src/mcp.rs`). How that wiring reaches the agent CLI differs per
   backend, since neither exposes an equivalent flag:
   - `claude`: `ClaudeSession::start_session` writes a `--mcp-config` file into the
     workspace and passes `--mcp-config <file> --strict-mcp-config`.
   - `opencode`: has no per-invocation config flag, only config-file discovery, but
     does have `OPENCODE_CONFIG_CONTENT` (inline config content via env var) as its
     own escape hatch for exactly this — `mcp_config_env` (`src/agent/opencode.rs`)
     sets it to a `{"mcp": {"symphony": {"type": "local", "command": [...]}}}` blob.
     Confirmed against opencode's own docs: config layers (global/project/env-var
     inline) *deep-merge*, they don't replace each other, so this never disturbs the
     separately-baked-in provider config (e.g. the image's own Fireworks setup) —
     the inline blob only ever adds the `mcp` key.
   Either way, the agent CLI spawns that command as its own MCP server subprocess
   whenever the model calls the tool.
3. That subprocess rebuilds the tracker adapter from the same config and executes the
   write itself — the coding agent process never touches `issues/*.md` directly. This
   matches the spec's tracker-write boundary (Section 11.5): mutations happen host-side,
   through the adapter, not via raw agent file access.
4. The tool is always auto-approved independent of `claude.permission_mode`/
   `opencode`'s own permission config, since it's host-mediated and scoped to exactly
   one tool: `claude` via `--allowedTools mcp__symphony__* --strict-mcp-config`;
   `opencode` has no equivalent allowlist flag, but `--auto` (its own high-trust
   default, see above) already approves every tool call, MCP included, and a
   restricted `OPENCODE_PERMISSION` deny-rule (SweBot's own sessions) only ever
   targets `edit`/`write`/`patch`, not MCP tool calls, so this reaches the same
   effective posture without needing one.

`codex` doesn't get this wiring yet — Codex's own dynamic-tool-call mechanism
(Section 10.5) would need separate plumbing in the (already best-effort) Codex client.

**A second, independent tool source**: when `repo.pull_request: true` (see "Git repo
as first-class input" above), `open_pull_request({title, body})` from the configured
`RepoHost` (`src/repo_host/{github,gitlab}.rs`, behind the `RepoHost` trait in
`src/repo_host/mod.rs`) is exposed the same way, *alongside* whatever the tracker
itself exposes — `run_stdio_server` merges both tool lists and routes each
`tools/call` by name to whichever side owns it. This is deliberately not a
`TrackerAdapter` tool: a pull/merge request is a property of `repo:` (the code host),
not `tracker:` (the issue board), so it's kept as its own capability rather than
folded into the tracker's own tool set. `repo.pull_request` also works with
`tracker.kind: local` — the MCP subprocess gets spawned whenever *either* side has a
tool to offer, not just when the tracker does.

## What's implemented (Section 18.1 core conformance)

- `WORKFLOW.md` loading: front matter + prompt body split, defaults, `$VAR` and `~`
  resolution, hot reload on file change (checked once per poll tick — see below).
- Strict prompt rendering (`{{ issue.field }}`, `{{ x | default: "…" }}`,
  `{% for x in issue.labels %}`) that fails on unknown variables/filters.
- A single-authority orchestrator: poll → reconcile → dispatch, with sorted candidate
  selection, global + per-state concurrency limits, exponential retry backoff, and
  continuation turns within one worker run up to `agent.max_turns`.
- Reconciliation: stall detection (`stall_timeout_ms`) and tracker-state refresh each
  tick, per Section 8.5.
- Workspace manager: sanitized + collision-resistant workspace keys, root-containment
  checks before every agent launch, all four lifecycle hooks with timeouts.
- Structured `tracing` logs with `issue_id` / `identifier` / `session_id` context.

## What's deliberately out of scope

- **Only one tracker adapter ships**: `tracker.kind: local`, a directory of Markdown
  files (see `src/tracker/local.rs` for the exact schema). It exists so the whole
  system is runnable and testable without a real tracker credential. `TrackerAdapter`
  is a trait (`src/tracker/mod.rs`); adding GitHub/Jira/Linear adapters is additive
  and doesn't touch the orchestrator. The `local` adapter supports an extension field,
  `depends_on: [identifier, ...]`, that ANDs `dispatchable` with "every listed
  dependency is at state `done`" and populates `blocked_by` — this is how you get real
  dependency graphs (and therefore safe parallel dispatch of independent tickets)
  instead of relying purely on `created_at` ordering under `max_concurrent_agents: 1`.
- **No `/api/v1/*` JSON API** (Section 13.7, OPTIONAL). There is a minimal live HTML
  dashboard (`--port <PORT>`, see above), but it's a debug/dev view, not a stable API.
- **No SSH worker extension** (Appendix A, OPTIONAL). Workers always run locally.
- **Provider-native tools only exist for `local` + `claude`** (see above); `codex`
  doesn't get them yet.
- **Reload detection is poll-tick-granularity**, not a filesystem watcher: `WORKFLOW.md`
  mtime is checked once per tick (Section 6.2 permits this — "re-validate/reload
  defensively... in case filesystem watch events are missed" — we just made that the
  only mechanism, for one fewer dependency).
- **Token accounting is per-turn, not cumulative-absolute**: the Claude backend spawns
  one subprocess per turn (no persistent thread process with an absolute running
  total), so each turn's `usage` is treated as a delta and summed. This differs from
  Section 13.5's "prefer absolute thread totals" guidance, which assumes a long-lived
  app-server thread.

## Trust and safety posture (Section 15.1)

This implementation targets **trusted, single-operator environments**. Both backends
auto-approve file edits and command execution for the session
(`claude.permission_mode: bypassPermissions`; Codex's `approval_policy`/`sandbox` are
pass-through and default to whatever your installed Codex build defaults to). There is
no operator-approval channel — a run that would require interactive approval or
user-input fails the run rather than stalling. If you need stricter sandboxing, tighten
`claude.permission_mode` / `codex.approval_policy` and `codex.turn_sandbox_policy`, or
enable **Docker mode** (see above) for filesystem/resource isolation per ticket
(Section 15.5) — Claude backend only today, and not sufficient on its own for
untrusted-input use; see that section's security posture notes for exactly what it
does and does not cover.

Filesystem safety invariants from Section 9.5 are enforced unconditionally (not
configurable): the agent's `cwd` is always validated to be a sanitized, root-contained
per-issue workspace before every launch.

`symphony serve`'s multi-repo web UI (see above) extends this same single-operator
posture, not a multi-tenant one: one shared admin token, no per-repo access control,
no audit log beyond the existing per-project event log. Fine for one operator managing
their own set of repos; not a step toward letting untrusted users register repos of
their own.

## Development

```bash
cargo test      # unit tests across every module
cargo clippy --all-targets
```
