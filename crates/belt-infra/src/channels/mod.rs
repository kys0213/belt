//! `NotificationChannel` implementations.

pub mod github;

pub use github::{GitHubChannelConfig, GitHubOriginChannel};

/// `notifications.channels[].type` values this crate can build. The origin
/// channel is configured through `notifications.origin`, not as a channel type,
/// so the list is empty.
pub const SUPPORTED_CHANNEL_TYPES: &[&str] = &[];
