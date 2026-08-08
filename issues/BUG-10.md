---
identifier: BUG-10
title: "record_artifact's kind/path arguments allow arbitrary file write/read, bypassing ToolPolicy's edit restrictions"
state: todo
priority: 1
labels: [bug, security, mcp, ai-roadmap]
dispatchable: true
---
## Context

Surfaced live and independently verified during a real dogfood run: Symphony's own
Security Agent stage (AIR-8), reviewing an unrelated diff (BUG-5's fix), traced what its
new code called into and found a real, pre-existing vulnerability in the `record_artifact`
MCP tool (`src/artifacts.rs`) that the diff under review made more consequential to
exploit. Independently confirmed by reading the current code directly (not just trusting
the agent's report):

**S1 — arbitrary file write via `kind` (CWE-22, high).** `record` (`src/artifacts.rs`,
~line 180) builds the on-disk artifact path directly from the caller-supplied `kind`
argument, unsanitized:

```rust
let id = format!("{}-{}", new.kind, &hash[..16]);   // line 193
...
let disk_path = cycle_dir.join(format!("{id}.{ext}"));   // line 201
```

`kind` comes straight from the `record_artifact` MCP tool's arguments
(`execute_tool`, ~line 300: `let Some(kind) = arguments.get("kind").and_then(|v|
v.as_str())`) — a value the *coding agent itself* controls on every call. A `kind`
containing `../` sequences is never rejected or normalized before being folded into
`disk_path` via `PathBuf::join`, which does not sanitize `..` components in a relative
segment — the resulting write can land outside `cycle_dir` entirely, at a location and
with content (whatever `content`/`path` the same call supplies) chosen by the agent.

**S2 — arbitrary file read via `path` (CWE-22, medium).** The same tool's read path
(`execute_tool`, ~line 322) has the identical problem in the other direction:

```rust
let full = workspace_dir.join(path);   // path: &str from the same MCP call's arguments
```

with no traversal check, so a crafted `path` can read a file outside the workspace and
have its content persisted into a queryable, later-readable artifact (`/artifacts`,
`/evidence`) — an exfiltration primitive, not just a read.

**Why this defeats an access-control boundary the codebase clearly cares about.** Every
MCP tool (`mcp__symphony__*`, including `record_artifact`) is *always* passed via
`--allowedTools` independent of `ToolPolicy`/`permission_mode` — confirmed in
`src/agent/claude.rs`'s own doc comment: *"The MCP tool names are always auto-approved
... since they are host-mediated and adapter-scoped, not raw command/file access."* That
design assumes each MCP tool's host-side implementation enforces its own scoping.
`record_artifact` doesn't. Concretely: the built-in Reviewer role is deliberately
constructed with `ToolPolicy::SWEBOT` (`allow_edits: false`) specifically so "a Reviewer
that can edit files is not a reviewer" (AIR-7's own stated rationale, `roles/mod.rs`) —
but that same Reviewer session can still reach `record_artifact` and, via S1, write
arbitrary file content anywhere the process can write, with no `Edit`/`Write` tool
involved at all. The restriction is real at the native-tool layer and silently absent at
the MCP-tool layer for this one tool.

## Scope

Sanitize both `kind` (write path) and `path` (read path) in `record_artifact`'s
implementation before they ever reach a filesystem path:

- **`kind`**: validate against an allowlist pattern (e.g. `^[a-zA-Z0-9_-]+$`) before it's
  used to build `id`/`disk_path` — reject (return `ToolResult::error`, don't silently
  truncate/rewrite) anything containing a path separator or `..`. `KNOWN_KINDS` already
  exists as a recognized-kinds list; an unrecognized-but-safe kind is explicitly allowed
  today (flagged, not rejected) — that behavior should be preserved for genuinely novel
  but well-formed kinds, only the character-set/traversal check is new.
- **`path`**: resolve it against `workspace_dir`, canonicalize, and verify the result is
  still within `workspace_dir` before reading — the same "resolve and contain" pattern
  this codebase should already be following for any other user-influenced path (check
  whether `crate::workspace` or elsewhere already has a reusable helper for this before
  writing a new one).
- Add regression tests: a `kind` containing `../../../evil` is rejected, not written
  outside `cycle_dir`; a `path` containing `../../../../etc/hosts`-style traversal (or
  the Windows equivalent used in this codebase's own test conventions) is rejected, not
  read from outside `workspace_dir`; the existing legitimate-kind/legitimate-path cases
  continue to work unchanged.

## Acceptance criteria

- [ ] A `record_artifact` call with a path-traversal `kind` is rejected with a clear
      error, and no file is written outside the intended `cycle_dir`.
- [ ] A `record_artifact` call with a path-traversal `path` argument is rejected with a
      clear error, and no file outside `workspace_dir` is read.
- [ ] A `ToolPolicy`-restricted role (e.g. Reviewer, `allow_edits: false`) still cannot
      achieve an arbitrary file write via `record_artifact` after this fix — add a test
      that specifically exercises this from a restricted-role context if one doesn't
      already exist, since that's the actual access-control property being restored.
- [ ] Legitimate existing usage (a normal `kind` like `"plan"`/`"review_findings"`, a
      normal in-workspace `path`) is unaffected.
- [ ] `cargo test` and `cargo clippy --all-targets` stay clean.

## Out of scope

- Any change to *which* MCP tools are always-allowed independent of `ToolPolicy` — that
  design (host-mediated, adapter-scoped tools bypass native-tool restrictions) is sound
  in general; this ticket fixes the one tool that doesn't actually enforce its own
  scoping, not the overall posture.
- S3 (noted by the security stage but explicitly not a new issue): `auto_approve_when`
  trusting an LLM-self-reported `risk` field is an accepted AIR-5 tradeoff, unrelated to
  the path-traversal issues here.
