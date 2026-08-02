# symphony

Symphony watches an issue tracker (or a GitHub repo's issues/PRs/discussions) and runs
a coding agent (Claude Code, Codex, or opencode) against each ticket in its own
workspace — hands-off, continuously, unattended. It's a minimal Rust implementation of
the [Symphony service specification](https://github.com/openai/symphony/blob/main/SPEC.md).

For configuration reference, architecture notes, Docker mode, SweBot, and everything
else beyond this quick start, see [AGENTS.md](AGENTS.md).

## Install

```bash
cargo build --release
```

Or build the Docker image (bundles the `claude`/`opencode` CLIs too):

```bash
docker build -t symphony-base:latest .
```

Requires `bash` on `PATH` (Git for Windows, WSL, or any POSIX host) for workspace
hooks and the Codex backend. On Windows, prefer Docker mode (see AGENTS.md) if you hit
path-spelling errors like `ssh: Could not resolve hostname c`.

## Quick start: one repo, one process

```bash
./target/release/symphony ./WORKFLOW.md
```

With no argument it looks for `./WORKFLOW.md` in the current directory. This repo
ships a working example (`WORKFLOW.md` + `issues/DEMO-1.md`) using the built-in
`local` tracker, so you can run it immediately with no external credentials: edit the
`state:` field in `issues/*.md` (or add new files) to drive dispatch, and watch
`.symphony/workspaces/` get created.

Add `--port 7777` for a live status dashboard at `http://127.0.0.1:7777`.

Run it unattended, restarting on crash, as a Docker container:

```bash
symphony daemon start ./WORKFLOW.md --port 7777
```

## Quick start: the multi-repo web service

Instead of pointing one process at one local `WORKFLOW.md`, run a service that manages
any number of repos, registered through a browser:

```bash
SYMPHONY_ADMIN_TOKEN=<a-shared-secret> ./target/release/symphony serve --port 8080
```

Open `http://localhost:8080`, log in with your admin token, and register a GitHub
repo (its URL, branch, and the path to its `WORKFLOW.md`). Symphony fetches that
config straight from GitHub and starts polling it — no local checkout needed. See
"Long-running multi-repo service" in [AGENTS.md](AGENTS.md) for how tokens, auth, and
persistence work.

## Development

```bash
cargo test
cargo clippy --all-targets
```
