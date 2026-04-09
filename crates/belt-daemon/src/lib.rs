// Daemon: execution loop, cron engine, concurrency control.
// TODO: main loop, state machine driver.

pub mod advancer;
pub mod concurrency;
pub mod cron;
pub mod daemon;
pub mod evaluator;
pub mod executor;
