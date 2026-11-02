use crate::core::eui::Eui64;
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
    /// Gateways the rule applies to. Empty list matches every gateway.
    pub gateways: Vec<Eui64>,
}

impl FilterRule {
    /// Returns `true` if this rule matches the given direction, MType and
    /// gateway.
    ///
    /// `mtype` is `None` when the frame's type is not known, which is the case
    /// for a malformed frame: its "type" is only its first byte read as an
    /// MHDR, and for noise that means nothing.  Only a rule with an empty type
    /// list matches it.  A rule listing a type would otherwise match about one
    /// noise frame in eight and, in Semtech UDP, hold back the whole PUSH_DATA
    /// with it, valid frames included.
    ///
    /// `gateway` is `None` when the frame's gateway could not be determined.
    /// A rule restricted to specific gateways does not match then: blocking
    /// traffic on a guess would hit gateways the rule was never meant for,
    /// while letting it through only leaves the traffic unfiltered.
    pub fn matches(
        &self,
        direction: &Direction,
        mtype: Option<&MType>,
        gateway: Option<&Eui64>,
    ) -> bool {
        if !self.enabled {
            return false;
        }

        if let Some(ref rule_dir) = self.direction {
            if rule_dir != direction {
                return false;
            }
        }

        if !self.mtypes.is_empty() && !mtype.is_some_and(|t| self.mtypes.contains(t)) {
            return false;
        }

        if !self.gateways.is_empty() {
            match gateway {
                Some(gw) if self.gateways.contains(gw) => {}
                _ => return false,
            }
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
///
/// `gateway` is the gateway that received (uplink) or will transmit
/// (downlink) the packet, or `None` when it is not known.
///
/// `mtype` is `None` for a malformed frame; see [`FilterRule::matches`].
pub fn evaluate(
    rules: &[FilterRule],
    direction: &Direction,
    mtype: Option<&MType>,
    gateway: Option<&Eui64>,
) -> FilterDecision {
    for rule in rules {
        if rule.matches(direction, mtype, gateway) {
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

    fn make_rule(
        name: &str,
        dir: Option<Direction>,
        mtypes: Vec<MType>,
        suppress: bool,
    ) -> FilterRule {
        FilterRule {
            name: name.to_string(),
            enabled: true,
            direction: dir,
            suppress_ack: suppress,
            mtypes,
            gateways: vec![],
        }
    }

    const GW_A: Eui64 = Eui64([0xAA, 0xBB, 0xCC, 0xDD, 0x00, 0x11, 0x22, 0x33]);
    const GW_B: Eui64 = Eui64([0x58, 0xA0, 0xCB, 0xFF, 0xFE, 0x80, 0x12, 0x34]);
    const GW_C: Eui64 = Eui64([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08]);

    fn rule_for_gateways(name: &str, gateways: Vec<Eui64>) -> FilterRule {
        FilterRule {
            gateways,
            ..make_rule(name, None, vec![], false)
        }
    }

    // --- FilterRule::matches ---

    #[test]
    fn disabled_rule_never_matches() {
        let mut rule = make_rule("r", None, vec![], false);
        rule.enabled = false;
        assert!(!rule.matches(&Direction::Up, Some(&MType::UnconfirmedDataUp), None));
    }

    #[test]
    fn matches_any_direction_when_none() {
        let rule = make_rule("r", None, vec![], false);
        assert!(rule.matches(&Direction::Up, Some(&MType::UnconfirmedDataUp), None));
        assert!(rule.matches(&Direction::Down, Some(&MType::UnconfirmedDataDown), None));
    }

    #[test]
    fn direction_filter_rejects_wrong_direction() {
        let rule = make_rule("r", Some(Direction::Up), vec![], false);
        assert!(rule.matches(&Direction::Up, Some(&MType::UnconfirmedDataUp), None));
        assert!(!rule.matches(&Direction::Down, Some(&MType::UnconfirmedDataDown), None));
    }

    #[test]
    fn empty_mtype_list_matches_all() {
        let rule = make_rule("r", None, vec![], false);
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), None));
        assert!(rule.matches(&Direction::Up, Some(&MType::ConfirmedDataUp), None));
    }

    #[test]
    fn mtype_filter_matches_listed_types_only() {
        let rule = make_rule("r", None, vec![MType::ConfirmedDataUp], false);
        assert!(rule.matches(&Direction::Up, Some(&MType::ConfirmedDataUp), None));
        assert!(!rule.matches(&Direction::Up, Some(&MType::UnconfirmedDataUp), None));
        assert!(!rule.matches(&Direction::Up, Some(&MType::JoinRequest), None));
    }

    #[test]
    fn multiple_mtypes_in_rule() {
        let rule = make_rule(
            "r",
            None,
            vec![
                MType::UnconfirmedDataDown,
                MType::ConfirmedDataDown,
                MType::JoinAccept,
            ],
            true,
        );
        assert!(rule.matches(&Direction::Down, Some(&MType::JoinAccept), None));
        assert!(rule.matches(&Direction::Down, Some(&MType::ConfirmedDataDown), None));
        assert!(!rule.matches(&Direction::Down, Some(&MType::JoinRequest), None));
    }

    // --- evaluate ---

    #[test]
    fn evaluate_no_rules_forwards() {
        let d = evaluate(&[], &Direction::Up, Some(&MType::UnconfirmedDataUp), None);
        assert!(d.blocked_by.is_none());
        assert!(!d.suppress_ack);
    }

    #[test]
    fn evaluate_first_matching_rule_wins() {
        let rules = vec![
            make_rule(
                "block-confirmed",
                Some(Direction::Up),
                vec![MType::ConfirmedDataUp],
                false,
            ),
            make_rule("block-all-up", Some(Direction::Up), vec![], false),
        ];
        let d = evaluate(&rules, &Direction::Up, Some(&MType::ConfirmedDataUp), None);
        assert_eq!(d.blocked_by.as_deref(), Some("block-confirmed"));
    }

    #[test]
    fn evaluate_second_rule_if_first_misses() {
        let rules = vec![
            make_rule(
                "block-confirmed",
                Some(Direction::Up),
                vec![MType::ConfirmedDataUp],
                false,
            ),
            make_rule(
                "block-unconfirmed",
                Some(Direction::Up),
                vec![MType::UnconfirmedDataUp],
                false,
            ),
        ];
        let d = evaluate(
            &rules,
            &Direction::Up,
            Some(&MType::UnconfirmedDataUp),
            None,
        );
        assert_eq!(d.blocked_by.as_deref(), Some("block-unconfirmed"));
    }

    #[test]
    fn evaluate_non_matching_rule_forwards() {
        let rules = vec![make_rule(
            "block-down",
            Some(Direction::Down),
            vec![],
            false,
        )];
        let d = evaluate(
            &rules,
            &Direction::Up,
            Some(&MType::UnconfirmedDataUp),
            None,
        );
        assert!(d.blocked_by.is_none());
    }

    #[test]
    fn evaluate_suppress_ack_propagated() {
        let rules = vec![make_rule("stealth", None, vec![], true)];
        let d = evaluate(&rules, &Direction::Up, Some(&MType::JoinRequest), None);
        assert!(d.blocked_by.is_some());
        assert!(d.suppress_ack);
    }

    // --- frames without a known type (malformed) ---

    #[test]
    fn type_rule_does_not_match_an_unknown_type() {
        // Whatever types the rule lists, a frame without a type is not one of them.
        for mtypes in [
            vec![MType::JoinRequest],
            vec![MType::UnconfirmedDataUp, MType::ConfirmedDataUp],
            vec![MType::Unknown],
            vec![MType::Proprietary],
        ] {
            let rule = make_rule("r", None, mtypes.clone(), false);
            assert!(!rule.matches(&Direction::Up, None, None), "{mtypes:?}");
            assert!(
                !rule.matches(&Direction::Down, None, Some(&GW_A)),
                "{mtypes:?}"
            );

            let rules = [rule];
            let d = evaluate(&rules, &Direction::Up, None, None);
            assert!(d.blocked_by.is_none(), "{mtypes:?}");
            assert!(!d.suppress_ack, "{mtypes:?}");
        }
    }

    #[test]
    fn type_rule_with_matching_direction_and_gateway_still_skips_an_unknown_type() {
        let mut rule = rule_for_gateways("a-join-requests", vec![GW_A]);
        rule.direction = Some(Direction::Up);
        rule.mtypes = vec![MType::JoinRequest];
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), Some(&GW_A)));
        assert!(!rule.matches(&Direction::Up, None, Some(&GW_A)));
    }

    #[test]
    fn direction_only_and_gateway_only_rules_match_an_unknown_type() {
        let by_direction = make_rule("up", Some(Direction::Up), vec![], true);
        assert!(by_direction.matches(&Direction::Up, None, None));
        assert!(!by_direction.matches(&Direction::Down, None, None));

        let by_gateway = rule_for_gateways("only-a", vec![GW_A]);
        assert!(by_gateway.matches(&Direction::Up, None, Some(&GW_A)));
        assert!(!by_gateway.matches(&Direction::Up, None, Some(&GW_B)));
        assert!(!by_gateway.matches(&Direction::Up, None, None));

        let everything = make_rule("all", None, vec![], false);
        assert!(everything.matches(&Direction::Down, None, None));

        let d = evaluate(&[by_direction], &Direction::Up, None, Some(&GW_A));
        assert_eq!(d.blocked_by.as_deref(), Some("up"));
        assert!(d.suppress_ack);
    }

    #[test]
    fn evaluate_skips_type_rules_for_an_unknown_type_and_reaches_a_later_rule() {
        let rules = [
            make_rule("join", None, vec![MType::JoinRequest], false),
            make_rule("all-up", Some(Direction::Up), vec![], false),
        ];
        let d = evaluate(&rules, &Direction::Up, None, None);
        assert_eq!(d.blocked_by.as_deref(), Some("all-up"));
        let d = evaluate(&rules, &Direction::Up, Some(&MType::JoinRequest), None);
        assert_eq!(d.blocked_by.as_deref(), Some("join"));
    }

    // --- gateways ---

    #[test]
    fn gateway_list_limits_rule() {
        let rule = rule_for_gateways("only-a-and-b", vec![GW_A, GW_B]);
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), Some(&GW_A)));
        assert!(rule.matches(&Direction::Down, Some(&MType::JoinAccept), Some(&GW_B)));
        assert!(!rule.matches(&Direction::Up, Some(&MType::JoinRequest), Some(&GW_C)));

        let rules = [rule];
        let d = evaluate(
            &rules,
            &Direction::Up,
            Some(&MType::JoinRequest),
            Some(&GW_B),
        );
        assert_eq!(d.blocked_by.as_deref(), Some("only-a-and-b"));
        let d = evaluate(
            &rules,
            &Direction::Up,
            Some(&MType::JoinRequest),
            Some(&GW_C),
        );
        assert!(d.blocked_by.is_none());
    }

    #[test]
    fn empty_gateway_list_matches_all() {
        let rule = rule_for_gateways("everyone", vec![]);
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), Some(&GW_A)));
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), Some(&GW_C)));
        // Even when the gateway is not known: the rule never asked for one.
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), None));
    }

    #[test]
    fn unknown_gateway_does_not_match_listed_rule() {
        let rules = [rule_for_gateways("only-a", vec![GW_A])];
        assert!(!rules[0].matches(&Direction::Up, Some(&MType::JoinRequest), None));
        let d = evaluate(&rules, &Direction::Up, Some(&MType::JoinRequest), None);
        assert!(d.blocked_by.is_none());
        assert!(!d.suppress_ack);
    }

    #[test]
    fn gateway_list_combines_with_other_criteria() {
        let mut rule = rule_for_gateways("a-uplinks", vec![GW_A]);
        rule.direction = Some(Direction::Up);
        rule.mtypes = vec![MType::JoinRequest];
        assert!(rule.matches(&Direction::Up, Some(&MType::JoinRequest), Some(&GW_A)));
        // The right gateway is not enough when direction or MType differ.
        assert!(!rule.matches(&Direction::Down, Some(&MType::JoinRequest), Some(&GW_A)));
        assert!(!rule.matches(&Direction::Up, Some(&MType::UnconfirmedDataUp), Some(&GW_A)));
    }

    #[test]
    fn rule_for_other_gateway_does_not_shadow_later_rule() {
        let rules = [
            rule_for_gateways("only-a", vec![GW_A]),
            rule_for_gateways("everyone", vec![]),
        ];
        let d = evaluate(
            &rules,
            &Direction::Up,
            Some(&MType::JoinRequest),
            Some(&GW_B),
        );
        assert_eq!(d.blocked_by.as_deref(), Some("everyone"));
        let d = evaluate(
            &rules,
            &Direction::Up,
            Some(&MType::JoinRequest),
            Some(&GW_A),
        );
        assert_eq!(d.blocked_by.as_deref(), Some("only-a"));
    }
}
