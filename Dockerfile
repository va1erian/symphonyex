# Symphony base image (Docker mode -- see README.md "Docker mode").
#
# Every per-ticket container a project runs is either this image directly, or a
# project-specific image `FROM` this one that layers on whatever language toolchain
# its tickets need (e.g. bsky-archiver adds the Rust toolchain on top, mirroring what
# that project's own AR-12 ticket built for the *application's* Docker image -- same
# pattern, different purpose). Keep this base minimal and generic; toolchain decisions
# belong with the project, not the orchestrator.

# --- Builder: compiles the Linux `symphony` binary used for the in-container MCP
# tool-server subcommand (see src/container.rs's CONTAINER_SYMPHONY_BIN). Building
# inside Docker always produces a Linux binary regardless of the host OS running
# `docker build` -- no cross-compilation toolchain (musl target, `cross`, etc.) needed
# on the host.
FROM rust:1-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release

# --- Runtime: what every per-ticket container actually runs, and (for images used
# with `symphony daemon start`, see README.md "Daemonizing Symphony") what the daemon
# container itself runs too.
FROM debian:bookworm-slim
ARG DOCKER_CLI_VERSION=27.3.1
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash git ca-certificates curl gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    # The Claude Code CLI's published package name/install method; verify against
    # https://docs.claude.com/en/docs/claude-code if this has since changed.
    && npm install -g @anthropic-ai/claude-code \
    # opencode CLI (agent.backend: opencode) -- package name per
    # https://opencode.ai/docs/ ; verify if this has since changed.
    && npm install -g opencode-ai \
    # docker CLI (client only, no engine/daemon) -- lets a daemonized Symphony spawn
    # per-ticket sibling containers through a mounted host Docker socket
    # (Docker-outside-of-Docker). Static official binary, not an apt package: avoids
    # adding Docker's own apt repo/GPG key just for the client. Harmless to include in
    # every image even when not used for `symphony daemon start`.
    && curl -fsSL "https://download.docker.com/linux/static/stable/x86_64/docker-${DOCKER_CLI_VERSION}.tgz" \
       | tar -xz --strip-components=1 -C /usr/local/bin docker/docker \
    && apt-get purge -y curl gnupg && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/symphony /usr/local/bin/symphony

# `claude` refuses `--dangerously-skip-permissions`/bypassPermissions when running as
# root ("cannot be used with root/sudo privileges for security reasons") -- found by
# actually running this against a real project, not something caught by any earlier
# testing. A fixed uid/gid (1000:1000, the conventional "first real user" on Debian)
# rather than a name: `workspace.docker.user: "1000:1000"` in a project's WORKFLOW.md
# passes this straight through to `docker run --user`, and a project Dockerfile that
# layers on a toolchain (rustup, etc., typically installed as root during `docker
# build`) just needs to make sure whatever paths that toolchain writes to at runtime
# (e.g. `$CARGO_HOME`) are readable/writable by this same uid -- see this repo's own
# bsky-archiver-agent Dockerfile for the pattern.
RUN groupadd -g 1000 agent && useradd -u 1000 -g 1000 -m -s /bin/bash agent
ENV HOME=/home/agent

# Git's "dubious ownership" protection refuses to operate on a mounted repo it
# doesn't own unless `safe.directory` allow-lists it. The synthesized `repo:` hooks
# (config.rs) also set this via `git config --global`, but that lands in
# /home/agent/.gitconfig -- part of the *container's* own filesystem layer, not the
# persistent project volume/mount. A container that gets recreated (image rebuild,
# `docker rm`, a host reboot) loses that global config, yet the workspace's own
# `.symphony-initialized` marker (on the persistent volume/mount) then skips
# `after_create` forever, so it never gets re-applied -- a real recurring failure
# mode, not a hypothetical one. A system-level, wildcarded allow-list baked into the
# image itself isn't subject to either problem: every container from this image is
# already single-project/single-tenant, so trusting any mounted repo is safe here.
RUN git config --system --add safe.directory '*'

# Placeholder for `workspace.docker.mount_claude_credentials` (see config.rs's doc
# comment): pre-create this as an actual file, owned by the agent user, so a `docker
# run -v <host-credentials-file>:/home/agent/.claude/.credentials.json:ro` bind-mounts
# a file onto a file. Docker infers the mount target's type from this path when it
# doesn't already exist, which is unreliable across Docker versions -- pre-creating it
# removes the ambiguity outright.
RUN mkdir -p /home/agent/.claude && touch /home/agent/.claude/.credentials.json \
    && chown -R 1000:1000 /home/agent/.claude

# `opencode`'s own global provider config (see README.md "Coding-agent backends"):
# baked in once here rather than left to each container's `/connect` TUI flow, which
# isn't usable in a headless run anyway. Declares Fireworks AI as an OpenAI-compatible
# provider whose API key is read from the container's own `FIREWORKS_API_KEY` env var
# at request time (`{env:...}` syntax) -- never a literal secret in this image or in
# WORKFLOW.md. `FIREWORKS_API_KEY` itself still has to reach the container at runtime:
# reference it as `opencode.api_key: $FIREWORKS_API_KEY` in a project's WORKFLOW.md so
# `envsub::collect_var_refs` forwards it via `docker run -e` (see config.rs). The one
# model listed below is just what `opencode.model: fireworks/<model-id>` defaults
# to if unset in WORKFLOW.md -- any other Fireworks model id works too, listed or not,
# since `models` only affects the interactive picker, not what the API itself accepts.
RUN mkdir -p /home/agent/.config/opencode && echo '{"$schema":"https://opencode.ai/config.json","provider":{"fireworks":{"npm":"@ai-sdk/openai-compatible","name":"Fireworks AI","options":{"baseURL":"https://api.fireworks.ai/inference/v1","apiKey":"{env:FIREWORKS_API_KEY}"},"models":{"accounts/fireworks/models/kimi-k2p7-code":{"name":"Kimi K2.7 Code"}}}}}' > /home/agent/.config/opencode/opencode.json \
    && chown -R 1000:1000 /home/agent/.config

# Every workspace hook and the coding agent itself run with a container bind-mount at
# this path (`container::CONTAINER_PROJECT_ROOT`) -- create it so `docker run -v
# <host>:/project -w /project` has somewhere to land even before the first mount.
RUN mkdir -p /project && chown 1000:1000 /project
WORKDIR /project
