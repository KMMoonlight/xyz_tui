use std::{fmt, time::Duration};

use reqwest::{Client, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const ORIGIN: &str = "https://web-api.xiaoyuzhoufm.com";
const REFRESH_ORIGIN: &str = "https://api.xiaoyuzhoufm.com";
const USER_AGENT: &str = "Mozilla/5.0 (Linux; Android 13; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Mobile Safari/537.36";

// Deliberately no Debug: these values must never appear in logs or errors.
#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
}

impl Credentials {
    pub fn is_complete(&self) -> bool {
        !self.access_token.trim().is_empty() && !self.refresh_token.trim().is_empty()
    }
}

#[derive(Deserialize)]
pub struct Challenge {
    pub id: String,
    pub url: String,
}

pub enum Scan {
    Waiting,
    Scanned,
    Expired,
    Authenticated(Credentials),
}

#[derive(Debug)]
pub enum Error {
    Network,
    Timeout,
    Http(StatusCode),
    RateLimited(Duration),
    InvalidResponse,
    MissingCredentials,
    PlaylistChanged,
}

impl Error {
    pub fn retry_delay(&self, attempt: u32) -> Option<Duration> {
        match self {
            Self::RateLimited(delay) => Some(*delay),
            Self::Network | Self::Timeout => Some(Duration::from_secs(2_u64.pow(attempt))),
            Self::Http(status) if status.is_server_error() => {
                Some(Duration::from_secs(2_u64.pow(attempt)))
            }
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network => f.write_str("网络连接失败"),
            Self::Timeout => f.write_str("连接超时"),
            Self::RateLimited(_) => f.write_str("请求过于频繁，稍后重试"),
            Self::Http(StatusCode::UNAUTHORIZED) => f.write_str("登录已失效"),
            Self::Http(status) => write!(f, "服务暂不可用（{}）", status.as_u16()),
            Self::InvalidResponse => f.write_str("响应数据异常"),
            Self::MissingCredentials => f.write_str("未收到登录凭据，请重新扫码"),
            Self::PlaylistChanged => f.write_str("播放列表已更新，请刷新后重试"),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            Self::Timeout
        } else {
            Self::Network
        }
    }
}

#[derive(Clone)]
pub struct Api {
    client: Client,
    origin: String,
    refresh_origin: String,
}

impl Api {
    #[cfg(test)]
    pub fn for_test(origin: String) -> Self {
        let mut api = Self::new().unwrap();
        api.origin = origin.clone();
        api.refresh_origin = origin;
        api
    }

    pub fn new() -> Result<Self, Error> {
        let mut headers = header::HeaderMap::new();
        headers.insert(
            header::ACCEPT,
            "application/json, text/plain, */*".parse().unwrap(),
        );
        headers.insert(
            header::ORIGIN,
            "https://accounts.xiaoyuzhoufm.com".parse().unwrap(),
        );
        headers.insert(
            header::REFERER,
            "https://accounts.xiaoyuzhoufm.com/".parse().unwrap(),
        );
        headers.insert("x-midway-app-id", "v6worU4NnWyL".parse().unwrap());
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            origin: ORIGIN.to_owned(),
            refresh_origin: REFRESH_ORIGIN.to_owned(),
        })
    }

    pub async fn create(&self) -> Result<Challenge, Error> {
        let response = self
            .client
            .post(format!("{}/v1/auth/qrcode/create", self.origin))
            .json(&json!({"clientId": "xyz-web"}))
            .send()
            .await?;
        let challenge: Challenge = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        let url = reqwest::Url::parse(&challenge.url).map_err(|_| Error::InvalidResponse)?;
        if challenge.id.trim().is_empty()
            || url.scheme() != "https"
            || url.host_str() != Some("h5.xiaoyuzhoufm.com")
            || url.path() != "/oauth"
            || !url
                .query_pairs()
                .any(|(key, value)| key == "qrcode_id" && value == challenge.id)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(challenge)
    }

    pub async fn poll(&self, id: &str) -> Result<Scan, Error> {
        let response = self
            .client
            .post(format!("{}/v1/auth/qrcode/login", self.origin))
            .json(&json!({"id": id}))
            .send()
            .await?;
        if matches!(
            response.status(),
            StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED
        ) {
            return Ok(Scan::Expired);
        }
        let response = checked(response).await?;
        // The successful polling response can be consumed only once.
        let credentials = Credentials {
            access_token: token(response.headers(), "x-jike-access-token"),
            refresh_token: token(response.headers(), "x-jike-refresh-token"),
        };
        let data: Value = response.json().await.map_err(|_| Error::InvalidResponse)?;
        match data.get("status").and_then(Value::as_str) {
            Some("WAITTING" | "WAITING" | "PENDING") => Ok(Scan::Waiting),
            Some("SCANNED") => Ok(Scan::Scanned),
            Some("EXPIRED" | "CANCELLED" | "CANCELED") => Ok(Scan::Expired),
            Some("USED" | "CONFIRMED") if credentials.is_complete() => {
                Ok(Scan::Authenticated(credentials))
            }
            Some("USED" | "CONFIRMED") => Err(Error::MissingCredentials),
            _ => Err(Error::InvalidResponse),
        }
    }

    pub async fn validate(&self, credentials: &Credentials) -> Result<bool, Error> {
        let mut access = header::HeaderValue::from_str(&credentials.access_token)
            .map_err(|_| Error::InvalidResponse)?;
        access.set_sensitive(true);
        let response = self
            .client
            .get(format!("{}/web/user/get-me", self.origin))
            .header(header::ORIGIN, "https://www.xiaoyuzhoufm.com")
            .header(header::REFERER, "https://www.xiaoyuzhoufm.com/")
            .header("x-jike-access-token", access)
            .send()
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            return Ok(false);
        }
        let data: Value = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        if data
            .pointer("/data/uid")
            .and_then(Value::as_str)
            .is_some_and(|uid| !uid.is_empty())
        {
            Ok(true)
        } else {
            Err(Error::InvalidResponse)
        }
    }

    pub async fn refresh(&self, credentials: &Credentials) -> Result<Credentials, Error> {
        let mut refresh = header::HeaderValue::from_str(&credentials.refresh_token)
            .map_err(|_| Error::InvalidResponse)?;
        refresh.set_sensitive(true);
        let response = self
            .client
            .post(format!("{}/app_auth_tokens.refresh", self.refresh_origin))
            .header("x-jike-refresh-token", refresh)
            .header(header::CONTENT_TYPE, "application/json")
            .send()
            .await?;
        let response = checked(response).await?;
        let mut renewed = Credentials {
            access_token: token(response.headers(), "x-jike-access-token"),
            refresh_token: token(response.headers(), "x-jike-refresh-token"),
        };
        // Some deployments return tokens in headers, others in the JSON body.
        if !renewed.is_complete() {
            let body: Value = response.json().await.map_err(|_| Error::InvalidResponse)?;
            for (destination, name) in [
                (&mut renewed.access_token, "x-jike-access-token"),
                (&mut renewed.refresh_token, "x-jike-refresh-token"),
            ] {
                if destination.trim().is_empty() {
                    *destination = body
                        .get(name)
                        .or_else(|| body.get("data").and_then(|data| data.get(name)))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                }
            }
        }
        if !renewed.is_complete()
            || header::HeaderValue::from_str(&renewed.access_token).is_err()
            || header::HeaderValue::from_str(&renewed.refresh_token).is_err()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(renewed)
    }
}

fn token(headers: &header::HeaderMap, name: &str) -> String {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

pub(crate) async fn checked(response: Response) -> Result<Response, Error> {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        let seconds = response
            .headers()
            .get(header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(30)
            .max(1);
        return Err(Error::RateLimited(Duration::from_secs(seconds)));
    }
    if !response.status().is_success() {
        return Err(Error::Http(response.status()));
    }
    Ok(response)
}
