use std::fmt;
use std::str::FromStr;

/// A radio frequency in Hz, written in MHz: `"122.800"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Frequency(u32);

impl Frequency {
    pub const fn from_hz(hz: u32) -> Self {
        Self(hz)
    }

    pub const fn from_khz(khz: u32) -> Self {
        Self(khz * 1000)
    }

    pub const fn hz(self) -> u32 {
        self.0
    }

    pub const fn is_nav_vhf(self) -> bool {
        108_000_000 <= self.0 && self.0 <= 117_950_000
    }

    pub const fn is_voice_vhf(self) -> bool {
        118_000_000 <= self.0 && self.0 < 137_000_000
    }

    pub const fn is_vhf(self) -> bool {
        108_000_000 <= self.0 && self.0 < 137_000_000
    }

    pub const fn is_833_channel(self) -> bool {
        self.is_voice_vhf()
            && match (self.0 / 1000) % 25 {
                5 | 10 | 15 => true,
                _ => false,
            }
    }

    /// HF radios (below 30 MHz) are single sideband and sound different. 0 is not HF.
    pub const fn is_hf(self) -> bool {
        afv_proto::consts::is_hf(self.0)
    }
}

impl fmt::Display for Frequency {
    /// MHz with exactly three decimals: `122.800`, `8.891`, `118.005`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (mhz, khz) = (self.0 / 1_000_000, (self.0 % 1_000_000) / 1000);
        write!(f, "{mhz}.{khz:03}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid frequency {0:?}: expected MHz such as \"122.800\"")]
pub struct ParseFrequencyError(String);

impl FromStr for Frequency {
    type Err = ParseFrequencyError;

    /// Parses MHz with one to three decimals, exactly: `"121.5"` is 121.500 MHz.
    /// VHF must be on the channel grid: 50 kHz for nav, 25 kHz or 8.33 kHz for voice.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseFrequencyError(s.to_owned());
        let (mhz, fraction) = s.trim().split_once('.').ok_or_else(err)?;
        if fraction.len() > 3 {
            return Err(err());
        }
        let mhz: u32 = mhz.parse().map_err(|_| err())?;
        let khz = fraction.parse::<u32>().map_err(|_| err())? * 10u32.pow(3 - fraction.len() as u32);
        let f = mhz.checked_mul(1_000_000).and_then(|hz| hz.checked_add(khz * 1000)).map(Self).ok_or_else(err)?;
        let spacing = if f.is_nav_vhf() { 50 } else { 25 };
        let off_grid = f.is_vhf() && !khz.is_multiple_of(spacing) && !f.is_833_channel();
        if f.0 == 0 || off_grid { Err(err()) } else { Ok(f) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_exactly() {
        for (text, khz) in [
            ("122.800", 122_800),
            ("122.8", 122_800),
            ("121.5", 121_500),
            ("119.105", 119_105),
            ("123.45", 123_450),
            (" 118.005 ", 118_005),
            ("8.891", 8_891),
            ("136.990", 136_990),
            ("112.150", 112_150),
            ("117.975", 117_975),
            ("137.001", 137_001),
            ("4294.967", 4_294_967),
        ] {
            assert_eq!(text.parse(), Ok(Frequency::from_khz(khz)), "{text:?}");
        }
        for bad in [
            "", ".", "121", "122.", ".5", "118.0125", "122.8000", "abc", "-1.0", "1 22.8", "1.2.3", "4294.968",
            "5000.0", "122,800", "118.001", "118.020", "121.545", "136.995", "108.004", "108.005", "112.110",
            "108.025", "117.925", "117.990", "0.0", "0.000",
        ] {
            assert!(bad.parse::<Frequency>().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn displays_in_mhz() {
        assert_eq!(Frequency::from_hz(122_800_000).to_string(), "122.800");
        assert_eq!(Frequency::from_hz(118_005_000).to_string(), "118.005");
        assert_eq!(Frequency::from_hz(121_050_000).to_string(), "121.050");
        assert_eq!(Frequency::from_hz(8_891_000).to_string(), "8.891");
        assert_eq!(Frequency::from_hz(0).to_string(), "0.000");
        for khz in [122_800, 119_105, 118_005, 8_891, 4_294_967] {
            let f = Frequency::from_khz(khz);
            assert_eq!(f.to_string().parse(), Ok(f));
        }
    }

    #[test]
    fn vhf_833_channels() {
        assert!(Frequency::from_hz(119_105_000).is_833_channel());
        assert!(!Frequency::from_hz(121_500_000).is_833_channel());
        assert!(!Frequency::from_hz(138_105_000).is_833_channel());
    }
}
