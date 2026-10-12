#!/bin/sh
set -eu

# Named volumes are created root-owned; hand over only the cache mount points.
uid=$(id -u)
gid=$(id -g)
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
mkdir -p "$cargo_home" target
sudo chown "$uid:$gid" "$cargo_home"
sudo chown -R "$uid:$gid" target "$cargo_home/registry" "$cargo_home/git"

# Silence direnv output (direnv 2.36+ ignores DIRENV_LOG_FORMAT without a config).
# See: https://github.com/direnv/direnv/issues/1418
if command -v direnv >/dev/null 2>&1 && [ ! -e "$HOME/.config/direnv/direnv.toml" ]; then
  mkdir -p "$HOME/.config/direnv"
  cat > "$HOME/.config/direnv/direnv.toml" <<'TOML'
[global]
log_format = ""
hide_env_diff = true
TOML
fi

# Project-level Codex overrides only (model and effort); an existing config is
# never touched. Providers, auth, history and project trust live in the shared
# $CODEX_HOME (/home/vscode/.codex), not here. The directory is gitignored.
mkdir -p .codex
if [ ! -e .codex/config.toml ]; then
  cat > .codex/config.toml <<'TOML'
model = "gpt-6.1-sol"
model_reasoning_effort = "xhigh"
TOML
fi

cargo fetch --locked
