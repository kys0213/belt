//! 사람 대상 알림 channel 계약과 `notifications` 설정 타입.

use std::collections::HashSet;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::hitl::{HitlAction, HitlId};

/// 출처 시스템 channel의 이름. 추가 channel은 이 이름을 쓸 수 없다.
pub const ORIGIN_CHANNEL: &str = "origin";

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
            if channel.name == ORIGIN_CHANNEL {
                anyhow::bail!(
                    "notifications.channels name `{ORIGIN_CHANNEL}` is reserved for the origin channel"
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

/// 이벤트를 받을 channel 이름 목록. origin이 먼저, 그 뒤 `channels` 설정 순서다.
///
/// 순수 정책이다. channel 구현이 실제로 있는지는 호출자가 확인한다(origin 구현이
/// 없으면 dashboard only). 회신(확인 요청, `already_handled`)은 이벤트가 아니므로
/// 이 함수를 거치지 않고 응답을 보낸 channel로 간다.
pub fn route(event: ChannelEvent, config: &NotificationsConfig) -> Vec<String> {
    let origin = (config.origin.enabled && config.origin.events.contains(&event))
        .then(|| ORIGIN_CHANNEL.to_string());
    let extra = config
        .channels
        .iter()
        .filter(|channel| channel.events.contains(&event))
        .map(|channel| channel.name.clone());
    origin.into_iter().chain(extra).collect()
}

/// channel이 발송한 메시지의 외부 참조(예: GitHub 코멘트 id). 불투명 문자열이다.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageRef(pub String);

/// 발송 메시지의 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    /// 이벤트 알림. HITL 요청이면 `hitl_id`가 있다.
    Event(ChannelEvent),
    /// 응답에 대한 회신(확인 요청, `already_handled` 등). 이벤트 필터와 무관하다.
    Reply,
}

/// channel에 보내는 메시지. 본문 문구는 호출자가 정하고, channel은 전달 형식만 맡는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundMessage {
    pub kind: MessageKind,
    /// 대상 아이템. origin channel은 이것으로 출처 이슈를 찾는다.
    pub work_id: String,
    /// 있으면 channel은 응답 상관관계용 `hitl_id` 토큰을 메시지에 심는다.
    pub hitl_id: Option<HitlId>,
    pub text: String,
}

/// 응답이 어느 HITL 요청에 대한 것인지 가리키는 단서.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitlRef {
    /// 메시지에 심은 `hitl_id` 토큰.
    Token(HitlId),
    /// 발송한 메시지에 대한 답글.
    ReplyTo(MessageRef),
}

/// 정규화된 응답 본문.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundBody {
    /// 명시 액션.
    Action(HitlAction),
    /// 자연어. LLM 제안과 응답자 확인을 거쳐야 확정된다.
    Text(String),
    /// 대기 중인 제안에 대한 확인.
    Confirm,
}

/// channel이 정규화한 외부 응답.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundResponse {
    /// channel 안에서 응답마다 고유하고 재조회해도 같은 값. 1회 처리의 키다.
    pub external_id: String,
    pub respondent: String,
    /// 단서가 없으면 `None`이고 호출자가 `not_found`로 기록한다.
    pub hitl_ref: Option<HitlRef>,
    pub body: InboundBody,
}

/// polling 대상 HITL 요청.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollTarget {
    pub hitl_id: HitlId,
    pub work_id: String,
    /// 이 channel로 보낸 요청 메시지. 아직 못 보냈으면 `None`.
    pub message_ref: Option<MessageRef>,
    /// 요청이 열린 시각(RFC 3339). 이 이후의 응답만 대상이다.
    pub since: String,
    /// 아직 열린 요청인지. 확정된 요청(늦은 응답 창)은 명시 `hitl_id` 응답에
    /// `already_handled`를 회신하려고만 남으므로, id 없는 응답의 상관 후보가 아니다.
    pub open: bool,
}

/// 한 번의 발송 결과. 발송 실패는 `Err`로 따로 돌려준다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// 보냈다. 외부 참조를 줄 수 없는 channel이면 `None`.
    Sent(Option<MessageRef>),
    /// 이 channel에는 아이템의 주소가 없다(예: origin channel과 다른 출처의 아이템).
    /// 실패가 아니며 재시도해도 결과가 같다.
    NoAddress,
}

/// 사람 대상 메시지 발송 seam. 구현은 infra, 선택과 재시도 정책은 daemon이 가진다.
#[async_trait]
pub trait NotificationChannel: Send + Sync {
    /// 설정의 channel 이름(origin은 [`ORIGIN_CHANNEL`]).
    fn name(&self) -> &str;

    /// 메시지를 보내고 결과를 돌려준다. 아이템에 이 channel의 주소가 없으면
    /// 보내지 않고 [`NotifyOutcome::NoAddress`].
    ///
    /// # Errors
    /// 발송 실패. 재시도 여부는 호출자가 정한다.
    async fn notify(&self, msg: &OutboundMessage) -> anyhow::Result<NotifyOutcome>;

    /// 수신을 지원하지 않으면 `None`(발송 전용).
    fn inbox(&self) -> Option<&dyn ResponseInbox>;
}

/// 외부 응답 수신 seam.
#[async_trait]
pub trait ResponseInbox: Send + Sync {
    /// `targets`에 대한 응답을 정규화해 돌려준다. 이미 처리한 응답을 다시 줘도 된다
    /// (호출자가 `external_id`로 중복을 거른다).
    ///
    /// # Errors
    /// 조회 실패.
    async fn poll(&self, targets: &[PollTarget]) -> anyhow::Result<Vec<InboundResponse>>;
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

    fn routed(yaml: &str, event: ChannelEvent) -> Vec<String> {
        route(event, &with_channels(yaml))
    }

    #[test]
    fn route_default_config_sends_default_events_to_origin_only() {
        let cfg = NotificationsConfig::default();
        for event in [
            ChannelEvent::Started,
            ChannelEvent::Failed,
            ChannelEvent::HitlRequested,
        ] {
            assert_eq!(route(event, &cfg), vec![ORIGIN_CHANNEL.to_string()]);
        }
        for event in [ChannelEvent::Done, ChannelEvent::Skipped] {
            assert!(route(event, &cfg).is_empty());
        }
    }

    #[test]
    fn route_applies_origin_event_filter() {
        let yaml = "origin:\n  events: [done]\n";
        assert_eq!(routed(yaml, ChannelEvent::Done), vec!["origin"]);
        assert!(routed(yaml, ChannelEvent::Started).is_empty());
    }

    #[test]
    fn route_skips_disabled_origin() {
        let yaml = "origin:\n  enabled: false\n";
        assert!(routed(yaml, ChannelEvent::Started).is_empty());
    }

    #[test]
    fn route_fans_out_to_channels_by_their_own_filter() {
        let yaml = "channels:\n  - name: chat\n    type: chat\n    events: [done, hitl_requested]\n  - name: pager\n    type: pager\n    events: [failed]\n";
        assert_eq!(routed(yaml, ChannelEvent::Done), vec!["chat"]);
        assert_eq!(
            routed(yaml, ChannelEvent::HitlRequested),
            vec!["origin", "chat"]
        );
        assert_eq!(routed(yaml, ChannelEvent::Failed), vec!["origin", "pager"]);
        assert!(routed(yaml, ChannelEvent::Skipped).is_empty());
    }

    #[test]
    fn route_reaches_channels_even_when_origin_is_disabled() {
        let yaml = "origin:\n  enabled: false\n\nchannels:\n  - name: chat\n    type: chat\n    events: [started]\n";
        assert_eq!(routed(yaml, ChannelEvent::Started), vec!["chat"]);
    }

    #[test]
    fn validate_channels_rejects_reserved_origin_name() {
        let cfg =
            with_channels("channels:\n  - name: origin\n    type: chat\n    events: [done]\n");
        let err = cfg.validate_channels(&["chat"]).unwrap_err().to_string();
        assert!(err.contains("origin"), "{err}");
    }
}
