use std::{
    collections::HashMap,
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
    home::{Action, Home},
    player::{self, Player},
    session::Store,
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
    Player(player::Event),
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
    generation: u64,
    sender: mpsc::UnboundedSender<(u64, Update)>,
    task: Option<JoinHandle<()>>,
    account: Option<Arc<Account>>,
    epoch: u64,
    renewal: Option<Renewal>,
    player: Player,
    play_request: u64,
    play_task: Option<JoinHandle<()>>,
    sync_task: Option<JoinHandle<()>>,
    flush_again: bool,
    remove_task: Option<JoinHandle<()>>,
    dirty: HashMap<String, ProgressUpdate>,
    last_synced: HashMap<String, u64>,
    sync_after: Instant,
    retry_after: Option<Instant>,
    notice: Option<String>,
    sync_error: Option<String>,
    logging_out: bool,
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Some(task) = &self.play_task {
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
        self.renewal = None;
        self.player.stop();
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
            Update::Renewed(renewal) => {
                if renewal.epoch == self.epoch {
                    self.renewal = Some(renewal);
                    self.save_renewal();
                }
                return;
            }
            Update::Player(event) => {
                let flush = self.player.apply(event);
                self.remember_progress();
                if flush {
                    self.flush_progress(true);
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
                        self.notice = None;
                        let sender = self.sender.clone();
                        self.player.play(
                            source,
                            Arc::new(move |event| {
                                let _ = sender.send((0, Update::Player(event)));
                            }),
                        );
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
                match result {
                    Ok(()) => {
                        self.generation += 1;
                        if let Some(task) = self.task.take() {
                            task.abort();
                        }
                        if let View::Home(home) = &mut self.view {
                            home.remove(&eid);
                        }
                        self.notice = None;
                    }
                    Err(error) => self.notice = Some(format!("移除失败：{error}")),
                }
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
            | Update::Resolved { .. }
            | Update::Synced { .. }
            | Update::Removed { .. } => unreachable!(),
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
            self.notice = Some("登录凭据保存失败 · r 重试".into());
            self.renewal = Some(renewal);
        }
    }

    fn key(&mut self, key: KeyCode) {
        if key == KeyCode::Char(' ') {
            if let Some(current) = &self.player.current {
                if current.ended {
                    self.play(current.eid.clone());
                } else {
                    self.player.toggle();
                }
            }
            return;
        }
        if key == KeyCode::Char('r') && self.renewal.is_some() {
            self.save_renewal();
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
                Action::Remove(eid) => self.remove(eid),
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

    fn play(&mut self, eid: String) {
        if self.logging_out {
            return;
        }
        if let Some(task) = self.play_task.take() {
            task.abort();
        }
        self.play_request += 1;
        if let Some(current) = &self.player.current
            && current.eid == eid
            && !current.ended
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

    fn remove(&mut self, eid: String) {
        if self.remove_task.is_some() {
            return;
        }
        let Some(account) = self.account.clone() else {
            return;
        };
        let epoch = self.epoch;
        let sender = self.sender.clone();
        self.notice = Some("正在移除…".into());
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
        self.remember_progress();
        self.flush_progress(false);
    }

    fn draw(&mut self, frame: &mut ratatui::Frame) {
        let (code, message, hint) = match &mut self.view {
            View::Loading => (None, "连接中…", "q 退出"),
            View::Qr {
                code,
                scanned,
                reconnecting,
            } => {
                let message = if frame.area().width < code.width() + 2
                    || frame.area().height < code.height() + 3
                {
                    "请放大终端窗口"
                } else if *reconnecting {
                    "重新连接中…"
                } else if *scanned {
                    "请在手机上确认"
                } else {
                    "用小宇宙扫码"
                };
                (Some(code), message, "r 刷新 · q 退出")
            }
            View::Expired => (None, "二维码已过期", "r 刷新 · q 退出"),
            View::Failed { message, .. } => (None, message.as_str(), "r 重试 · q 退出"),
            View::SaveFailed(_) => (None, "登录凭据保存失败", "r 重试 · q 退出"),
            View::Home(home) => {
                let mut area = frame.area();
                if self.player.current.is_some()
                    || self.notice.is_some()
                    || self.sync_error.is_some()
                {
                    area.height = area.height.saturating_sub(2);
                }
                home.draw(frame, area);
                self.draw_player(frame);
                return;
            }
        };
        ui::draw(frame, code.as_deref(), message, hint);
    }
    fn draw_player(&self, frame: &mut ratatui::Frame) {
        use ratatui::{
            layout::Rect,
            style::{Color, Style},
            widgets::Paragraph,
        };
        let area = frame.area();
        if area.height < 2 {
            return;
        }
        if let Some(current) = &self.player.current {
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
            frame.render_widget(
                Paragraph::new(title).style(Style::default().fg(Color::Cyan)),
                Rect::new(area.x, area.bottom() - 2, area.width, 1),
            );
            let duration = current
                .duration
                .map(|seconds| crate::playlist::format_duration(seconds as u64))
                .unwrap_or_else(|| "--:--".into());
            let hint = self
                .player
                .error
                .as_ref()
                .or(self.notice.as_ref())
                .or(self.sync_error.as_ref())
                .cloned()
                .unwrap_or_else(|| {
                    format!(
                        "{} / {duration} · Space {}",
                        crate::playlist::format_duration(current.position as u64),
                        if current.paused { "继续" } else { "暂停" }
                    )
                });
            frame.render_widget(
                Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
            );
        } else if let Some(notice) = self.notice.as_ref().or(self.sync_error.as_ref()) {
            frame.render_widget(
                Paragraph::new(notice.as_str())
                    .centered()
                    .style(Style::default().fg(Color::DarkGray)),
                Rect::new(area.x, area.bottom() - 1, area.width, 1),
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
        generation: 0,
        sender,
        task: None,
        account: None,
        epoch: 0,
        renewal: None,
        player: Player::default(),
        play_request: 0,
        play_task: None,
        sync_task: None,
        flush_again: false,
        remove_task: None,
        dirty: HashMap::new(),
        last_synced: HashMap::new(),
        sync_after: Instant::now(),
        retry_after: None,
        notice: None,
        sync_error: None,
        logging_out: false,
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
                    KeyCode::Esc if !matches!(app.view, View::Home(_)) => break Ok(()),
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
    app.play_request += 1;
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
            || app
                .account
                .as_ref()
                .is_some_and(|account| !account.is_idle())
        {
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
            app.apply(generation, update);
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
            generation: 2,
            sender,
            task: None,
            account: None,
            epoch: 0,
            renewal: None,
            player: Player::default(),
            play_request: 0,
            play_task: None,
            sync_task: None,
            flush_again: false,
            remove_task: None,
            dirty: HashMap::new(),
            last_synced: HashMap::new(),
            sync_after: Instant::now(),
            retry_after: None,
            notice: None,
            sync_error: None,
            logging_out: false,
        }
    }

    fn credentials() -> Credentials {
        Credentials {
            access_token: "test-access".into(),
            refresh_token: "test-refresh".into(),
        }
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
            assert!(
                rendered(&mut app)
                    .replace(' ', "")
                    .contains(if status == 200 {
                        "暂无单集"
                    } else {
                        "Enter"
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
        assert!(rendered(&mut app).contains("Enter"));
        app.key(KeyCode::Enter);
        assert!(matches!(app.view, View::Loading));
        assert!(app.credentials.is_none());
        assert!(app.store.load().unwrap().is_some());
    }
    #[test]
    fn space_controls_playback_on_every_page_and_navigation_keeps_the_track() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = app(directory.path().to_owned());
        app.view = View::Home(Home::default());
        let (player, mut commands) = Player::simulated();
        app.player = player;
        for menu in ['1', '2', '3', '4'] {
            app.key(KeyCode::Char(' '));
            assert!(commands.try_recv().is_ok());
            app.key(KeyCode::Char(menu));
            app.key(KeyCode::Enter);
            app.key(KeyCode::Char(' '));
            assert!(commands.try_recv().is_ok());
            assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
            assert!(rendered(&mut app).replace(' ', "").contains("测试单集"));
            app.key(KeyCode::Esc);
        }
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
    fn remote_remove_failure_preserves_row_and_success_preserves_playing_audio() {
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
        app.apply(
            0,
            Update::Removed {
                epoch: 0,
                eid: "episode".into(),
                result: Ok(()),
            },
        );
        assert!(!rendered(&mut app).replace(' ', "").contains("列表里的标题"));
        assert_eq!(app.player.current.as_ref().unwrap().eid, "episode");
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
}
