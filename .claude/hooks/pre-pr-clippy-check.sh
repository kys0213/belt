#!/usr/bin/env bash
# Pre-PR quality gate: cargo clippy
# Triggered by PreToolUse on Bash when gh pr create is detected

set -euo pipefail

CMD=$(cat | jq -r '.tool_input.command // empty')

# Only run on gh pr create commands
echo "$CMD" | grep -qE 'gh\s+pr\s+create' || exit 0

# cargo clippy with warnings as errors
if ! cargo clippy -- -D warnings 2>&1; then
  echo '{"continue":false,"stopReason":"cargo clippy -- -D warnings failed. Fix clippy warnings before creating PR."}'
  exit 0
fi
