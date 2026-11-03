//! The radio model: what the user tunes, and how that becomes the transceivers and
//! cross-couple groups the server knows about.
//!
//! Pilot, ATC and ATIS clients differ only in their radio configurations. A pilot radio
//! transmits from the aircraft's position; an ATC radio usually from a station's antenna sites.

use afv_proto::api::{CrossCoupleGroup, StationTransceiver, Transceiver};

pub use afv_audio::{Hardware, Pan};

use crate::Frequency;

/// Identifies a radio within a client. Never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RadioId(pub(crate) u32);

/// A position on the earth (WGS84).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Position {
    pub lat_deg: f64,
    pub lon_deg: f64,
    /// Height above mean sea level, metres.
    pub height_msl_m: f64,
    /// Height above ground, metres.
    pub height_agl_m: f64,
}

/// An antenna site.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Site {
    pub position: Position,
    /// Remote controlled outlet.
    pub rco: bool,
}

impl From<&StationTransceiver> for Site {
    fn from(t: &StationTransceiver) -> Self {
        Self {
            position: Position {
                lat_deg: t.lat_deg,
                lon_deg: t.lon_deg,
                height_msl_m: t.height_msl_m,
                height_agl_m: t.height_agl_m,
            },
            rco: t.rco,
        }
    }
}

/// Where a radio transmits and receives from.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Sites {
    /// The client's own position (see `Client::set_position`).
    #[default]
    Own,
    /// Fixed antenna sites, typically a station's from `Client::station_transceivers`.
    Fixed(Vec<Site>),
}

impl Sites {
    pub fn from_station(transceivers: &[StationTransceiver]) -> Self {
        Self::Fixed(transceivers.iter().map(Site::from).collect())
    }
}

/// Cross-coupling (ATC): the server retransmits what one transceiver receives on the others of
/// its group, so stations that cannot hear each other directly do. Only applies to radios with
/// both `rx` and `tx` enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum Coupling {
    #[default]
    None,
    /// Couple this radio's own sites with each other.
    Sites,
    /// Couple with every other radio set to `Across`.
    Across,
}

/// The two output buses, usually driven by different devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub enum OutputBus {
    #[default]
    Headset,
    Speaker,
}

/// Where a radio's audio is played.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Output {
    pub bus: OutputBus,
    pub pan: Pan,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RadioConfig {
    pub frequency: Frequency,
    pub sites: Sites,
    /// Play what is received.
    pub rx: bool,
    /// Transmit while PTT is pressed.
    pub tx: bool,
    pub coupling: Coupling,
    pub output: Output,
    /// Output gain, linear.
    pub gain: f32,
    pub hardware: Hardware,
    /// Simulate static, noise and the radio's frequency response.
    pub effects: bool,
    /// Suppress HF background noise.
    pub hf_squelch: bool,
}

impl RadioConfig {
    /// A receive-only radio at the client's position; set `tx` to transmit on it.
    pub fn pilot(frequency: Frequency) -> Self {
        Self {
            frequency,
            sites: Sites::Own,
            rx: true,
            tx: false,
            coupling: Coupling::None,
            output: Output::default(),
            gain: 1.0,
            hardware: Hardware::default(),
            effects: true,
            hf_squelch: false,
        }
    }

    /// A receive-only radio on a station's antenna sites.
    pub fn atc(frequency: Frequency, sites: Sites) -> Self {
        Self { sites, ..Self::pilot(frequency) }
    }

    /// Whether the server needs transceivers for this radio.
    fn active(&self) -> bool {
        self.frequency.hz() != 0 && (self.rx || self.tx)
    }

    /// The local audio settings for this radio.
    pub(crate) fn audio_settings(&self) -> afv_audio::RadioSettings {
        afv_audio::RadioSettings {
            frequency: self.frequency.hz(),
            gain: self.gain,
            effects: self.effects,
            hf_squelch: self.hf_squelch,
            hardware: self.hardware,
            muted: false,
            pan: self.output.pan,
        }
    }
}

/// What the server must be told for a set of radios, and which transceivers to transmit on.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Plan {
    pub transceivers: Vec<Transceiver>,
    /// `None` if no radio uses coupling.
    pub groups: Option<Vec<CrossCoupleGroup>>,
    /// Transceivers of radios with `tx` enabled.
    pub tx: Vec<u16>,
}

impl Plan {
    /// Assigns transceiver ids 0, 1, … in radio order, then site order. A radio on its own
    /// position gets no transceiver until the position is known.
    pub fn build<'a>(radios: impl IntoIterator<Item = &'a RadioConfig>, position: Option<Position>) -> Self {
        let mut plan = Self::default();
        let mut across = Vec::new();
        let mut site_groups = Vec::new();
        let mut uses_coupling = false;
        for radio in radios {
            uses_coupling |= radio.coupling != Coupling::None;
            if !radio.active() {
                continue;
            }
            let own;
            let sites: &[Site] = match &radio.sites {
                Sites::Own => match position {
                    Some(position) => {
                        own = [Site { position, rco: false }];
                        &own
                    }
                    None => &[],
                },
                Sites::Fixed(sites) => sites,
            };
            let first = plan.transceivers.len() as u16;
            for site in sites {
                plan.transceivers.push(Transceiver {
                    id: plan.transceivers.len() as u16,
                    frequency: radio.frequency.hz(),
                    lat_deg: site.position.lat_deg,
                    lon_deg: site.position.lon_deg,
                    height_msl_m: site.position.height_msl_m,
                    height_agl_m: site.position.height_agl_m,
                    rco: site.rco,
                });
            }
            let ids = first..plan.transceivers.len() as u16;
            if radio.tx {
                plan.tx.extend(ids.clone());
            }
            if radio.rx && radio.tx && !ids.is_empty() {
                match radio.coupling {
                    Coupling::None => {}
                    Coupling::Sites => site_groups.push(ids.collect()),
                    Coupling::Across => across.extend(ids),
                }
            }
        }
        if uses_coupling {
            // Group 0 is always present; each `Sites` radio gets its own group after it.
            let groups = std::iter::once(across).chain(site_groups);
            plan.groups = Some(
                groups.enumerate().map(|(id, ids)| CrossCoupleGroup { id: id as u16, transceiver_ids: ids }).collect(),
            );
        }
        plan
    }

    /// Whether the server's view differs: everything except positions, which only need to reach
    /// the server with the next periodic refresh.
    pub fn differs_from(&self, other: &Plan) -> bool {
        let key = |p: &Plan| p.transceivers.iter().map(|t| (t.id, t.frequency)).collect::<Vec<_>>();
        key(self) != key(other) || self.groups != other.groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POS: Position = Position { lat_deg: 60.0, lon_deg: 11.0, height_msl_m: 300.0, height_agl_m: 0.0 };

    fn site(lat: f64) -> Site {
        Site { position: Position { lat_deg: lat, ..POS }, rco: false }
    }

    fn freq(mhz: &str) -> Frequency {
        mhz.parse().unwrap()
    }

    fn summary(plan: &Plan) -> Vec<(u16, u32, f64)> {
        plan.transceivers.iter().map(|t| (t.id, t.frequency, t.lat_deg)).collect()
    }

    #[test]
    fn pilot_radios_get_one_transceiver_each_at_the_aircraft() {
        let com1 = RadioConfig { tx: true, ..RadioConfig::pilot(freq("122.800")) };
        let com2 = RadioConfig::pilot(freq("121.500"));
        let plan = Plan::build([&com1, &com2], Some(POS));
        assert_eq!(summary(&plan), [(0, 122_800_000, 60.0), (1, 121_500_000, 60.0)]);
        assert_eq!(plan.tx, [0]);
        assert_eq!(plan.groups, None);
    }

    #[test]
    fn own_sites_wait_for_a_position() {
        let com1 = RadioConfig { tx: true, ..RadioConfig::pilot(freq("122.800")) };
        let plan = Plan::build([&com1], None);
        assert!(plan.transceivers.is_empty() && plan.tx.is_empty());
    }

    #[test]
    fn inactive_radios_are_left_out_and_ids_stay_dense() {
        let off = RadioConfig { rx: false, tx: false, ..RadioConfig::pilot(freq("122.800")) };
        let untuned = RadioConfig::pilot(Frequency::from_hz(0));
        let tx_only = RadioConfig { rx: false, tx: true, ..RadioConfig::pilot(freq("121.500")) };
        let plan = Plan::build([&off, &untuned, &tx_only], Some(POS));
        // A transmit-only radio still needs a transceiver to transmit from.
        assert_eq!(summary(&plan), [(0, 121_500_000, 60.0)]);
        assert_eq!(plan.tx, [0]);
    }

    #[test]
    fn atc_radios_use_station_sites_and_couple() {
        let twr = RadioConfig {
            tx: true,
            coupling: Coupling::Across,
            ..RadioConfig::atc(freq("118.300"), Sites::Fixed(vec![site(60.1), site(60.2)]))
        };
        let gnd = RadioConfig { tx: true, coupling: Coupling::Across, ..RadioConfig::atc(freq("121.700"), Sites::Own) };
        let app = RadioConfig {
            tx: true,
            coupling: Coupling::Sites,
            ..RadioConfig::atc(freq("120.450"), Sites::Fixed(vec![site(59.0), site(61.0), site(62.0)]))
        };
        let monitor = RadioConfig { coupling: Coupling::Across, ..RadioConfig::atc(freq("121.500"), Sites::Own) };
        let plan = Plan::build([&twr, &gnd, &app, &monitor], Some(POS));
        assert_eq!(
            summary(&plan),
            [
                (0, 118_300_000, 60.1),
                (1, 118_300_000, 60.2),
                (2, 121_700_000, 60.0),
                (3, 120_450_000, 59.0),
                (4, 120_450_000, 61.0),
                (5, 120_450_000, 62.0),
                (6, 121_500_000, 60.0),
            ]
        );
        assert_eq!(plan.tx, [0, 1, 2, 3, 4, 5]);
        // The receive-only monitor radio is not coupled.
        assert_eq!(
            plan.groups,
            Some(vec![
                CrossCoupleGroup { id: 0, transceiver_ids: vec![0, 1, 2] },
                CrossCoupleGroup { id: 1, transceiver_ids: vec![3, 4, 5] },
            ])
        );
    }

    #[test]
    fn coupling_sends_an_empty_group_0_when_nothing_qualifies() {
        let rx_only = RadioConfig { coupling: Coupling::Across, ..RadioConfig::atc(freq("118.300"), Sites::Own) };
        let plan = Plan::build([&rx_only], Some(POS));
        assert_eq!(plan.groups, Some(vec![CrossCoupleGroup { id: 0, transceiver_ids: vec![] }]));
    }

    #[test]
    fn only_frequencies_and_groups_count_as_a_difference() {
        let com1 = RadioConfig::pilot(freq("122.800"));
        let base = Plan::build([&com1], Some(POS));
        let moved = Plan::build([&com1], Some(Position { lat_deg: 61.0, ..POS }));
        assert!(!moved.differs_from(&base));
        let retuned = Plan::build([&RadioConfig::pilot(freq("121.500"))], Some(POS));
        assert!(retuned.differs_from(&base));
        let coupled = Plan::build([&RadioConfig { coupling: Coupling::Across, ..com1 }], Some(POS));
        assert!(coupled.differs_from(&base));
    }

    #[test]
    fn station_transceivers_become_sites() {
        let st = StationTransceiver {
            id: "x".into(),
            name: "ENGM".into(),
            lat_deg: 60.19,
            lon_deg: 11.1,
            height_msl_m: 210.0,
            height_agl_m: 25.0,
            rco: true,
        };
        let Sites::Fixed(sites) = Sites::from_station(&[st]) else { panic!() };
        assert_eq!(sites[0].position.height_agl_m, 25.0);
        assert!(sites[0].rco);
    }
}
