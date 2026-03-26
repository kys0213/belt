#!/usr/bin/env bash
# Branch freshness verification
# Triggered by PreToolUse on Bash when git push is detected
# Ensures the branch is up-to-date with its upstream before pushing

set -euo pipefail

CMD=$(cat | jq -r '.tool_input.command // empty')

# Only run on git push commands
echo "$CMD" | grep -qE 'git\s+push' || exit 0

# Fetch latest from origin
git fetch origin --quiet 2>/dev/null || true

# Get current branch
branch=$(git rev-parse --abbrev-ref HEAD 2>/dev/null)

# Check if upstream exists for this branch
upstream=$(git rev-parse --abbrev-ref "@{upstream}" 2>/dev/null) || exit 0

# Check if local is behind upstream
behind=$(git rev-list --count HEAD.."$upstream" 2>/dev/null || echo "0")

if [ "$behind" -gt 0 ]; then
  echo "{\"continue\":false,\"stopReason\":\"Branch '$branch' is $behind commit(s) behind '$upstream'. Pull/rebase before pushing.\"}"
  exit 0
fi
