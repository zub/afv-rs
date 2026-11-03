use std::fmt;
use std::str::FromStr;

const HZ_PER_MHZ: u32 = 1_000_000;
/// Decimal places of MHz that resolve 1 Hz…
const MHZ_DECIMALS: usize = 6;
/// …and the fewest shown, as frequencies are usually written.
const MHZ_DECIMALS_SHOWN: usize = 3;

/// A radio frequency in Hz, written in MHz: `"122.800"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Frequency(u32);

impl Frequency {
    pub const fn from_hz(hz: u32) -> Self {
        Self(hz)
    }

    /// Rounds to the nearest Hz. Prefer [`str::parse`] for exact values from text.
    ///
    /// # Panics
    ///
    /// If `mhz` is negative, not finite or above 4294.967295 MHz.
    pub fn mhz(mhz: f64) -> Self {
        let hz = (mhz * HZ_PER_MHZ as f64).round();
        assert!(hz.is_finite() && (0.0..=u32::MAX as f64).contains(&hz), "frequency {mhz} MHz out of range");
        Self(hz as u32)
    }

    pub const fn hz(self) -> u32 {
        self.0
    }

    /// HF radios (below 30 MHz) are single sideband and sound different. 0 is not HF.
    pub const fn is_hf(self) -> bool {
        afv_proto::consts::is_hf(self.0)
    }
}

impl fmt::Display for Frequency {
    /// MHz with at least three decimals: `122.800`, `8.891`, `118.0125`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (mhz, hz) = (self.0 / HZ_PER_MHZ, self.0 % HZ_PER_MHZ);
        let fraction = format!("{hz:0MHZ_DECIMALS$}");
        let fraction = fraction.trim_end_matches('0');
        write!(f, "{mhz}.{fraction:0<MHZ_DECIMALS_SHOWN$}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid frequency {0:?}: expected MHz such as \"122.800\"")]
pub struct ParseFrequencyError(String);

impl FromStr for Frequency {
    type Err = ParseFrequencyError;

    /// Parses MHz exactly, without going through floating point.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseFrequencyError(s.to_owned());
        let digits = |p: &str| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit());
        let (mhz, fraction) = match s.trim().split_once('.') {
            Some((mhz, fraction)) if digits(fraction) => (mhz, fraction),
            Some(_) => return Err(err()),
            None => (s.trim(), ""),
        };
        if !digits(mhz) || fraction.len() > MHZ_DECIMALS {
            return Err(err());
        }
        let mhz: u64 = mhz.parse().map_err(|_| err())?;
        let fraction: u64 = format!("{fraction:0<MHZ_DECIMALS$}").parse().map_err(|_| err())?;
        let hz = mhz.checked_mul(HZ_PER_MHZ as u64).and_then(|h| h.checked_add(fraction)).ok_or_else(err)?;
        u32::try_from(hz).map(Self).map_err(|_| err())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exactly() {
        assert_eq!("122.800".parse(), Ok(Frequency::from_hz(122_800_000)));
        assert_eq!("122.8".parse(), Ok(Frequency::from_hz(122_800_000)));
        assert_eq!(" 118.0125 ".parse(), Ok(Frequency::from_hz(118_012_500)));
        assert_eq!("8.891".parse(), Ok(Frequency::from_hz(8_891_000)));
        assert_eq!("121".parse(), Ok(Frequency::from_hz(121_000_000)));
        assert_eq!("4294.967295".parse(), Ok(Frequency::from_hz(u32::MAX)));
        for bad in ["", ".", "122.", ".5", "122.8000001", "abc", "-1.0", "1 22.8", "4294.967296", "122,800"] {
            assert!(bad.parse::<Frequency>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn displays_in_mhz() {
        assert_eq!(Frequency::from_hz(122_800_000).to_string(), "122.800");
        assert_eq!(Frequency::from_hz(118_012_500).to_string(), "118.0125");
        assert_eq!(Frequency::from_hz(8_891_000).to_string(), "8.891");
        assert_eq!(Frequency::from_hz(0).to_string(), "0.000");
        for hz in [122_800_000, 118_012_500, 8_891_000, 1, u32::MAX] {
            let f = Frequency::from_hz(hz);
            assert_eq!(f.to_string().parse(), Ok(f));
        }
    }

    #[test]
    fn mhz_rounds_to_hz() {
        assert_eq!(Frequency::mhz(122.8), Frequency::from_hz(122_800_000));
        assert_eq!(Frequency::mhz(118.0125), Frequency::from_hz(118_012_500));
        assert!(Frequency::mhz(29.999).is_hf() && !Frequency::mhz(30.0).is_hf());
        assert!(!Frequency::from_hz(0).is_hf(), "untuned is not HF");
    }
}
