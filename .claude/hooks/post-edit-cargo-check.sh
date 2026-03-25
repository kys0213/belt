#!/usr/bin/env bash
# Post-edit compilation check for .rs files
# Triggered by PostToolUse on Edit|Write

INPUT=$(cat)

# Only run for .rs files
echo "$INPUT" | grep -q '\.rs"' || exit 0

cargo check --quiet 2>&1 | head -20
exit 0
