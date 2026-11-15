//! Policies propose setpoints; the arbiter combines the proposals per zone.
//!
//! Phase 1 policies:
//! - [`UserSetpointBase`]: the base is whatever the user set.
//! - [`PeakGuardCaps`]: caps for the zones the guard currently sheds (the shedding decision
//!   itself lives in [`crate::guard`], which owns the guard's state).
//! - [`ManualOverride`]: the user's value wins while an override is active.

use std::collections::BTreeMap;
use std::fmt;

use jiff::Timestamp;

use crate::guard::GuardState;
use crate::ids::ZoneId;
use crate::units::{Celsius, CelsiusDelta};
use crate::zone::Zone;

/// Which policy produced a proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PolicyName {
    /// [`UserSetpointBase`].
    UserSetpointBase,
    /// [`PeakGuardCaps`].
    PeakGuard,
    /// [`ManualOverride`].
    ManualOverride,
}

impl PolicyName {
    /// Stable name for logs and the API.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserSetpointBase => "user_setpoint_base",
            Self::PeakGuard => "peak_guard",
            Self::ManualOverride => "manual_override",
        }
    }
}

/// Why a proposal was made.
#[derive(Debug, Clone, PartialEq)]
pub struct Reason {
    /// The proposing policy.
    pub policy: PolicyName,
    /// Human-readable explanation.
    pub detail: String,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.policy.as_str(), self.detail)
    }
}

/// What a proposal asks for.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProposalKind {
    /// What the zone wants absent other constraints. Required for any decision.
    Base(Celsius),
    /// A relative change (future: weather, tariff shifting).
    Adjust(CelsiusDelta),
    /// An upper limit (peak guard).
    Cap(Celsius),
    /// The human wins.
    Override(Celsius),
}

/// One policy's wish for one zone.
#[derive(Debug, Clone, PartialEq)]
pub struct Proposal {
    /// Target zone.
    pub zone: ZoneId,
    /// What is proposed.
    pub kind: ProposalKind,
    /// Why.
    pub reason: Reason,
}

/// Read-only view policies decide from.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// The current instant.
    pub now: Timestamp,
    /// All zones.
    pub zones: &'a BTreeMap<ZoneId, Zone>,
    /// The guard's current shed set.
    pub guard: &'a GuardState,
}

/// A source of proposals.
pub trait Policy {
    /// The policy's name.
    fn name(&self) -> PolicyName;
    /// Proposals for the current situation.
    fn propose(&self, ctx: &Context<'_>) -> Vec<Proposal>;
}

/// The user's own setpoint is the base.
#[derive(Debug, Clone, Copy, Default)]
pub struct UserSetpointBase;

impl Policy for UserSetpointBase {
    fn name(&self) -> PolicyName {
        PolicyName::UserSetpointBase
    }

    fn propose(&self, ctx: &Context<'_>) -> Vec<Proposal> {
        ctx.zones
            .values()
            .filter_map(|z| {
                let base = z.base?;
                Some(Proposal {
                    zone: z.id().clone(),
                    kind: ProposalKind::Base(base),
                    reason: Reason {
                        policy: self.name(),
                        detail: format!("user setpoint {base}"),
                    },
                })
            })
            .collect()
    }
}

/// Caps for the zones the guard currently sheds.
#[derive(Debug, Clone, Copy, Default)]
pub struct PeakGuardCaps;

impl Policy for PeakGuardCaps {
    fn name(&self) -> PolicyName {
        PolicyName::PeakGuard
    }

    fn propose(&self, ctx: &Context<'_>) -> Vec<Proposal> {
        ctx.guard
            .shed
            .iter()
            .filter_map(|s| {
                let cap = ctx.zones.get(&s.zone)?.shed_cap()?;
                Some(Proposal {
                    zone: s.zone.clone(),
                    kind: ProposalKind::Cap(cap),
                    reason: Reason {
                        policy: self.name(),
                        detail: format!("shed to {cap}: {}", s.reason),
                    },
                })
            })
            .collect()
    }
}

/// The user's value wins while an override is active.
#[derive(Debug, Clone, Copy, Default)]
pub struct ManualOverride;

impl Policy for ManualOverride {
    fn name(&self) -> PolicyName {
        PolicyName::ManualOverride
    }

    fn propose(&self, ctx: &Context<'_>) -> Vec<Proposal> {
        ctx.zones
            .values()
            .filter_map(|z| {
                let o = z.active_override(ctx.now)?;
                Some(Proposal {
                    zone: z.id().clone(),
                    kind: ProposalKind::Override(o.value),
                    reason: Reason {
                        policy: self.name(),
                        detail: format!("user set {} while shed; kept until {}", o.value, o.until),
                    },
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::guard::Shed;
    use crate::units::Kw;
    use crate::zone::Override;
    use crate::zone::tests::{config, ts};

    fn zones() -> BTreeMap<ZoneId, Zone> {
        let mut bad = Zone::new(config("bad", 1));
        bad.base = Some(Celsius::new(26.5));
        let mut gang = Zone::new(config("gang", 3));
        gang.base = Some(Celsius::new(22.0));
        gang.manual = Some(Override {
            value: Celsius::new(24.0),
            until: ts("2026-11-15T19:00:00Z"),
        });
        let kjeller = Zone::new(config("kjeller", 6));
        [bad, gang, kjeller]
            .into_iter()
            .map(|z| (z.id().clone(), z))
            .collect()
    }

    fn shed(zone: &str) -> Shed {
        Shed {
            zone: ZoneId::new(zone).unwrap(),
            since: ts("2026-11-15T17:00:00Z"),
            load: Kw::new(1.0),
            reason: "projected 6 kWh > limit 4.7 kWh".to_owned(),
        }
    }

    #[test]
    fn names() {
        assert_eq!(UserSetpointBase.name().as_str(), "user_setpoint_base");
        assert_eq!(PeakGuardCaps.name().as_str(), "peak_guard");
        assert_eq!(ManualOverride.name().as_str(), "manual_override");
    }

    #[test]
    fn base_only_for_zones_with_a_known_setpoint() {
        let zones = zones();
        let guard = GuardState::default();
        let ctx = Context {
            now: ts("2026-11-15T17:00:00Z"),
            zones: &zones,
            guard: &guard,
        };
        let p = UserSetpointBase.propose(&ctx);
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].kind, ProposalKind::Base(Celsius::new(26.5)));
        assert_eq!(
            p[0].reason.to_string(),
            "user_setpoint_base: user setpoint 26.5 °C"
        );
    }

    #[test]
    fn caps_for_shed_zones_with_a_base() {
        let zones = zones();
        let guard = GuardState {
            shed: vec![shed("bad"), shed("kjeller"), shed("gone")],
            ..GuardState::default()
        };
        let ctx = Context {
            now: ts("2026-11-15T17:00:00Z"),
            zones: &zones,
            guard: &guard,
        };
        let p = PeakGuardCaps.propose(&ctx);
        assert_eq!(p.len(), 1, "kjeller has no base, 'gone' is unknown");
        assert_eq!(p[0].kind, ProposalKind::Cap(Celsius::new(22.5)));
        assert!(p[0].reason.detail.starts_with("shed to 22.5 °C: projected"));
    }

    #[test]
    fn override_only_while_active() {
        let zones = zones();
        let guard = GuardState::default();
        let mut ctx = Context {
            now: ts("2026-11-15T17:00:00Z"),
            zones: &zones,
            guard: &guard,
        };
        let p = ManualOverride.propose(&ctx);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].kind, ProposalKind::Override(Celsius::new(24.0)));
        assert!(
            p[0].reason
                .detail
                .contains("kept until 2026-11-15T19:00:00Z")
        );
        ctx.now = ts("2026-11-15T19:00:00Z");
        assert!(ManualOverride.propose(&ctx).is_empty());
    }
}
