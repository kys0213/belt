// Daemon: execution loop, cron engine, concurrency control.
// TODO: main loop, state machine driver.

pub mod advancer;
pub mod concurrency;
pub mod cron;
pub mod daemon;
pub mod evaluation_stages;
pub mod evaluator;
pub mod executor;
pub mod hitl_service;
pub mod hook_cache;
