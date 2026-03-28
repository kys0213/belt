#!/usr/bin/env bash
# PreToolUse hook: validates PR title follows conventional commit format
# Triggers on: Bash tool calls containing "gh pr create"

set -euo pipefail

TOOL_INPUT="${TOOL_INPUT:-}"

# Only trigger on gh pr create commands
if ! echo "$TOOL_INPUT" | grep -q "gh pr create"; then
  exit 0
fi

# Extract --title value
TITLE=$(echo "$TOOL_INPUT" | grep -oP '(?<=--title\s")[^"]+' || echo "$TOOL_INPUT" | grep -oP "(?<=--title\s')[^']+" || true)

if [ -z "$TITLE" ]; then
  exit 0
fi

# Conventional commit pattern: type(scope): description  or  type: description
PATTERN='^(feat|fix|docs|refactor|test|ci|chore|perf|build|style|revert)(\([a-zA-Z0-9_-]+\))?(!)?: .+'

if ! echo "$TITLE" | grep -qP "$PATTERN"; then
  echo "BLOCKED"
  echo "PR title does not follow conventional commit format."
  echo ""
  echo "  Expected: <type>(<scope>): <description>"
  echo "  Got:      $TITLE"
  echo ""
  echo "  Valid types: feat, fix, docs, refactor, test, ci, chore, perf, build, style, revert"
  echo "  Example:  feat(cli): add belt bootstrap --llm flag"
  exit 2
fi

exit 0
