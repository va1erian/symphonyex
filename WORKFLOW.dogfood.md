---
tracker:
  kind: local
  provider:
    dir: ./issues
  active_states: [todo, in progress]
  terminal_states: [done, cancelled]
  required_labels: [phase-1]

polling:
  interval_ms: 5000

workspace:
  root: ./.symphony/dogfood-workspaces

hooks:
  timeout_ms: 120000

repo:
  url: /c/Users/hadri/dogfood-mirrors/symphony.git
  default_branch: claude/symphony-ai-roadmap-features-b9b80d

pipeline:
  enabled: true
  blocked_state: blocked
  stages:
    - id: implement
      role: developer
      max_turns: 30
    - id: verify
      role: reviewer
      max_turns: 8
      on_failure: skip

agent:
  backend: claude
  max_concurrent_agents: 2
  max_turns: 30
  max_retry_backoff_ms: 300000

claude:
  command: claude
  model: claude-sonnet-5
  permission_mode: bypassPermissions
  turn_timeout_ms: 3600000
  stall_timeout_ms: 600000
---
You are working on the following ticket from the team's tracker, inside a real clone
of the Symphony Rust codebase.

Issue: {{ issue.identifier }} - {{ issue.title }}
State: {{ issue.state }}
Priority: {{ issue.priority | default: "none" }}

Description:
{{ issue.description | default: "(no description provided)" }}

Labels: {% for label in issue.labels %}{{ label }} {% endfor %}

Instructions:
1. Work only inside this workspace directory. It is a full clone of the repository on
   its own branch -- read neighboring modules first to match existing conventions
   before adding anything new.
2. The ticket description above (including its "Acceptance criteria" and "Global
   constraints" sections) is the actual scope and quality bar. Implement it as a real,
   working change: add or modify Rust source under `src/`, and add unit tests that
   cover the acceptance criteria.
3. Before finishing, run `cargo test` and `cargo clippy --all-targets` yourself and fix
   anything they report. Do not declare the ticket done with a red test suite or
   clippy warnings.
4. This ticket runs across up to two sequential work phases within this same
   conversation. If you're continuing a conversation where you already completed and
   verified the implementation, use this phase only to double-check tests/clippy are
   clean and there's nothing left unfinished -- do not redo work you already did.
5. Do not push, open a pull request, or touch git branches/remotes yourself --
   committing and pushing this workspace's changes is handled automatically outside
   your turn.
6. When you believe the ticket is fully resolved (implemented, tested, clippy-clean),
   call the `update_issue_state` tool with `state: "done"`. That is the only supported
   way to advance this issue -- the tracker's storage is not directly reachable from
   this workspace, so do not try to edit tracker files yourself.
