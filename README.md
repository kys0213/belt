# Belt

Conveyor belt for autonomous development. Issues flow in one direction through a pipeline — if something is missing, a new item is created and placed back on the belt.

## Overview

Belt is a daemon that watches external systems (GitHub, Jira, ...), picks up work items, runs LLM agents and scripts in isolated git worktrees, and manages the results through an 8-phase state machine.

```
DataSource.collect() → Pending → Ready → Running → Completed → Done
                                                              → HITL
                                                              → Failed
                                                              → Skipped
```

### Key Concepts

- **Workspace** = 1 repo. Each workspace defines its own workflow via YAML.
- **Daemon** = state machine + executor. Knows nothing about GitHub labels or PR conventions — just runs what YAML defines.
- **DataSource** = external system abstraction (collect + context). New system = new impl, zero core changes.
- **AgentRuntime** = LLM abstraction (Claude, Gemini, Codex). New LLM = new impl, zero core changes.
- **Cron Engine** = infrastructure maintenance + quality loops (evaluate, gap-detection).

## Architecture

```
crates/
  belt-core/     Core traits, state machine, queue logic (zero external deps)
  belt-infra/    DataSource/AgentRuntime impls, SQLite, worktree management
  belt-daemon/   Execution loop, cron engine, concurrency control
  belt-cli/      CLI entry point (clap-based command tree)
```

### Dependency Direction

```
belt-cli → belt-daemon → belt-infra → belt-core
                                         ↑
                              (traits only, no infra deps)
```

## Build

```bash
cargo build --release
# Binary: target/release/belt
```

## Usage

```bash
# Register a workspace
belt workspace add --config workspace.yaml

# Start the daemon
belt start

# Check status
belt status --format rich

# Query item context (used by scripts)
belt context $WORK_ID --json

# Queue operations
belt queue list --phase completed
belt queue done $WORK_ID
belt queue hitl $WORK_ID --reason "needs human review"
```

## Spec

Design documents are in [`spec/`](./spec/):

- [`DESIGN-v5.md`](./spec/DESIGN-v5.md) — design philosophy + architecture overview
- [`concerns/`](./spec/concerns/) — implementation-level specs (state machine, daemon, datasource, runtime, claw, cron, CLI)
- [`flows/`](./spec/flows/) — user-facing scenarios (onboarding, spec lifecycle, issue pipeline, failure/HITL, monitoring)

## License

Apache-2.0
