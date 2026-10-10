#!/bin/sh
set -eu

if [ -f .envrc ] && command -v direnv >/dev/null 2>&1; then
  direnv allow
fi
