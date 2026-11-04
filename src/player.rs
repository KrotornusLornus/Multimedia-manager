use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

#[derive(Debug, Clone, Default)]
pub struct PlaybackState {
    pub active: bool,
    pub paused: bool,
    pub position: f64,
    pub duration: f64,
    pub title: Option<String>,
    pub last_error: Option<String>,
}

pub struct Player {
    program: String,
    ytdl_format: String,
    socket: PathBuf,
    child: Option<Child>,
    writer: Option<UnixStream>,
    state: Arc<Mutex<PlaybackState>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
}

impl Player {
    pub fn new(program: String, ytdl_format: String) -> Self {
        let socket =
            std::env::temp_dir().join(format!("video-player-mpv-{}.sock", std::process::id()));

        Self {
            program,
            ytdl_format,
            socket,
            child: None,
            writer: None,
            state: Arc::new(Mutex::new(PlaybackState::default())),
            stderr_tail: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    pub fn snapshot(&self) -> PlaybackState {
        self.state
            .lock()
            .map(|state| state.clone())
            .unwrap_or_default()
    }

    pub fn take_last_error(&self) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|mut state| state.last_error.take())
    }

    pub fn play_playlist(
        &mut self,
        targets: &[String],
        format: Option<&str>,
        audio_only: bool,
    ) -> Result<()> {
        let Some(first) = targets.first() else {
            return Ok(());
        };

        if let Some(format) = format {
            self.ytdl_format = format.to_string();
        }
        self.ensure_running()?;
        let _ = self.send(json!({
            "command": [
                "set_property",
                "vid",
                if audio_only { "no" } else { "auto" }
            ]
        }));
        let _ = self.send(json!({
            "command": [
                "set_property",
                "force-window",
                if audio_only { "no" } else { "yes" }
            ]
        }));
        if let Some(format) = format
            && self
                .send(json!({ "command": ["set_property", "ytdl-format", format] }))
                .is_err()
        {
            self.ensure_running()?;
        }

        if let Ok(mut tail) = self.stderr_tail.lock() {
            tail.clear();
        }
        if let Ok(mut state) = self.state.lock() {
            state.active = true;
            state.paused = false;
            state.position = 0.0;
            state.duration = 0.0;
            state.title = None;
            state.last_error = None;
        }

        if self
            .send(json!({ "command": ["loadfile", first] }))
            .is_err()
        {
            self.ensure_running()?;
            self.send(json!({ "command": ["loadfile", first] }))?;
        }
        for target in &targets[1..] {
            if self
                .send(json!({ "command": ["loadfile", target, "append"] }))
                .is_err()
            {
                self.ensure_running()?;
                self.send(json!({ "command": ["loadfile", target, "append"] }))?;
            }
        }

        Ok(())
    }

    pub fn toggle_pause(&mut self) {
        let _ = self.send(json!({ "command": ["cycle", "pause"] }));
    }

    pub fn playlist_next(&mut self) {
        let _ = self.send(json!({ "command": ["playlist_next"] }));
    }

    pub fn playlist_prev(&mut self) {
        let _ = self.send(json!({ "command": ["playlist_prev"] }));
    }

    pub fn seek(&mut self, position: f64) {
        let _ = self.send(json!({ "command": ["seek", position, "absolute"] }));
        if let Ok(mut state) = self.state.lock() {
            state.position = position;
        }
    }

    pub fn stop(&mut self) {
        let _ = self.send(json!({ "command": ["stop"] }));
        if let Ok(mut state) = self.state.lock() {
            state.active = false;
            state.paused = false;
            state.position = 0.0;
            state.duration = 0.0;
            state.title = None;
        }
    }

    fn ensure_running(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            let running = child.try_wait()?.is_none();
            if running && self.writer.is_some() {
                return Ok(());
            }
        }

        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.writer = None;
        let _ = fs::remove_file(&self.socket);

        let ipc_arg = format!("--input-ipc-server={}", self.socket.display());
        let ytdl_arg = format!("--ytdl-format={}", self.ytdl_format);
        let mut command = Command::new(&self.program);
        command.args(["--idle=yes", "--force-window=yes", "--keep-open=no"]);
        command.arg(ipc_arg);
        command.arg(ytdl_arg);

        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("could not run {}", self.program))?;

        if let Some(stderr) = child.stderr.take() {
            let tail = Arc::clone(&self.stderr_tail);
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut pending = String::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let read = match reader.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(_) => break,
                    };
                    pending.push_str(&String::from_utf8_lossy(&chunk[..read]));
                    while let Some(index) = pending.find(['\n', '\r']) {
                        let line = pending[..index].trim().to_string();
                        pending.drain(..=index);
                        if line.is_empty() {
                            continue;
                        }
                        if let Ok(mut tail) = tail.lock() {
                            tail.push_back(line);
                            while tail.len() > 80 {
                                tail.pop_front();
                            }
                        }
                    }
                }
            });
        }

        let mut last_error = None;
        for _ in 0..100 {
            match UnixStream::connect(&self.socket) {
                Ok(stream) => {
                    let reader = stream.try_clone()?;
                    self.writer = Some(stream);
                    self.child = Some(child);
                    self.start_reader(reader);
                    self.observe_properties()?;
                    return Ok(());
                }
                Err(error) => {
                    last_error = Some(error);
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }

        let _ = fs::remove_file(&self.socket);
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();

        match last_error {
            Some(error) => Err(error).context("mpv did not open the IPC socket"),
            None => bail!("mpv did not open the IPC socket"),
        }
    }

    fn observe_properties(&mut self) -> Result<()> {
        for (id, name) in [
            (1, "time-pos"),
            (2, "pause"),
            (3, "duration"),
            (4, "media-title"),
        ] {
            self.send(json!({ "command": ["observe_property", id, name] }))?;
        }
        Ok(())
    }

    fn start_reader(&self, stream: UnixStream) {
        let state = Arc::clone(&self.state);
        let stderr_tail = Arc::clone(&self.stderr_tail);

        thread::spawn(move || {
            let reader = BufReader::new(stream);

            for line in reader.lines().map_while(|line| line.ok()) {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };

                let mut state = match state.lock() {
                    Ok(state) => state,
                    Err(_) => break,
                };

                match value.get("event").and_then(Value::as_str) {
                    Some("property-change") => {
                        let name = value.get("name").and_then(Value::as_str);
                        let data = value.get("data");

                        match name {
                            Some("time-pos") => {
                                state.position = data.and_then(Value::as_f64).unwrap_or(0.0);
                            }
                            Some("duration") => {
                                state.duration = data.and_then(Value::as_f64).unwrap_or(0.0);
                            }
                            Some("pause") => {
                                state.paused = data.and_then(Value::as_bool).unwrap_or(false);
                            }
                            Some("media-title") => {
                                state.title = data.and_then(Value::as_str).map(str::to_string);
                            }
                            _ => {}
                        }
                    }
                    Some("start-file") => {
                        state.active = true;
                        state.last_error = None;
                    }
                    Some("file-loaded") => {
                        state.active = true;
                    }
                    Some("end-file") => {
                        let reason = value.get("reason").and_then(Value::as_str);
                        let error = value.get("error").and_then(Value::as_str);
                        let file_error = value.get("file_error").and_then(Value::as_str);

                        state.active = false;
                        state.paused = false;
                        state.position = 0.0;
                        state.duration = 0.0;
                        state.title = None;

                        if reason == Some("error") || error.is_some() || file_error.is_some() {
                            state.last_error = Some(build_playback_error(
                                error,
                                file_error,
                                stderr_tail.as_ref(),
                            ));
                        }
                    }
                    _ => {}
                }
            }

            if let Ok(mut state) = state.lock() {
                *state = PlaybackState::default();
            }
        });
    }

    #[cfg(test)]
    pub fn break_writer_for_test(&mut self) {
        let (writer, _peer) = UnixStream::pair().expect("could not create test socket");
        drop(_peer);
        self.writer = Some(writer);
    }

    fn send(&mut self, message: Value) -> Result<()> {
        let result = (|| -> Result<()> {
            let Some(writer) = self.writer.as_mut() else {
                bail!("mpv is not running");
            };

            let text = serde_json::to_string(&message)?;
            writer.write_all(text.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            Ok(())
        })();

        if result.is_err() {
            self.writer = None;
        }
        result
    }
}

pub fn spawn_detached(
    program: &str,
    targets: &[String],
    format: Option<&str>,
    audio_only: bool,
) -> Result<()> {
    if targets.is_empty() {
        return Ok(());
    }

    let mut command = Command::new(program);
    if let Some(format) = format {
        command.arg(format!("--ytdl-format={format}"));
    }
    if audio_only {
        command.arg("--no-video");
    }
    command.args(targets);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("could not run {program}"))?;
    Ok(())
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.send(json!({ "command": ["quit"] }));
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_file(&self.socket);
    }
}

fn build_playback_error(
    error: Option<&str>,
    file_error: Option<&str>,
    stderr_tail: &Mutex<VecDeque<String>>,
) -> String {
    let mut message = String::new();
    if let Some(error) = error {
        message.push_str(error);
    }
    if let Some(file_error) = file_error {
        if !message.is_empty() {
            message.push('\n');
        }
        message.push_str(file_error);
    }

    let lines: Vec<String> = stderr_tail
        .lock()
        .map(|tail| tail.iter().cloned().collect())
        .unwrap_or_default();
    let relevant: Vec<String> = lines
        .iter()
        .filter(|line| is_mpv_error_line(line))
        .cloned()
        .collect();
    let detail = if relevant.is_empty() {
        lines.join("\n")
    } else {
        relevant.join("\n")
    };

    if !detail.is_empty() {
        if !message.is_empty() {
            message.push_str("\n\n");
        }
        message.push_str(&detail);
    }
    if message.is_empty() {
        message = "mpv could not play the file".to_string();
    }
    message
}

fn is_mpv_error_line(line: &str) -> bool {
    let lower = line.to_lowercase();
    lower.contains("error")
        || lower.contains("failed")
        || lower.contains("forbidden")
        || lower.contains("403")
        || lower.contains("cannot")
        || lower.contains("no video or audio streams")
        || lower.contains("could not open")
}

#[cfg(test)]
mod tests {
    use super::{Player, build_playback_error};
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn builds_playback_error_with_stderr_context() {
        use std::collections::VecDeque;
        use std::sync::Mutex;

        let tail = Mutex::new(VecDeque::from([
            "some status".to_string(),
            "[ffmpeg] https: HTTP error 403 Forbidden".to_string(),
            "No video or audio streams selected.".to_string(),
        ]));
        let error = build_playback_error(Some("loading failed"), None, &tail);
        assert!(error.contains("loading failed"));
        assert!(error.contains("403 Forbidden"));
        assert!(error.contains("No video or audio streams"));
    }

    #[test]
    #[ignore]
    fn controls_mpv_over_ipc() {
        let base =
            std::env::temp_dir().join(format!("video-player-mpv-test-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let video = base.join("sample.mp4");
        let video_path = video.to_string_lossy().to_string();

        let rendered = Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=3:size=320x240:rate=25",
                "-pix_fmt",
                "yuv420p",
                &video_path,
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();

        if !rendered.map(|status| status.success()).unwrap_or(false) {
            return;
        }

        let mut player = Player::new("mpv".to_string(), "b".to_string());
        player
            .play_playlist(std::slice::from_ref(&video_path), None, false)
            .unwrap();

        let mut ready = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            let state = player.snapshot();
            if state.active && state.duration > 0.0 {
                ready = true;
                break;
            }
        }
        assert!(ready, "mpv did not report duration");

        player.toggle_pause();
        std::thread::sleep(Duration::from_millis(300));
        assert!(player.snapshot().paused);

        player.break_writer_for_test();
        player
            .play_playlist(
                std::slice::from_ref(&video_path),
                Some("bv*[height<=720]+ba/b[height<=720]/b"),
                false,
            )
            .expect("mpv did not recover from the broken pipe");

        let mut recovered = false;
        for _ in 0..60 {
            std::thread::sleep(Duration::from_millis(100));
            if player.snapshot().duration > 0.0 {
                recovered = true;
                break;
            }
        }
        assert!(recovered, "mpv did not resume playback after restart");

        player.stop();
        std::thread::sleep(Duration::from_millis(300));
        assert!(!player.snapshot().active);

        drop(player);
        let _ = std::fs::remove_dir_all(&base);
    }
}
