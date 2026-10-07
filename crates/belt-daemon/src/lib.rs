// Daemon: execution loop, cron engine, concurrency control.
// TODO: main loop, state machine driver.

// Driven by `Daemon`; public for integration tests, whose entry points panic
// without a database (see their `# Panics`).
#[doc(hidden)]
pub mod advancer;
pub mod cancel;
pub mod concurrency;
pub mod cron;
pub mod daemon;
mod escalation_path;
pub mod evaluation_stages;
pub mod evaluator;
pub mod executor;
pub mod hitl;
pub mod hook_cache;
pub mod notify;
pub mod post_processing;
