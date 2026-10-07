//! 사람 대상 알림 channel 계약과 `notifications` 설정 타입.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};

/// channel로 내보낼 수 있는 이벤트. `hitl_resolved`는 내부 이벤트라 선택할 수 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelEvent {
    Started,
    Done,
    Failed,
    Skipped,
    HitlRequested,
}

/// 응답 허용 설정. `allow`가 비어 있으면 외부 응답을 받지 않는다(발송 전용).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RespondConfig {
    #[serde(default)]
    pub allow: Vec<String>,
}

impl RespondConfig {
    /// 외부 응답을 받는 설정인지.
    pub fn accepts_responses(&self) -> bool {
        !self.allow.is_empty()
    }
}

/// 출처 시스템(origin channel) 설정.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OriginConfig {
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_origin_events")]
    pub events: Vec<ChannelEvent>,
    #[serde(default)]
    pub respond: RespondConfig,
}

fn default_enabled() -> bool {
    true
}

fn default_origin_events() -> Vec<ChannelEvent> {
    vec![
        ChannelEvent::Started,
        ChannelEvent::Failed,
        ChannelEvent::HitlRequested,
    ]
}

impl Default for OriginConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            events: default_origin_events(),
            respond: RespondConfig::default(),
        }
    }
}

/// origin 외 추가 channel 항목. 코어는 `config`를 해석하지 않는다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub channel_type: String,
    pub events: Vec<ChannelEvent>,
    #[serde(default)]
    pub respond: RespondConfig,
    #[serde(default = "empty_config")]
    pub config: serde_json::Value,
}

fn empty_config() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// workspace.yaml의 `notifications` 절. 생략하면 origin 기본값만 적용된다.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsConfig {
    #[serde(default)]
    pub origin: OriginConfig,
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
}

impl NotificationsConfig {
    /// 지원하지 않는 channel type과 중복 이름을 거부한다. 무시하거나 대체하지 않는다.
    ///
    /// `supported`는 구현이 제공하는 channel type 목록이다.
    pub fn validate_channels(&self, supported: &[&str]) -> anyhow::Result<()> {
        let mut names = HashSet::new();
        for channel in &self.channels {
            if !supported.contains(&channel.channel_type.as_str()) {
                anyhow::bail!(
                    "notifications.channels[{}].type `{}` is not supported (allowed: {})",
                    channel.name,
                    channel.channel_type,
                    if supported.is_empty() {
                        "none; leave `channels` empty".to_string()
                    } else {
                        supported.join(", ")
                    }
                );
            }
            if !names.insert(channel.name.as_str()) {
                anyhow::bail!(
                    "notifications.channels name `{}` is duplicated (names must be unique within a workspace)",
                    channel.name
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_notifications_use_spec_defaults() {
        let cfg: NotificationsConfig = serde_yaml::from_str("{}").unwrap();
        assert!(cfg.origin.enabled);
        assert_eq!(
            cfg.origin.events,
            vec![
                ChannelEvent::Started,
                ChannelEvent::Failed,
                ChannelEvent::HitlRequested
            ]
        );
        assert!(cfg.origin.respond.allow.is_empty());
        assert!(!cfg.origin.respond.accepts_responses());
        assert!(cfg.channels.is_empty());
        assert_eq!(cfg, NotificationsConfig::default());
    }

    #[test]
    fn parses_full_config() {
        let yaml = r#"
origin:
  enabled: false
  events: [done, skipped]
  respond:
    allow: ["octocat"]
channels:
  - name: team-chat
    type: chat
    events: [hitl_requested]
    config:
      room: ops
"#;
        let cfg: NotificationsConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(!cfg.origin.enabled);
        assert_eq!(
            cfg.origin.events,
            vec![ChannelEvent::Done, ChannelEvent::Skipped]
        );
        assert!(cfg.origin.respond.accepts_responses());
        assert_eq!(cfg.channels.len(), 1);
        assert_eq!(cfg.channels[0].channel_type, "chat");
        assert!(cfg.channels[0].respond.allow.is_empty());
        assert_eq!(cfg.channels[0].config["room"], "ops");
    }

    #[test]
    fn channel_config_defaults_to_empty_object() {
        let yaml = "channels:\n  - name: a\n    type: chat\n    events: [done]\n";
        let cfg: NotificationsConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.channels[0].config, empty_config());
    }

    #[test]
    fn rejects_internal_event() {
        let yaml = "origin:\n  events: [hitl_resolved]\n";
        assert!(serde_yaml::from_str::<NotificationsConfig>(yaml).is_err());
    }

    #[test]
    fn channel_requires_events() {
        let yaml = "channels:\n  - name: a\n    type: chat\n";
        assert!(serde_yaml::from_str::<NotificationsConfig>(yaml).is_err());
    }

    #[test]
    fn rejects_unknown_field() {
        assert!(serde_yaml::from_str::<NotificationsConfig>("origin:\n  enable: true\n").is_err());
    }

    fn with_channels(yaml: &str) -> NotificationsConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    #[test]
    fn validate_channels_rejects_unsupported_type() {
        let cfg = with_channels("channels:\n  - name: a\n    type: pager\n    events: [done]\n");
        let err = cfg.validate_channels(&["chat"]).unwrap_err().to_string();
        assert!(err.contains("pager"), "{err}");
        assert!(err.contains("chat"), "{err}");
    }

    #[test]
    fn validate_channels_rejects_any_channel_when_none_supported() {
        let cfg = with_channels("channels:\n  - name: a\n    type: chat\n    events: [done]\n");
        assert!(cfg.validate_channels(&[]).is_err());
    }

    #[test]
    fn validate_channels_rejects_duplicate_names() {
        let cfg = with_channels(
            "channels:\n  - name: a\n    type: chat\n    events: [done]\n  - name: a\n    type: chat\n    events: [done]\n",
        );
        let err = cfg.validate_channels(&["chat"]).unwrap_err().to_string();
        assert!(err.contains("duplicated"), "{err}");
    }

    #[test]
    fn validate_channels_accepts_supported_and_empty() {
        let cfg = with_channels("channels:\n  - name: a\n    type: chat\n    events: [done]\n");
        assert!(cfg.validate_channels(&["chat"]).is_ok());
        assert!(
            NotificationsConfig::default()
                .validate_channels(&[])
                .is_ok()
        );
    }
}
