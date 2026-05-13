use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use typemap::Key;

use crate::global::functions::paths;

pub struct EmbeddedVideo {
    pub url: String,
    pub fullscreen: bool,
    pub rect: Rect,
    pub child: Option<Child>,
    pub ipc_path: PathBuf,
    pub log_path: PathBuf,
    /// Latched at `start()` time: when the caller knows where the preview
    /// thumbnail is, we pin panel-mode resizes to the same cell rect instead of
    /// the fallback hardcoded `panel_rect`. Cleared by `stop()`.
    panel_hint: Option<Rect>,
    /// Direct stream URLs from a separate `yt-dlp -g` invocation kicked off in
    /// the background at `start()` time. Once populated, every subsequent
    /// respawn (fullscreen toggle / terminal resize) skips mpv's own yt-dlp
    /// hook via `--no-ytdl`, which removes the multi-second resolution delay
    /// that made Shift+F feel like a restart.
    /// `(video_url, optional_audio_url)` — audio is `None` when yt-dlp picked
    /// a single muxed format.
    direct_urls: Arc<Mutex<Option<(String, Option<String>)>>>,
}

impl Clone for EmbeddedVideo {
    fn clone(&self) -> Self {
        panic!("EmbeddedVideo is not cloneable; the running child mpv process cannot be duplicated")
    }
}

impl Key for EmbeddedVideo {
    type Value = Self;
}

impl EmbeddedVideo {
    pub fn empty() -> Self {
        let cache = paths::cache_dir();
        Self {
            url: String::new(),
            fullscreen: false,
            rect: Rect::default(),
            child: None,
            ipc_path: cache.join("mpv-embedded.sock"),
            log_path: cache.join("mpv-embedded.log"),
            panel_hint: None,
            direct_urls: Arc::new(Mutex::new(None)),
        }
    }

    pub fn is_playing(&self) -> bool {
        self.child.is_some()
    }

    pub fn is_fullscreen_active(&self) -> bool {
        self.is_playing() && self.fullscreen
    }

    pub fn start(
        &mut self,
        url: &str,
        term_size: (u16, u16),
        panel_hint: Option<Rect>,
    ) -> Result<(), String> {
        if self.is_playing() {
            self.stop();
        }
        self.url = url.to_string();
        self.fullscreen = false;
        self.panel_hint = panel_hint.and_then(Self::sanitize_panel_rect);
        self.rect = self.current_panel_rect(term_size);
        *self.direct_urls.lock().unwrap() = None;
        self.spawn_url_prefetch();
        self.spawn_mpv(None)
    }

    pub fn stop(&mut self) {
        let _ = self.kill_mpv();
        Self::clear_graphics();
        self.panel_hint = None;
        *self.direct_urls.lock().unwrap() = None;
    }

    // Resolve the direct CDN URLs for the current video in a background
    // thread. Once cached, every respawn skips mpv's yt-dlp hook (`--no-ytdl`)
    // and just opens the URLs directly — that's what makes Shift+F feel
    // instant instead of a multi-second restart.
    fn spawn_url_prefetch(&self) {
        let url = self.url.clone();
        let cache = self.direct_urls.clone();
        thread::spawn(move || {
            // Request separate video+audio streams so audio is best available
            // quality (muxed `best[height<=480]` would lock us into itag 18's
            // 96kbps AAC). yt-dlp returns two URLs; spawn_mpv pairs them via
            // `--audio-file=`.
            let output = Command::new("yt-dlp")
                .args([
                    "-g",
                    "-f",
                    "bestvideo[height<=480]+bestaudio/best",
                    "--no-warnings",
                    "--",
                    &url,
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            let Ok(out) = output else { return };
            if !out.status.success() {
                return;
            }
            let s = String::from_utf8_lossy(&out.stdout);
            let urls: Vec<String> = s
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(String::from)
                .collect();
            if let Some(video) = urls.first() {
                let audio = urls.get(1).cloned();
                *cache.lock().unwrap() = Some((video.clone(), audio));
            }
        });
    }

    pub fn toggle_fullscreen(&mut self, term_size: (u16, u16)) -> Result<(), String> {
        if !self.is_playing() {
            return Ok(());
        }
        let pos = self.kill_mpv();
        Self::clear_graphics();
        self.fullscreen = !self.fullscreen;
        self.rect = if self.fullscreen {
            Self::fullscreen_rect(term_size)
        } else {
            self.current_panel_rect(term_size)
        };
        self.spawn_mpv(pos)
    }

    pub fn resize_for(&mut self, term_size: (u16, u16)) -> Result<(), String> {
        if !self.is_playing() {
            return Ok(());
        }
        let pos = self.kill_mpv();
        Self::clear_graphics();
        self.rect = if self.fullscreen {
            Self::fullscreen_rect(term_size)
        } else {
            self.current_panel_rect(term_size)
        };
        self.spawn_mpv(pos)
    }

    fn current_panel_rect(&self, term_size: (u16, u16)) -> Rect {
        self.panel_hint
            .filter(|r| r.width > 0 && r.height > 0)
            .unwrap_or_else(|| Self::panel_rect(term_size))
    }

    // Inset the thumbnail rect on the top and left by one cell so the embedded
    // video doesn't kiss the iteminfo column borders, and extend the right and
    // bottom edges one cell past the thumbnail bounds so the image fills the
    // available column.
    fn sanitize_panel_rect(r: Rect) -> Option<Rect> {
        const MARGIN: u16 = 1;
        const EXTEND: u16 = 1;
        if r.width < MARGIN + 4 || r.height < MARGIN + 2 {
            return None;
        }
        Some(Rect {
            x: r.x + MARGIN,
            y: r.y + MARGIN,
            width: r.width - MARGIN + EXTEND,
            height: r.height - MARGIN + EXTEND,
        })
    }

    // Panel matches the preview-thumbnail position on the single-video page:
    // top-left, ~30% terminal width (the left column of the 30/70 grid), below
    // the 1-row search bar (+1 row of padding). Height keeps a 16:9 video
    // aspect ratio assuming roughly 2:1 character cells.
    fn panel_rect(term_size: (u16, u16)) -> Rect {
        let (cols, rows) = term_size;
        let w = ((cols as u32 * 30) / 100).max(20).min(cols as u32) as u16;
        // 16:9 video, char aspect ~1:2  =>  h_cells ≈ w_cells * 9 / 32
        let h = (((w as u32) * 9) / 32).max(8) as u16;
        let h = h.min(rows.saturating_sub(4));
        Rect {
            x: 1,
            y: 3,
            width: w,
            height: h.max(4),
        }
    }

    // Fullscreen places a centered 16:9 cell rect inside the terminal grid.
    // `vo_kitty` only emits the scaled video pixels (not the canvas), so we
    // can't rely on mpv to letterbox/center for us — we pre-compute the cell
    // origin so the kitty image lands centered. For non-16:9 videos mpv's
    // `--keepaspect=yes` further shrinks the rendered frame inside this rect;
    // the visible result is still centered (just with more black around it).
    fn fullscreen_rect(term_size: (u16, u16)) -> Rect {
        let (cols, rows) = term_size;
        let (cell_w, cell_h) = Self::cell_pixels(term_size);
        let cw = cell_w.max(1) as u32;
        let ch = cell_h.max(1) as u32;
        let canvas_w = cols.max(1) as u32 * cw;
        let canvas_h = rows.max(1) as u32 * ch;
        let (vid_w, vid_h) = if canvas_w * 9 >= canvas_h * 16 {
            (canvas_h * 16 / 9, canvas_h)
        } else {
            (canvas_w, canvas_w * 9 / 16)
        };
        let off_x = canvas_w.saturating_sub(vid_w) / 2;
        let off_y = canvas_h.saturating_sub(vid_h) / 2;
        Rect {
            x: (off_x / cw) as u16 + 1,
            y: (off_y / ch) as u16 + 1,
            width: (vid_w / cw).max(1) as u16,
            height: (vid_h / ch).max(1) as u16,
        }
    }

    // Estimate pixels per cell from the terminal's reported pixel size.
    // Crossterm's window_size() returns 0 on terminals that don't implement
    // TIOCGWINSZ pixel fields — fall back to plausible kitty/ghostty defaults
    // for a retina-resolution font (~12x24).
    fn cell_pixels(term_size: (u16, u16)) -> (u16, u16) {
        let (term_w, term_h) = term_size;
        if let Ok(ws) = crossterm::terminal::window_size() {
            if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 {
                let cw = (ws.width / ws.columns).max(4);
                let ch = (ws.height / ws.rows).max(8);
                return (cw, ch);
            }
        }
        let _ = (term_w, term_h);
        (12, 24)
    }

    fn spawn_mpv(&mut self, start_pos: Option<f64>) -> Result<(), String> {
        if let Some(parent) = self.ipc_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create cache dir {}: {e}", parent.display()))?;
        }
        let _ = std::fs::remove_file(&self.ipc_path);

        let stderr = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.log_path)
            .map(Stdio::from)
            .unwrap_or_else(|_| Stdio::null());

        let term_size = match crossterm::terminal::size() {
            Ok(s) => s,
            Err(_) => (80, 24),
        };
        let (cell_w_px, cell_h_px) = Self::cell_pixels(term_size);
        let img_w_px = (self.rect.width as u32) * (cell_w_px as u32);
        let img_h_px = (self.rect.height as u32) * (cell_h_px as u32);

        let direct = self.direct_urls.lock().unwrap().clone();

        let mut args: Vec<String> = vec![
            "--vo=kitty".into(),
            format!("--vo-kitty-left={}", self.rect.x),
            format!("--vo-kitty-top={}", self.rect.y),
            format!("--vo-kitty-cols={}", term_size.0),
            format!("--vo-kitty-rows={}", term_size.1),
            format!("--vo-kitty-width={img_w_px}"),
            format!("--vo-kitty-height={img_h_px}"),
            "--vo-kitty-alt-screen=no".into(),
            "--vo-kitty-config-clear=no".into(),
            "--vo-kitty-use-shm=no".into(),
            "--profile=sw-fast".into(),
            "--msg-level=all=error".into(),
            "--no-terminal".into(),
            format!("--input-ipc-server={}", self.ipc_path.display()),
            "--keep-open=no".into(),
        ];
        match &direct {
            Some((_, audio)) => {
                args.push("--no-ytdl".into());
                if let Some(audio_url) = audio {
                    args.push(format!("--audio-file={audio_url}"));
                }
                // Aggressive startup options for the toggle/respawn path:
                // skip most demuxer probing (we know it's an mp4/webm http
                // stream) and use keyframe-accurate seek so resuming the
                // saved playback position doesn't pull extra data.
                args.push(
                    "--demuxer-lavf-o-add=probesize=131072,analyzeduration=200000"
                        .into(),
                );
                args.push("--hr-seek=no".into());
            }
            None => {
                args.push(
                    "--ytdl-format=bestvideo[height<=480]+bestaudio/best[height<=480]"
                        .into(),
                );
            }
        }
        if self.fullscreen {
            args.extend([
                "--background=color".into(),
                "--background-color=#FF000000".into(),
                "--keepaspect=yes".into(),
                "--video-align-x=0".into(),
                "--video-align-y=0".into(),
            ]);
        }
        if let Some(pos) = start_pos {
            args.push(format!("--start={pos}"));
        }
        let primary_url = match &direct {
            Some((video, _)) => video.clone(),
            None => self.url.clone(),
        };
        args.push(primary_url);

        let mut child = Command::new("mpv")
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(stderr)
            .spawn()
            .map_err(|e| format!("failed to spawn mpv: {e}"))?;

        if let Some(stdout) = child.stdout.take() {
            spawn_kitty_proxy(stdout);
        }
        self.child = Some(child);
        Ok(())
    }

    fn kill_mpv(&mut self) -> Option<f64> {
        let pos = self.ipc_get_f64("time-pos");
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_file(&self.ipc_path);
        pos
    }

    fn ipc_get_f64(&self, prop: &str) -> Option<f64> {
        let resp =
            self.ipc_send(&format!("{{\"command\":[\"get_property\",\"{prop}\"]}}"))?;
        let key = "\"data\":";
        let idx = resp.find(key)?;
        let tail = resp[idx + key.len()..].trim_start();
        let end = tail.find([',', '}']).unwrap_or(tail.len());
        tail[..end].trim().parse::<f64>().ok()
    }

    fn ipc_send(&self, json: &str) -> Option<String> {
        if !self.ipc_path.exists() {
            return None;
        }
        let mut stream = UnixStream::connect(&self.ipc_path).ok()?;
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok()?;
        stream.write_all(json.as_bytes()).ok()?;
        stream.write_all(b"\n").ok()?;
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        while Instant::now() < deadline {
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.contains(&b'\n') {
                        break;
                    }
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
        String::from_utf8(buf).ok()
    }

    pub fn cycle_pause(&self) {
        let _ = self.ipc_send("{\"command\":[\"cycle\",\"pause\"]}");
    }

    pub fn send_keypress(&self, name: &str) {
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        let _ = self.ipc_send(&format!(
            "{{\"command\":[\"keypress\",\"{escaped}\"]}}"
        ));
    }

    fn clear_graphics() {
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        let _ = handle.write_all(b"\x1b_Ga=d,d=A\x1b\\");
        let _ = handle.flush();
    }
}

impl Drop for EmbeddedVideo {
    fn drop(&mut self) {
        if self.is_playing() {
            self.stop();
        }
    }
}

// mpv's `--vo=kitty` emits Kitty graphics APC sequences with default z-index = 0,
// which means cells written by ratatui appear *over* the image. We intercept mpv's
// stdout, find each `ESC _ G ... ESC \` placement command, and inject `z=1` so the
// image renders above any text in those cells.
fn spawn_kitty_proxy(mut reader: ChildStdout) {
    thread::spawn(move || {
        let mut leftover: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 32 * 1024];
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    leftover.extend_from_slice(&chunk[..n]);
                    let (processed, remainder) = rewrite_kitty_z(&leftover);
                    if !processed.is_empty() {
                        let stdout = std::io::stdout();
                        let mut handle = stdout.lock();
                        let _ = handle.write_all(&processed);
                        let _ = handle.flush();
                    }
                    leftover = remainder;
                }
                Err(_) => break,
            }
        }
    });
}

fn rewrite_kitty_z(buf: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut out: Vec<u8> = Vec::with_capacity(buf.len() + 64);
    let n = buf.len();
    let mut i = 0;
    while i < n {
        let b = buf[i];
        if b == 0x1b {
            if i + 2 >= n {
                return (out, buf[i..].to_vec());
            }
            if buf[i + 1] == b'_' && buf[i + 2] == b'G' {
                let mut j = i + 3;
                let mut end: Option<usize> = None;
                while j + 1 < n {
                    if buf[j] == 0x1b && buf[j + 1] == b'\\' {
                        end = Some(j);
                        break;
                    }
                    j += 1;
                }
                let Some(end_pos) = end else {
                    return (out, buf[i..].to_vec());
                };
                let mut sep = end_pos;
                let mut k = i + 3;
                while k < end_pos {
                    if buf[k] == b';' {
                        sep = k;
                        break;
                    }
                    k += 1;
                }
                let header = &buf[i + 3..sep];
                let header_str = std::str::from_utf8(header).unwrap_or("");
                let mut has_z = false;
                let mut is_placement = false;
                for kv in header_str.split(',') {
                    if kv.starts_with("z=") {
                        has_z = true;
                    }
                    if kv == "a=T" || kv == "a=p" {
                        is_placement = true;
                    }
                }
                out.extend_from_slice(&buf[i..i + 3]);
                out.extend_from_slice(header);
                if is_placement && !has_z {
                    if !header.is_empty() {
                        out.push(b',');
                    }
                    out.extend_from_slice(b"z=1");
                }
                out.extend_from_slice(&buf[sep..end_pos + 2]);
                i = end_pos + 2;
                continue;
            }
        }
        out.push(b);
        i += 1;
    }
    (out, Vec::new())
}

pub fn crossterm_to_mpv_key(key: &KeyEvent) -> Option<String> {
    let (base, shift_implicit) = match key.code {
        KeyCode::Char(' ') => ("SPACE".to_string(), false),
        KeyCode::Char(c) => (c.to_string(), c.is_ascii_uppercase()),
        KeyCode::Left => ("LEFT".to_string(), false),
        KeyCode::Right => ("RIGHT".to_string(), false),
        KeyCode::Up => ("UP".to_string(), false),
        KeyCode::Down => ("DOWN".to_string(), false),
        KeyCode::Enter => ("ENTER".to_string(), false),
        KeyCode::Backspace => ("BS".to_string(), false),
        KeyCode::Tab => ("TAB".to_string(), false),
        KeyCode::Home => ("HOME".to_string(), false),
        KeyCode::End => ("END".to_string(), false),
        KeyCode::PageUp => ("PGUP".to_string(), false),
        KeyCode::PageDown => ("PGDWN".to_string(), false),
        KeyCode::Delete => ("DEL".to_string(), false),
        KeyCode::Insert => ("INS".to_string(), false),
        KeyCode::F(n) => (format!("F{n}"), false),
        _ => return None,
    };

    let mods = key.modifiers;
    let mut out = String::new();
    if mods.contains(KeyModifiers::SHIFT) && !shift_implicit {
        out.push_str("shift+");
    }
    if mods.contains(KeyModifiers::CONTROL) {
        out.push_str("ctrl+");
    }
    if mods.contains(KeyModifiers::ALT) {
        out.push_str("alt+");
    }
    out.push_str(&base);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::rewrite_kitty_z;

    #[test]
    fn injects_z_into_placement() {
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b_Ga=T,f=32,s=8,v=8,i=1;AAAA\x1b\\");
        let (out, rem) = rewrite_kitty_z(&input);
        assert!(rem.is_empty());
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("z=1"), "got: {s}");
        assert!(s.starts_with("\x1b_G"));
        assert!(s.ends_with("\x1b\\"));
    }

    #[test]
    fn leaves_non_placement_alone() {
        let mut input = Vec::new();
        input.extend_from_slice(b"\x1b_Ga=d,d=A\x1b\\");
        let (out, _) = rewrite_kitty_z(&input);
        let s = String::from_utf8(out).unwrap();
        assert!(!s.contains("z="));
    }

    #[test]
    fn buffers_incomplete_apc() {
        let input = b"\x1b_Ga=T,i=1";
        let (out, rem) = rewrite_kitty_z(input);
        assert!(out.is_empty());
        assert_eq!(rem.len(), input.len());
    }

    #[test]
    fn preserves_existing_z() {
        let input = b"\x1b_Ga=T,z=5,i=1;X\x1b\\";
        let (out, _) = rewrite_kitty_z(input);
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("z=5"));
        assert!(!s.contains("z=1"));
    }
}
