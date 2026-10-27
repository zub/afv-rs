//! REST control plane: JSON bodies and endpoint paths.
//!
//! JSON key casing differs between endpoints; the serde attributes below reproduce exactly what
//! afv-native sends and accepts.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Deserializer, Serialize};

use crate::frame::ChannelConfig;
use crate::{Error, Result};

/// Endpoint paths, relative to the API base URL. Path segments are percent-encoded.
pub mod path {
    use super::encode_segment as seg;

    pub const AUTH: &str = "/api/v1/auth";
    pub const STATIONS_ALIASED: &str = "/api/v1/stations/aliased";

    /// `POST` allocates a voice session, `DELETE` releases it.
    pub fn callsign(username: &str, callsign: &str) -> String {
        format!("/api/v1/users/{}/callsigns/{}", seg(username), seg(callsign))
    }

    pub fn transceivers(username: &str, callsign: &str) -> String {
        format!("{}/transceivers", self::callsign(username, callsign))
    }

    pub fn cross_couple_groups(username: &str, callsign: &str) -> String {
        format!("{}/crossCoupleGroups", self::callsign(username, callsign))
    }

    pub fn station(name: &str) -> String {
        format!("/api/v1/stations/byName/{}", seg(name))
    }

    pub fn station_transceivers(name: &str) -> String {
        format!("{}/transceivers/allDistinctObeyExclusions", station(name))
    }

    pub fn station_vccs(name: &str) -> String {
        format!("{}/vccsStations", station(name))
    }
}

fn encode_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Body of `POST /api/v1/auth`. The response is the bearer token (a JWT) as plain text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthRequest {
    /// VATSIM CID.
    pub username: String,
    pub password: String,
    /// Free-form client software name.
    pub client: String,
}

/// Response of `POST /api/v1/users/{username}/callsigns/{callsign}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostCallsignResponse {
    #[serde(rename = "voiceServer")]
    pub voice_server: VoiceServer,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceServer {
    /// `host:port` of the UDP voice server.
    #[serde(rename = "addressIpV4")]
    pub address_ipv4: String,
    #[serde(rename = "addressIpV6", default, skip_serializing_if = "Option::is_none")]
    pub address_ipv6: Option<String>,
    #[serde(rename = "channelConfig")]
    pub channel_config: ChannelConfig,
}

/// One transceiver in `POST .../transceivers`. The array replaces the full set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transceiver {
    #[serde(rename = "ID")]
    pub id: u16,
    /// Hz.
    #[serde(rename = "Frequency")]
    pub frequency: u32,
    #[serde(rename = "LatDeg")]
    pub lat_deg: f64,
    #[serde(rename = "LonDeg")]
    pub lon_deg: f64,
    #[serde(rename = "HeightMslM")]
    pub height_msl_m: f64,
    #[serde(rename = "HeightAglM")]
    pub height_agl_m: f64,
    /// Remote controlled outlet.
    #[serde(rename = "rco", default)]
    pub rco: bool,
}

/// One group in `POST .../crossCoupleGroups`. Audio received on any transceiver of a group
/// is retransmitted by the server on the others.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrossCoupleGroup {
    #[serde(rename = "ID")]
    pub id: u16,
    #[serde(rename = "TransceiverIDs")]
    pub transceiver_ids: Vec<u16>,
}

/// A station from the station database.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Station {
    pub id: String,
    pub name: String,
    /// Hz. Missing or non-integer in the response becomes `None`.
    #[serde(default, deserialize_with = "lenient_u32", skip_serializing_if = "Option::is_none")]
    pub frequency: Option<u32>,
    #[serde(default, deserialize_with = "lenient_u32", skip_serializing_if = "Option::is_none")]
    pub frequency_alias: Option<u32>,
}

/// A physical antenna site belonging to a station.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StationTransceiver {
    pub id: String,
    pub name: String,
    pub lat_deg: f64,
    pub lon_deg: f64,
    pub height_msl_m: f64,
    pub height_agl_m: f64,
    #[serde(default)]
    pub rco: bool,
}

fn lenient_u32<'de, D: Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    Ok(v.as_u64().and_then(|n| u32::try_from(n).ok()).filter(|&n| n != 0))
}

/// Reads the `exp` claim (Unix seconds) from a JWT without verifying its signature,
/// which is all a client needs to schedule re-authentication.
pub fn token_expiry(token: &str) -> Result<Option<u64>> {
    let payload = token.trim().split('.').nth(1).ok_or(Error::InvalidToken("not a JWT"))?;
    let json = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| Error::InvalidToken("payload is not base64url"))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&json).map_err(|_| Error::InvalidToken("payload is not JSON"))?;
    Ok(claims.get("exp").and_then(|e| e.as_u64().or_else(|| e.as_f64().map(|f| f as u64))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths() {
        assert_eq!(path::callsign("1234567", "SAS123"), "/api/v1/users/1234567/callsigns/SAS123");
        assert_eq!(path::transceivers("1234567", "ENGM_TWR"), "/api/v1/users/1234567/callsigns/ENGM_TWR/transceivers");
        assert_eq!(path::station_vccs("A B/C"), "/api/v1/stations/byName/A%20B%2FC/vccsStations");
    }

    #[test]
    fn transceiver_json_keys() {
        let t = Transceiver {
            id: 0,
            frequency: 122_800_000,
            lat_deg: 59.9,
            lon_deg: 10.7,
            height_msl_m: 120.0,
            height_agl_m: 0.0,
            rco: false,
        };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"ID":0,"Frequency":122800000,"LatDeg":59.9,"LonDeg":10.7,"HeightMslM":120.0,"HeightAglM":0.0,"rco":false})
        );
    }

    #[test]
    fn cross_couple_json_keys() {
        let g = CrossCoupleGroup { id: 1, transceiver_ids: vec![2, 3] };
        assert_eq!(serde_json::to_value(&g).unwrap(), json!({"ID":1,"TransceiverIDs":[2,3]}));
    }

    #[test]
    fn post_callsign_response() {
        let key_a = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";
        let key_b = "/////////////////////////////////////////w8=";
        let body = json!({
            "voiceServer": {
                "addressIpV4": "1.2.3.4:50000",
                "addressIpV6": "",
                "channelConfig": {
                    "channelTag": "tag",
                    "aeadReceiveKey": key_a,
                    "aeadTransmitKey": key_b,
                    "hmacKey": null
                }
            }
        });
        let r: PostCallsignResponse = serde_json::from_value(body).unwrap();
        let cc = &r.voice_server.channel_config;
        assert_eq!(cc.channel_tag, "tag");
        assert_eq!(cc.receive_key[31], 31);
        assert_eq!(cc.transmit_key[31], 0x0f);
    }

    #[test]
    fn wrong_key_length_is_rejected() {
        let body = json!({"channelTag":"t","aeadReceiveKey":"AAAA","aeadTransmitKey":"AAAA"});
        assert!(serde_json::from_value::<ChannelConfig>(body).is_err());
    }

    #[test]
    fn station_with_missing_or_odd_frequencies() {
        let s: Station = serde_json::from_value(json!({"id":"1","name":"X","frequency":null})).unwrap();
        assert_eq!(s.frequency, None);
        let s: Station =
            serde_json::from_value(json!({"id":"1","name":"X","frequency":118300000,"frequencyAlias":"x"})).unwrap();
        assert_eq!(s.frequency, Some(118_300_000));
        assert_eq!(s.frequency_alias, None);
    }

    #[test]
    fn jwt_expiry() {
        // {"alg":"none"} . {"sub":"x","exp":1900000000} .
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"x","exp":1900000000}"#);
        let token = format!("eyJhbGciOiJub25lIn0.{payload}.");
        assert_eq!(token_expiry(&token).unwrap(), Some(1_900_000_000));
        let no_exp = format!("eyJhbGciOiJub25lIn0.{}.", URL_SAFE_NO_PAD.encode(b"{}"));
        assert_eq!(token_expiry(&no_exp).unwrap(), None);
        assert!(token_expiry("garbage").is_err());
    }
}
