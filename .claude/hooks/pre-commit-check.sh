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

# cargo fmt check (matches CI: cargo fmt --all -- --check)
if ! cargo fmt --all -- --check >/dev/null 2>&1; then
  echo '{"continue":false,"stopReason":"cargo fmt --all -- --check failed. Run cargo fmt first."}'
  exit 0
fi

# cargo clippy check (matches CI: --all-targets --all-features, RUSTFLAGS="-D warnings")
if ! RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features 2>&1 | tail -5; then
  echo '{"continue":false,"stopReason":"cargo clippy --all-targets --all-features failed (warnings as errors). Fix clippy warnings first."}'
  exit 0
fi
