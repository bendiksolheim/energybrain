//! Grid-rent tariff model.
//!
//! The capacity charge (kapasitetsledd) is a monthly price picked from a table of power bands,
//! using the average of the month's three highest daily-max hourly consumptions. Bands are
//! half-open: `[from, to)`, so an average of exactly 5.0 kW lands in the 5–10 kW band.
//!
//! The energy charge (day/night) is intentionally not modelled yet.

use crate::units::{Kr, Kw};

/// Position of a band in the table, 0 = cheapest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TierIndex(pub usize);

/// One bounded band, given by its exclusive upper bound. Its lower bound is the previous
/// band's upper bound (or 0).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    /// Exclusive upper bound.
    pub to: Kw,
    /// Monthly price.
    pub price: Kr,
}

/// A fully described band.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tier {
    /// Position in the table.
    pub index: TierIndex,
    /// Inclusive lower bound.
    pub from: Kw,
    /// Exclusive upper bound; `None` for the open-ended top band.
    pub to: Option<Kw>,
    /// Monthly price.
    pub price: Kr,
}

/// Why a tier table was rejected. Indexes refer to the bounded steps.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TariffError {
    /// A bound is zero, negative or not finite.
    #[error("tier {index}: upper bound {to} must be a positive, finite number")]
    InvalidBound {
        /// Offending step.
        index: usize,
        /// Its bound.
        to: Kw,
    },
    /// A bound is not above the previous one.
    #[error("tier {index}: upper bound {to} must be greater than the previous tier's {prev}")]
    NotIncreasing {
        /// Offending step.
        index: usize,
        /// Its bound.
        to: Kw,
        /// The previous bound.
        prev: Kw,
    },
}

/// The capacity tariff: bounded steps in increasing order plus an open-ended top band.
/// Contiguity is guaranteed by construction (each band starts where the previous ended).
#[derive(Debug, Clone, PartialEq)]
pub struct CapacityTariff {
    steps: Vec<Step>,
    top_price: Kr,
}

impl CapacityTariff {
    /// Validates and builds a tariff.
    pub fn new(steps: Vec<Step>, top_price: Kr) -> Result<Self, TariffError> {
        let mut prev = Kw::ZERO;
        for (index, step) in steps.iter().enumerate() {
            let to = step.to;
            if !to.value().is_finite() || to <= Kw::ZERO {
                return Err(TariffError::InvalidBound { index, to });
            }
            if to <= prev {
                return Err(TariffError::NotIncreasing { index, to, prev });
            }
            prev = to;
        }
        Ok(Self { steps, top_price })
    }

    /// Number of bands, including the open-ended top band.
    pub fn len(&self) -> usize {
        self.steps.len() + 1
    }

    /// Always false: there is at least the open-ended band.
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The band an average consumption falls in.
    pub fn tier_for(&self, avg: Kw) -> Tier {
        let index = self.steps.iter().take_while(|s| s.to <= avg).count();
        self.build(index)
    }

    /// The band at `index`, if the table has that many.
    pub fn tier(&self, index: TierIndex) -> Option<Tier> {
        (index.0 < self.len()).then(|| self.build(index.0))
    }

    /// All bands, cheapest first.
    pub fn tiers(&self) -> impl Iterator<Item = Tier> + '_ {
        (0..self.len()).map(|i| self.build(i))
    }

    /// Builds the tier at an index known to be in range.
    fn build(&self, index: usize) -> Tier {
        let from = index
            .checked_sub(1)
            .and_then(|i| self.steps.get(i))
            .map_or(Kw::ZERO, |s| s.to);
        let step = self.steps.get(index);
        Tier {
            index: TierIndex(index),
            from,
            to: step.map(|s| s.to),
            price: step.map_or(self.top_price, |s| s.price),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// Tensio TS 2026 (kW, kr/month).
    fn tensio() -> CapacityTariff {
        let table = [
            (2.0, 131.0),
            (5.0, 233.0),
            (10.0, 397.0),
            (15.0, 585.0),
            (20.0, 775.0),
            (25.0, 964.0),
            (50.0, 1655.0),
            (75.0, 2599.0),
            (100.0, 3544.0),
            (150.0, 5117.0),
            (200.0, 7003.0),
            (300.0, 10147.0),
            (400.0, 13925.0),
            (500.0, 17697.0),
        ];
        let steps = table
            .iter()
            .map(|&(to, price)| Step {
                to: Kw::new(to),
                price: Kr::new(price),
            })
            .collect();
        CapacityTariff::new(steps, Kr::new(21473.0)).unwrap()
    }

    #[test]
    fn bands_are_half_open() {
        let t = tensio();
        assert_eq!(t.tier_for(Kw::new(0.0)).index, TierIndex(0));
        assert_eq!(t.tier_for(Kw::new(4.99)).index, TierIndex(1));
        let at_five = t.tier_for(Kw::new(5.0));
        assert_eq!(
            at_five,
            Tier {
                index: TierIndex(2),
                from: Kw::new(5.0),
                to: Some(Kw::new(10.0)),
                price: Kr::new(397.0),
            }
        );
    }

    #[test]
    fn top_band_is_open_ended() {
        let t = tensio();
        let top = t.tier_for(Kw::new(900.0));
        assert_eq!(top.index, TierIndex(14));
        assert_eq!(top.from, Kw::new(500.0));
        assert_eq!(top.to, None);
        assert_eq!(top.price, Kr::new(21473.0));
    }

    #[test]
    fn lookup_by_index() {
        let t = tensio();
        assert_eq!(t.len(), 15);
        assert!(!t.is_empty());
        assert_eq!(t.tier(TierIndex(1)).unwrap().to, Some(Kw::new(5.0)));
        assert_eq!(t.tier(TierIndex(15)), None);
        assert_eq!(t.tiers().count(), 15);
    }

    #[test]
    fn single_open_band() {
        let t = CapacityTariff::new(vec![], Kr::new(100.0)).unwrap();
        assert_eq!(t.tier_for(Kw::new(3.0)).to, None);
    }

    #[test]
    fn rejects_bad_tables() {
        let step = |to| Step {
            to: Kw::new(to),
            price: Kr::ZERO,
        };
        let err = CapacityTariff::new(vec![step(0.0)], Kr::ZERO).unwrap_err();
        assert_eq!(
            err,
            TariffError::InvalidBound {
                index: 0,
                to: Kw::ZERO
            }
        );
        assert_eq!(
            err.to_string(),
            "tier 0: upper bound 0 kW must be a positive, finite number"
        );
        assert!(matches!(
            CapacityTariff::new(vec![step(f64::NAN)], Kr::ZERO),
            Err(TariffError::InvalidBound { index: 0, .. })
        ));
        let err = CapacityTariff::new(vec![step(5.0), step(5.0)], Kr::ZERO).unwrap_err();
        assert_eq!(
            err.to_string(),
            "tier 1: upper bound 5 kW must be greater than the previous tier's 5 kW"
        );
    }

    proptest! {
        /// The chosen band always contains the average.
        #[test]
        fn chosen_band_contains_average(avg in 0.0f64..1000.0) {
            let t = tensio();
            let tier = t.tier_for(Kw::new(avg));
            prop_assert!(tier.from <= Kw::new(avg));
            prop_assert!(tier.to.is_none_or(|to| Kw::new(avg) < to));
        }
    }
}
