//! REST control-plane client.

use std::sync::RwLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use afv_proto::api::{
    self, AuthRequest, CrossCoupleGroup, PostCallsignResponse, Station, StationTransceiver, Transceiver, VoiceServer,
    path,
};
use afv_proto::consts;
use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::de::DeserializeOwned;
use tokio::time::Instant;

use crate::config::Config;
use crate::error::{AuthError, Error, Result};

pub(crate) struct Api {
    http: reqwest::Client,
    config: Config,
    token: RwLock<Option<String>>,
}

impl Api {
    pub(crate) fn new(config: Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("afv-rs/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(config.timing.connect_timeout)
            .timeout(config.timing.http_timeout)
            .build()?;
        Ok(Self { http, config, token: RwLock::new(None) })
    }

    pub(crate) fn config(&self) -> &Config {
        &self.config
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.config.api_url.trim_end_matches('/'), path)
    }

    /// Logs in and stores the bearer token. Returns when the token should be refreshed.
    pub(crate) async fn authenticate(&self) -> Result<Instant> {
        let body = AuthRequest {
            username: self.config.credentials.username.clone(),
            password: self.config.credentials.password.clone(),
            client: self.config.client_name.clone(),
        };
        let resp = self.http.post(self.url(path::AUTH)).json(&body).send().await?;
        match resp.status() {
            StatusCode::BAD_REQUEST => return Err(AuthError::BadRequest.into()),
            StatusCode::UNAUTHORIZED => return Err(AuthError::BadPassword.into()),
            StatusCode::FORBIDDEN => return Err(AuthError::Rejected.into()),
            s if !s.is_success() => return Err(status_error(path::AUTH, s)),
            _ => {}
        }
        let token = resp.text().await?.trim().to_owned();
        let invalid = |message: String| Error::InvalidResponse { endpoint: path::AUTH.into(), message };
        if token.is_empty() {
            return Err(invalid("empty token".into()));
        }
        let expiry = api::token_expiry(&token).map_err(|e| invalid(e.to_string()))?;
        let margin = self.config.timing.token_refresh_margin;
        let lifetime = match expiry {
            Some(exp) => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
                Duration::from_secs(exp).saturating_sub(now)
            }
            None => consts::DEFAULT_TOKEN_LIFETIME,
        };
        if lifetime <= margin {
            return Err(AuthError::TokenExpired.into());
        }
        *self.token.write().unwrap() = Some(token);
        tracing::debug!(?lifetime, "authenticated");
        Ok(Instant::now() + (lifetime - margin))
    }

    pub(crate) fn clear_token(&self) {
        *self.token.write().unwrap() = None;
    }

    fn request(&self, method: Method, path: &str) -> Result<RequestBuilder> {
        let token = self.token.read().unwrap().clone().ok_or(Error::NotConnected)?;
        Ok(self.http.request(method, self.url(path)).bearer_auth(token))
    }

    /// Sends `req`; an unsuccessful status is an error.
    async fn send(&self, req: RequestBuilder, endpoint: &str) -> Result<Response> {
        successful(req.send().await?, endpoint)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        json(self.send(self.request(Method::GET, path)?, path).await?, path).await
    }

    fn callsign_path(&self) -> String {
        path::callsign(&self.config.credentials.username, &self.config.callsign)
    }

    /// Allocates a voice session for the configured callsign.
    pub(crate) async fn allocate_voice_session(&self) -> Result<VoiceServer> {
        let path = self.callsign_path();
        let resp = self.send(self.request(Method::POST, &path)?, &path).await?;
        Ok(json::<PostCallsignResponse>(resp, &path).await?.voice_server)
    }

    pub(crate) async fn release_voice_session(&self) -> Result<()> {
        let path = self.callsign_path();
        self.send(self.request(Method::DELETE, &path)?, &path).await.map(drop)
    }

    pub(crate) async fn post_transceivers(&self, transceivers: &[Transceiver]) -> Result<()> {
        let path = path::transceivers(&self.config.credentials.username, &self.config.callsign);
        let body: Vec<&Transceiver> = transceivers.iter().filter(|t| t.frequency > 0).collect();
        self.send(self.request(Method::POST, &path)?.json(&body), &path).await.map(drop)
    }

    pub(crate) async fn post_cross_couple_groups(&self, groups: &[CrossCoupleGroup]) -> Result<()> {
        let path = path::cross_couple_groups(&self.config.credentials.username, &self.config.callsign);
        self.send(self.request(Method::POST, &path)?.json(groups), &path).await.map(drop)
    }

    pub(crate) async fn station(&self, name: &str) -> Result<Option<Station>> {
        let path = path::station(name);
        let resp = self.request(Method::GET, &path)?.send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        json(successful(resp, &path)?, &path).await.map(Some)
    }

    pub(crate) async fn station_transceivers(&self, name: &str) -> Result<Vec<StationTransceiver>> {
        self.get_json(&path::station_transceivers(name)).await
    }

    pub(crate) async fn vccs(&self, name: &str) -> Result<Vec<Station>> {
        self.get_json(&path::station_vccs(name)).await
    }

    pub(crate) async fn aliased_stations(&self) -> Result<Vec<Station>> {
        self.get_json(path::STATIONS_ALIASED).await
    }
}

fn status_error(endpoint: &str, status: StatusCode) -> Error {
    Error::Status { endpoint: endpoint.into(), status: status.as_u16() }
}

fn successful(resp: Response, endpoint: &str) -> Result<Response> {
    if resp.status().is_success() { Ok(resp) } else { Err(status_error(endpoint, resp.status())) }
}

async fn json<T: DeserializeOwned>(resp: Response, endpoint: &str) -> Result<T> {
    resp.json().await.map_err(|e| Error::InvalidResponse { endpoint: endpoint.into(), message: e.to_string() })
}
