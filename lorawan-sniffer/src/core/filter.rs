use crate::core::metadata::{Direction, MType};

/// A compiled filter rule ready for matching.
#[derive(Debug, Clone)]
pub struct FilterRule {
    /// Human-readable name shown in logs.
    pub name: String,
    /// Whether this rule is active.
    pub enabled: bool,
    /// Direction to match. `None` matches both directions.
    pub direction: Option<Direction>,
    /// When `true`, the corresponding ACK is suppressed (stealth mode).
    pub suppress_ack: bool,
    /// MTypes to match. Empty list matches any MType.
    pub mtypes: Vec<MType>,
}

impl FilterRule {
    /// Returns `true` if this rule matches the given direction and MType.
    pub fn matches(&self, direction: &Direction, mtype: &MType) -> bool {
        if !self.enabled {
            return false;
        }

        if let Some(ref rule_dir) = self.direction {
            if rule_dir != direction {
                return false;
            }
        }

        if !self.mtypes.is_empty() && !self.mtypes.contains(mtype) {
            return false;
        }

        true
    }
}

/// Result of evaluating a set of filter rules against a packet.
#[derive(Debug)]
pub struct FilterDecision {
    /// Name of the rule that matched, or `None` if no rule matched.
    pub blocked_by: Option<String>,
    /// Whether to suppress the corresponding protocol ACK.
    pub suppress_ack: bool,
}

/// Evaluate `rules` against a packet and return the forwarding decision.
///
/// Rules are evaluated in order; the first matching enabled rule wins.
/// If no rule matches the packet is forwarded transparently.
pub fn evaluate(rules: &[FilterRule], direction: &Direction, mtype: &MType) -> FilterDecision {
    for rule in rules {
        if rule.matches(direction, mtype) {
            return FilterDecision {
                blocked_by: Some(rule.name.clone()),
                suppress_ack: rule.suppress_ack,
            };
        }
    }
    FilterDecision {
        blocked_by: None,
        suppress_ack: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_rule(name: &str, dir: Option<Direction>, mtypes: Vec<MType>, suppress: bool) -> FilterRule {
        FilterRule {
            name: name.to_string(),
            enabled: true,
            direction: dir,
            suppress_ack: suppress,
            mtypes,
        }
    }

    // --- FilterRule::matches ---

    #[test]
    fn disabled_rule_never_matches() {
        let mut rule = make_rule("r", None, vec![], false);
        rule.enabled = false;
        assert!(!rule.matches(&Direction::Up, &MType::UnconfirmedDataUp));
    }

    #[test]
    fn matches_any_direction_when_none() {
        let rule = make_rule("r", None, vec![], false);
        assert!(rule.matches(&Direction::Up, &MType::UnconfirmedDataUp));
        assert!(rule.matches(&Direction::Down, &MType::UnconfirmedDataDown));
    }

    #[test]
    fn direction_filter_rejects_wrong_direction() {
        let rule = make_rule("r", Some(Direction::Up), vec![], false);
        assert!(rule.matches(&Direction::Up, &MType::UnconfirmedDataUp));
        assert!(!rule.matches(&Direction::Down, &MType::UnconfirmedDataDown));
    }

    #[test]
    fn empty_mtype_list_matches_all() {
        let rule = make_rule("r", None, vec![], false);
        assert!(rule.matches(&Direction::Up, &MType::JoinRequest));
        assert!(rule.matches(&Direction::Up, &MType::ConfirmedDataUp));
    }

    #[test]
    fn mtype_filter_matches_listed_types_only() {
        let rule = make_rule("r", None, vec![MType::ConfirmedDataUp], false);
        assert!(rule.matches(&Direction::Up, &MType::ConfirmedDataUp));
        assert!(!rule.matches(&Direction::Up, &MType::UnconfirmedDataUp));
        assert!(!rule.matches(&Direction::Up, &MType::JoinRequest));
    }

    #[test]
    fn multiple_mtypes_in_rule() {
        let rule = make_rule(
            "r",
            None,
            vec![MType::UnconfirmedDataDown, MType::ConfirmedDataDown, MType::JoinAccept],
            true,
        );
        assert!(rule.matches(&Direction::Down, &MType::JoinAccept));
        assert!(rule.matches(&Direction::Down, &MType::ConfirmedDataDown));
        assert!(!rule.matches(&Direction::Down, &MType::JoinRequest));
    }

    // --- evaluate ---

    #[test]
    fn evaluate_no_rules_forwards() {
        let d = evaluate(&[], &Direction::Up, &MType::UnconfirmedDataUp);
        assert!(d.blocked_by.is_none());
        assert!(!d.suppress_ack);
    }

    #[test]
    fn evaluate_first_matching_rule_wins() {
        let rules = vec![
            make_rule("block-confirmed", Some(Direction::Up), vec![MType::ConfirmedDataUp], false),
            make_rule("block-all-up", Some(Direction::Up), vec![], false),
        ];
        let d = evaluate(&rules, &Direction::Up, &MType::ConfirmedDataUp);
        assert_eq!(d.blocked_by.as_deref(), Some("block-confirmed"));
    }

    #[test]
    fn evaluate_second_rule_if_first_misses() {
        let rules = vec![
            make_rule("block-confirmed", Some(Direction::Up), vec![MType::ConfirmedDataUp], false),
            make_rule("block-unconfirmed", Some(Direction::Up), vec![MType::UnconfirmedDataUp], false),
        ];
        let d = evaluate(&rules, &Direction::Up, &MType::UnconfirmedDataUp);
        assert_eq!(d.blocked_by.as_deref(), Some("block-unconfirmed"));
    }

    #[test]
    fn evaluate_non_matching_rule_forwards() {
        let rules = vec![make_rule("block-down", Some(Direction::Down), vec![], false)];
        let d = evaluate(&rules, &Direction::Up, &MType::UnconfirmedDataUp);
        assert!(d.blocked_by.is_none());
    }

    #[test]
    fn evaluate_suppress_ack_propagated() {
        let rules = vec![make_rule("stealth", None, vec![], true)];
        let d = evaluate(&rules, &Direction::Up, &MType::JoinRequest);
        assert!(d.blocked_by.is_some());
        assert!(d.suppress_ack);
    }
}
