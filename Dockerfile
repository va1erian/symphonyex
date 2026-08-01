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

# --- Runtime: what every per-ticket container actually runs.
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        bash git ca-certificates curl gnupg \
    && curl -fsSL https://deb.nodesource.com/setup_20.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    # The Claude Code CLI's published package name/install method; verify against
    # https://docs.claude.com/en/docs/claude-code if this has since changed.
    && npm install -g @anthropic-ai/claude-code \
    && apt-get purge -y curl gnupg && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/symphony /usr/local/bin/symphony

# Every workspace hook and the coding agent itself run with a container bind-mount at
# this path (`container::CONTAINER_PROJECT_ROOT`) -- create it so `docker run -v
# <host>:/project -w /project` has somewhere to land even before the first mount.
RUN mkdir -p /project
WORKDIR /project
