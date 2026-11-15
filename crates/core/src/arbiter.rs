//! Combines one zone's proposals into a decision.
//!
//! `Base` (required; without it the zone is left alone) → plus the sum of `Adjust` → at most
//! the lowest `Cap` → replaced by `Override` → clamped to `[min_setpoint, max_setpoint]`.

use crate::policy::{Proposal, ProposalKind, Reason};
use crate::units::{Celsius, CelsiusDelta};
use crate::zone::ZoneConfig;

/// The setpoint a zone should have, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Target setpoint.
    pub value: Celsius,
    /// Every contributing reason, in proposal order.
    pub reasons: Vec<Reason>,
}

/// Decides one zone's setpoint from the proposals addressed to it.
pub fn arbitrate<'a>(
    zone: &ZoneConfig,
    proposals: impl IntoIterator<Item = &'a Proposal>,
) -> Option<Decision> {
    let mut base = None;
    let mut adjust = CelsiusDelta::ZERO;
    let mut cap: Option<Celsius> = None;
    let mut over = None;
    let mut reasons = Vec::new();
    for p in proposals {
        match p.kind {
            ProposalKind::Base(v) => base = Some(v),
            ProposalKind::Adjust(d) => adjust += d,
            ProposalKind::Cap(c) => cap = Some(cap.map_or(c, |x| x.min(c))),
            ProposalKind::Override(v) => over = Some(v),
        }
        reasons.push(p.reason.clone());
    }
    let wanted = base? + adjust;
    let capped = cap.map_or(wanted, |c| wanted.min(c));
    let value = over.unwrap_or(capped);
    Some(Decision {
        value: value.clamp(zone.min_setpoint, zone.max_setpoint),
        reasons,
    })
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::policy::PolicyName;
    use crate::zone::tests::config;

    fn p(kind: ProposalKind) -> Proposal {
        Proposal {
            zone: crate::ids::ZoneId::new("bad").unwrap(),
            kind,
            reason: Reason {
                policy: PolicyName::UserSetpointBase,
                detail: String::new(),
            },
        }
    }

    fn c(v: f64) -> Celsius {
        Celsius::new(v)
    }

    #[test]
    fn no_base_no_decision() {
        let cfg = config("bad", 1);
        assert_eq!(arbitrate(&cfg, &[p(ProposalKind::Cap(c(18.0)))]), None);
    }

    #[test]
    fn base_adjust_cap_override_clamp() {
        let cfg = config("bad", 1);
        let d = arbitrate(&cfg, &[p(ProposalKind::Base(c(22.0)))]).unwrap();
        assert_eq!(d.value, c(22.0));
        assert_eq!(d.reasons.len(), 1);

        let adjusted = [
            p(ProposalKind::Base(c(22.0))),
            p(ProposalKind::Adjust(CelsiusDelta::new(1.0))),
            p(ProposalKind::Adjust(CelsiusDelta::new(-0.5))),
        ];
        assert_eq!(arbitrate(&cfg, &adjusted).unwrap().value, c(22.5));

        let capped = [
            p(ProposalKind::Cap(c(20.0))),
            p(ProposalKind::Base(c(22.0))),
            p(ProposalKind::Cap(c(18.0))),
        ];
        let d = arbitrate(&cfg, &capped).unwrap();
        assert_eq!(d.value, c(18.0), "lowest cap wins");
        assert_eq!(d.reasons.len(), 3);

        let cap_above_base = [
            p(ProposalKind::Base(c(22.0))),
            p(ProposalKind::Cap(c(25.0))),
        ];
        assert_eq!(arbitrate(&cfg, &cap_above_base).unwrap().value, c(22.0));

        let overridden = [
            p(ProposalKind::Base(c(22.0))),
            p(ProposalKind::Cap(c(18.0))),
            p(ProposalKind::Override(c(24.0))),
        ];
        assert_eq!(arbitrate(&cfg, &overridden).unwrap().value, c(24.0));

        let too_hot = [
            p(ProposalKind::Base(c(22.0))),
            p(ProposalKind::Override(c(50.0))),
        ];
        assert_eq!(arbitrate(&cfg, &too_hot).unwrap().value, c(35.0));
        let too_cold = [
            p(ProposalKind::Base(c(22.0))),
            p(ProposalKind::Cap(c(-3.0))),
        ];
        assert_eq!(arbitrate(&cfg, &too_cold).unwrap().value, c(5.0));
    }

    fn kind() -> impl Strategy<Value = ProposalKind> {
        prop_oneof![
            (-10.0f64..50.0).prop_map(|v| ProposalKind::Base(Celsius::new(v))),
            (-10.0f64..10.0).prop_map(|v| ProposalKind::Adjust(CelsiusDelta::new(v))),
            (-10.0f64..50.0).prop_map(|v| ProposalKind::Cap(Celsius::new(v))),
            (-10.0f64..50.0).prop_map(|v| ProposalKind::Override(Celsius::new(v))),
        ]
    }

    proptest! {
        #[test]
        fn decision_is_always_within_bounds(kinds in prop::collection::vec(kind(), 0..8)) {
            let cfg = config("bad", 1);
            let proposals: Vec<_> = kinds.into_iter().map(p).collect();
            if let Some(d) = arbitrate(&cfg, &proposals) {
                prop_assert!(d.value >= cfg.min_setpoint && d.value <= cfg.max_setpoint);
                prop_assert_eq!(d.reasons.len(), proposals.len());
            }
        }
    }
}
