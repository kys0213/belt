#!/usr/bin/env bash
# Block .env file modifications
# Triggered by PreToolUse on Write|Edit

INPUT=$(cat)

echo "$INPUT" | grep -q '\.env"' && {
  echo '{"continue":false,"stopReason":".env file modification blocked"}'
  exit 0
}

exit 0
