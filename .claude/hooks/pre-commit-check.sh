#!/usr/bin/env bash
# Pre-commit quality gate: fmt + check (fast)
# Triggered by PreToolUse on Bash when git commit is detected
# Speed: cargo check only — clippy + test는 pre-pr hook에서 실행 (pre-pr-clippy-check.sh)

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

# Flags mirror ci.yml
if ! cargo fmt --all -- --check >/dev/null 2>&1; then
  echo '{"continue":false,"stopReason":"cargo fmt --all -- --check failed. Run cargo fmt first."}'
  exit 0
fi

if ! RUSTFLAGS="-D warnings" cargo check --all-targets >/dev/null 2>&1; then
  echo '{"continue":false,"stopReason":"cargo check --all-targets failed. Fix compilation errors first."}'
  exit 0
fi
