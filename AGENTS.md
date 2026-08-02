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
  turn count, last event) and the retry queue. Live-updates every 2s via a small
  inline `fetch()` + `innerHTML` swap against `/fragment` — not a `<meta>` full-page
  refresh (the original mechanism, and genuinely janky: every tick was a real browser
  navigation, resetting scroll position and any open selection). The new mechanism has
  none of that; nothing outside the two data containers ever reloads.
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

**Pull requests instead of a silently pushed branch**: by default the synthesized
`after_run` hook pushes each ticket's branch and leaves it there for a human to
notice on their own. Set `repo.pull_request: true` (requires `repo.url` to be a
`github.com` URL and `repo.token` to be set) to expose a second agent tool,
`open_pull_request(title, body)`, alongside whatever the tracker itself exposes (see
"Provider-native tracker tool" below) — the agent calls it once it's pushed and happy
with the change, supplying its own title and rationale. Calling it again (a retry, or
more work landing on the same branch) updates the existing PR in place rather than
creating a duplicate.

This changes how issues close: put `Closes #<issue-number>` in the PR body and GitHub
closes the tracker issue automatically **when a human merges the PR**, not when the
agent decides it's done. The existing GitHub tracker adapter already treats a closed
issue as done regardless of how it got closed, so nothing else needs to change for
the polling loop to pick this up.

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

## SweBot

A software-engineering assistant with three capabilities, all GitHub-native (no new
chat UI, no webhook receiver — just another poll loop, like everything else here):

1. **Q&A**: answers questions asked in the repo's `Q&A`-category GitHub Discussions.
2. **Ticket drafting**: turns a rough idea posted in the `Ideas`-category Discussions
   into a properly scoped issue through a clarifying dialogue, then creates a **new**
   issue via the tracker (Discussions stays the messy conversational space; Issues
   stays the clean, actionable backlog Symphony's own dispatch loop watches).
3. **PR review**: reviews the pull requests Symphony's own coding agents open
   (branch name matching `issue-<identifier>`, not every PR a human might open by
   hand) and posts an approve/request-changes/comment verdict. **Never merges** — a
   human always does that.

```yaml
swebot:
  enabled: true
  token: $SWEBOT_GITHUB_TOKEN         # optional; see "Two identities" below
  qa:
    discussion_category: "Q&A"        # default
  drafting:
    discussion_category: "Ideas"      # default
  review:
    enabled: true                     # defaults to `swebot.enabled`'s own value
```

Off by default, same posture as `repo.pull_request`: this posts real comments and
reviews on a real GitHub repo. Requires `repo.url` to be a `github.com` URL and
either `repo.token` or `swebot.token` to be set — SweBot keys off `repo:` (the same
source of truth `repo.pull_request` uses) rather than `tracker.provider.repo`, so it
works the same regardless of `tracker.kind`.

**Two identities — why `swebot.token` exists**: by default SweBot authenticates with
the same `repo.token` the coding agent uses to push branches and open PRs. That's fine
for Q&A and drafting, but it breaks PR review specifically: GitHub's API rejects an
`APPROVE`/`REQUEST_CHANGES` review from the same account that authored the pull
request (422 "Can not approve your own pull request"), and since Symphony's coding
agent is always the PR's author, single-identity SweBot can *never* actually approve
one — `swebot/review.rs` falls back to posting a plain comment instead so a poll cycle
doesn't 422-loop forever, but a genuine approval never lands. Set `swebot.token` to a
second GitHub credential (a separate bot account/App installation, e.g. "codebot" for
the coding agent's own `repo.token` and "swebot" for this one) to give SweBot a
distinct identity from the PR author, and approvals go through for real. `swebot.token`
follows the same `$VAR_NAME`-env-var-reference convention as `repo.token` (see
`config::RepoConfig::token_env`'s doc comment) — the value itself is never embedded in
config, only the env var's name.

**Persona and quality bar**: every SweBot prompt shares one tone/rubric prefix
(`swebot::PERSONA`) — friendly and direct, but holding a genuinely high bar
(correctness against the original ticket's acceptance criteria, security, performance,
matching the project's own conventions) rather than a rubber stamp. `request_changes`
means something genuinely fails one of those; `approve` means "I'd merge this."

**Why GitHub Discussions, not issue comments**: Discussions' built-in `Q&A`/`Ideas`
categories are a much closer semantic fit for open-ended conversation than Issues,
which are meant to be scoped, actionable work items. The trade-off: Discussions has no
REST API, only GraphQL (`GithubRepoHost::graphql`, alongside the plain-REST calls
everything else here uses) — see `src/repo_host.rs`'s own module doc comment.

**No local persistence**: which comments SweBot has already answered, and which PR
commits it has already reviewed, are both derived from hidden HTML-comment markers
embedded in SweBot's own past replies/reviews (`<!-- swebot:answered:<id> -->`,
`<!-- swebot:reviewed:<sha> -->`), scanned out of GitHub's own stored data on every
poll. Nothing to lose on a restart, and no separate database. Ticket drafting doesn't
use `claude`'s own `--resume` across poll cycles either (a human's next reply may come
hours later, well past a restart) — each poll reconstructs the full transcript from
the Discussion's own comment history and sends it fresh, which the model already needs
context for anyway.

**Isolation from the coding agent's own trust boundary**: SweBot's sessions run with
`Edit`/`Write`/`NotebookEdit` explicitly disallowed (`--disallowedTools`, on top of
the same `bypassPermissions` mode ticket dispatch uses so Bash/read tools still work
freely for exploring code and running tests during review). It answers, drafts, and
reviews — it never edits the repo directly. That stays the coding agent's job, gated
by the normal ticket-dispatch flow this whole document is otherwise about.

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

## Provider-native tracker tool (Section 10.5)

The coding agent only ever runs inside the per-issue workspace — it has no visibility
into `issues/` (or wherever the tracker actually stores state). Without a way to write
back, an issue never leaves its active state, so the orchestrator just keeps
redispatching it forever (continuation retries) once the agent thinks it's done.

`LocalTrackerAdapter` fixes this by exposing one provider-native tool,
`update_issue_state({state})`. The wiring (`claude`-backend only):

1. `TrackerAdapter` has two default (opt-in) methods, `agent_tool_specs()` and
   `execute_agent_tool()` — an adapter with nothing to expose just doesn't override
   them (Section 10.5's `agent_tool_specs()` / `execute_agent_tool()` hooks).
2. If the active adapter returns any specs, `ClaudeSession::start_session` writes a
   `--mcp-config` file into the workspace pointing back at **this same `symphony`
   binary**, run as `symphony __mcp_tool_server --tracker-kind ... --issue-id ...` (a
   hidden subcommand; see `src/mcp.rs`). `claude` spawns that as its own MCP server
   subprocess whenever the model calls the tool.
3. That subprocess rebuilds the tracker adapter from the same config and executes the
   write itself — the `claude` agent process never touches `issues/*.md` directly. This
   matches the spec's tracker-write boundary (Section 11.5): mutations happen host-side,
   through the adapter, not via raw agent file access.
4. The tool is always auto-approved (`--allowedTools mcp__symphony__*`,
   `--strict-mcp-config`) independent of `claude.permission_mode`, since it's
   host-mediated and scoped to exactly one tool.

`codex` doesn't get this wiring yet — Codex's own dynamic-tool-call mechanism
(Section 10.5) would need separate plumbing in the (already best-effort) Codex client.

**A second, independent tool source**: when `repo.pull_request: true` (see "Git repo
as first-class input" above), `open_pull_request({title, body})` from
`src/repo_host.rs` is exposed the same way, *alongside* whatever the tracker itself
exposes — `run_stdio_server` merges both tool lists and routes each `tools/call` by
name to whichever side owns it. This is deliberately not a `TrackerAdapter` tool: a
pull request is a property of `repo:` (the code host), not `tracker:` (the issue
board), so it's kept as its own capability rather than folded into the tracker's own
tool set. `repo.pull_request` also works with `tracker.kind: local` — the MCP
subprocess gets spawned whenever *either* side has a tool to offer, not just when the
tracker does.

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
