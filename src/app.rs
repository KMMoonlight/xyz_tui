use std::{
    collections::{HashMap, VecDeque},
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use futures_util::StreamExt;
use ratatui::DefaultTerminal;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::{
    account::{Account, Renewal},
    auth::{Api, Credentials, Error, Scan},
    content::{self, Playable, PlaylistEntry, ProgressUpdate},
    help::{Context, Help},
    home::{Action, Home},
    player::{self, Player},
    session::Store,
    subscriptions,
    transcript::Transcript,
    ui::{self, Code},
};

enum View {
    Loading,
    Qr {
        code: Code,
        scanned: bool,
        reconnecting: bool,
    },
    Expired,
    Failed {
        message: String,
        retry_saved: bool,
    },
    SaveFailed(Credentials),
    Home(Home),
}

enum Update {
    Qr(String),
    Scanned,
    Waiting,
    Reconnecting,
    Expired,
    Authenticated(Credentials),
    Restored(Credentials),
    Renewed(Renewal),
    Resolved {
        epoch: u64,
        request: u64,
        result: Result<Playable, Error>,
    },
    Recent {
        epoch: u64,
        request: u64,
        result: Result<Option<content::RecentEpisode>, Error>,
    },
    Player(player::Event),
    Transcript {
        epoch: u64,
        request: u64,
        result: Result<Transcript, Error>,
    },
    Synced {
        epoch: u64,
        progress: Vec<ProgressUpdate>,
        result: Result<(), Error>,
    },
    Removed {
        epoch: u64,
        eid: String,
        result: Result<(), Error>,
    },
    Playlist(Result<Vec<PlaylistEntry>, Error>),
    Browse {
        epoch: u64,
        slot: usize,
        request: u64,
        source: subscriptions::Source,
        response: subscriptions::Response,
    },
    Added {
        epoch: u64,
        result: Result<(), Error>,
    },
    Failed {
        message: String,
        retry_saved: bool,
    },
}

struct App {
    api: Api,
    content: content::Api,
    credentials: Option<Credentials>,
    store: Store,
    view: View,
    help: Help,
    generation: u64,
    sender: mpsc::UnboundedSender<(u64, Update)>,
    task: Option<JoinHandle<()>>,
    account: Option<Arc<Account>>,
    epoch: u64,
    renewal: Option<Renewal>,
    player: Player,
    player_visible: bool,
    player_expanded: bool,
    play_request: u64,
    play_task: Option<JoinHandle<()>>,
    recent_task: Option<JoinHandle<()>>,
    recent_pending: bool,
    recent_retry_at: Option<Instant>,
    transcript: Transcript,
    transcript_request: u64,
    transcript_task: Option<JoinHandle<()>>,
    sync_task: Option<JoinHandle<()>>,
    flush_again: bool,
    remove_task: Option<JoinHandle<()>>,
    remove_queue: VecDeque<String>,
    browse_tasks: [Option<JoinHandle<()>>; BROWSE_SLOTS],
    browse_requests: [u64; BROWSE_SLOTS],
    add_task: Option<JoinHandle<()>>,
    add_queue: VecDeque<String>,
    add_retry_at: Option<Instant>,
    added_notice_until: Option<Instant>,
    dirty: HashMap<String, ProgressUpdate>,
    last_synced: HashMap<String, u64>,
    sync_after: Instant,
    retry_after: Option<Instant>,
    notice: Option<String>,
    sync_error: Option<String>,
    logging_out: bool,
    exiting: bool,
}

const BROWSE_SLOTS: usize = subscriptions::Source::SLOTS;

impl Drop for App {
    fn drop(&mut self) {
        for task in self
            .browse_tasks
            .iter()
            .flatten()
            .chain(self.add_task.iter())
        {
            task.abort();
        }
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(task) = &self.play_task {
            task.abort();
        }
        if let Some(task) = &self.recent_task {
            task.abort();
        }
        if let Some(task) = &self.transcript_task {
            task.abort();
        }
        if let Some(task) = &self.sync_task {
            task.abort();
        }
        if let Some(task) = &self.remove_task {
            task.abort();
        }
        if let Some(account) = &self.account {
            account.cancel();
        }
    }
}

impl App {
    fn start(&mut self, use_saved: bool) {
        if let Some(task) = self.recent_task.take() {
            task.abort();
        }
        self.recent_pending = false;
        self.recent_retry_at = None;
        for task in &mut self.browse_tasks {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
        if let Some(task) = self.add_task.take() {
            task.abort();
        }
        self.add_queue.clear();
        self.add_retry_at = None;
        self.added_notice_until = None;
        if let Some(account) = self.account.take() {
            account.cancel();
        }
        if let Some(task) = self.play_task.take() {
            task.abort();
        }
        if let Some(task) = self.sync_task.take() {
            task.abort();
        }
        if let Some(task) = self.remove_task.take() {
            task.abort();
        }
        self.remove_queue.clear();
        self.renewal = None;
        self.player.stop();
        self.player_visible = false;
        self.player_expanded = false;
        self.clear_transcript();
        self.dirty.clear();
        self.flush_again = false;
        self.last_synced.clear();
        self.epoch += 1;
        self.logging_out = false;
        self.notice = None;
        self.sync_error = None;
        self.sync_after = Instant::now();
        self.retry_after = None;
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation += 1;
        self.credentials = None;
        self.view = View::Loading;
        let saved = if use_saved {
            match self.store.load() {
                Ok(saved) => saved,
                Err(_) => {
                    self.view = View::Failed {
                        message: "登录信息无法读取".into(),
                        retry_saved: false,
                    };
                    return;
                }
            }
        } else {
            None
        };
        let api = self.api.clone();
        let sender = self.sender.clone();
        let generation = self.generation;
        self.task = Some(tokio::spawn(async move {
            login(api, saved, generation, sender).await;
        }));
    }

    fn apply(&mut self, generation: u64, update: Update) {
        let update = match update {
            Update::Recent {
                epoch,
                request,
                result,
            } => {
                if epoch != self.epoch || request != self.play_request {
                    return;
                }
                self.recent_task = None;
                if self.exiting
                    || self.logging_out
                    || self.player.current.is_some()
                    || self.play_task.is_some()
                {
                    return;
                }
                if self.notice.as_deref() == Some("正在恢复最近收听…") {
                    self.notice = None;
                }
                match result {
                    Ok(Some(recent)) => {
                        let eid = recent.episode.eid.clone();
                        let media_id = recent.transcript_media_id.clone();
                        self.player.restore(recent);
                        self.load_transcript(eid, media_id);
                    }
                    Ok(None) => (),
                    Err(error) => {
                        if let Error::RateLimited(delay) = &error {
                            self.recent_retry_at = Instant::now().checked_add(*delay);
                        }
                        self.notice = Some(format!("最近收听加载失败：{error}"));
                    }
                }
                return;
            }
            Update::Browse {
                epoch,
                slot,
                request,
                source,
                response,
            } => {
                if epoch == self.epoch && request == self.browse_requests[slot] {
                    self.browse_tasks[slot] = None;
                    if let View::Home(home) = &mut self.view {
                        home.apply_browse(source, response);
                    }
                }
                return;
            }
            Update::Added { epoch, result } => {
                if epoch != self.epoch {
                    return;
                }
                self.add_task = None;
                self.add_queue.pop_front();
                match result {
                    Ok(()) => {
                        if !self
                            .notice
                            .as_deref()
                            .is_some_and(|notice| notice.starts_with("无法播放："))
                        {
                            self.notice = Some("已加入播放列表".into());
                            self.added_notice_until = Some(Instant::now() + Duration::from_secs(3));
                        }
                        if !self.exiting && !self.logging_out {
                            self.load_playlist();
                        }
                    }
                    Err(error) => {
                        if let Error::RateLimited(delay) = &error {
                            self.add_retry_at = Instant::now().checked_add(*delay);
                        }
                        self.notice = Some(format!("加入播放列表失败：{error}"));
                    }
                }
                self.start_addition();
                return;
            }
            Update::Renewed(renewal) => {
                if renewal.epoch == self.epoch {
                    self.renewal = Some(renewal);
                    self.save_renewal();
                }
                return;
            }
            Update::Player(event) => {
                let finished = matches!(event, player::Event::Ended(_, false))
                    && self
                        .player
                        .current
                        .as_ref()
                        .is_some_and(|current| !current.ended);
                let flush = self.player.apply(event);
                self.remember_progress();
                if flush {
                    self.flush_progress(true);
                    if self
                        .player
                        .current
                        .as_ref()
                        .is_some_and(|current| current.ended)
                    {
                        self.clear_transcript();
                    }
                    if finished {
                        self.finish_playback();
                    }
                }
                return;
            }
            Update::Transcript {
                epoch,
                request,
                result,
            } => {
                if epoch == self.epoch && request == self.transcript_request {
                    self.transcript_task = None;
                    self.transcript = result.unwrap_or_default();
                }
                return;
            }
            Update::Resolved {
                epoch,
                request,
                result,
            } => {
                if epoch != self.epoch || request != self.play_request {
                    return;
                }
                self.play_task = None;
                match result {
                    Ok(mut source) => {
                        self.remember_progress();
                        if let Some(progress) = self.dirty.get(&source.eid) {
                            source.start = if source
                                .duration
                                .is_some_and(|duration| progress.progress >= duration)
                            {
                                0.0
                            } else {
                                progress.progress as f64
                            };
                        }
                        self.flush_progress(true);
                        if self.notice.as_deref() == Some("音频加载中…") {
                            self.notice = None;
                        }
                        let sender = self.sender.clone();
                        let eid = source.eid.clone();
                        let media_id = source.transcript_media_id.clone();
                        self.player.play(
                            source,
                            Arc::new(move |event| {
                                let _ = sender.send((0, Update::Player(event)));
                            }),
                        );
                        self.load_transcript(eid, media_id);
                    }
                    Err(error) => self.notice = Some(format!("无法播放：{error}")),
                }
                return;
            }
            Update::Synced {
                epoch,
                progress,
                result,
            } => {
                if epoch != self.epoch {
                    return;
                }
                self.sync_task = None;
                let succeeded = result.is_ok();
                let flush_again = std::mem::take(&mut self.flush_again);
                match result {
                    Ok(()) => {
                        for item in progress {
                            self.last_synced.insert(item.eid.clone(), item.progress);
                            if self.dirty.get(&item.eid) == Some(&item) {
                                self.dirty.remove(&item.eid);
                            }
                        }
                        self.sync_error = None;
                        self.sync_after = Instant::now() + Duration::from_secs(15);
                        self.retry_after = None;
                    }
                    Err(error) => {
                        self.sync_after = Instant::now()
                            + error.retry_delay(3).unwrap_or(Duration::from_secs(30));
                        self.retry_after = Some(self.sync_after);
                        self.sync_error = Some(format!("进度同步失败：{error}"));
                    }
                }
                if self.logging_out {
                    if self.dirty.is_empty() {
                        self.finish_logout();
                    } else if succeeded {
                        self.flush_progress(true);
                    } else {
                        self.logging_out = false;
                    }
                } else if succeeded && flush_again {
                    self.flush_progress(true);
                }
                return;
            }
            Update::Removed { epoch, eid, result } => {
                if epoch != self.epoch {
                    return;
                }
                self.remove_task = None;
                self.remove_queue.pop_front();
                match result {
                    Ok(()) => {
                        self.generation += 1;
                        if let Some(task) = self.task.take() {
                            task.abort();
                        }
                        let next = if let View::Home(home) = &mut self.view {
                            let next = home.next_after(&eid);
                            home.remove(&eid);
                            next
                        } else {
                            None
                        };
                        if self.notice.as_deref() == Some("正在移除…") {
                            self.notice = None;
                        }
                        // Natural completion already advances playback before its removal is confirmed.
                        if self
                            .player
                            .current
                            .as_ref()
                            .is_some_and(|current| current.eid == eid && !current.ended)
                        {
                            self.remember_progress();
                            self.flush_progress(true);
                            self.player.stop();
                            self.clear_transcript();
                            if self.play_task.is_none()
                                && let Some(next) = next
                            {
                                self.play(next);
                            }
                        }
                    }
                    Err(error) => self.notice = Some(format!("移除失败：{error}")),
                }
                self.start_removal();
                return;
            }
            update => update,
        };
        if generation != self.generation {
            return;
        }
        match update {
            Update::Qr(url) => match Code::new(&url) {
                Ok(code) => {
                    self.view = View::Qr {
                        code,
                        scanned: false,
                        reconnecting: false,
                    }
                }
                Err(_) => {
                    self.view = View::Failed {
                        message: "无法生成二维码".into(),
                        retry_saved: false,
                    }
                }
            },
            Update::Scanned | Update::Waiting | Update::Reconnecting => {
                if let View::Qr {
                    scanned,
                    reconnecting,
                    ..
                } = &mut self.view
                {
                    if matches!(update, Update::Scanned) {
                        *scanned = true;
                    }
                    *reconnecting = matches!(update, Update::Reconnecting);
                }
            }
            Update::Expired => self.view = View::Expired,
            Update::Authenticated(credentials) => self.save(credentials),
            Update::Restored(credentials) => {
                self.activate(credentials);
                self.view = View::Home(Home::default());
            }
            Update::Playlist(result) => {
                if let View::Home(home) = &mut self.view {
                    home.apply_playlist(result);
                    if let Some(playing) = &self.player.current {
                        home.progress(&playing.eid, playing.position);
                    }
                }
            }
            Update::Renewed(_)
            | Update::Player(_)
            | Update::Transcript { .. }
            | Update::Resolved { .. }
            | Update::Recent { .. }
            | Update::Synced { .. }
            | Update::Removed { .. } => unreachable!(),
            Update::Browse { .. } | Update::Added { .. } => unreachable!(),
            Update::Failed {
                message,
                retry_saved,
            } => {
                self.view = View::Failed {
                    message,
                    retry_saved,
                }
            }
        }
    }

    fn save(&mut self, credentials: Credentials) {
        if self.store.save(&credentials).is_ok() {
            self.activate(credentials);
            if !matches!(self.view, View::Home(_)) {
                self.view = View::Home(Home::default());
            }
        } else {
            self.view = View::SaveFailed(credentials);
        }
    }

    fn retry(&mut self) {
        if self.renewal.is_some() {
            self.save_renewal();
            return;
        }
        match &self.view {
            View::Loading | View::Home(_) => (),
            View::SaveFailed(credentials) => self.save(credentials.clone()),
            View::Failed { retry_saved, .. } => self.start(*retry_saved),
            _ => self.start(false),
        }
    }

    fn activate(&mut self, credentials: Credentials) {
        let sender = self.sender.clone();
        self.account = Some(Arc::new(Account::new(
            credentials.clone(),
            self.api.clone(),
            self.content.clone(),
            self.epoch,
            Arc::new(move |renewal| {
                let _ = sender.send((0, Update::Renewed(renewal)));
            }),
        )));
        self.credentials = Some(credentials);
        self.recent_pending = true;
    }

    fn save_renewal(&mut self) {
        let Some(renewal) = self.renewal.take() else {
            return;
        };
        if self.store.save(&renewal.credentials).is_ok() {
            self.credentials = Some(renewal.credentials);
            let _ = renewal.saved.send(true);
            self.notice = None;
        } else {
            self.notice = Some("登录凭据保存失败".into());
            self.renewal = Some(renewal);
        }
    }

    fn key(&mut self, key: KeyCode) {
        if matches!(key, KeyCode::Char('?' | '？')) {
            self.help.toggle();
            return;
        }
        if key == KeyCode::Char('t') {
            self.player_visible = !self.player_visible;
            if !self.player_visible {
                self.player_expanded = false;
            }
            return;
        }
        if key == KeyCode::Char('T') {
            if self.player_shown() {
                self.player_expanded = !self.player_expanded;
            }
            return;
        }
        if let KeyCode::Char(direction @ ('a' | 'd')) = key {
            self.player.seek(if direction == 'a' { -15 } else { 15 });
            return;
        }
        if key == KeyCode::Char(' ') {
            if let Some(current) = &self.player.current {
                if current.ended || self.player.awaiting_resume() {
                    self.play(current.eid.clone());
                } else {
                    self.player.toggle();
                }
            } else {
                self.restore_recent(true);
            }
            return;
        }
        if self.help.open {
            self.help.key(key);
            return;
        }
        if key == KeyCode::Char('r') && self.renewal.is_some() {
            self.save_renewal();
            return;
        }
        if self.player_expanded && self.player_shown() {
            if matches!(key, KeyCode::Esc | KeyCode::Backspace) {
                self.player_expanded = false;
            }
            return;
        }
        if let View::Home(home) = &mut self.view {
            match home.key(key) {
                Action::Logout => {
                    if self.logging_out {
                        return;
                    }
                    self.remember_progress();
                    self.player.stop();
                    self.clear_transcript();
                    self.play_request += 1;
                    if let Some(task) = self.play_task.take() {
                        task.abort();
                    }
                    self.logging_out = true;
                    if self.dirty.is_empty() {
                        self.finish_logout();
                    } else {
                        self.flush_progress(true);
                    }
                }
                Action::LoadPlaylist => self.load_playlist(),
                Action::Login => self.start(false),
                Action::Play(eid) => self.play(eid),
                Action::PlayAndAdd(eid) => {
                    self.play(eid.clone());
                    self.add(eid);
                }
                Action::Remove(eid) => self.remove(eid),
                Action::Add(eid) => self.add(eid),
                Action::Browse(source, requests) => {
                    for request in requests {
                        self.browse(source, request);
                    }
                }
                Action::None => (),
            }
        } else if key == KeyCode::Char('r') {
            self.retry();
        }
    }

    fn finish_logout(&mut self) {
        if self.store.clear().is_ok() {
            self.start(false);
        } else {
            self.logging_out = false;
            if let View::Home(home) = &mut self.view {
                home.logout_failed();
            }
        }
    }

    fn load_playlist(&mut self) {
        let View::Home(home) = &mut self.view else {
            return;
        };
        let Some(account) = self.account.clone() else {
            return;
        };
        home.loading_playlist();
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.generation += 1;
        let generation = self.generation;
        let sender = self.sender.clone();
        self.task = Some(tokio::spawn(async move {
            let result = account
                .request(move |api, credentials| async move { api.playlist(&credentials).await })
                .await;
            let _ = sender.send((generation, Update::Playlist(result)));
        }));
    }

    fn browse(&mut self, source: subscriptions::Source, operation: subscriptions::Request) {
        let Some(account) = self.account.clone() else {
            return;
        };
        if self.exiting || self.logging_out {
            return;
        }
        let slot = source.slot() + operation.slot();
        if let Some(task) = self.browse_tasks[slot].take() {
            task.abort();
        }
        self.browse_requests[slot] += 1;
        let request = self.browse_requests[slot];
        let epoch = self.epoch;
        let sender = self.sender.clone();
        self.browse_tasks[slot] = Some(tokio::spawn(async move {
            let fallback = operation.clone();
            let result = account
                .request(move |api, credentials| {
                    let operation = operation.clone();
                    async move {
                        operation
                            .fetch(api, credentials, source)
                            .await
                            .into_result()
                    }
                })
                .await;
            let response = result.unwrap_or_else(|error| fallback.failed(error));
            let _ = sender.send((
                0,
                Update::Browse {
                    epoch,
                    slot,
                    request,
                    source,
                    response,
                },
            ));
        }));
    }

    fn add(&mut self, eid: String) {
        if self.exiting
            || self.logging_out
            || self.account.is_none()
            || self.add_queue.contains(&eid)
        {
            return;
        }
        if self
            .add_retry_at
            .is_some_and(|until| Instant::now() < until)
        {
            self.add_queue.push_back(eid);
            self.notice = Some("播放列表暂时限流，稍后自动加入".into());
            return;
        }
        self.add_queue.push_back(eid);
        self.start_addition();
    }

    fn start_addition(&mut self) {
        if self.add_task.is_some()
            || self
                .add_retry_at
                .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let Some(eid) = self.add_queue.front().cloned() else {
            return;
        };
        let Some(account) = self.account.clone() else {
            return;
        };
        let epoch = self.epoch;
        let sender = self.sender.clone();
        self.notice = Some("正在加入播放列表…".into());
        self.add_task = Some(tokio::spawn(async move {
            let result = account
                .request(move |api, credentials| {
                    let eid = eid.clone();
                    async move { api.add_to_playlist(&credentials, &eid).await }
                })
                .await;
            let _ = sender.send((0, Update::Added { epoch, result }));
        }));
    }

    fn play(&mut self, eid: String) {
        if self.logging_out || self.exiting {
            return;
        }
        self.recent_pending = false;
        if let Some(task) = self.recent_task.take() {
            task.abort();
        }
        if let Some(task) = self.play_task.take() {
            task.abort();
        }
        self.play_request += 1;
        if let Some(current) = &self.player.current
            && current.eid == eid
            && !current.ended
            && !self.player.awaiting_resume()
        {
            if current.paused {
                self.player.toggle();
            }
            self.notice = None;
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let request = self.play_request;
        self.player_visible = true;
        let epoch = self.epoch;
        let sender = self.sender.clone();
        self.notice = Some("音频加载中…".into());
        self.play_task = Some(tokio::spawn(async move {
            let result = account
                .request(move |api, credentials| {
                    let eid = eid.clone();
                    async move { api.playable(&credentials, &eid).await }
                })
                .await;
            let _ = sender.send((
                0,
                Update::Resolved {
                    epoch,
                    request,
                    result,
                },
            ));
        }));
    }

    fn finish_playback(&mut self) {
        let Some(current) = &self.player.current else {
            return;
        };
        let eid = current.eid.clone();
        let next = match &self.view {
            View::Home(home) => home.next_after(&eid),
            _ => None,
        };
        self.remove(eid);
        // A manually requested track may still be resolving when the old one ends.
        if self.play_task.is_none()
            && let Some(next) = next
        {
            self.play(next);
        }
    }

    fn clear_transcript(&mut self) {
        self.transcript_request += 1;
        if let Some(task) = self.transcript_task.take() {
            task.abort();
        }
        self.transcript = Transcript::default();
    }

    fn load_transcript(&mut self, eid: String, media_id: Option<String>) {
        self.clear_transcript();
        let Some(media_id) = media_id else {
            return;
        };
        let Some(account) = self.account.clone() else {
            return;
        };
        let content = self.content.clone();
        let sender = self.sender.clone();
        let epoch = self.epoch;
        let request = self.transcript_request;
        self.transcript_task = Some(tokio::spawn(async move {
            let mut attempt = 0;
            let result = loop {
                let eid = eid.clone();
                let media_id = media_id.clone();
                let result = async {
                    let url = account
                        .request(move |api, credentials| {
                            let eid = eid.clone();
                            let media_id = media_id.clone();
                            async move { api.transcript_url(&credentials, &eid, &media_id).await }
                        })
                        .await?;
                    match url {
                        Some(url) => content.fetch_transcript(url).await,
                        None => Ok(Transcript::default()),
                    }
                }
                .await;
                if let Err(error) = &result
                    && attempt < 2
                    && let Some(delay) = error.retry_delay(attempt + 1)
                {
                    attempt += 1;
                    tokio::time::sleep(delay).await;
                    continue;
                }
                break result;
            };
            let _ = sender.send((
                0,
                Update::Transcript {
                    epoch,
                    request,
                    result,
                },
            ));
        }));
    }

    fn remove(&mut self, eid: String) {
        if self.logging_out || self.account.is_none() || self.remove_queue.contains(&eid) {
            return;
        }
        if self
            .notice
            .as_deref()
            .is_some_and(|notice| notice.starts_with("移除失败："))
        {
            self.notice = None;
        }
        self.remove_queue.push_back(eid);
        self.start_removal();
    }

    fn start_removal(&mut self) {
        if self.remove_task.is_some() {
            return;
        }
        let Some(eid) = self.remove_queue.front().cloned() else {
            return;
        };
        let Some(account) = self.account.clone() else {
            return;
        };
        let epoch = self.epoch;
        let sender = self.sender.clone();
        if self.notice.is_none() {
            self.notice = Some("正在移除…".into());
        }
        self.remove_task = Some(tokio::spawn(async move {
            let removed_eid = eid.clone();
            let result = account
                .request(move |api, credentials| {
                    let eid = eid.clone();
                    async move { api.remove_from_playlist(&credentials, &eid).await }
                })
                .await;
            let _ = sender.send((
                0,
                Update::Removed {
                    epoch,
                    eid: removed_eid,
                    result,
                },
            ));
        }));
    }

    fn remember_progress(&mut self) {
        let Some(progress) = self.player.progress() else {
            return;
        };
        if let View::Home(home) = &mut self.view {
            home.progress(&progress.eid, progress.progress as f64);
        }
        if self
            .dirty
            .get(&progress.eid)
            .is_some_and(|old| old.progress == progress.progress)
            || (!self.dirty.contains_key(&progress.eid)
                && self.last_synced.get(&progress.eid) == Some(&progress.progress))
        {
            return;
        }
        self.dirty.insert(progress.eid.clone(), progress);
    }

    fn flush_progress(&mut self, force: bool) {
        if self.sync_task.is_some() {
            self.flush_again |= force;
            return;
        }
        if self.dirty.is_empty()
            || self.retry_after.is_some_and(|until| Instant::now() < until)
            || (!force && Instant::now() < self.sync_after)
        {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let progress = self.dirty.values().cloned().collect::<Vec<_>>();
        let epoch = self.epoch;
        let sender = self.sender.clone();
        self.sync_task = Some(tokio::spawn(async move {
            let snapshot = progress.clone();
            let result = account
                .request(move |api, credentials| {
                    let progress = progress.clone();
                    async move { api.update_progress(&credentials, &progress).await }
                })
                .await;
            let _ = sender.send((
                0,
                Update::Synced {
                    epoch,
                    progress: snapshot,
                    result,
                },
            ));
        }));
    }

    fn tick(&mut self) {
        if std::mem::take(&mut self.recent_pending)
            && self.player.current.is_none()
            && self.play_task.is_none()
        {
            self.restore_recent(false);
        }
        if self
            .added_notice_until
            .is_some_and(|until| Instant::now() >= until)
        {
            self.added_notice_until = None;
            if self.notice.as_deref() == Some("已加入播放列表") {
                self.notice = None;
            }
        }
        self.start_addition();
        self.remember_progress();
        self.flush_progress(false);
    }

    fn restore_recent(&mut self, show_player: bool) {
        if self.exiting
            || self.logging_out
            || self.recent_task.is_some()
            || self.play_task.is_some()
            || self.player.current.is_some()
            || self
                .recent_retry_at
                .is_some_and(|until| Instant::now() < until)
        {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        self.recent_pending = false;
        self.recent_retry_at = None;
        if show_player {
            self.player_visible = true;
        }
        self.notice = Some("正在恢复最近收听…".into());
        let epoch = self.epoch;
        let request = self.play_request;
        let sender = self.sender.clone();
        self.recent_task = Some(tokio::spawn(async move {
            let result = account
                .request(|api, credentials| async move { api.recent_episode(&credentials).await })
                .await;
            let _ = sender.send((
                0,
                Update::Recent {
                    epoch,
                    request,
                    result,
                },
            ));
        }));
    }

    fn escape_exits(&self) -> bool {
        !self.help.open && !matches!(self.view, View::Home(_))
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        use ratatui::{
            layout::Rect,
            style::{Color, Style},
            widgets::Paragraph,
        };
        let terminal = frame.area();
        let header_height = terminal.height.min(1);
        let body = Rect::new(
            terminal.x,
            terminal.y + header_height,
            terminal.width,
            terminal.height - header_height,
        );
        let context = match &self.view {
            View::Home(_) if self.player_expanded && self.player_shown() => Context::Player,
            View::Home(home) => home.help_context(),
            _ => Context::Login,
        };
        let login = match &mut self.view {
            View::Loading => Some((None, "连接中…")),
            View::Qr {
                code,
                scanned,
                reconnecting,
            } => {
                let message = if body.width < code.width() + 2 || body.height < code.height() + 2 {
                    "请放大终端窗口"
                } else if *reconnecting {
                    "重新连接中…"
                } else if *scanned {
                    "请在手机上确认"
                } else {
                    "用小宇宙扫码"
                };
                Some((Some(code), message))
            }
            View::Expired => Some((None, "二维码已过期")),
            View::Failed { message, .. } => Some((None, message.as_str())),
            View::SaveFailed(_) => Some((None, "登录凭据保存失败")),
            View::Home(home) => {
                home.draw(frame, body);
                None
            }
        };
        if let Some((code, message)) = login {
            ui::draw(frame, body, code.as_deref(), message);
        } else {
            self.draw_player(frame);
        }
        frame.render_widget(
            Paragraph::new("？帮助")
                .right_aligned()
                .style(Style::default().fg(Color::Cyan)),
            Rect::new(terminal.x, terminal.y, terminal.width, header_height),
        );
        self.help.draw(frame, body, context, self.renewal.is_some());
    }
    fn player_shown(&self) -> bool {
        self.player_visible
            && matches!(self.view, View::Home(_))
            && (self.player.current.is_some() || self.notice.is_some() || self.sync_error.is_some())
    }

    fn subtitle_height(&self, terminal_height: u16) -> u16 {
        if !self.player_visible
            || !self.player.current.as_ref().is_some_and(|current| {
                !current.loading && !current.ended && self.transcript.has_current(current.position)
            })
        {
            return 0;
        }
        // Leave the global header visible. The page stays beneath the overlay.
        let panel = if self.player_expanded {
            terminal_height.saturating_sub(1)
        } else {
            (terminal_height / 2)
                .clamp(14, 18)
                .min(terminal_height.saturating_sub(1))
        };
        // Outer borders, a section divider, and four rows for audio information.
        let height = panel.saturating_sub(7);
        if height >= 3 { height } else { 0 }
    }

    fn player_height(&self, terminal_height: u16) -> u16 {
        if !self.player_visible {
            return 0;
        }
        let height = if self.player.current.is_some() {
            let subtitles = self.subtitle_height(terminal_height);
            if subtitles > 0 { subtitles + 7 } else { 6 }
        } else if self.notice.is_some() || self.sync_error.is_some() {
            3
        } else {
            0
        };
        if self.player_expanded && height > 0 {
            terminal_height.saturating_sub(1)
        } else {
            height.min(terminal_height.saturating_sub(1))
        }
    }

    fn draw_player(&self, frame: &mut ratatui::Frame) {
        use ratatui::{
            layout::Rect,
            style::{Color, Style},
            text::{Line, Span},
            widgets::{Block, BorderType, Borders, Clear, Paragraph},
        };
        let terminal = frame.area();
        let height = self.player_height(terminal.height);
        if height == 0 {
            return;
        }
        let panel = Rect::new(
            terminal.x,
            terminal.bottom() - height,
            terminal.width,
            height,
        );
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(Span::styled(
                if self.player.current.is_some() {
                    " 播放区 "
                } else {
                    " 状态 "
                },
                Style::default().fg(Color::Cyan),
            ));
        let inner = block.inner(panel);
        frame.render_widget(Clear, panel);
        frame.render_widget(block, panel);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        if let Some(current) = &self.player.current {
            let mut information = inner;
            let height = self.subtitle_height(terminal.height);
            if height > 0 {
                self.transcript.draw(
                    frame,
                    Rect::new(inner.x, inner.y, inner.width, height),
                    current.position,
                );
                let section = Rect::new(
                    inner.x,
                    inner.y + height,
                    inner.width,
                    inner.height - height,
                );
                let divider = Block::default()
                    .borders(Borders::TOP)
                    .border_style(Style::default().fg(Color::DarkGray));
                information = divider.inner(section);
                frame.render_widget(divider, section);
            }
            let padding = 2.min(information.width / 4);
            let vertical_padding = u16::from(information.height >= 4);
            let area = Rect::new(
                information.x + padding,
                information.y + vertical_padding,
                information.width - padding * 2,
                information.height - vertical_padding * 2,
            );
            let status = if current.loading {
                "加载中"
            } else if current.ended {
                "已结束"
            } else if current.paused {
                "已暂停"
            } else {
                "播放中"
            };
            let title = crate::playlist::fit(
                &format!("{status} · {}", current.title),
                area.width as usize,
            );
            if area.height >= 2 {
                frame.render_widget(
                    Paragraph::new(title).style(Style::default().fg(Color::Cyan)),
                    Rect::new(area.x, area.bottom() - 2, area.width, 1),
                );
            }
            let progress = self
                .player
                .error
                .as_ref()
                .or(self.notice.as_ref())
                .or(self.sync_error.as_ref())
                .map(|message| Line::raw(message.as_str()))
                .unwrap_or_else(|| {
                    ui::playback_progress(current.position, current.duration, area.width)
                });
            frame.render_widget(
                Paragraph::new(progress).style(Style::default().fg(Color::DarkGray)),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
        } else if let Some(notice) = self.notice.as_ref().or(self.sync_error.as_ref()) {
            frame.render_widget(
                Paragraph::new(notice.as_str())
                    .centered()
                    .style(Style::default().fg(Color::DarkGray)),
                inner,
            );
        }
    }
}

async fn login(
    api: Api,
    saved: Option<Credentials>,
    generation: u64,
    sender: mpsc::UnboundedSender<(u64, Update)>,
) {
    let send = |update| sender.send((generation, update)).is_ok();
    if let Some(saved) = saved {
        match api.validate(&saved).await {
            Ok(true) => {
                send(Update::Restored(saved));
                return;
            }
            Ok(false) => match api.refresh(&saved).await {
                Ok(renewed) => {
                    // Save rotated credentials before any further network request.
                    send(Update::Authenticated(renewed));
                    return;
                }
                Err(Error::Http(reqwest::StatusCode::UNAUTHORIZED)) => (),
                Err(error) => {
                    send(Update::Failed {
                        message: error.to_string(),
                        retry_saved: true,
                    });
                    return;
                }
            },
            Err(error) => {
                send(Update::Failed {
                    message: error.to_string(),
                    retry_saved: true,
                });
                return;
            }
        }
    }
    let challenge = match api.create().await {
        Ok(challenge) => challenge,
        Err(error) => {
            send(Update::Failed {
                message: error.to_string(),
                retry_saved: false,
            });
            return;
        }
    };
    if !send(Update::Qr(challenge.url)) {
        return;
    }
    let mut failures = 0;
    let mut delay = Duration::from_secs(2);
    loop {
        tokio::time::sleep(delay).await;
        let update = match api.poll(&challenge.id).await {
            Ok(Scan::Authenticated(credentials)) => {
                send(Update::Authenticated(credentials));
                return;
            }
            Ok(Scan::Expired) => {
                send(Update::Expired);
                return;
            }
            Ok(Scan::Waiting) => Update::Waiting,
            Ok(Scan::Scanned) => Update::Scanned,
            Err(error) => {
                failures += 1;
                if let Some(wait) = error.retry_delay(failures).filter(|_| failures <= 3) {
                    delay = wait;
                    if !send(Update::Reconnecting) {
                        return;
                    }
                    continue;
                }
                send(Update::Failed {
                    message: error.to_string(),
                    retry_saved: false,
                });
                return;
            }
        };
        failures = 0;
        delay = Duration::from_secs(2);
        if !send(update) {
            return;
        }
    }
}

pub async fn run(terminal: &mut DefaultTerminal) -> io::Result<()> {
    let (sender, mut receiver) = mpsc::unbounded_channel();
    let store = Store::new()?;
    let device_id = store.device_id()?;
    let mut app = App {
        api: Api::new().map_err(io::Error::other)?,
        content: content::Api::new()
            .map_err(io::Error::other)?
            .with_device_id(device_id),
        credentials: None,
        store,
        view: View::Loading,
        help: Help::default(),
        generation: 0,
        sender,
        task: None,
        account: None,
        epoch: 0,
        renewal: None,
        player: Player::default(),
        player_visible: false,
        player_expanded: false,
        play_request: 0,
        play_task: None,
        recent_task: None,
        recent_pending: false,
        recent_retry_at: None,
        transcript: Transcript::default(),
        transcript_request: 0,
        transcript_task: None,
        sync_task: None,
        flush_again: false,
        remove_task: None,
        remove_queue: VecDeque::new(),
        browse_tasks: std::array::from_fn(|_| None),
        browse_requests: [0; BROWSE_SLOTS],
        add_task: None,
        add_queue: VecDeque::new(),
        add_retry_at: None,
        added_notice_until: None,
        dirty: HashMap::new(),
        last_synced: HashMap::new(),
        sync_after: Instant::now(),
        retry_after: None,
        notice: None,
        sync_error: None,
        logging_out: false,
        exiting: false,
    };
    app.start(true);
    let mut events = EventStream::new();
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    let exit_result = loop {
        if let Err(error) = terminal.draw(|frame| app.draw(frame)) {
            break Err(error);
        }
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => break Ok(()),
                    KeyCode::Esc if app.escape_exits() => break Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Ok(()),
                    code if !key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => app.key(code),
                    _ => (),
                },
                Some(Err(error)) => break Err(error),
                None => break Ok(()),
                _ => (),
            },
            Some((generation, update)) = receiver.recv() => app.apply(generation, update),
            _ = interval.tick() => app.tick(),
            _ = tokio::signal::ctrl_c() => break Ok(()),
        }
    };
    // Finish queued position events before stopping the decoder and uploading the last sample.
    app.exiting = true;
    app.clear_transcript();
    app.play_request += 1;
    if let Some(task) = app.recent_task.take() {
        task.abort();
    }
    if let Some(task) = app.play_task.take() {
        task.abort();
    }
    if let Some(task) = app.task.take() {
        task.abort();
    }
    while let Ok((generation, update)) = receiver.try_recv() {
        app.apply(generation, update);
    }
    app.remember_progress();
    app.player.stop();
    let flush = async {
        while !app.dirty.is_empty()
            || app.sync_task.is_some()
            || app.remove_task.is_some()
            || app.add_task.is_some()
            || !app.add_queue.is_empty()
            || app
                .account
                .as_ref()
                .is_some_and(|account| !account.is_idle())
        {
            if app.add_task.is_none()
                && !app.add_queue.is_empty()
                && app.add_retry_at.is_some_and(|until| Instant::now() < until)
            {
                return Err(io::Error::other("加入播放列表受到限流，尚有单集未加入"));
            }
            if app.retry_after.is_some_and(|until| Instant::now() < until)
                && app.sync_task.is_none()
            {
                return Err(io::Error::other("进度同步暂不可用，最后进度尚未上传"));
            }
            app.flush_progress(true);
            let message = tokio::select! {
                update = receiver.recv() => update,
                _ = tokio::time::sleep(Duration::from_millis(25)) => continue,
            };
            let Some((generation, update)) = message else {
                return Err(io::Error::other("播放进度未同步"));
            };
            let failed = matches!(&update, Update::Synced { result: Err(_), .. });
            let addition_failed = matches!(&update, Update::Added { result: Err(_), .. });
            app.apply(generation, update);
            if addition_failed {
                return Err(io::Error::other("加入播放列表失败，请重新启动后重试"));
            }
            if failed {
                return Err(io::Error::other("播放进度同步失败，请联网后重试"));
            }
        }
        Ok(())
    };
    tokio::time::timeout(Duration::from_secs(30), flush)
        .await
        .map_err(|_| io::Error::other("播放进度同步超时"))??;
    exit_result
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header, path},
    };

    fn app(directory: std::path::PathBuf) -> App {
        let (sender, _) = mpsc::unbounded_channel();
        App {
            api: Api::new().unwrap(),
            content: content::Api::new().unwrap(),
            credentials: None,
            store: Store::for_test(directory),
            view: View::Loading,
            help: Help::default(),
            generation: 2,
            sender,
            task: None,
            account: None,
            epoch: 0,
            renewal: None,
            player: Player::default(),
            player_visible: true,
            player_expanded: false,
            play_request: 0,
            play_task: None,
            recent_task: None,
            recent_pending: false,
            recent_retry_at: None,
            transcript: Transcript::default(),
            transcript_request: 0,
            transcript_task: None,
            sync_task: None,
            flush_again: false,
            remove_task: None,
            remove_queue: VecDeque::new(),
            browse_tasks: std::array::from_fn(|_| None),
            browse_requests: [0; BROWSE_SLOTS],
            add_task: None,
            add_queue: VecDeque::new(),
            add_retry_at: None,
            added_notice_until: None,
            dirty: HashMap::new(),
            last_synced: HashMap::new(),
            sync_after: Instant::now(),
            retry_after: None,
            notice: None,
            sync_error: None,
            logging_out: false,
            exiting: false,
        }
    }

    fn credentials() -> Credentials {
        Credentials {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
        }
    }

    fn sample_transcript() -> Transcript {
        Transcript::from_json(
            &serde_json::to_vec(&json!([
                {"startMs":0,"text":"opening sentence"},
                {"startMs":15000,"text":"middle sentence"},
                {"startMs":30500,"text":"ending sentence"}
            ]))
            .unwrap(),
        )
        .unwrap()
    }

    fn recent_episode() -> content::RecentEpisode {
        content::RecentEpisode {
            episode: content::Episode {
                eid: "recent".into(),
                title: "最近收听".into(),
                duration: Some(60),
                ..Default::default()
            },
            pid: "podcast".into(),
            progress: 12.5,
            transcript_media_id: None,
        }
    }

    #[tokio::test]
    async fn startup_restores_paused_audio_and_space_resolves_fresh_cloud_progress() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/episode-played/list-history"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data":[{"episode":{
                    "eid":"recent","pid":"podcast","title":"最近收听","duration":60
                }}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let reads = std::sync::atomic::AtomicUsize::new(0);
        Mock::given(path("/v1/playback-progress/list"))
            .respond_with(move |_: &wiremock::Request| {
                let progress = if reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    12.5
                } else {
                    31.25
                };
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"eid":"recent","progress":progress}]}))
            })
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(path("/v1/episode/get"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{
                "eid":"recent","pid":"podcast","title":"最近收听","duration":60,
                "media":{"source":{"url":"https://media.example/recent.mp3"}}
            }})))
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.content = content::Api::for_test(server.uri());
        app.player_visible = false; // Startup begins with the overlay hidden.
        app.apply(2, Update::Restored(credentials()));
        app.tick();
        assert!(!app.player_visible);
        assert_eq!(app.player_height(24), 0);
        app.generation += 1; // Navigating or refreshing a list must not discard restoration.
        let (generation, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply(generation, update);
        let current = app.player.current.as_ref().unwrap();
        assert_eq!(current.eid, "recent");
        assert_eq!(current.position, 12.5);
        assert!(current.paused && !current.loading && app.player.awaiting_resume());
        assert!(!app.player_visible);
        assert!(!rendered(&mut app).replace(' ', "").contains("播放区"));
        app.key(KeyCode::Char('t'));
        assert!(
            rendered(&mut app)
                .replace(' ', "")
                .contains("已暂停·最近收听")
        );
        app.key(KeyCode::Char('t'));
        app.tick();
        assert!(app.dirty.is_empty());
        assert!(app.player.progress().is_none());
        assert!(app.play_task.is_none());
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        app.key(KeyCode::Char(' '));
        assert!(app.player_visible);
        let (_, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let Update::Resolved {
            result: Ok(source), ..
        } = update
        else {
            panic!("expected playback resolution");
        };
        assert_eq!(source.eid, "recent");
        assert_eq!(source.start, 31.25);
        // Inspect resolution without starting an audio decoder or downloading media.
    }

    #[test]
    fn stale_recent_history_cannot_replace_manual_playback_or_a_new_account() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Home(Home::default());
        app.play("manual".into());
        app.apply(
            0,
            Update::Recent {
                epoch: app.epoch,
                request: app.play_request - 1,
                result: Ok(Some(recent_episode())),
            },
        );
        assert!(app.player.current.is_none());
        app.apply(
            0,
            Update::Recent {
                epoch: app.epoch + 1,
                request: app.play_request,
                result: Ok(Some(recent_episode())),
            },
        );
        assert!(app.player.current.is_none());
        app.player = Player::simulated().0;
        app.apply(
            0,
            Update::Recent {
                epoch: app.epoch,
                request: app.play_request,
                result: Ok(Some(recent_episode())),
            },
        );
        assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
    }

    #[tokio::test]
    async fn recent_history_failure_keeps_browsing_available_and_space_retries_after_rate_limit() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.activate(credentials());
        app.view = View::Home(Home::default());
        app.apply(
            0,
            Update::Recent {
                epoch: app.epoch,
                request: app.play_request,
                result: Err(Error::RateLimited(Duration::from_secs(30))),
            },
        );
        assert!(app.notice.as_deref().unwrap().contains("最近收听加载失败"));
        app.key(KeyCode::Char(' '));
        assert!(app.recent_task.is_none());
        app.key(KeyCode::Char('4'));
        app.key(KeyCode::Enter);
        assert!(rendered(&mut app).replace(' ', "").contains("退出登录"));
        app.recent_retry_at = None;
        app.key(KeyCode::Char(' '));
        assert!(app.recent_task.is_some());
        app.recent_task.take().unwrap().abort();
        app.exiting = true;
        app.apply(
            0,
            Update::Recent {
                epoch: app.epoch,
                request: app.play_request,
                result: Ok(Some(recent_episode())),
            },
        );
        assert!(app.player.current.is_none());
    }

    fn highlighted_subtitle(app: &mut App) -> String {
        use ratatui::style::Color;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let top = 24 - app.player_height(24) + 1;
        (top..top + app.subtitle_height(24))
            .flat_map(|y| (0..80).map(move |x| (x, y)))
            .map(|position| &buffer[position])
            .filter(|cell| cell.fg == Color::Cyan)
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim()
            .to_owned()
    }

    #[test]
    fn subtitles_follow_playback_and_seeks_while_paused_on_other_pages() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        with_playlist(&mut app, &["episode"]);
        app.transcript = sample_transcript();
        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('2'));
        app.key(KeyCode::Enter);
        assert_eq!(highlighted_subtitle(&mut app), "opening sentence");
        app.apply(0, Update::Player(player::Event::Paused(1, true)));
        for (position, text) in [
            (30.49, "middle sentence"),
            (30.5, "ending sentence"),
            (15.0, "middle sentence"),
            (0.0, "opening sentence"),
        ] {
            app.apply(0, Update::Player(player::Event::Position(1, position)));
            assert_eq!(highlighted_subtitle(&mut app), text);
            assert!(app.player.current.as_ref().unwrap().paused);
        }
        app.apply(0, Update::Player(player::Event::Ended(1, true)));
        assert_eq!(app.subtitle_height(24), 0);
    }

    #[test]
    fn absent_failed_and_stale_transcripts_leave_no_placeholder_or_extra_space() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        with_playlist(&mut app, &["episode"]);
        let baseline = rendered(&mut app);
        for result in [Ok(Transcript::default()), Err(Error::Network)] {
            app.apply(
                0,
                Update::Transcript {
                    epoch: 0,
                    request: 0,
                    result,
                },
            );
            assert_eq!(app.subtitle_height(24), 0);
            assert_eq!(rendered(&mut app), baseline);
        }
        app.transcript = sample_transcript();
        app.load_transcript("episode".into(), None);
        assert!(app.transcript_task.is_none());
        assert_eq!(rendered(&mut app), baseline);
        for (epoch, request) in [(0, 0), (1, app.transcript_request)] {
            app.apply(
                0,
                Update::Transcript {
                    epoch,
                    request,
                    result: Ok(sample_transcript()),
                },
            );
            assert_eq!(rendered(&mut app), baseline);
        }
    }

    #[test]
    fn player_overlay_preserves_page_layout_and_toggle_does_not_stop_playback() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        with_playlist(&mut app, &["a", "b", "c", "d", "e", "f", "g", "h"]);
        let (player, mut commands) = Player::simulated();
        app.player = player;
        app.transcript = sample_transcript();
        app.player_visible = false;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let page = terminal.backend().buffer().clone();

        app.key(KeyCode::Char('T'));
        assert!(!app.player_visible && !app.player_expanded);

        app.key(KeyCode::Char('t'));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let overlay = terminal.backend().buffer();
        let top = 24 - app.player_height(24);
        assert!(app.subtitle_height(24) > 5);
        for y in 0..top {
            for x in 0..80 {
                assert_eq!(overlay[(x, y)], page[(x, y)], "page moved at {x},{y}");
            }
        }
        // The overlay's padding must erase text from the underlying list.
        assert!((top + 1..23).any(|y| {
            (1..79).any(|x| page[(x, y)].symbol() != " " && overlay[(x, y)].symbol() == " ")
        }));
        let separator = top + 1 + app.subtitle_height(24);
        assert!((1..79).any(|x| overlay[(x, separator)].symbol() == "─"));

        let collapsed = overlay.clone();
        app.key(KeyCode::Char('T'));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        assert!(app.player_expanded);
        assert_eq!(app.player_height(24), 23);
        assert_eq!(app.subtitle_height(24), 16);
        let expanded = terminal.backend().buffer().clone();
        assert_eq!(expanded[(0, 1)].symbol(), "╭");
        assert_eq!(expanded[(79, 23)].symbol(), "╯");
        for x in 0..80 {
            assert_eq!(expanded[(x, 0)], page[(x, 0)]);
        }
        // Covered page controls must not change the selected episode or play it.
        for key in [KeyCode::Down, KeyCode::Enter, KeyCode::Char('x')] {
            app.key(key);
        }
        assert!(app.play_task.is_none() && app.remove_queue.is_empty());
        app.key(KeyCode::Char('T'));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        for y in 0..24 {
            let mut x = 0;
            while x < 80 {
                assert_eq!(
                    terminal.backend().buffer()[(x, y)],
                    collapsed[(x, y)],
                    "restored cell {x},{y}"
                );
                x += ratatui::text::Line::from(collapsed[(x, y)].symbol())
                    .width()
                    .max(1) as u16;
            }
        }
        assert!(!app.player_expanded);
        app.key(KeyCode::Char('T'));
        app.key(KeyCode::Esc);
        assert!(app.player_visible && !app.player_expanded);
        app.key(KeyCode::Char('T'));

        app.key(KeyCode::Char('t'));
        assert!(!app.player_expanded);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        for y in 0..24 {
            let mut x = 0;
            while x < 80 {
                assert_eq!(terminal.backend().buffer()[(x, y)], page[(x, y)]);
                // A terminal ignores cells covered by the preceding wide character.
                x += ratatui::text::Line::from(page[(x, y)].symbol())
                    .width()
                    .max(1) as u16;
            }
        }
        assert_eq!(app.player_height(24), 0);
        assert!(commands.try_recv().is_err());
        app.apply(0, Update::Player(player::Event::Position(1, 30.5)));
        assert!(!app.player_visible);
        app.key(KeyCode::Char(' '));
        assert_eq!(commands.try_recv().unwrap(), player::Control::TogglePause);

        app.key(KeyCode::Char('?'));
        app.key(KeyCode::Char('t'));
        assert!(app.player_visible && app.help.open);
        app.key(KeyCode::Char('T'));
        assert!(app.player_expanded && app.help.open);
        assert!(
            rendered(&mut app)
                .replace(' ', "")
                .contains("快捷键·播放区")
        );
        app.key(KeyCode::Esc);
        assert!(app.player_expanded && !app.help.open);
        assert_eq!(highlighted_subtitle(&mut app), "ending sentence");
        for (width, height) in [(0, 0), (1, 1), (3, 3), (24, 8), (32, 12), (120, 40)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            assert_eq!(app.player_height(height), height.saturating_sub(1));
        }
        app.transcript = Transcript::default();
        assert_eq!(app.subtitle_height(24), 0);
        assert_eq!(app.player_height(24), 23);
        terminal.draw(|frame| app.draw(frame)).unwrap();
        app.player.current = None;
        app.notice = None;
        app.sync_error = None;
        app.player_expanded = false;
        app.key(KeyCode::Char('T'));
        assert!(!app.player_expanded);
        assert_eq!(app.player_height(24), 0);
    }

    #[tokio::test]
    async fn new_play_request_opens_player_but_resuming_current_audio_keeps_it_hidden() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.activate(credentials());
        with_playlist(&mut app, &["episode", "next"]);
        app.key(KeyCode::Char('t'));
        app.player.current.as_mut().unwrap().paused = true;
        app.play("episode".into());
        assert!(!app.player_visible);
        app.play("next".into());
        assert!(app.player_visible);
        assert!(app.play_task.is_some());
        // Cancel before yielding so this state test performs no network or decoder work.
        app.play_task.take().unwrap().abort();
        app.key(KeyCode::Char('t'));
        app.apply(
            0,
            Update::Resolved {
                epoch: app.epoch,
                request: app.play_request.saturating_sub(1),
                result: Err(Error::Network),
            },
        );
        assert!(!app.player_visible);
    }

    #[tokio::test]
    async fn subtitles_load_in_background_without_holding_account_lock_during_download() {
        let server = MockServer::start().await;
        let cdn = MockServer::start().await;
        Mock::given(path("/v1/episode-transcript/get"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    json!({"data":{"transcriptUrl":format!("{}/captions",cdn.uri())}}),
                ),
            )
            .expect(1)
            .mount(&server)
            .await;
        let downloading = Arc::new(tokio::sync::Notify::new());
        let signal = downloading.clone();
        Mock::given(path("/captions"))
            .respond_with(move |_: &wiremock::Request| {
                signal.notify_one();
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(200))
                    .set_body_json(json!([{"startMs":0,"text":"loaded caption"}]))
            })
            .expect(1)
            .mount(&cdn)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.content = content::Api::for_test(server.uri());
        app.activate(credentials());
        with_playlist(&mut app, &["episode"]);
        app.load_transcript("episode".into(), Some("media".into()));
        assert!(app.transcript_task.is_some());
        assert_eq!(app.subtitle_height(24), 0);
        tokio::time::timeout(Duration::from_secs(2), downloading.notified())
            .await
            .unwrap();
        tokio::time::timeout(
            Duration::from_millis(100),
            app.account
                .as_ref()
                .unwrap()
                .request(|_, _| async { Ok(()) }),
        )
        .await
        .unwrap()
        .unwrap();
        let (generation, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply(generation, update);
        assert_eq!(highlighted_subtitle(&mut app), "loaded caption");
        assert!(!app.player.current.as_ref().unwrap().paused);
    }

    fn with_playlist(app: &mut App, ids: &[&str]) {
        let mut home = Home::default();
        home.key(KeyCode::Enter);
        home.apply_playlist(Ok(ids
            .iter()
            .map(|eid| PlaylistEntry {
                eid: (*eid).into(),
                episode: Ok(content::Episode {
                    eid: (*eid).into(),
                    title: format!("queue {eid}"),
                    duration: Some(60),
                    podcast: None,
                    ..Default::default()
                }),
                progress: Some(0.0),
                progress_failed: false,
            })
            .collect()));
        app.view = View::Home(home);
        app.player = Player::simulated().0;
    }

    #[tokio::test]
    async fn natural_end_removes_finished_episode_and_starts_next_with_saved_progress() {
        assert_removal_advances_playback(true).await;
    }

    #[tokio::test]
    async fn removing_current_episode_stops_it_and_starts_next_with_saved_progress() {
        assert_removal_advances_playback(false).await;
    }

    async fn assert_removal_advances_playback(finished: bool) {
        use std::sync::Mutex;
        use wiremock::matchers::{body_json, query_param};

        let progress = if finished { 60 } else { 10 };
        let server = MockServer::start().await;
        let queue = Arc::new(Mutex::new(vec!["before", "episode", "next", "selected"]));
        let shared = queue.clone();
        Mock::given(path("/v1/playlist/pull"))
            .respond_with(move |_: &wiremock::Request| {
                ResponseTemplate::new(200).set_body_json(
                    json!({"data":{"list":*shared.lock().unwrap(),"sha":"revision"}}),
                )
            })
            .expect(2)
            .mount(&server)
            .await;
        let shared = queue.clone();
        Mock::given(path("/v1/playlist/patch"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(
                    body["ops"],
                    json!([{"action":"rem","item":"episode","pos":1}])
                );
                shared.lock().unwrap().retain(|eid| *eid != "episode");
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":{"kind":"ACK","id":body["id"],"sha":"updated"}}))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/playback-progress/update"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["data"][0]["eid"], "episode");
                assert_eq!(body["data"][0]["progress"], progress);
                ResponseTemplate::new(200).set_body_json(json!({}))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/episode/get"))
            .and(query_param("eid", "next"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{
                "eid":"next", "pid":"podcast", "title":"next title", "duration":60,
                "media":{"source":{"url":"https://media.example/next.mp3"}}
            }})))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/playback-progress/list"))
            .and(body_json(json!({"eids":["next"]})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"eid":"next","progress":12.5}]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.content = content::Api::for_test(server.uri());
        app.activate(credentials());
        with_playlist(&mut app, &["before", "episode", "next", "selected"]);
        if !finished {
            app.key(KeyCode::Down);
            app.key(KeyCode::Char('x'));
            assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
            assert!(app.play_task.is_none());
        }
        app.key(KeyCode::End);
        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('2'));
        app.key(KeyCode::Enter);
        app.player_visible = false;
        if finished {
            app.apply(0, Update::Player(player::Event::Ended(1, false)));
            assert_eq!(app.dirty["episode"].progress, progress);
            assert!(app.play_task.is_some());
            let request = app.play_request;
            app.apply(0, Update::Player(player::Event::Ended(1, false)));
            assert_eq!(app.play_request, request);
        }
        assert!(app.remove_task.is_some());

        let mut started = false;
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.sync_task.is_some() || app.remove_task.is_some() || app.play_task.is_some() {
                let (generation, update) = receiver.recv().await.unwrap();
                let resolved = matches!(&update, Update::Resolved { result: Ok(_), .. });
                let removed = matches!(&update, Update::Removed { result: Ok(()), .. });
                app.apply(generation, update);
                if removed && !finished {
                    assert!(app.player.current.is_none());
                    assert!(app.play_task.is_some());
                    let request = app.play_request;
                    app.apply(0, Update::Player(player::Event::Ended(1, false)));
                    assert_eq!(app.play_request, request);
                    assert_eq!(app.dirty["episode"].progress, progress);
                }
                if resolved {
                    assert!(app.player_visible);
                    let current = app.player.current.as_ref().unwrap();
                    assert_eq!(current.eid, "next");
                    assert_eq!(current.position, 12.5);
                    assert!(!current.paused);
                    assert!(!current.ended);
                    started = true;
                    // Verify the handoff without launching mpv or fetching remote media.
                    app.player.stop();
                }
            }
        })
        .await
        .unwrap();
        assert!(started);
        assert_eq!(*queue.lock().unwrap(), ["before", "next", "selected"]);
        assert_eq!(app.last_synced["episode"], progress);
        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('1'));
        app.key(KeyCode::Enter);
        assert!(!rendered(&mut app).contains("queue episode"));
        let View::Home(home) = &mut app.view else {
            panic!()
        };
        assert!(matches!(home.key(KeyCode::Enter), Action::Play(eid) if eid == "selected"));
    }

    #[tokio::test]
    async fn removal_failure_keeps_the_row_and_notice_regardless_of_when_next_track_loads() {
        for removed_first in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut app = app(directory.path().to_owned());
            app.activate(credentials());
            with_playlist(&mut app, &["episode", "next"]);
            app.apply(0, Update::Player(player::Event::Ended(1, false)));
            app.remove_task.take().unwrap().abort();
            app.play_task.take().unwrap().abort();
            let removed = Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Err(Error::Network),
            };
            let resolved = Update::Resolved {
                epoch: 0,
                request: app.play_request,
                result: Ok(Playable {
                    eid: "next".into(),
                    pid: "podcast".into(),
                    title: "next title".into(),
                    url: "https://media.example/next.mp3".into(),
                    duration: Some(60),
                    start: 0.0,
                    transcript_media_id: None,
                }),
            };
            for update in if removed_first {
                [removed, resolved]
            } else {
                [resolved, removed]
            } {
                app.apply(0, update);
            }
            assert_eq!(app.player.current.as_ref().unwrap().eid, "next");
            app.player.stop();
            assert!(rendered(&mut app).contains("queue episode"));
            assert!(app.notice.as_deref().unwrap().starts_with("移除失败："));
            app.remove("episode".into());
            app.remove_task.take().unwrap().abort();
            app.apply(
                0,
                Update::Removed {
                    epoch: 0,
                    eid: "episode".into(),
                    result: Ok(()),
                },
            );
            assert!(app.notice.is_none());
            assert!(!rendered(&mut app).contains("queue episode"));
        }
    }

    #[tokio::test]
    async fn stale_failed_and_paused_events_do_not_remove_or_advance() {
        for event in [
            player::Event::Ended(0, false),
            player::Event::Ended(1, true),
            player::Event::Failed(1, "decoder failed".into()),
            player::Event::Paused(1, true),
            player::Event::Position(1, 60.0),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let mut app = app(directory.path().to_owned());
            app.activate(credentials());
            with_playlist(&mut app, &["episode", "next"]);
            // Keep progress uploads out of this state-transition test.
            app.sync_task = Some(tokio::spawn(std::future::pending()));
            app.apply(0, Update::Player(event));
            assert!(app.remove_task.is_none());
            assert!(app.play_task.is_none());
            assert_eq!(app.play_request, 0);
            assert!(rendered(&mut app).contains("queue episode"));
        }
    }

    #[tokio::test]
    async fn finishing_last_episode_removes_it_without_wrapping_to_the_start() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.activate(credentials());
        with_playlist(&mut app, &["before", "episode"]);
        app.apply(0, Update::Player(player::Event::Ended(1, false)));
        assert!(app.remove_task.is_some());
        assert!(app.play_task.is_none());
        assert_eq!(app.play_request, 0);
        app.remove_task.take().unwrap().abort();
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Ok(()),
            },
        );
        assert!(!rendered(&mut app).contains("queue episode"));
        assert!(app.player.current.as_ref().unwrap().ended);
    }

    #[tokio::test]
    async fn natural_end_preserves_a_manual_play_request_and_does_not_play_during_exit() {
        for exiting in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut app = app(directory.path().to_owned());
            app.activate(credentials());
            with_playlist(&mut app, &["episode", "next", "chosen"]);
            app.exiting = exiting;
            if !exiting {
                app.play("chosen".into());
            }
            let request = app.play_request;
            app.apply(0, Update::Player(player::Event::Ended(1, false)));
            assert!(app.remove_task.is_some());
            assert_eq!(app.play_request, request);
            assert_eq!(app.play_task.is_some(), !exiting);
        }
    }

    #[tokio::test]
    async fn completion_removal_waits_for_an_in_flight_manual_removal() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.activate(credentials());
        with_playlist(&mut app, &["other", "episode"]);
        app.remove("other".into());
        app.apply(0, Update::Player(player::Event::Ended(1, false)));
        assert_eq!(app.remove_queue, ["other", "episode"]);
        app.remove_task.take().unwrap().abort();
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "other".into(),
                result: Ok(()),
            },
        );
        assert_eq!(app.remove_queue, ["episode"]);
        assert!(app.remove_task.is_some());
        app.remove_task.take().unwrap().abort();
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Ok(()),
            },
        );
        assert!(app.remove_queue.is_empty());
        assert!(app.remove_task.is_none());
        assert!(!rendered(&mut app).contains("queue episode"));
    }

    #[test]
    fn refreshed_qr_ignores_old_success_and_does_not_save_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.apply(1, Update::Authenticated(credentials()));
        assert!(matches!(app.view, View::Loading));
        assert!(app.store.load().unwrap().is_none());
    }

    #[test]
    fn failed_save_retains_credentials_for_retry_without_another_scan() {
        let directory = tempfile::tempdir().unwrap();
        let location = directory.path().join("state");
        std::fs::write(&location, "blocks directory creation").unwrap();
        let mut app = app(location.clone());
        app.apply(2, Update::Authenticated(credentials()));
        assert!(matches!(app.view, View::SaveFailed(_)));
        std::fs::remove_file(location).unwrap();
        app.retry();
        assert!(matches!(app.view, View::Home(_)));
        assert_eq!(
            app.store.load().unwrap().unwrap().refresh_token,
            "test-refresh"
        );
    }

    #[tokio::test]
    async fn dropping_app_cancels_pending_login_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        app.task = Some(task);
        drop(app);
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }

    #[tokio::test]
    async fn home_logout_returns_to_login_and_preserves_other_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.store.save(&credentials()).unwrap();
        let preferences = directory.path().join("preferences.json");
        std::fs::write(&preferences, "{}").unwrap();
        app.apply(2, Update::Restored(credentials()));
        assert!(matches!(app.view, View::Home(_)));

        // Login shortcuts and Enter on a list must not invalidate the session.
        app.key(KeyCode::Char('r'));
        app.key(KeyCode::Char('2'));
        app.key(KeyCode::Enter);
        assert_eq!(app.generation, 2);
        assert!(app.store.load().unwrap().is_some());

        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('4'));
        app.key(KeyCode::Enter);
        assert!(app.store.load().unwrap().is_some());
        assert_eq!(app.generation, 2);
        app.key(KeyCode::Tab);
        app.key(KeyCode::Enter);
        assert!(matches!(app.view, View::Loading));
        assert_eq!(app.generation, 3);
        assert!(app.store.load().unwrap().is_none());
        assert_eq!(std::fs::read_to_string(preferences).unwrap(), "{}");
    }

    #[tokio::test]
    async fn failed_logout_keeps_home_until_clear_can_be_retried() {
        let directory = tempfile::tempdir().unwrap();
        let location = directory.path().join("session.json");
        std::fs::create_dir(&location).unwrap();
        let mut app = app(directory.path().to_owned());
        app.apply(2, Update::Restored(credentials()));
        app.key(KeyCode::Char('4'));
        app.key(KeyCode::Enter);
        app.key(KeyCode::Tab);
        app.key(KeyCode::Enter);
        assert!(matches!(app.view, View::Home(_)));
        assert_eq!(app.generation, 2);

        std::fs::remove_dir(location).unwrap();
        app.key(KeyCode::Enter);
        assert!(matches!(app.view, View::Loading));
        assert_eq!(app.generation, 3);
    }

    fn rendered(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[tokio::test]
    async fn startup_renews_saved_login_instead_of_asking_for_another_scan() {
        let server = MockServer::start().await;
        Mock::given(path("/web/user/get-me"))
            .and(header("x-jike-access-token", "test-access"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        Mock::given(path("/app_auth_tokens.refresh"))
            .and(header("x-jike-refresh-token", "test-refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-jike-access-token", "renewed-access")
                    .insert_header("x-jike-refresh-token", "renewed-refresh")
                    .set_body_json(json!({})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/web/user/get-me"))
            .and(header("x-jike-access-token", "renewed-access"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data":{"uid":"test-user"}})),
            )
            .mount(&server)
            .await;
        Mock::given(path("/v1/auth/qrcode/create"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id":"test-qr", "url":"https://h5.xiaoyuzhoufm.com/oauth?qrcode_id=test-qr"
            })))
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.store.save(&credentials()).unwrap();
        app.api = Api::for_test(server.uri());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.start(true);
        let (generation, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply(generation, update);
        assert!(
            matches!(app.view, View::Home(_)),
            "已有可续期的登录状态，启动却没有进入主界面"
        );
        assert!(
            app.store
                .load()
                .unwrap()
                .is_some_and(|saved| saved.access_token == "renewed-access"
                    && saved.refresh_token == "renewed-refresh")
        );
        app.start(true);
        let (generation, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply(generation, update);
        assert!(
            matches!(app.view, View::Home(_)),
            "续期后再次启动应复用新凭据"
        );
        let requests = server.received_requests().await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter(|r| r.url.path() == "/app_auth_tokens.refresh")
                .count(),
            1
        );
        assert!(
            !requests
                .iter()
                .any(|r| r.url.path() == "/v1/auth/qrcode/create")
        );
    }

    #[tokio::test]
    async fn startup_only_requests_a_new_scan_when_refresh_is_rejected() {
        for status in [401, 429, 503, 400] {
            let server = MockServer::start().await;
            Mock::given(path("/web/user/get-me"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            Mock::given(path("/app_auth_tokens.refresh"))
                .respond_with(ResponseTemplate::new(status))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(path("/v1/auth/qrcode/create"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id":"test-qr", "url":"https://h5.xiaoyuzhoufm.com/oauth?qrcode_id=test-qr"
                })))
                .expect(if status == 401 { 1 } else { 0 })
                .mount(&server)
                .await;
            let directory = tempfile::tempdir().unwrap();
            let mut app = app(directory.path().to_owned());
            app.store.save(&credentials()).unwrap();
            app.api = Api::for_test(server.uri());
            let (sender, mut receiver) = mpsc::unbounded_channel();
            app.sender = sender;
            app.start(true);
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            app.apply(generation, update);
            if status == 401 {
                assert!(matches!(app.view, View::Qr { .. }));
            } else {
                assert!(matches!(
                    app.view,
                    View::Failed {
                        retry_saved: true,
                        ..
                    }
                ));
            }
            assert!(
                app.store
                    .load()
                    .unwrap()
                    .is_some_and(|saved| saved.refresh_token == "test-refresh")
            );
        }
    }

    #[tokio::test]
    async fn playlist_renews_once_saves_tokens_then_retries_without_losing_the_page() {
        for status in [200, 401] {
            let server = MockServer::start().await;
            Mock::given(path("/v1/playlist/pull"))
                .and(header("x-jike-access-token", "test-access"))
                .respond_with(ResponseTemplate::new(401))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(path("/app_auth_tokens.refresh"))
                .and(header("x-jike-refresh-token", "test-refresh"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("x-jike-access-token", "renewed-access")
                        .insert_header("x-jike-refresh-token", "renewed-refresh"),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(path("/v1/playlist/pull"))
                .and(header("x-jike-access-token", "renewed-access"))
                .respond_with(
                    ResponseTemplate::new(status).set_body_json(json!({"data":{"list":[]}})),
                )
                .expect(1)
                .mount(&server)
                .await;
            let directory = tempfile::tempdir().unwrap();
            let mut app = app(directory.path().to_owned());
            app.api = Api::for_test(server.uri());
            app.content = content::Api::for_test(server.uri());
            let (sender, mut receiver) = mpsc::unbounded_channel();
            app.sender = sender;
            app.apply(2, Update::Restored(credentials()));
            app.key(KeyCode::Enter);
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            app.apply(generation, update);
            assert!(
                app.store
                    .load()
                    .unwrap()
                    .is_some_and(|saved| saved.access_token == "renewed-access")
            );
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            app.apply(generation, update);
            assert!(matches!(app.view, View::Home(_)));
            if status != 200 {
                app.key(KeyCode::Char('?'));
            }
            assert!(
                rendered(&mut app)
                    .replace(' ', "")
                    .contains(if status == 200 {
                        "暂无单集"
                    } else {
                        "重新扫码登录"
                    })
            );
        }
    }

    #[test]
    fn stale_renewal_never_overwrites_the_current_session() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.store.save(&credentials()).unwrap();
        app.apply(
            1,
            Update::Renewed(Renewal {
                epoch: app.epoch + 1,
                credentials: Credentials {
                    access_token: "stale-access".into(),
                    refresh_token: "stale-refresh".into(),
                },
                saved: tokio::sync::oneshot::channel().0,
            }),
        );
        assert!(
            app.store
                .load()
                .unwrap()
                .is_some_and(|saved| saved.access_token == "test-access")
        );
        assert!(matches!(app.view, View::Loading));
    }

    #[tokio::test]
    async fn playlist_load_uses_session_and_returns_without_reopening_the_page() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/playlist/pull"))
            .and(header("x-jike-access-token", "test-access"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data":{"list":["test-episode"]}})),
            )
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/episode/get"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data":{"eid":"test-episode","title":"fixture episode","duration":65}}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.content = content::Api::for_test(server.uri());
        app.apply(2, Update::Restored(credentials()));
        app.key(KeyCode::Enter);
        assert_eq!(app.generation, 3);
        app.key(KeyCode::Char('r'));
        assert_eq!(app.generation, 3);
        app.key(KeyCode::Esc);
        let (generation, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply(generation, update);
        assert!(!rendered(&mut app).contains("fixture episode"));
        app.key(KeyCode::Enter);
        assert_eq!(app.generation, 3);
        assert!(rendered(&mut app).contains("fixture episode"));

        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('4'));
        app.key(KeyCode::Enter);
        app.key(KeyCode::Tab);
        app.key(KeyCode::Enter);
        assert!(app.credentials.is_none());
        app.apply(generation, Update::Playlist(Err(Error::Network)));
        assert!(matches!(app.view, View::Loading));
    }

    #[tokio::test]
    async fn expired_playlist_session_offers_qr_login_without_deleting_saved_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.store.save(&credentials()).unwrap();
        app.apply(2, Update::Restored(credentials()));
        app.key(KeyCode::Enter);
        app.apply(
            3,
            Update::Playlist(Err(Error::Http(reqwest::StatusCode::UNAUTHORIZED))),
        );
        assert!(!rendered(&mut app).contains("Enter"));
        app.key(KeyCode::Char('?'));
        assert!(rendered(&mut app).replace(' ', "").contains("重新扫码登录"));
        app.key(KeyCode::Esc);
        app.key(KeyCode::Enter);
        assert!(matches!(app.view, View::Loading));
        assert!(app.credentials.is_none());
        assert!(app.store.load().unwrap().is_some());
    }
    #[test]
    fn playback_shortcuts_work_on_every_page_and_navigation_keeps_the_track() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Home(Home::default());
        let (player, mut commands) = Player::simulated();
        app.player = player;
        for menu in ['1', '2', '3', '4'] {
            for (key, control) in [
                (' ', player::Control::TogglePause),
                ('a', player::Control::Seek(-15)),
                ('d', player::Control::Seek(15)),
            ] {
                app.key(KeyCode::Char(key));
                assert_eq!(commands.try_recv().unwrap(), control);
            }
            app.key(KeyCode::Char(menu));
            app.key(KeyCode::Enter);
            for (key, control) in [
                (' ', player::Control::TogglePause),
                ('a', player::Control::Seek(-15)),
                ('d', player::Control::Seek(15)),
            ] {
                app.key(KeyCode::Char(key));
                assert_eq!(commands.try_recv().unwrap(), control);
            }
            assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
            assert!(rendered(&mut app).replace(' ', "").contains("测试单集"));
            app.key(KeyCode::Esc);
        }
    }

    #[test]
    fn help_captures_page_actions_and_restores_each_page_while_playback_controls_work() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Home(Home::default());
        let (player, mut commands) = Player::simulated();
        app.player = player;
        for (menu, title) in [
            (None, "主菜单"),
            (Some('1'), "播放列表"),
            (Some('2'), "订阅列表"),
            (Some('3'), "推荐列表"),
            (Some('4'), "设置"),
        ] {
            if let Some(menu) = menu {
                app.key(KeyCode::Char(menu));
                app.key(KeyCode::Enter);
            }
            let before = rendered(&mut app);
            assert!(before.replace(' ', "").contains("？帮助"));
            for hint in ["Enter", "Esc", "Space", "Tab", "PgUp"] {
                assert!(!before.contains(hint), "unexpected hint: {hint}");
            }
            app.key(KeyCode::Char('?'));
            assert!(
                rendered(&mut app)
                    .replace(' ', "")
                    .contains(&format!("快捷键·{title}"))
            );
            for key in [
                KeyCode::Enter,
                KeyCode::Char('x'),
                KeyCode::Char('y'),
                KeyCode::Char('r'),
                KeyCode::Char('2'),
                KeyCode::Tab,
                KeyCode::Down,
                KeyCode::End,
                KeyCode::Left,
            ] {
                app.key(key);
            }
            assert!(app.help.open);
            for (key, control) in [
                (' ', player::Control::TogglePause),
                ('a', player::Control::Seek(-15)),
                ('d', player::Control::Seek(15)),
            ] {
                app.key(KeyCode::Char(key));
                assert_eq!(commands.try_recv().unwrap(), control);
            }
            app.key(KeyCode::Esc);
            assert!(!app.help.open);
            assert_eq!(rendered(&mut app), before);
            app.key(KeyCode::Char('？'));
            app.key(KeyCode::Char('?'));
            assert!(!app.help.open);
            if menu.is_some() {
                app.key(KeyCode::Esc);
            }
        }
    }

    #[test]
    fn escape_closes_login_help_before_it_can_exit_the_application() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Expired;
        let before = rendered(&mut app);
        assert!(app.escape_exits());
        app.key(KeyCode::Char('?'));
        assert!(!app.escape_exits());
        app.key(KeyCode::Char('r'));
        assert!(matches!(app.view, View::Expired));
        assert!(rendered(&mut app).replace(' ', "").contains("快捷键·登录"));
        app.key(KeyCode::Esc);
        assert!(app.escape_exits());
        assert_eq!(rendered(&mut app), before);
    }

    #[test]
    fn older_upload_ack_keeps_newer_progress_and_retry_after_blocks_forced_uploads() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.player = Player::simulated().0;
        app.remember_progress();
        let old = app.dirty["episode"].clone();
        app.player.apply(player::Event::Position(1, 22.0));
        app.remember_progress();
        app.apply(
            0,
            Update::Synced {
                epoch: 0,
                progress: vec![old],
                result: Ok(()),
            },
        );
        assert_eq!(app.dirty["episode"].progress, 22);
        app.apply(
            0,
            Update::Synced {
                epoch: 0,
                progress: vec![],
                result: Err(Error::RateLimited(Duration::from_secs(60))),
            },
        );
        app.flush_progress(true);
        assert!(app.sync_task.is_none());
        assert_eq!(app.dirty["episode"].progress, 22);
    }

    #[test]
    fn removing_the_last_episode_only_stops_playback_after_success() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        let mut home = Home::default();
        home.key(KeyCode::Enter);
        home.apply_playlist(Ok(vec![PlaylistEntry {
            eid: "episode".into(),
            episode: Ok(content::Episode {
                eid: "episode".into(),
                title: "列表里的标题".into(),
                duration: Some(60),
                podcast: None,
                ..Default::default()
            }),
            progress: Some(10.0),
            progress_failed: false,
        }]));
        app.view = View::Home(home);
        app.player = Player::simulated().0;
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Err(Error::Network),
            },
        );
        assert!(rendered(&mut app).replace(' ', "").contains("列表里的标题"));
        assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Ok(()),
            },
        );
        assert!(!rendered(&mut app).replace(' ', "").contains("列表里的标题"));
        assert!(app.player.current.is_none());
        assert!(app.play_task.is_none());
        assert_eq!(app.dirty["episode"].progress, 10);
    }

    #[test]
    fn removing_another_episode_keeps_current_playback() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        with_playlist(&mut app, &["other", "episode", "next"]);
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "other".into(),
                result: Ok(()),
            },
        );
        assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
        assert_eq!(app.player.current.as_ref().unwrap().position, 10.0);
        assert!(!app.player.current.as_ref().unwrap().paused);
        assert!(app.play_task.is_none());
    }

    #[tokio::test]
    async fn removing_current_episode_preserves_a_pending_manual_play_request() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.activate(credentials());
        with_playlist(&mut app, &["episode", "next", "chosen"]);
        app.play("chosen".into());
        let request = app.play_request;
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Ok(()),
            },
        );
        assert!(app.player.current.is_none());
        assert!(app.play_task.is_some());
        assert_eq!(app.play_request, request);
        assert_eq!(app.dirty["episode"].progress, 10);
    }
    #[tokio::test]
    async fn pausing_during_an_upload_sends_latest_position_as_soon_as_it_finishes() {
        let server = MockServer::start().await;
        Mock::given(path("/v1/playback-progress/update"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.content = content::Api::for_test(server.uri());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.activate(credentials());
        app.player = Player::simulated().0;
        app.remember_progress();
        let old = app.dirty["episode"].clone();
        app.sync_task = Some(tokio::spawn(std::future::ready(())));
        app.player.apply(player::Event::Position(1, 22.0));
        app.apply(0, Update::Player(player::Event::Paused(1, true)));
        assert!(app.flush_again);
        app.apply(
            0,
            Update::Synced {
                epoch: 0,
                progress: vec![old],
                result: Ok(()),
            },
        );
        assert!(app.sync_task.is_some());
        let (generation, update) = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply(generation, update);
        assert!(app.dirty.is_empty());
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = requests[0].body_json().unwrap();
        assert_eq!(body["data"][0]["progress"], 22);
    }
    #[tokio::test]
    async fn subscription_requests_renew_login_and_survive_playlist_generation_changes() {
        assert_browse_renewal('2').await;
    }

    #[tokio::test]
    async fn recommendation_requests_renew_login_and_reuse_episode_details_and_playback() {
        assert_browse_renewal('3').await;
    }

    #[tokio::test]
    async fn settings_requests_renew_login_and_reuse_episode_details_and_playback() {
        assert_browse_renewal('4').await;
    }

    async fn assert_browse_renewal(menu: char) {
        let recommendations = menu == '3';
        let history = menu == '4';
        let server = MockServer::start().await;
        let endpoint = if recommendations {
            "/v1/top-list/get"
        } else if history {
            "/v1/episode-played/list-history"
        } else {
            "/v2/inbox/list"
        };
        if history {
            Mock::given(path("/v1/profile/get"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"data":{"uid":"me"}})),
                )
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(path("/v1/user-stats/get"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(json!({"data":{"totalPlayedSeconds":3661}})),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        Mock::given(path(endpoint))
            .and(header("x-jike-access-token", "test-access"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/app_auth_tokens.refresh"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-jike-access-token", "renewed-access")
                    .insert_header("x-jike-refresh-token", "renewed-refresh"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let episode = json!({"eid":"a","title":"订阅单集","duration":600,"pubDate":"2026-09-14T17:30:00Z","podcast":{"title":"测试播客"}});
        let response = if recommendations {
            json!({"data":{"category":"HOT_EPISODES_IN_24_HOURS","targetType":"EPISODE","items":[{"item":episode}]}})
        } else if history {
            json!({"data":[{"episode":episode}]})
        } else {
            json!({"data":[episode]})
        };
        Mock::given(path(endpoint))
            .and(header("x-jike-access-token", "renewed-access"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/episode/get"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":{"eid":"a","title":"订阅单集","shownotes":"# 完整简介","podcast":{"title":"测试播客"}}})))
            .mount(&server).await;
        Mock::given(path("/v1/comment/list-primary"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data":[{"id":"c","text":"真实布局的评论","author":{"nickname":"听众"}}]}),
            ))
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.api = Api::for_test(server.uri());
        app.content = content::Api::for_test(server.uri());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.apply(2, Update::Restored(credentials()));
        app.key(KeyCode::Char(menu));
        app.key(KeyCode::Enter);
        for _ in 0..if history { 3 } else { 2 } {
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            // A playlist refresh or removal must not discard these responses.
            app.generation += 1;
            app.apply(generation, update);
        }
        assert_eq!(
            app.store.load().unwrap().unwrap().access_token,
            "renewed-access"
        );
        assert!(rendered(&mut app).replace(' ', "").contains("订阅单集"));
        if history {
            assert!(
                rendered(&mut app)
                    .replace(' ', "")
                    .contains("累计收听·1小时1分钟")
            );
        }
        app.key(KeyCode::Enter);
        for _ in 0..2 {
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            app.apply(generation, update);
        }
        let text = rendered(&mut app).replace(' ', "");
        assert!(text.contains("完整简介"), "{text}");
        assert!(!text.contains("真实布局的评论"), "{text}");
        app.key(KeyCode::Tab);
        let text = rendered(&mut app).replace(' ', "");
        assert!(text.contains("真实布局的评论"), "{text}");
        assert!(!text.contains("完整简介"), "{text}");
        let previous = app.play_request;
        app.key(KeyCode::Enter);
        assert_eq!(app.play_request, previous + 1);
        assert!(app.play_task.is_some());
        app.key(KeyCode::Esc);
        assert!(
            rendered(&mut app)
                .replace(' ', "")
                .contains(if recommendations {
                    "推荐列表"
                } else if history {
                    "收听历史"
                } else {
                    "订阅列表"
                })
        );
    }

    #[test]
    fn settings_responses_survive_navigation_but_ignore_old_requests_and_accounts() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Home(Home::default());
        app.key(KeyCode::Char('4'));
        app.key(KeyCode::Enter);
        let slot =
            subscriptions::Source::History.slot() + subscriptions::Request::ListeningTime.slot();
        app.browse_requests[slot] = 2;
        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('2'));
        app.key(KeyCode::Enter);
        for (epoch, request, seconds) in [
            (app.epoch, 2, 3600),
            (app.epoch, 1, 7200),
            (app.epoch + 1, 2, 10800),
        ] {
            app.apply(
                0,
                Update::Browse {
                    epoch,
                    slot,
                    request,
                    source: subscriptions::Source::History,
                    response: subscriptions::Response::ListeningTime(Ok(seconds)),
                },
            );
        }
        assert!(!rendered(&mut app).contains("累计收听"));
        app.key(KeyCode::Esc);
        app.key(KeyCode::Char('4'));
        app.key(KeyCode::Enter);
        let body = rendered(&mut app).replace(' ', "");
        assert!(body.contains("累计收听·1小时0分钟"));
        assert!(!body.contains("2小时") && !body.contains("3小时"));
    }

    #[test]
    fn stale_subscription_detail_and_old_account_responses_are_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Home(Home::default());
        let View::Home(home) = &mut app.view else {
            unreachable!()
        };
        home.key(KeyCode::Char('2'));
        home.key(KeyCode::Enter);
        let episode = || content::Episode {
            eid: "a".into(),
            title: "当前单集".into(),
            ..Default::default()
        };
        home.apply_browse(
            subscriptions::Source::Subscriptions,
            subscriptions::Response::Feed(Ok(content::Page {
                items: vec![episode()],
                cursor: None,
            })),
        );
        home.key(KeyCode::Enter);
        app.browse_requests[1] = 2;
        for (epoch, request) in [(app.epoch, 1), (app.epoch + 1, 2)] {
            let mut old = episode();
            old.title = "过时标题".into();
            app.apply(
                0,
                Update::Browse {
                    epoch,
                    slot: 1,
                    request,
                    source: subscriptions::Source::Subscriptions,
                    response: subscriptions::Response::Detail("a".into(), Ok(old)),
                },
            );
        }
        let text = rendered(&mut app).replace(' ', "");
        assert!(text.contains("当前单集"));
        assert!(!text.contains("过时标题"));
    }

    #[tokio::test]
    async fn switching_categories_keeps_pending_feeds_separate_from_subscriptions() {
        use crate::recommendations::Kind;
        let server = MockServer::start().await;
        for kind in [Kind::Hot, Kind::Trending] {
            Mock::given(path("/v1/top-list/get"))
                .and(wiremock::matchers::query_param("category", kind.category().unwrap()))
                .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(20)).set_body_json(json!({"data":{
                    "category":kind.category(),"targetType":"EPISODE","items":[{"item":{"eid":kind.label(),"title":format!("{}单集",kind.label())}}]
                }}))).expect(1).mount(&server).await;
        }
        Mock::given(path("/v2/inbox/list"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"eid":"sub","title":"独立订阅单集"}]})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.api = Api::for_test(server.uri());
        app.content = content::Api::for_test(server.uri());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.apply(2, Update::Restored(credentials()));
        for key in [
            KeyCode::Char('3'),
            KeyCode::Enter,
            KeyCode::Tab,
            KeyCode::Esc,
            KeyCode::Char('2'),
            KeyCode::Enter,
        ] {
            app.key(key);
        }
        for _ in 0..3 {
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            app.apply(generation, update);
        }
        assert!(rendered(&mut app).replace(' ', "").contains("独立订阅单集"));
        for key in [KeyCode::Esc, KeyCode::Char('3'), KeyCode::Enter] {
            app.key(key);
        }
        let text = rendered(&mut app).replace(' ', "");
        assert!(text.contains("锋芒榜单集") && !text.contains("独立订阅单集"));
        app.key(KeyCode::BackTab);
        assert!(rendered(&mut app).replace(' ', "").contains("最热榜单集"));
        assert!(app.browse_tasks.iter().all(Option::is_none));
    }
    #[tokio::test]
    async fn subscription_y_waits_for_confirmation_refreshes_queue_and_keeps_playing() {
        assert_subscription_addition(false, false).await;
    }

    #[tokio::test]
    async fn subscription_play_also_adds_to_queue_without_duplicate_entries() {
        assert_subscription_addition(true, false).await;
    }

    #[tokio::test]
    async fn subscription_addition_failure_does_not_cancel_requested_playback() {
        assert_subscription_addition(true, true).await;
    }

    async fn assert_subscription_addition(play: bool, fail_addition: bool) {
        use std::sync::Mutex;
        let server = MockServer::start().await;
        let ids = Arc::new(Mutex::new(vec!["queued"]));
        let shared = ids.clone();
        Mock::given(path("/v1/playlist/pull"))
            .respond_with(move |_: &wiremock::Request| {
                ResponseTemplate::new(200).set_body_json(
                    json!({"data":{"list":*shared.lock().unwrap(),"sha":"revision"}}),
                )
            })
            .mount(&server)
            .await;
        let shared = ids.clone();
        Mock::given(path("/v1/playlist/patch"))
            .respond_with(move |request: &wiremock::Request| {
                let body: serde_json::Value = request.body_json().unwrap();
                assert_eq!(body["ops"], json!([{"action":"add","item":"a","pos":1}]));
                if fail_addition {
                    return ResponseTemplate::new(503);
                }
                shared.lock().unwrap().push("a");
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":{"kind":"ACK","id":body["id"],"sha":"added"}}))
                    .set_delay(Duration::from_millis(50))
            })
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(path("/v1/playback-progress/list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[]})))
            .mount(&server)
            .await;
        Mock::given(path("/v1/episode/get"))
            .respond_with(|request: &wiremock::Request| {
                let eid = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "eid")
                    .unwrap()
                    .1
                    .into_owned();
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":{"eid":eid,"pid":"podcast","title":"队列单集","duration":60,"media":{"source":{"url":"https://media.example/episode.mp3"}}}}))
            })
            .mount(&server)
            .await;
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.content = content::Api::for_test(server.uri());
        let (sender, mut receiver) = mpsc::unbounded_channel();
        app.sender = sender;
        app.apply(2, Update::Restored(credentials()));
        with_playlist(&mut app, &["queued"]);
        let View::Home(home) = &mut app.view else {
            unreachable!()
        };
        home.key(KeyCode::Esc);
        home.key(KeyCode::Char('2'));
        home.key(KeyCode::Enter);
        home.apply_browse(
            subscriptions::Source::Subscriptions,
            subscriptions::Response::Feed(Ok(content::Page {
                items: vec![content::Episode {
                    eid: "a".into(),
                    title: "订阅单集".into(),
                    ..Default::default()
                }],
                cursor: None,
            })),
        );
        if play {
            home.key(KeyCode::Enter);
        }
        let (player, mut controls) = Player::simulated();
        app.player = player;
        let key = if play {
            KeyCode::Enter
        } else {
            KeyCode::Char('y')
        };
        let previous_request = app.play_request;
        app.key(key);
        app.key(key);
        assert_eq!(
            app.play_request,
            previous_request + if play { 2 } else { 0 }
        );
        assert_eq!(app.add_queue.len(), 1);
        let View::Home(home) = &app.view else {
            unreachable!()
        };
        assert!(home.next_after("queued").is_none());
        let mut resolved = false;
        for _ in 0..(1 + usize::from(!fail_addition) + usize::from(play)) {
            let (generation, update) =
                tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                    .await
                    .unwrap()
                    .unwrap();
            if let Update::Resolved {
                request, result, ..
            } = update
            {
                assert_eq!(request, app.play_request);
                let source = result.unwrap();
                assert_eq!(source.eid, "a");
                assert_eq!(source.start, 0.0);
                resolved = true;
                // Inspect the resolved source without starting a real audio decoder in this test.
            } else {
                app.apply(generation, update);
            }
        }
        assert_eq!(resolved, play);
        let View::Home(home) = &app.view else {
            unreachable!()
        };
        assert_eq!(
            home.next_after("queued").as_deref(),
            if fail_addition { None } else { Some("a") }
        );
        if fail_addition {
            assert!(app.notice.as_deref().unwrap().contains("加入播放列表失败"));
            assert!(app.play_task.is_some());
        }
        assert!(app.add_queue.is_empty());
        assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
        assert!(controls.try_recv().is_err());
        assert!(rendered(&mut app).replace(' ', "").contains(if play {
            "订阅单集"
        } else {
            "订阅列表"
        }));
    }
}
