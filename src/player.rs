use std::{process::Stdio, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    sync::mpsc,
    task::JoinHandle,
};

use crate::content::{Playable, ProgressUpdate, RecentEpisode};

pub enum Event {
    Position(u64, f64),
    Paused(u64, bool),
    Loaded(u64, Option<f64>),
    Ended(u64, bool),
    Failed(u64, String),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Control {
    TogglePause,
    Seek(i64),
}

pub struct Playing {
    pub eid: String,
    pub pid: String,
    pub title: String,
    pub duration: Option<f64>,
    pub position: f64,
    pub paused: bool,
    pub loading: bool,
    pub ended: bool,
    confirmed_position: bool,
}

#[derive(Default)]
pub struct Player {
    pub current: Option<Playing>,
    pub error: Option<String>,
    serial: u64,
    commands: Option<mpsc::UnboundedSender<Control>>,
    task: Option<JoinHandle<()>>,
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}

impl Player {
    pub fn restore(&mut self, recent: RecentEpisode) {
        self.stop();
        self.error = None;
        self.current = Some(Playing {
            eid: recent.episode.eid,
            pid: recent.pid,
            title: recent.episode.title,
            duration: recent.episode.duration.map(|duration| duration as f64),
            position: recent.progress,
            paused: true,
            loading: false,
            ended: false,
            confirmed_position: false,
        });
    }

    pub fn awaiting_resume(&self) -> bool {
        self.current.is_some() && self.commands.is_none()
    }

    pub fn play(&mut self, source: Playable, emit: Arc<dyn Fn(Event) + Send + Sync>) {
        self.stop();
        self.serial += 1;
        self.error = None;
        self.current = Some(Playing {
            eid: source.eid.clone(),
            pid: source.pid.clone(),
            title: source.title.clone(),
            duration: source.duration.map(|value| value as f64),
            position: source.start,
            paused: false,
            loading: true,
            ended: false,
            confirmed_position: false,
        });
        let (commands, receiver) = mpsc::unbounded_channel();
        self.commands = Some(commands);
        let serial = self.serial;
        self.task = Some(tokio::spawn(async move {
            if let Err(message) = run(source, serial, receiver, emit.clone(), None).await {
                emit(Event::Failed(serial, message));
            }
        }));
    }

    pub fn toggle(&self) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(Control::TogglePause);
        }
    }

    pub fn seek(&self, seconds: i64) {
        if self
            .current
            .as_ref()
            .is_some_and(|current| !current.loading && !current.ended)
            && let Some(commands) = &self.commands
        {
            let _ = commands.send(Control::Seek(seconds));
        }
    }

    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        self.commands = None;
        self.serial += 1;
        self.current = None;
    }

    /// Returns true when pausing or finishing requires an immediate progress upload.
    pub fn apply(&mut self, event: Event) -> bool {
        let id = match &event {
            Event::Position(id, _)
            | Event::Paused(id, _)
            | Event::Loaded(id, _)
            | Event::Ended(id, _)
            | Event::Failed(id, _) => *id,
        };
        if id != self.serial {
            return false;
        }
        let Some(current) = &mut self.current else {
            return false;
        };
        match event {
            Event::Position(_, position) => {
                if position.is_finite() && position >= 0.0 {
                    current.position = current
                        .duration
                        .map_or(position, |duration| position.min(duration));
                    current.confirmed_position = true;
                }
            }
            Event::Loaded(_, duration) => {
                current.loading = false;
                if let Some(duration) = duration.filter(|value| value.is_finite() && *value > 0.0) {
                    current.duration = Some(duration);
                }
            }
            Event::Paused(_, paused) => {
                current.paused = paused;
                return paused;
            }
            Event::Ended(_, failed) => {
                current.loading = false;
                current.ended = true;
                current.paused = true;
                if failed {
                    self.error = Some("音频播放失败".into());
                } else if current.confirmed_position
                    && let Some(duration) = current.duration
                {
                    current.position = duration;
                }
                return true;
            }
            Event::Failed(_, message) => {
                current.loading = false;
                current.ended = true;
                current.paused = true;
                self.error = Some(message);
                return true;
            }
        }
        false
    }

    pub fn progress(&self) -> Option<ProgressUpdate> {
        let current = self.current.as_ref()?;
        current.confirmed_position.then(|| {
            ProgressUpdate::new(&current.eid, &current.pid, current.position.floor() as u64)
        })
    }
}

async fn run(
    source: Playable,
    serial: u64,
    mut commands: mpsc::UnboundedReceiver<Control>,
    emit: Arc<dyn Fn(Event) + Send + Sync>,
    audio_output: Option<&str>,
) -> Result<(), String> {
    // A private, short path also fits macOS's Unix socket path limit.
    let directory = tempfile::Builder::new()
        .prefix("xyz-audio-")
        .tempdir_in("/tmp")
        .map_err(|_| "无法创建播放器连接".to_owned())?;
    let socket = directory.path().join("ipc");
    let binary = std::env::var_os("XYZ_TUI_MPV").unwrap_or_else(|| "mpv".into());
    let mut command = Command::new(binary);
    if let Some(output) = audio_output {
        command.arg(format!("--ao={output}"));
    }
    let mut child = command
        .args([
            "--no-config",
            "--no-video",
            "--no-terminal",
            "--input-terminal=no",
            "--input-media-keys=no",
            "--input-default-bindings=no",
            "--load-scripts=no",
            "--ytdl=no",
            "--pause=yes",
            "--keep-open=no",
        ])
        .arg(format!("--input-ipc-server={}", socket.display()))
        .arg(format!("--start={:.3}", source.start))
        .arg("--")
        .arg(&source.url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "找不到 mpv，请安装后重试".to_owned()
            } else {
                "无法启动音频播放器".to_owned()
            }
        })?;
    let stream = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stream) = UnixStream::connect(&socket).await {
                return Ok(stream);
            }
            if child.try_wait().ok().flatten().is_some() {
                return Err("播放器启动失败".to_owned());
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|_| "连接播放器超时".to_owned())??;
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    for (index, property) in ["time-pos", "pause", "duration"].into_iter().enumerate() {
        send(&mut writer, json!(["observe_property", index, property])).await?;
    }
    send(&mut writer, json!(["set_property", "pause", false])).await?;
    let mut desired_pause = false;
    let mut loaded = false;
    let mut position = None;
    let mut duration = None;
    let mut last_sent = None;
    let mut interval = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(Control::TogglePause) => {
                    desired_pause = !desired_pause;
                    send(&mut writer, json!(["set_property", "pause", desired_pause])).await?;
                }
                Some(Control::Seek(seconds)) => {
                    send(&mut writer, json!(["seek", seconds, "relative+exact"])).await?;
                }
                None => break,
            },
            line = lines.next_line() => {
                let Some(line) = line.map_err(|_| "播放器连接中断".to_owned())? else { break; };
                let Ok(value) = serde_json::from_str::<Value>(&line) else { continue; };
                match value.get("event").and_then(Value::as_str) {
                    Some("file-loaded") => {
                        loaded = true;
                        emit(Event::Loaded(serial, duration));
                        send(&mut writer, json!(["set_property", "pause", desired_pause])).await?;
                    }
                    Some("property-change") => match value.get("name").and_then(Value::as_str) {
                        Some("time-pos") => {
                            if let Some(value) = value.get("data").and_then(Value::as_f64) {
                                if !loaded { loaded = true; emit(Event::Loaded(serial, duration)); }
                                // mpv's audio clock can briefly be negative after seeking to the start.
                                position = Some(value.max(0.0));
                            }
                        }
                        Some("duration") => {
                            duration = value.get("data").and_then(Value::as_f64).or(duration);
                            if loaded { emit(Event::Loaded(serial, duration)); }
                        }
                        Some("pause") if loaded => {
                            if let Some(paused) = value.get("data").and_then(Value::as_bool) {
                                if let Some(position) = position { emit(Event::Position(serial, position)); }
                                emit(Event::Paused(serial, paused));
                            }
                        }
                        _ => (),
                    },
                    Some("end-file") => {
                        if let Some(position) = position { emit(Event::Position(serial, position)); }
                        let failed = value.get("reason").and_then(Value::as_str) != Some("eof");
                        emit(Event::Ended(serial, failed));
                        let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
                        return Ok(());
                    }
                    _ => (),
                }
            },
            _ = interval.tick() => {
                if let Some(position) = position
                    && last_sent != Some(position)
                {
                    emit(Event::Position(serial, position));
                    last_sent = Some(position);
                }
            }
        }
    }
    Err("播放器已断开".into())
}

async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, command: Value) -> Result<(), String> {
    let data = format!("{}\n", json!({"command":command}));
    writer
        .write_all(data.as_bytes())
        .await
        .map_err(|_| "播放器控制失败".into())
}

#[cfg(test)]
impl Player {
    pub fn simulated() -> (Self, mpsc::UnboundedReceiver<Control>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            Self {
                current: Some(Playing {
                    eid: "episode".into(),
                    pid: "podcast".into(),
                    title: "测试单集".into(),
                    duration: Some(60.0),
                    position: 10.0,
                    paused: false,
                    loading: false,
                    ended: false,
                    confirmed_position: true,
                }),
                serial: 1,
                commands: Some(sender),
                error: None,
                task: None,
            },
            receiver,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_confirmed_positions_can_be_uploaded_and_stale_events_are_ignored() {
        let (mut player, _) = Player::simulated();
        player.current.as_mut().unwrap().confirmed_position = false;
        assert!(player.progress().is_none());
        player.apply(Event::Position(0, 59.0));
        assert!(player.progress().is_none());
        player.apply(Event::Position(1, f64::NAN));
        assert!(player.progress().is_none());
        player.apply(Event::Position(1, 12.7));
        assert_eq!(player.progress().unwrap().progress, 12);
        player.apply(Event::Ended(1, true));
        assert_eq!(player.progress().unwrap().progress, 12);
        player.apply(Event::Ended(1, false));
        assert_eq!(player.progress().unwrap().progress, 60);
        player.stop();
        player.apply(Event::Position(1, 1.0));
        assert!(player.progress().is_none());
    }

    #[tokio::test]
    #[ignore = "requires mpv and a local IPC socket; run explicitly with --ignored"]
    async fn mpv_decodes_seeks_pauses_resumes_and_finishes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("silence.wav");
        let samples = 36 * 8000;
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36_u32 + samples * 2).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8000_u32.to_le_bytes());
        wav.extend_from_slice(&16000_u32.to_le_bytes());
        wav.extend_from_slice(&2_u16.to_le_bytes());
        wav.extend_from_slice(&16_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&(samples * 2).to_le_bytes());
        wav.resize(44 + samples as usize * 2, 0);
        std::fs::write(&path, wav).unwrap();
        let source = Playable {
            eid: "episode".into(),
            pid: "podcast".into(),
            title: "silent fixture".into(),
            url: path.to_str().unwrap().into(),
            duration: Some(36),
            start: 16.0,
            transcript_media_id: None,
        };
        let (commands, receiver) = mpsc::unbounded_channel();
        let (sender, mut events) = mpsc::unbounded_channel();
        let (mut player, _) = Player::simulated();
        player.current.as_mut().unwrap().confirmed_position = false;
        let task = tokio::spawn(run(
            source,
            1,
            receiver,
            Arc::new(move |event| {
                let _ = sender.send(event);
            }),
            Some("null"),
        ));
        let mut phase = "load";
        let outcome = tokio::time::timeout(Duration::from_secs(12), async {
            while player.progress().is_none() {
                player.apply(events.recv().await.unwrap());
            }
            assert!(player.current.as_ref().unwrap().position >= 15.9);
            phase = "pause";
            commands.send(Control::TogglePause).unwrap();
            while !player.current.as_ref().unwrap().paused {
                player.apply(events.recv().await.unwrap());
            }
            let paused = player.current.as_ref().unwrap().position;
            tokio::time::sleep(Duration::from_millis(600)).await;
            while let Ok(event) = events.try_recv() {
                player.apply(event);
            }
            assert!((player.current.as_ref().unwrap().position - paused).abs() < 0.15);
            for seconds in [-15, -15, 15, 15] {
                let target = (player.current.as_ref().unwrap().position + seconds as f64).max(0.0);
                phase = if seconds < 0 {
                    "seek backward"
                } else {
                    "seek forward"
                };
                commands.send(Control::Seek(seconds)).unwrap();
                // Allow for the audio output buffer and the 250 ms position sampling interval.
                while (player.current.as_ref().unwrap().position - target).abs() >= 0.5 {
                    player.apply(events.recv().await.unwrap());
                    assert!(player.current.as_ref().unwrap().paused);
                    assert!(!player.current.as_ref().unwrap().ended);
                }
                // mpv's sample timestamps can fall just below a whole-second boundary.
                assert!(
                    player
                        .progress()
                        .unwrap()
                        .progress
                        .abs_diff(target.floor() as u64)
                        <= 1
                );
            }
            phase = "resume";
            commands.send(Control::TogglePause).unwrap();
            while player.current.as_ref().unwrap().paused {
                player.apply(events.recv().await.unwrap());
            }
            phase = "finish";
            while !player.current.as_ref().unwrap().ended {
                player.apply(events.recv().await.unwrap());
            }
            assert!(player.error.is_none());
            assert_eq!(player.progress().unwrap().progress, 36);
        })
        .await;
        let current = player.current.as_ref().unwrap();
        assert!(
            outcome.is_ok(),
            "timed out during {phase}: position={}, paused={}, ended={}, error={:?}",
            current.position,
            current.paused,
            current.ended,
            player.error
        );
        task.await.unwrap().unwrap();
    }
}
