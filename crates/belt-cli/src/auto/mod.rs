//! `/auto` slash command plugin for Claude Code integration.
//!
//! Provides installation and management of the `/auto` slash command that maps
//! daemon control commands (`start`, `stop`, `restart`, `status`) to their
//! corresponding `belt` CLI invocations within a Claude Code session.

pub mod plugin;
