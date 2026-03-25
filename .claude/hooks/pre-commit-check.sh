#!/usr/bin/env bash
# Pre-commit quality gate: cargo fmt + clippy
# Triggered by PreToolUse on Bash when git commit is detected

set -euo pipefail

CMD=$(cat | jq -r '.tool_input.command // empty')

# Only run on git commit commands
echo "$CMD" | grep -qE 'git\s+commit' || exit 0

# Block direct commits on main/master
branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)
if [ "$branch" = "main" ] || [ "$branch" = "master" ]; then
  echo '{"continue":false,"stopReason":"Direct commit on main/master blocked"}'
  exit 0
fi

# cargo fmt check
if ! cargo fmt --check >/dev/null 2>&1; then
  echo '{"continue":false,"stopReason":"cargo fmt --check failed. Run cargo fmt first."}'
  exit 0
fi

# cargo clippy check
if ! cargo clippy -- -D warnings >/dev/null 2>&1; then
  echo '{"continue":false,"stopReason":"cargo clippy -- -D warnings failed. Fix clippy warnings first."}'
  exit 0
fi
