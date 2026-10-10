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

# Optional local Codex gateway template; an existing config is never touched.
# The directory is gitignored. Remove or edit it to route Codex elsewhere.
mkdir -p .codex
if [ ! -e .codex/config.toml ]; then
  cat > .codex/config.toml <<'TOML'
model_provider = "gateway"
model = "codex,gpt-6.1-sol"
model_reasoning_effort = "low"

[model_providers.gateway]
name = "gateway"
model = "codex,gpt-6.1-sol"
base_url = "https://ai.qleap.jp/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
TOML
fi

cargo fetch --locked
