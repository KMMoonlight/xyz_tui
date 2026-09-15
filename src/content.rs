use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use futures_util::{StreamExt, TryStreamExt, stream};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    auth::{Credentials, Error, checked},
    recommendations::Kind,
    transcript::Transcript,
};

const TRANSCRIPT_USER_AGENT: &str = "Xiaoyuzhou/2.99.1(android 28)";

#[derive(Clone, Default, Deserialize)]
pub struct Episode {
    pub eid: String,
    pub title: String,
    pub duration: Option<u64>,
    pub podcast: Option<Podcast>,
    #[serde(rename = "pubDate")]
    pub pub_date: Option<String>,
    pub shownotes: Option<String>,
    pub description: Option<String>,
}

pub struct Page<T> {
    pub items: Vec<T>,
    pub cursor: Option<Value>,
}

#[derive(Deserialize)]
pub struct Comment {
    pub id: String,
    pub text: String,
    pub author: Option<CommentAuthor>,
    #[serde(rename = "createdAt")]
    pub created_at: Option<String>,
    #[serde(default, rename = "likeCount")]
    pub like_count: u64,
}

#[derive(Deserialize)]
pub struct CommentAuthor {
    pub nickname: String,
}

pub struct Playable {
    pub eid: String,
    pub pid: String,
    pub title: String,
    pub url: String,
    pub duration: Option<u64>,
    pub start: f64,
    pub transcript_media_id: Option<String>,
}

pub struct RecentEpisode {
    pub episode: Episode,
    pub pid: String,
    pub progress: f64,
    pub transcript_media_id: Option<String>,
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

#[derive(Clone, Deserialize)]
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

    pub async fn recent_episode(
        &self,
        credentials: &Credentials,
    ) -> Result<Option<RecentEpisode>, Error> {
        #[derive(Deserialize)]
        struct HistoryItem {
            episode: Value,
        }
        let history: Page<HistoryItem> = self
            .page(
                credentials,
                "/v1/episode-played/list-history",
                json!({}),
                None,
            )
            .await?;
        let Some(item) = history.items.into_iter().next() else {
            return Ok(None);
        };
        let data = item.episode;
        let mut episode: Episode =
            serde_json::from_value(data.clone()).map_err(|_| Error::InvalidResponse)?;
        sanitize_episode(&mut episode)?;
        let pid = data
            .get("pid")
            .and_then(Value::as_str)
            .filter(|pid| !pid.trim().is_empty())
            .ok_or(Error::InvalidResponse)?
            .to_owned();
        let progress = self
            .playback_progress(&[episode.eid.clone()], &access_header(credentials)?)
            .await?
            .get(&episode.eid)
            .copied()
            .ok_or(Error::InvalidResponse)?;
        let progress = episode
            .duration
            .map_or(progress, |duration| progress.min(duration as f64));
        Ok(Some(RecentEpisode {
            episode,
            pid,
            progress,
            transcript_media_id: transcript_media_id(&data),
        }))
    }

    pub async fn listening_seconds(&self, credentials: &Credentials) -> Result<u64, Error> {
        #[derive(Deserialize)]
        struct Profile {
            uid: String,
        }
        #[derive(Deserialize)]
        struct Stats {
            #[serde(rename = "totalPlayedSeconds")]
            seconds: u64,
        }
        let access = access_header(credentials)?;
        let response = self
            .client
            .get(format!("{}/v1/profile/get", self.origin))
            .header("x-jike-access-token", access.clone())
            .header("x-jike-device-id", &self.device_id)
            .send()
            .await?;
        let profile: Envelope<Profile> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        if profile.data.uid.trim().is_empty() {
            return Err(Error::InvalidResponse);
        }
        let mut url = reqwest::Url::parse(&format!("{}/v1/user-stats/get", self.origin))
            .map_err(|_| Error::InvalidResponse)?;
        url.query_pairs_mut().append_pair("uid", &profile.data.uid);
        let response = self
            .client
            .get(url)
            .header("x-jike-access-token", access)
            .header("x-jike-device-id", &self.device_id)
            .send()
            .await?;
        let stats: Envelope<Stats> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        Ok(stats.data.seconds)
    }

    pub async fn listening_history(
        &self,
        credentials: &Credentials,
        cursor: Option<Value>,
    ) -> Result<Page<Episode>, Error> {
        #[derive(Deserialize)]
        struct HistoryItem {
            episode: Episode,
        }
        let page: Page<HistoryItem> = self
            .page(
                credentials,
                "/v1/episode-played/list-history",
                json!({}),
                cursor,
            )
            .await?;
        let mut items = Vec::with_capacity(page.items.len());
        let mut ids = HashSet::new();
        for mut item in page.items {
            sanitize_episode(&mut item.episode)?;
            if ids.insert(item.episode.eid.clone()) {
                items.push(item.episode);
            }
        }
        Ok(Page {
            items,
            cursor: page.cursor,
        })
    }

    pub async fn subscriptions(
        &self,
        credentials: &Credentials,
        cursor: Option<Value>,
    ) -> Result<Page<Episode>, Error> {
        let mut page: Page<Episode> = self
            .page(credentials, "/v2/inbox/list", json!({"limit":20}), cursor)
            .await?;
        let mut ids = HashSet::new();
        for episode in &mut page.items {
            sanitize_episode(episode)?;
        }
        page.items.retain(|episode| ids.insert(episode.eid.clone()));
        Ok(page)
    }

    pub async fn recommendations(
        &self,
        credentials: &Credentials,
        kind: Kind,
        cursor: Option<Value>,
    ) -> Result<Page<Episode>, Error> {
        if let Some(category) = kind.category() {
            if cursor.is_some() {
                return Err(Error::InvalidResponse);
            }
            #[derive(Deserialize)]
            struct RankedEpisode {
                item: Episode,
            }
            #[derive(Deserialize)]
            struct Ranking {
                category: String,
                #[serde(rename = "targetType")]
                target_type: String,
                items: Vec<RankedEpisode>,
            }
            let mut url = reqwest::Url::parse(&format!("{}/v1/top-list/get", self.origin))
                .map_err(|_| Error::InvalidResponse)?;
            url.query_pairs_mut().append_pair("category", category);
            let response = self
                .client
                .get(url)
                .header("x-jike-access-token", access_header(credentials)?)
                .header("x-jike-device-id", &self.device_id)
                .send()
                .await?;
            let ranking: Envelope<Ranking> = checked(response)
                .await?
                .json()
                .await
                .map_err(|_| Error::InvalidResponse)?;
            if ranking.data.category != category
                || ranking.data.target_type != "EPISODE"
                || ranking.cursor.is_some()
            {
                return Err(Error::InvalidResponse);
            }
            return episode_page(
                ranking
                    .data
                    .items
                    .into_iter()
                    .map(|entry| entry.item)
                    .collect(),
                None,
            );
        }
        if kind == Kind::Editor {
            #[derive(Deserialize)]
            struct Pick {
                episode: Episode,
            }
            #[derive(Deserialize)]
            struct Day {
                picks: Vec<Pick>,
            }
            let page: Page<Day> = self
                .page(
                    credentials,
                    "/v1/editor-pick/list-history",
                    json!({}),
                    cursor,
                )
                .await?;
            return episode_page(
                page.items
                    .into_iter()
                    .flat_map(|day| day.picks)
                    .map(|pick| pick.episode)
                    .collect(),
                page.cursor,
            );
        }

        // Discovery contains banners and podcast-only collections as well as episodes.
        // Follow opaque cursors across those pages without treating them as the end.
        let mut cursor = cursor;
        let mut visited = Vec::new();
        for _ in 0..8 {
            visited.push(cursor.clone());
            let page: Page<Value> = self
                .page(
                    credentials,
                    "/v1/discovery-feed/list",
                    json!({"returnAll":false}),
                    cursor,
                )
                .await?;
            if page.cursor.is_some() && visited.contains(&page.cursor) {
                return Err(Error::InvalidResponse);
            }
            let mut episodes = Vec::new();
            for module in page.items {
                match module.get("type").and_then(Value::as_str) {
                    Some("PRESET_CONTENT") => append_recommended(
                        &mut episodes,
                        module.pointer("/data/contents"),
                        "episode",
                    )?,
                    Some("DISCOVERY_COLLECTION") => {
                        let collections = module
                            .get("data")
                            .and_then(Value::as_array)
                            .ok_or(Error::InvalidResponse)?;
                        for collection in collections {
                            if collection.get("targetType").and_then(Value::as_str)
                                == Some("EPISODE")
                            {
                                append_recommended(
                                    &mut episodes,
                                    collection.get("target"),
                                    "episode",
                                )?;
                            }
                        }
                    }
                    Some("DISCOVERY_PICK") => {
                        append_recommended(&mut episodes, module.get("data"), "episode")?
                    }
                    _ => (),
                }
            }
            if !episodes.is_empty() || page.cursor.is_none() {
                return episode_page(episodes, page.cursor);
            }
            cursor = page.cursor;
        }
        Err(Error::InvalidResponse)
    }

    pub async fn episode_detail(
        &self,
        credentials: &Credentials,
        eid: &str,
    ) -> Result<Episode, Error> {
        self.episode(eid, &access_header(credentials)?).await
    }

    pub async fn comments(
        &self,
        credentials: &Credentials,
        eid: &str,
        cursor: Option<Value>,
    ) -> Result<Page<Comment>, Error> {
        let mut page: Page<Comment> = self
            .page(
                credentials,
                "/v1/comment/list-primary",
                json!({"owner":{"id":eid,"type":"EPISODE"},"order":"HOT","limit":20}),
                cursor,
            )
            .await?;
        let mut ids = HashSet::new();
        for comment in &mut page.items {
            if comment.id.trim().is_empty() {
                return Err(Error::InvalidResponse);
            }
            comment.text = safe_multiline(&comment.text);
            if let Some(author) = &mut comment.author {
                author.nickname = plain_text(&author.nickname);
            }
        }
        page.items.retain(|comment| ids.insert(comment.id.clone()));
        Ok(page)
    }

    async fn page<T: serde::de::DeserializeOwned>(
        &self,
        credentials: &Credentials,
        endpoint: &str,
        mut body: Value,
        cursor: Option<Value>,
    ) -> Result<Page<T>, Error> {
        if let Some(cursor) = &cursor {
            body["loadMoreKey"] = cursor.clone();
        }
        let response = self
            .client
            .post(format!("{}{endpoint}", self.origin))
            .header("x-jike-access-token", access_header(credentials)?)
            .header("x-jike-device-id", &self.device_id)
            .json(&body)
            .send()
            .await?;
        let page: Envelope<Vec<T>> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        if page.cursor.is_some() && (page.data.is_empty() || page.cursor == cursor) {
            return Err(Error::InvalidResponse);
        }
        Ok(Page {
            items: page.data,
            cursor: page.cursor,
        })
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
            transcript_media_id: transcript_media_id(&data),
        })
    }

    pub async fn transcript_url(
        &self,
        credentials: &Credentials,
        eid: &str,
        media_id: &str,
    ) -> Result<Option<reqwest::Url>, Error> {
        let response = self
            .client
            .post(format!("{}/v1/episode-transcript/get", self.origin))
            .header("x-jike-access-token", access_header(credentials)?)
            .header("x-jike-device-id", &self.device_id)
            .header(header::USER_AGENT, TRANSCRIPT_USER_AGENT)
            .header("os", "android")
            .header("app-version", "2.99.1")
            .header("app-buildno", "1362")
            .header("applicationid", "app.podcast.cosmos")
            .header("local-time", utc_timestamp())
            .timeout(Duration::from_secs(5))
            .json(&json!({"eid":eid, "mediaId":media_id}))
            .send()
            .await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let response: Envelope<Value> = checked(response)
            .await?
            .json()
            .await
            .map_err(|_| Error::InvalidResponse)?;
        let data = response
            .data
            .get("data")
            .filter(|value| value.is_object())
            .unwrap_or(&response.data);
        let Some(url) = data.get("transcriptUrl").filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        let url = url.as_str().ok_or(Error::InvalidResponse)?;
        if url.trim().is_empty() {
            return Ok(None);
        }
        let url = reqwest::Url::parse(url).map_err(|_| Error::InvalidResponse)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(Some(url))
    }

    // Fetch signed CDN URLs outside Account::request: downloads need no credentials or auth lock.
    pub async fn fetch_transcript(&self, url: reqwest::Url) -> Result<Transcript, Error> {
        const MAX_BYTES: usize = 10 * 1024 * 1024;
        let mut response = checked(
            self.client
                .get(url)
                .header(header::USER_AGENT, TRANSCRIPT_USER_AGENT)
                .send()
                .await?,
        )
        .await?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len().saturating_add(chunk.len()) > MAX_BYTES {
                return Err(Error::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk);
        }
        Transcript::from_json(&bytes)
    }

    pub async fn remove_from_playlist(
        &self,
        credentials: &Credentials,
        eid: &str,
    ) -> Result<(), Error> {
        self.set_playlist_membership(credentials, eid, false).await
    }

    pub async fn add_to_playlist(&self, credentials: &Credentials, eid: &str) -> Result<(), Error> {
        self.set_playlist_membership(credentials, eid, true).await
    }

    async fn set_playlist_membership(
        &self,
        credentials: &Credentials,
        eid: &str,
        present: bool,
    ) -> Result<(), Error> {
        let access = access_header(credentials)?;
        for _ in 0..3 {
            let queue = self.queue_revision(&access).await?;
            let position = queue.list.iter().position(|item| item == eid);
            if position.is_some() == present {
                return Ok(());
            }
            let position = position.unwrap_or(queue.list.len());
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
                    "ops": [{"action":if present { "add" } else { "rem" }, "item":eid, "pos":position}]
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
                    return if confirmed.list.iter().any(|item| item == eid) == present {
                        Ok(())
                    } else {
                        Err(Error::PlaylistChanged)
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
        if episode.data.eid != eid {
            return Err(Error::InvalidResponse);
        }
        sanitize_episode(&mut episode.data)?;
        Ok(episode.data)
    }
}

fn append_recommended(
    episodes: &mut Vec<Episode>,
    value: Option<&Value>,
    field: &str,
) -> Result<(), Error> {
    for item in value
        .and_then(Value::as_array)
        .ok_or(Error::InvalidResponse)?
    {
        let episode = item.get(field).ok_or(Error::InvalidResponse)?;
        episodes.push(serde_json::from_value(episode.clone()).map_err(|_| Error::InvalidResponse)?);
    }
    Ok(())
}

fn episode_page(mut items: Vec<Episode>, cursor: Option<Value>) -> Result<Page<Episode>, Error> {
    for episode in &mut items {
        sanitize_episode(episode)?;
    }
    let mut ids = HashSet::new();
    items.retain(|episode| ids.insert(episode.eid.clone()));
    if items.is_empty() && cursor.is_some() {
        return Err(Error::InvalidResponse);
    }
    Ok(Page { items, cursor })
}

fn transcript_media_id(data: &Value) -> Option<String> {
    [
        data.get("transcriptMediaId"),
        data.pointer("/transcript/mediaId"),
        data.pointer("/media/id"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .find(|id| !id.trim().is_empty())
    .map(str::to_owned)
}

fn access_header(credentials: &Credentials) -> Result<header::HeaderValue, Error> {
    let mut value = header::HeaderValue::from_str(&credentials.access_token)
        .map_err(|_| Error::InvalidResponse)?;
    value.set_sensitive(true);
    Ok(value)
}

pub(crate) fn plain_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control() || character.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn safe_multiline(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

fn sanitize_episode(episode: &mut Episode) -> Result<(), Error> {
    episode.title = plain_text(&episode.title);
    if episode.eid.trim().is_empty() || episode.title.is_empty() {
        return Err(Error::InvalidResponse);
    }
    if let Some(podcast) = &mut episode.podcast {
        podcast.title = plain_text(&podcast.title);
    }
    Ok(())
}
