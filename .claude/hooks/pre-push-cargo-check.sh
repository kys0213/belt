#!/usr/bin/env bash
# Pre-push quality gate: cargo check
# Triggered by PreToolUse on Bash when git push is detected

set -euo pipefail

CMD=$(cat | jq -r '.tool_input.command // empty')

# Only run on git push commands
echo "$CMD" | grep -qE 'git\s+push' || exit 0

# cargo check (matches CI: warnings as errors, all targets)
if ! RUSTFLAGS="-D warnings" cargo check --all-targets 2>&1 | tail -5; then
  echo '{"continue":false,"stopReason":"cargo check --all-targets failed (warnings as errors). Fix before pushing."}'
  exit 0
fi
