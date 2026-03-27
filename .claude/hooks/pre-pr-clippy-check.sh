#!/usr/bin/env bash
# Pre-PR quality gate: cargo clippy
# Triggered by PreToolUse on Bash when gh pr create is detected

set -euo pipefail

CMD=$(cat | jq -r '.tool_input.command // empty')

# Only run on gh pr create commands
echo "$CMD" | grep -qE 'gh\s+pr\s+create' || exit 0

# cargo fmt check (matches CI)
if ! cargo fmt --all -- --check >/dev/null 2>&1; then
  echo '{"continue":false,"stopReason":"cargo fmt --all -- --check failed. Run cargo fmt before creating PR."}'
  exit 0
fi

# cargo clippy (matches CI: --all-targets --all-features, RUSTFLAGS="-D warnings")
if ! RUSTFLAGS="-D warnings" cargo clippy --all-targets --all-features 2>&1; then
  echo '{"continue":false,"stopReason":"cargo clippy --all-targets --all-features failed. Fix clippy warnings before creating PR."}'
  exit 0
fi

# cargo test (matches CI: --workspace)
if ! cargo test --workspace 2>&1; then
  echo '{"continue":false,"stopReason":"cargo test --workspace failed. Fix tests before creating PR."}'
  exit 0
fi
