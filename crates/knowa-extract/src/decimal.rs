//! Exact rational arithmetic for EXIF values and their deterministic decimal form.
//!
//! Formatting rule (used for every decimal the extractors emit): round the exact
//! value half away from zero to at most `MAX_FRACTION_DIGITS` fraction digits,
//! strip trailing zeros, drop the point for integers, and never write `-0`.
//! Seven digits is ~1.1 cm of latitude, finer than any consumer GPS.

pub(crate) const MAX_FRACTION_DIGITS: u32 = 7;

/// A non-negative fraction `num / den`, `den > 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ratio {
    num: u128,
    den: u128,
}

fn gcd(mut a: u128, mut b: u128) -> u128 {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

impl Ratio {
    /// `None` for a zero denominator.
    pub(crate) fn new(num: u128, den: u128) -> Option<Ratio> {
        if den == 0 {
            return None;
        }
        // den > 0, so the gcd is ≥ 1.
        let g = gcd(num, den);
        Some(Ratio { num: num / g, den: den / g })
    }

    /// `None` on overflow (only reachable with values far outside any valid range).
    pub(crate) fn add(self, other: Ratio) -> Option<Ratio> {
        let den = self.den.checked_mul(other.den)?;
        let num = self.num.checked_mul(other.den)?.checked_add(other.num.checked_mul(self.den)?)?;
        Ratio::new(num, den)
    }

    pub(crate) fn div_int(self, k: u128) -> Option<Ratio> {
        Ratio::new(self.num, self.den.checked_mul(k)?)
    }

    /// Decimal string per the module rule; `negative` applies the sign. `None` only
    /// for denominators near `u128::MAX`, which no EXIF value produces.
    pub(crate) fn to_decimal(self, negative: bool) -> Option<String> {
        let scale = 10u128.pow(MAX_FRACTION_DIGITS);
        let mut int = self.num / self.den;
        let mut rem = self.num % self.den;
        let mut frac = 0u128;
        for _ in 0..MAX_FRACTION_DIGITS {
            rem = rem.checked_mul(10)?;
            frac = frac * 10 + rem / self.den;
            rem %= self.den;
        }
        if rem.checked_mul(2)? >= self.den {
            frac += 1;
            if frac == scale {
                frac = 0;
                int += 1;
            }
        }
        let mut out = String::new();
        if negative && (int != 0 || frac != 0) {
            out.push('-');
        }
        out.push_str(&int.to_string());
        if frac != 0 {
            let digits = format!("{frac:0width$}", width = MAX_FRACTION_DIGITS as usize);
            out.push('.');
            out.push_str(digits.trim_end_matches('0'));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(n: u128, d: u128) -> Ratio {
        Ratio::new(n, d).unwrap()
    }

    fn dec(x: Ratio, negative: bool) -> String {
        x.to_decimal(negative).unwrap()
    }

    #[test]
    fn formats_deterministically() {
        assert_eq!(dec(r(13976, 100), false), "139.76");
        assert_eq!(dec(r(35, 1), false), "35");
        assert_eq!(dec(r(1, 3), false), "0.3333333");
        assert_eq!(dec(r(2, 3), true), "-0.6666667");
        // exactly half a unit in the 7th place rounds away from zero
        assert_eq!(dec(r(5, 100_000_000), false), "0.0000001");
        assert_eq!(dec(r(5, 100_000_000), true), "-0.0000001");
        // just below half rounds to zero, and zero is never negative
        assert_eq!(dec(r(4, 100_000_000), true), "0");
        // carry out of the fraction
        assert_eq!(dec(r(999_999_995, 1_000_000_000), false), "1");
    }

    #[test]
    fn rejects_zero_denominator_and_overflow() {
        assert!(Ratio::new(1, 0).is_none());
        let huge = r(1, u128::MAX / 3);
        assert!(huge.add(r(1, u128::MAX / 5)).is_none());
        assert!(r(u128::MAX / 3 - 1, u128::MAX / 3).to_decimal(false).is_none());
    }
}
