use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::auth::{Credentials, Error, checked};

#[derive(Deserialize)]
pub struct Episode {
    pub eid: String,
    pub title: String,
    pub duration: Option<u64>,
    pub podcast: Option<Podcast>,
}

pub struct Playable {
    pub eid: String,
    pub pid: String,
    pub title: String,
    pub url: String,
    pub duration: Option<u64>,
    pub start: f64,
}

#[derive(Clone, Serialize, PartialEq)]
pub struct ProgressUpdate {
    pub eid: String,
    pub pid: String,
    pub progress: u64,
    #[serde(rename = "playedAt")]
    pub played_at: String,
}

impl ProgressUpdate {
    pub fn new(eid: &str, pid: &str, progress: u64) -> Self {
        Self {
            eid: eid.into(),
            pid: pid.into(),
            progress,
            played_at: utc_timestamp(),
        }
    }
}

fn utc_timestamp() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

#[derive(Deserialize)]
pub struct Podcast {
    pub title: String,
}

pub struct PlaylistEntry {
    pub eid: String,
    pub episode: Result<Episode, Error>,
    pub progress: Option<f64>,
    pub progress_failed: bool,
}

impl PlaylistEntry {
    pub fn remaining(&self) -> Option<u64> {
        let duration = self.episode.as_ref().ok()?.duration?;
        let progress = self.progress?;
        Some(duration.saturating_sub(progress.floor() as u64))
    }
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
    #[serde(rename = "loadMoreKey")]
    cursor: Option<Value>,
}

#[derive(Deserialize)]
struct Queue {
    list: Vec<String>,
    sha: Option<String>,
}

#[derive(Deserialize)]
struct PlaybackProgress {
    eid: String,
    progress: f64,
}

#[derive(Clone)]
pub struct Api {
    client: Client,
    origin: String,
    device_id: String,
}

impl Api {
    pub fn new() -> Result<Self, Error> {
        Ok(Self {
            client: Client::builder()
                .user_agent(concat!("xyz-tui/", env!("CARGO_PKG_VERSION")))
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(8))
                .timeout(Duration::from_secs(15))
                .build()?,
            origin: "https://api.xiaoyuzhoufm.com".into(),
            device_id: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub fn with_device_id(mut self, device_id: String) -> Self {
        self.device_id = device_id;
        self
    }

    #[cfg(test)]
    pub fn for_test(origin: String) -> Self {
        let mut api = Self::new().unwrap();
        api.origin = origin;
        api
    }

    pub async fn playlist(&self, credentials: &Credentials) -> Result<Vec<PlaylistEntry>, Error> {
        let mut access = header::HeaderValue::from_str(&credentials.access_token)
            .map_err(|_| Error::InvalidResponse)?;
        access.set_sensitive(true);
        let ids = self.playlist_ids(&access).await?;
        let (progress, progress_failed) = match self.playback_progress(&ids, &access).await {
            Ok(progress) => (progress, false),
            Err(error @ (Error::Http(StatusCode::UNAUTHORIZED) | Error::RateLimited(_))) => {
                return Err(error);
            }
            Err(_) => (HashMap::new(), true),
        };
        // Resolve a few episodes at a time, retaining the queue's order.
        stream::iter(ids.into_iter().map(|eid| {
            let access = &access;
            let progress = progress.get(&eid).copied();
            async move {
                let episode = self.episode(&eid, access).await;
                match episode {
                    Err(Error::Http(StatusCode::UNAUTHORIZED)) => {
                        Err(Error::Http(StatusCode::UNAUTHORIZED))
                    }
                    Err(error @ Error::RateLimited(_)) => Err(error),
                    episode => Ok(PlaylistEntry {
                        eid,
                        episode,
                        progress,
                        progress_failed,
                    }),
                }
            }
        }))
        .buffered(4)
        .try_collect()
        .await
    }

    pub async fn playable(&self, credentials: &Credentials, eid: &str) -> Result<Playable, Error> {
        let access = access_header(credentials)?;
        let mut url = reqwest::Url::parse(&format!("{}/v1/episode/get", self.origin))
            .map_err(|_| Error::InvalidResponse)?;
        url.query_pairs_mut().append_pair("eid", eid);
        let response = self
            .client
            .get(url)
            .header("x-jike-access-token", access.clone())
            .send()
            .await?;
        let response: Envelope<Value> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        let data = response.data;
        let pid = data
            .get("pid")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(Error::InvalidResponse)?;
        let title = data
            .get("title")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(Error::InvalidResponse)?;
        if data.get("eid").and_then(Value::as_str) != Some(eid) {
            return Err(Error::InvalidResponse);
        }
        let duration = data.get("duration").and_then(Value::as_u64);
        let media_url = if data.get("isPrivateMedia").and_then(Value::as_bool) == Some(true) {
            let mut url = reqwest::Url::parse(&format!("{}/v1/private-media/get", self.origin))
                .map_err(|_| Error::InvalidResponse)?;
            url.query_pairs_mut()
                .append_pair("eid", eid)
                .append_pair("dubbing", "false");
            let response = self
                .client
                .get(url)
                .header("x-jike-access-token", access.clone())
                .send()
                .await?;
            let response: Envelope<Value> = checked(response)
                .await?
                .json()
                .await
                .map_err(|_| Error::InvalidResponse)?;
            response
                .data
                .get("url")
                .and_then(Value::as_str)
                .ok_or(Error::InvalidResponse)?
                .to_owned()
        } else {
            data.pointer("/media/source/url")
                .or_else(|| data.pointer("/enclosure/url"))
                .and_then(Value::as_str)
                .ok_or(Error::InvalidResponse)?
                .to_owned()
        };
        let parsed = reqwest::Url::parse(&media_url).map_err(|_| Error::InvalidResponse)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(Error::InvalidResponse);
        }
        let progress = self.playback_progress(&[eid.to_owned()], &access).await?;
        let start = progress.get(eid).copied().unwrap_or(0.0);
        let start = if duration.is_some_and(|duration| start >= duration as f64) {
            0.0
        } else {
            start
        };
        Ok(Playable {
            eid: eid.into(),
            pid: pid.into(),
            title: plain_text(title),
            url: media_url,
            duration,
            start,
        })
    }

    pub async fn remove_from_playlist(
        &self,
        credentials: &Credentials,
        eid: &str,
    ) -> Result<(), Error> {
        let access = access_header(credentials)?;
        for _ in 0..3 {
            let queue = self.queue_revision(&access).await?;
            let Some(position) = queue.list.iter().position(|item| item == eid) else {
                return Ok(());
            };
            let id = uuid::Uuid::new_v4().to_string().to_uppercase();
            let response = self
                .client
                .post(format!("{}/v1/playlist/patch", self.origin))
                .header("x-jike-access-token", access.clone())
                .header("x-jike-device-id", &self.device_id)
                .header("x-jike-device-name", "xyz-tui")
                .json(&json!({
                    "id": id,
                    "base": queue.sha,
                    "ops": [{"action":"rem", "item":eid, "pos":position}]
                }))
                .send()
                .await?;
            if response.status() == StatusCode::CONFLICT {
                continue;
            }
            let response: Envelope<Value> = checked(response)
                .await?
                .json()
                .await
                .map_err(|_| Error::InvalidResponse)?;
            match response.data.get("kind").and_then(Value::as_str) {
                Some("ACK") => {
                    if response.data.get("id").and_then(Value::as_str) != Some(&id)
                        || response
                            .data
                            .get("sha")
                            .and_then(Value::as_str)
                            .is_none_or(str::is_empty)
                    {
                        return Err(Error::InvalidResponse);
                    }
                    let confirmed = self.queue_revision(&access).await?;
                    return if confirmed.list.iter().any(|item| item == eid) {
                        Err(Error::PlaylistChanged)
                    } else {
                        Ok(())
                    };
                }
                // Reload the authoritative list and recompute the item's position.
                Some("FIX" | "REJECT") => continue,
                _ => return Err(Error::InvalidResponse),
            }
        }
        Err(Error::PlaylistChanged)
    }

    async fn queue_revision(&self, access: &header::HeaderValue) -> Result<Queue, Error> {
        let response = self
            .client
            .post(format!("{}/v1/playlist/pull", self.origin))
            .header("x-jike-access-token", access.clone())
            .json(&json!({}))
            .send()
            .await?;
        let response: Envelope<Queue> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        if response.cursor.is_some()
            || response.data.sha.as_deref().is_none_or(str::is_empty)
            || response.data.list.iter().any(|id| id.trim().is_empty())
        {
            return Err(Error::InvalidResponse);
        }
        Ok(response.data)
    }

    pub async fn update_progress(
        &self,
        credentials: &Credentials,
        progress: &[ProgressUpdate],
    ) -> Result<(), Error> {
        if progress.is_empty() {
            return Ok(());
        }
        let response = self
            .client
            .post(format!("{}/v1/playback-progress/update", self.origin))
            .header("x-jike-access-token", access_header(credentials)?)
            .header("local-time", utc_timestamp())
            .json(&json!({"data":progress}))
            .send()
            .await?;
        checked(response).await?;
        Ok(())
    }

    async fn playback_progress(
        &self,
        ids: &[String],
        access: &header::HeaderValue,
    ) -> Result<HashMap<String, f64>, Error> {
        let mut progress = HashMap::new();
        for batch in ids.chunks(50) {
            let response = self
                .client
                .post(format!("{}/v1/playback-progress/list", self.origin))
                .header("x-jike-access-token", access.clone())
                .json(&json!({"eids": batch}))
                .send()
                .await?;
            let response: Envelope<Vec<PlaybackProgress>> = checked(response)
                .await?
                .json()
                .await
                .map_err(|_| Error::InvalidResponse)?;
            for item in response.data {
                if !item.progress.is_finite() || item.progress < 0.0 {
                    return Err(Error::InvalidResponse);
                }
                if batch.contains(&item.eid) {
                    progress.insert(item.eid, item.progress);
                }
            }
        }
        Ok(progress)
    }

    async fn playlist_ids(&self, access: &header::HeaderValue) -> Result<Vec<String>, Error> {
        let mut ids = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut body = json!({});
        for _ in 0..100 {
            let response = self
                .client
                .post(format!("{}/v1/playlist/pull", self.origin))
                .header("x-jike-access-token", access.clone())
                .json(&body)
                .send()
                .await?;
            let page: Envelope<Queue> = checked(response)
                .await?
                .json()
                .await
                .map_err(|_| Error::InvalidResponse)?;
            if page.data.list.is_empty() && page.cursor.is_some() {
                return Err(Error::InvalidResponse);
            }
            for id in page.data.list {
                if id.trim().is_empty() {
                    return Err(Error::InvalidResponse);
                }
                if seen_ids.insert(id.clone()) {
                    ids.push(id);
                }
            }
            let Some(cursor) = page.cursor else {
                return Ok(ids);
            };
            if !seen_cursors.insert(cursor.to_string()) {
                return Err(Error::InvalidResponse);
            }
            body = json!({"loadMoreKey": cursor});
        }
        Err(Error::InvalidResponse)
    }

    async fn episode(&self, eid: &str, access: &header::HeaderValue) -> Result<Episode, Error> {
        let mut url = reqwest::Url::parse(&format!("{}/v1/episode/get", self.origin))
            .map_err(|_| Error::InvalidResponse)?;
        url.query_pairs_mut().append_pair("eid", eid);
        let response = self
            .client
            .get(url)
            .header("x-jike-access-token", access.clone())
            .send()
            .await?;
        let mut episode: Envelope<Episode> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        if episode.data.eid != eid || episode.data.title.trim().is_empty() {
            return Err(Error::InvalidResponse);
        }
        episode.data.title = plain_text(&episode.data.title);
        if let Some(podcast) = &mut episode.data.podcast {
            podcast.title = plain_text(&podcast.title);
        }
        Ok(episode.data)
    }
}

fn access_header(credentials: &Credentials) -> Result<header::HeaderValue, Error> {
    let mut value = header::HeaderValue::from_str(&credentials.access_token)
        .map_err(|_| Error::InvalidResponse)?;
    value.set_sensitive(true);
    Ok(value)
}

fn plain_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}
