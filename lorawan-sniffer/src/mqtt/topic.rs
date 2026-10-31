//! MQTT topic templates.
//!
//! The topic a record is published on is built from a template such as
//! `lorawan-sniffer/{gateway_eui}/{direction}/{mtype}`, so subscribers can
//! pick out one gateway, one direction or one message type with an ordinary
//! topic filter.
//!
//! Placeholders:
//!
//! | Placeholder     | Value                                              |
//! |-----------------|----------------------------------------------------|
//! | `{gateway_eui}` | gateway EUI, as in the record                      |
//! | `{direction}`   | `up` or `down`                                     |
//! | `{mtype}`       | message type, e.g. `JoinRequest`                   |
//! | `{status}`      | `Forwarded`, `Blocked` or `Captured`               |
//! | `{dev_addr}`    | DevAddr of a data frame                            |
//! | `{dev_eui}`     | DevEUI of a join request                           |
//!
//! A value the record does not have (a join request has no DevAddr, a
//! Concentratord downlink may have no gateway) renders as `unknown`, so the
//! topic keeps the same number of levels whatever the message.

use std::str::FromStr;

use thiserror::Error;

use crate::core::metadata::MessageRecord;

/// What a placeholder renders as when the record has no value for it.
const UNKNOWN: &str = "unknown";

/// Why a topic template was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TopicError {
    #[error("the topic template is empty")]
    Empty,
    #[error(
        "unknown placeholder {{{0}}} (valid: {{gateway_eui}}, {{direction}}, {{mtype}}, \
         {{status}}, {{dev_addr}}, {{dev_eui}})"
    )]
    UnknownPlaceholder(String),
    #[error("unbalanced braces in the topic template")]
    UnbalancedBraces,
    /// Wildcards are for subscriptions; a publish topic containing one is
    /// refused by every broker.
    #[error("'{0}' is a wildcard and cannot be used in a topic to publish on")]
    Wildcard(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    GatewayEui,
    Direction,
    MType,
    Status,
    DevAddr,
    DevEui,
}

/// A parsed topic template.
///
/// Parsing it once at startup turns every mistake into a startup error and
/// keeps the per-record work to appending strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicTemplate(Vec<Segment>);

impl FromStr for TopicTemplate {
    type Err = TopicError;

    fn from_str(template: &str) -> Result<Self, TopicError> {
        if template.is_empty() {
            return Err(TopicError::Empty);
        }
        if let Some(wildcard) = template.chars().find(|c| matches!(c, '+' | '#')) {
            return Err(TopicError::Wildcard(wildcard));
        }

        let mut segments = Vec::new();
        let mut literal = String::new();
        let mut chars = template.chars();

        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    let mut name = String::new();
                    loop {
                        match chars.next() {
                            Some('}') => break,
                            // A second opening brace or the end of the text
                            // means the placeholder was never closed.
                            Some('{') | None => return Err(TopicError::UnbalancedBraces),
                            Some(c) => name.push(c),
                        }
                    }
                    let segment = match name.as_str() {
                        "gateway_eui" => Segment::GatewayEui,
                        "direction" => Segment::Direction,
                        "mtype" => Segment::MType,
                        "status" => Segment::Status,
                        "dev_addr" => Segment::DevAddr,
                        "dev_eui" => Segment::DevEui,
                        _ => return Err(TopicError::UnknownPlaceholder(name)),
                    };
                    if !literal.is_empty() {
                        segments.push(Segment::Literal(std::mem::take(&mut literal)));
                    }
                    segments.push(segment);
                }
                '}' => return Err(TopicError::UnbalancedBraces),
                c => literal.push(c),
            }
        }
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }

        Ok(TopicTemplate(segments))
    }
}

impl TopicTemplate {
    /// The topic for `record`.
    pub fn render(&self, record: &MessageRecord) -> String {
        let mut topic = String::new();
        for segment in &self.0 {
            match segment {
                Segment::Literal(text) => topic.push_str(text),
                Segment::GatewayEui => {
                    topic.push_str(record.gateway_eui.as_deref().unwrap_or(UNKNOWN))
                }
                Segment::Direction => topic.push_str(&record.direction.to_string()),
                Segment::MType => topic.push_str(&record.mtype.to_string()),
                Segment::Status => topic.push_str(&record.status.to_string()),
                Segment::DevAddr => topic.push_str(record.dev_addr.as_deref().unwrap_or(UNKNOWN)),
                Segment::DevEui => topic.push_str(record.dev_eui.as_deref().unwrap_or(UNKNOWN)),
            }
        }
        topic
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::Utc;

    use crate::core::metadata::{Direction, MType, Protocol, Status};

    fn record() -> MessageRecord {
        MessageRecord {
            id: MessageRecord::new_id(),
            timestamp: Utc::now(),
            direction: Direction::Up,
            status: Status::Forwarded,
            blocked_by: None,
            source_protocol: Protocol::Udp,
            freq_mhz: 868.1,
            datarate: "SF7BW125".to_string(),
            rssi: None,
            snr: None,
            crc_status: None,
            mtype: MType::JoinRequest,
            dev_addr: None,
            dev_eui: None,
            fcnt: None,
            fport: None,
            payload_size: 23,
            toa_ms: None,
            gateway_eui: None,
            join_eui: None,
            dev_nonce: None,
            adr: None,
            adr_ack_req: None,
            ack: None,
            class_b: None,
            f_pending: None,
            f_opts_len: None,
        }
    }

    fn template(text: &str) -> TopicTemplate {
        text.parse().unwrap()
    }

    #[test]
    fn renders_every_placeholder() {
        let mut rec = record();
        rec.gateway_eui = Some("AA:BB:CC:DD:00:11:22:33".to_string());
        rec.direction = Direction::Down;
        rec.mtype = MType::UnconfirmedDataDown;
        rec.status = Status::Blocked;
        rec.dev_addr = Some("01ABCDEF".to_string());
        rec.dev_eui = Some("01:02:03:04:05:06:07:08".to_string());

        let topic =
            template("gw/{gateway_eui}/{direction}/{mtype}/{status}/addr/{dev_addr}/eui/{dev_eui}")
                .render(&rec);

        assert_eq!(
            topic,
            "gw/AA:BB:CC:DD:00:11:22:33/down/UnconfirmedDataDown/Blocked/addr/01ABCDEF/eui/\
             01:02:03:04:05:06:07:08"
        );
    }

    #[test]
    fn default_topic_renders_per_message() {
        let t = template("lorawan-sniffer/{gateway_eui}/{direction}/{mtype}");
        let mut rec = record();
        rec.gateway_eui = Some("58:A0:CB:FF:FE:80:12:34".to_string());

        assert_eq!(
            t.render(&rec),
            "lorawan-sniffer/58:A0:CB:FF:FE:80:12:34/up/JoinRequest"
        );
    }

    #[test]
    fn statuses_and_directions_render_as_documented() {
        let t = template("{direction}/{status}");
        let mut rec = record();

        for (direction, status, expected) in [
            (Direction::Up, Status::Forwarded, "up/Forwarded"),
            (Direction::Up, Status::Blocked, "up/Blocked"),
            (Direction::Down, Status::Captured, "down/Captured"),
        ] {
            rec.direction = direction;
            rec.status = status;
            assert_eq!(t.render(&rec), expected);
        }
    }

    #[test]
    fn missing_values_render_unknown() {
        let t = template("{gateway_eui}/{dev_addr}/{dev_eui}/{mtype}");

        // Nothing but the always-present fields is set.
        assert_eq!(t.render(&record()), "unknown/unknown/unknown/JoinRequest");
    }

    #[test]
    fn literal_topic_unchanged() {
        let t = template("sniffer/all/messages");

        assert_eq!(t.render(&record()), "sniffer/all/messages");
        // Nothing in the record can alter a template without placeholders.
        let mut rec = record();
        rec.gateway_eui = Some("AA:BB:CC:DD:00:11:22:33".to_string());
        assert_eq!(t.render(&rec), "sniffer/all/messages");
    }

    #[test]
    fn placeholders_may_repeat_and_touch_literals() {
        let t = template("a{mtype}b{mtype}{direction}c");

        assert_eq!(t.render(&record()), "aJoinRequestbJoinRequestupc");
    }

    #[test]
    fn rejects_unknown_placeholder() {
        assert_eq!(
            "a/{gateway}/b".parse::<TopicTemplate>(),
            Err(TopicError::UnknownPlaceholder("gateway".to_string()))
        );
        assert_eq!(
            "{}".parse::<TopicTemplate>(),
            Err(TopicError::UnknownPlaceholder(String::new()))
        );
        // Names are case sensitive.
        assert!("{MType}".parse::<TopicTemplate>().is_err());

        let message = "{nope}".parse::<TopicTemplate>().unwrap_err().to_string();
        assert!(
            message.contains("{nope}") && message.contains("{gateway_eui}"),
            "{message}"
        );
    }

    #[test]
    fn rejects_wildcards() {
        assert_eq!(
            "a/+/b".parse::<TopicTemplate>(),
            Err(TopicError::Wildcard('+'))
        );
        assert_eq!(
            "a/b/#".parse::<TopicTemplate>(),
            Err(TopicError::Wildcard('#'))
        );
        assert_eq!(
            "{mtype}/+".parse::<TopicTemplate>(),
            Err(TopicError::Wildcard('+'))
        );
    }

    #[test]
    fn rejects_unbalanced_braces() {
        for bad in [
            "a/{mtype",
            "a/mtype}",
            "{{mtype}}",
            "a/{mtype{direction}}",
            "}",
            "{",
        ] {
            assert_eq!(
                bad.parse::<TopicTemplate>(),
                Err(TopicError::UnbalancedBraces),
                "{bad}"
            );
        }
    }

    #[test]
    fn rejects_empty_template() {
        assert_eq!("".parse::<TopicTemplate>(), Err(TopicError::Empty));
    }
}
