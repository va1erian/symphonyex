# symphony

A minimal Rust implementation of the [Symphony service specification](https://github.com/openai/symphony/blob/main/SPEC.md):
a daemon that polls an issue tracker, creates a per-issue workspace, and runs a coding
agent inside it. This implementation intentionally trades completeness for something
small enough to read in one sitting and iterate on.

## Quick start

```bash
cargo build --release
./target/release/symphony ./WORKFLOW.md
```

With no argument it looks for `./WORKFLOW.md`. The repo ships a working example
(`WORKFLOW.md` + `issues/DEMO-1.md`) that uses the built-in `local` tracker, so you can
run it immediately without any external credentials: edit the `state:` field in
`issues/*.md` (or add new files) to drive dispatch, and watch `.symphony/workspaces/`
get created.

Requires `bash` on `PATH` (Git for Windows, WSL, or any POSIX host) — hooks and the
Codex backend both launch via `bash -lc`. On Windows specifically, having *both*
Git-for-Windows and WSL installed means whichever one `PATH` happens to resolve first
is what hooks actually run under, which can disagree with what the coding agent's own
Bash tool resolves to — see **Docker mode** below for the fix if you hit path-spelling
errors like `ssh: Could not resolve hostname c`.

## Live status dashboard

`symphony ./WORKFLOW.md --port 7777` additionally serves a minimal live dashboard at
`http://127.0.0.1:7777` (loopback only) — one plain server-rendered HTML page,
`<meta>`-refreshed every second, no JS, no JSON API (see `src/status.rs`). It shows
currently-running agents as cards (identifier, session id, elapsed time, turn count,
last event) and the retry queue. Useful for watching dispatch/concurrency behavior —
e.g. bump `agent.max_concurrent_agents` and you'll see multiple cards running at once.
Off by default; only starts when `--port` is passed.

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

## Coding-agent backends

Set `agent.backend: claude` (default) or `agent.backend: codex` in `WORKFLOW.md`.

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

Per-backend settings live under the `claude:` and `codex:` front-matter keys (mirroring
each other where it makes sense); `claude.*` is a spec extension, not part of the
normative schema.

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
while the coding agent's own `Bash` tool — invoked by `claude` running natively on the
host — resolves paths via Git Bash/MSYS. These disagree about how to spell a Windows
path for the same directory (`/mnt/c/...` vs `/c/...`), and there is no single spelling
that satisfies both; hit this twice in production (once via a hardcoded `git clone`
URL, once via an agent "fixing" a remote URL to match its own environment and breaking
the hooks'). Docker mode runs **both** the hooks and the `claude` process inside the
same Linux container, bind-mounted once to the whole project directory (`workflow_dir`
— covers the workspace, the tracker's `issues/`, and a mainline repo like
bsky-archiver's `app/` in one mount) at a fixed path (`/project`,
`container::CONTAINER_PROJECT_ROOT`). Inside the container there is exactly one
filesystem and one path convention, so this class of bug becomes structurally
impossible rather than just less likely.

As a side effect this also isolates concurrent agents from each other and the host:
each ticket gets its own persistent container (created alongside its workspace, torn
down alongside it), so a runaway agent is capped by `mem_limit`/`cpus` and can't touch
anything outside the bind mount.

**Building an image.** This repo's own `Dockerfile` is the base every project image
extends: `debian:bookworm-slim` + `bash`/`git`/the `claude` CLI + a Linux build of
`symphony` itself (for the MCP tool-server subcommand, `container::CONTAINER_SYMPHONY_BIN`
at `/usr/local/bin/symphony`) — built inside a multi-stage Docker build, so no
cross-compilation toolchain is needed on the host, just `docker build` itself:

```bash
docker build -t symphony-base:latest .
```

A project adds whatever toolchain its tickets need on top:

```dockerfile
FROM symphony-base:latest
RUN apt-get update && apt-get install -y --no-install-recommends <your toolchain> \
    && rm -rf /var/lib/apt/lists/*
```

**Currently Claude-backend only** (`agent.backend: claude`) — Codex's `start_session`
accepts and ignores the container parameter for now.

**Prerequisite**: Docker Desktop (or an equivalent daemon) running, `docker` on `PATH`.
Symphony checks this at startup when `docker.enabled: true` and fails fast with a clear
message rather than surfacing "docker: command not found" buried in a hook failure.

**Security posture — what this does and does not fix.** Real improvement: the agent's
`bypassPermissions`-mode shell can now only reach what's bind-mounted, not the whole
host filesystem, plus resource limits and process isolation that don't exist at all
today. What stays exactly as risky: `bypassPermissions` itself is unchanged (the agent
still has an unrestricted shell, just a smaller reachable filesystem); the bind mount
is full read-write across the *whole* project, so a bad agent on one ticket can still
corrupt another ticket's workspace or the mainline repo; `WORKFLOW.md` hooks remain a
trusted-script trust boundary, and running Docker at all typically requires
host-privileged access anyway; there's no image signing/scanning, and this is stock
`docker run` (namespaces + cgroups), not a hardened runtime (gVisor/Firecracker). This
is a real hardening step for a single trusted operator running their own
`WORKFLOW.md` locally — **not** sufficient for multi-tenant or untrusted-input
production use.

## Daemonizing Symphony

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
  dashboard (`--port <PORT>`, see below), but it's a debug/dev view, not a stable API.
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

## Development

```bash
cargo test      # unit tests across every module
cargo clippy --all-targets
```
