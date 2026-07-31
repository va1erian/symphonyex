---
tracker:
  kind: local
  provider:
    dir: ./issues
  active_states: [todo, in progress]
  terminal_states: [done, cancelled]
  required_labels: []

polling:
  interval_ms: 10000

workspace:
  root: ./.symphony/workspaces

hooks:
  timeout_ms: 60000

agent:
  backend: claude
  max_concurrent_agents: 2
  max_turns: 8
  max_retry_backoff_ms: 300000

claude:
  command: claude
  permission_mode: bypassPermissions
  turn_timeout_ms: 3600000
  stall_timeout_ms: 300000

codex:
  command: codex app-server
  turn_timeout_ms: 3600000
  read_timeout_ms: 5000
  stall_timeout_ms: 300000
---
You are working on the following issue from the team's tracker.

Issue: {{ issue.identifier }} - {{ issue.title }}
State: {{ issue.state }}
Priority: {{ issue.priority | default: "none" }}

Description:
{{ issue.description | default: "(no description provided)" }}

Labels: {% for label in issue.labels %}{{ label }} {% endfor %}

Instructions:
1. Work only inside this workspace directory.
2. Make the smallest change that correctly resolves the issue.
3. When you believe the issue is fully resolved, call the `update_issue_state` tool
   with `state: "done"`. That is the only supported way to advance this issue — the
   tracker's storage is not directly reachable from this workspace, so do not try to
   edit tracker files yourself.
