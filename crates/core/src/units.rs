//! Physical-quantity newtypes.
//!
//! Only arithmetic that makes physical sense is implemented: you can add two powers, multiply
//! a power by a duration to get energy, or subtract two temperatures to get a temperature
//! difference — but you cannot add a power to an energy, or two absolute temperatures.
//!
//! Values are plain `f64` inside. Constructors accept any value; adapters are responsible for
//! rejecting non-finite numbers at the parsing boundary.

use std::fmt;
use std::iter::Sum;
use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

use jiff::SignedDuration;

const SECONDS_PER_HOUR: f64 = 3600.0;

/// Shared plumbing for every unit: constructor, accessor, `Display` with a unit suffix,
/// and `min`/`max` helpers.
macro_rules! unit {
    ($(#[$doc:meta])* $name:ident, $suffix:literal) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
        pub struct $name(f64);

        impl $name {
            /// Zero of this unit.
            pub const ZERO: Self = Self(0.0);

            /// Wraps a raw value expressed in this unit.
            pub const fn new(value: f64) -> Self {
                Self(value)
            }

            /// The raw value expressed in this unit.
            pub const fn value(self) -> f64 {
                self.0
            }

            /// The larger of two values.
            #[must_use]
            pub fn max(self, other: Self) -> Self {
                Self(self.0.max(other.0))
            }

            /// The smaller of two values.
            #[must_use]
            pub fn min(self, other: Self) -> Self {
                Self(self.0.min(other.0))
            }
        }

        impl fmt::Display for $name {
            /// Honours precision, e.g. `format!("{:.1}", Kw::new(2.25))` → `"2.2 kW"`.
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)?;
                f.write_str($suffix)
            }
        }
    };
}

/// Arithmetic for quantities where addition is meaningful (amounts, not absolute levels).
macro_rules! additive {
    ($name:ident) => {
        impl Add for $name {
            type Output = Self;
            fn add(self, rhs: Self) -> Self {
                Self(self.0 + rhs.0)
            }
        }

        impl Sub for $name {
            type Output = Self;
            fn sub(self, rhs: Self) -> Self {
                Self(self.0 - rhs.0)
            }
        }

        impl AddAssign for $name {
            fn add_assign(&mut self, rhs: Self) {
                self.0 += rhs.0;
            }
        }

        impl SubAssign for $name {
            fn sub_assign(&mut self, rhs: Self) {
                self.0 -= rhs.0;
            }
        }

        impl Mul<f64> for $name {
            type Output = Self;
            fn mul(self, rhs: f64) -> Self {
                Self(self.0 * rhs)
            }
        }

        impl Div<f64> for $name {
            type Output = Self;
            fn div(self, rhs: f64) -> Self {
                Self(self.0 / rhs)
            }
        }

        impl Sum for $name {
            /// Starts at +0 (std's float `sum` starts at −0, which displays as "-0").
            fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
                Self(iter.fold(0.0, |acc, x| acc + x.0))
            }
        }
    };
}

unit!(
    /// Power in kilowatts.
    Kw,
    " kW"
);
unit!(
    /// Energy in kilowatt-hours.
    Kwh,
    " kWh"
);
unit!(
    /// Absolute temperature in degrees Celsius.
    Celsius,
    " °C"
);
unit!(
    /// Temperature difference in kelvin / degrees Celsius.
    CelsiusDelta,
    " K"
);
unit!(
    /// Money in Norwegian kroner.
    Kr,
    " kr"
);

additive!(Kw);
additive!(Kwh);
additive!(CelsiusDelta);
additive!(Kr);

impl Neg for CelsiusDelta {
    type Output = Self;
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

/// Power sustained for a duration gives energy.
impl Mul<SignedDuration> for Kw {
    type Output = Kwh;
    fn mul(self, rhs: SignedDuration) -> Kwh {
        Kwh(self.0 * rhs.as_secs_f64() / SECONDS_PER_HOUR)
    }
}

/// Energy spread over a duration gives average power. A zero duration yields an infinite or
/// NaN power, so callers must only divide by durations they know are positive.
impl Div<SignedDuration> for Kwh {
    type Output = Kw;
    fn div(self, rhs: SignedDuration) -> Kw {
        Kw(self.0 * SECONDS_PER_HOUR / rhs.as_secs_f64())
    }
}

impl Kwh {
    /// Total ordering for sorting (NaN sorts as the largest).
    pub fn total_cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }

    /// The average power if this energy was used in exactly one hour (kWh/h → kW). This is
    /// how the capacity tariff reads an hourly consumption.
    pub const fn per_hour(self) -> Kw {
        Kw(self.0)
    }
}

impl Kw {
    /// The energy used by sustaining this power for exactly one hour.
    pub const fn for_one_hour(self) -> Kwh {
        Kwh(self.0)
    }
}

impl Sub for Celsius {
    type Output = CelsiusDelta;
    fn sub(self, rhs: Self) -> CelsiusDelta {
        CelsiusDelta(self.0 - rhs.0)
    }
}

impl Add<CelsiusDelta> for Celsius {
    type Output = Self;
    fn add(self, rhs: CelsiusDelta) -> Self {
        Self(self.0 + rhs.0)
    }
}

impl Sub<CelsiusDelta> for Celsius {
    type Output = Self;
    fn sub(self, rhs: CelsiusDelta) -> Self {
        Self(self.0 - rhs.0)
    }
}

impl Celsius {
    /// Restricts the value to `[lo, hi]`. If `lo > hi`, `lo` wins.
    #[must_use]
    pub fn clamp(self, lo: Self, hi: Self) -> Self {
        self.min(hi).max(lo)
    }
}

impl CelsiusDelta {
    /// Absolute size of the difference.
    #[must_use]
    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_appends_unit_and_honours_precision() {
        assert_eq!(Kw::new(2.5).to_string(), "2.5 kW");
        assert_eq!(format!("{:.1}", Kwh::new(1.25)), "1.2 kWh");
        assert_eq!(Celsius::new(21.0).to_string(), "21 °C");
        assert_eq!(CelsiusDelta::new(-4.0).to_string(), "-4 K");
        assert_eq!(Kr::new(233.0).to_string(), "233 kr");
    }

    #[test]
    fn common_helpers() {
        assert_eq!(Kw::ZERO.value(), 0.0);
        assert_eq!(Kw::default(), Kw::ZERO);
        assert_eq!(Kw::new(1.0).max(Kw::new(2.0)), Kw::new(2.0));
        assert_eq!(Kw::new(1.0).min(Kw::new(2.0)), Kw::new(1.0));
        assert_eq!(Kwh::new(3.0).max(Kwh::ZERO), Kwh::new(3.0));
        assert_eq!(Kwh::new(3.0).min(Kwh::ZERO), Kwh::ZERO);
        assert_eq!(Celsius::new(20.0).max(Celsius::ZERO), Celsius::new(20.0));
        assert_eq!(Celsius::new(20.0).min(Celsius::ZERO), Celsius::ZERO);
        let d = CelsiusDelta::new(1.0);
        assert_eq!(d.max(CelsiusDelta::ZERO), d);
        assert_eq!(d.min(CelsiusDelta::ZERO), CelsiusDelta::ZERO);
        assert_eq!(Kr::new(5.0).max(Kr::ZERO), Kr::new(5.0));
        assert_eq!(Kr::new(5.0).min(Kr::ZERO), Kr::ZERO);
    }

    /// Exercises every additive operator for one type.
    fn check_additive<T>(a: T, b: T, sum: T, diff: T, double: T, half: T)
    where
        T: Add<Output = T>
            + Sub<Output = T>
            + AddAssign
            + SubAssign
            + Mul<f64, Output = T>
            + Div<f64, Output = T>
            + Sum
            + PartialEq
            + fmt::Debug
            + Copy,
    {
        assert_eq!(a + b, sum);
        assert_eq!(a - b, diff);
        assert_eq!(a * 2.0, double);
        assert_eq!(a / 2.0, half);
        let mut x = a;
        x += b;
        assert_eq!(x, sum);
        x -= b;
        assert_eq!(x, a);
        assert_eq!([a, b].into_iter().sum::<T>(), sum);
        assert!(!format!("{:?}", std::iter::empty::<T>().sum::<T>()).contains('-'));
    }

    #[test]
    fn additive_arithmetic() {
        check_additive(
            Kw::new(3.0),
            Kw::new(1.0),
            Kw::new(4.0),
            Kw::new(2.0),
            Kw::new(6.0),
            Kw::new(1.5),
        );
        check_additive(
            Kwh::new(3.0),
            Kwh::new(1.0),
            Kwh::new(4.0),
            Kwh::new(2.0),
            Kwh::new(6.0),
            Kwh::new(1.5),
        );
        check_additive(
            CelsiusDelta::new(3.0),
            CelsiusDelta::new(1.0),
            CelsiusDelta::new(4.0),
            CelsiusDelta::new(2.0),
            CelsiusDelta::new(6.0),
            CelsiusDelta::new(1.5),
        );
        check_additive(
            Kr::new(3.0),
            Kr::new(1.0),
            Kr::new(4.0),
            Kr::new(2.0),
            Kr::new(6.0),
            Kr::new(1.5),
        );
    }

    #[test]
    fn power_and_energy_conversions() {
        assert_eq!(Kw::new(6.0) * SignedDuration::from_mins(30), Kwh::new(3.0));
        assert_eq!(Kwh::new(3.0) / SignedDuration::from_mins(30), Kw::new(6.0));
        assert_eq!(Kwh::new(4.2).per_hour(), Kw::new(4.2));
        assert_eq!(Kw::new(4.2).for_one_hour(), Kwh::new(4.2));
    }

    #[test]
    fn kwh_total_order() {
        assert!(Kwh::new(f64::NAN).total_cmp(&Kwh::new(1.0)).is_gt());
        assert!(Kwh::new(1.0).total_cmp(&Kwh::new(2.0)).is_lt());
    }

    #[test]
    fn temperature_arithmetic() {
        let t = Celsius::new(22.0);
        assert_eq!(t - Celsius::new(18.0), CelsiusDelta::new(4.0));
        assert_eq!(t + CelsiusDelta::new(1.0), Celsius::new(23.0));
        assert_eq!(t - CelsiusDelta::new(4.0), Celsius::new(18.0));
        assert_eq!(-CelsiusDelta::new(4.0), CelsiusDelta::new(-4.0));
        assert_eq!(CelsiusDelta::new(-0.5).abs(), CelsiusDelta::new(0.5));
    }

    #[test]
    fn clamp() {
        let lo = Celsius::new(10.0);
        let hi = Celsius::new(30.0);
        assert_eq!(Celsius::new(5.0).clamp(lo, hi), lo);
        assert_eq!(Celsius::new(35.0).clamp(lo, hi), hi);
        assert_eq!(Celsius::new(20.0).clamp(lo, hi), Celsius::new(20.0));
        assert_eq!(
            Celsius::new(20.0).clamp(hi, lo),
            hi,
            "lo wins when bounds cross"
        );
    }
}
