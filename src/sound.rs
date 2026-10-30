//! Background sound: internet radio, procedural loops, Spotify and Feishin.
//!
//! Audio path (all open source): HTTP stream → Symphonia MP3/AAC decoder →
//! rodio → CoreAudio. Procedural loops are rendered in memory and repeated
//! by rodio. (AVPlayer was tried first but never finished loading Icecast
//! radio streams, so it is not used.)
//!
//! Spotify is driven through AppleScript; Feishin through its built-in
//! remote-control WebSocket (Settings → enable remote control server).

use std::collections::HashMap;
use std::io::Read;
use std::net::{SocketAddr, TcpStream};
use std::num::NonZero;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Source as _};
use serde::Deserialize;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSourceStream, ReadOnlySource};
use symphonia::core::meta::MetadataOptions;

use crate::soundsynth::{self, Synth};

/// `[sound]` table in the config file.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SoundSettings {
    pub lofi_url: String,
    pub jazz_url: String,
    pub lounge_url: String,
    pub classical_url: String,
    /// e.g. `spotify:playlist:…`; empty = resume whatever Spotify had.
    pub spotify_uri: String,
    /// Feishin remote-control server (Feishin → Settings → remote control).
    pub feishin_port: u16,
    pub feishin_username: String,
    pub feishin_password: String,
}

impl Default for SoundSettings {
    fn default() -> Self {
        Self {
            // Free internet radio (not open source, but free to listen to).
            lofi_url: "https://streams.ilovemusic.de/iloveradio17.mp3".into(), // I♥CHILLHOP
            jazz_url: "https://jazz-wr01.ice.infomaniak.ch/jazz-wr01-128.mp3".into(), // JZR Classic Jazz
            lounge_url: "https://live.radioart.com/fLounge.mp3".into(), // RadioArt Lounge
            // 好家庭聯播網 古典音樂台 FM 97.7, official host (listed in the
            // open Radio Browser directory).
            classical_url: "http://onair.family977.com.tw:8000/live.mp3".into(),
            spotify_uri: String::new(),
            feishin_port: 4333,
            feishin_username: "feishin".into(),
            feishin_password: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Stream(String),
    Loop(Synth),
    Spotify(String),
    Feishin,
}

/// One menu entry.
#[derive(Debug, Clone)]
pub struct Choice {
    pub title: &'static str,
    /// Menu group: entries with the same group sit in the same submenu.
    pub group: Option<&'static str>,
    pub source: Source,
}

/// Everything the menu offers, in display order.
pub fn catalog(s: &SoundSettings) -> Vec<Choice> {
    let c = |title, group, source| Choice { title, group, source };
    vec![
        c("Lofi（I♥CHILLHOP 電台）", None, Source::Stream(s.lofi_url.clone())),
        c("Jazz（JZR Classic Jazz 電台）", None, Source::Stream(s.jazz_url.clone())),
        c("Lounge（RadioArt 電台）", None, Source::Stream(s.lounge_url.clone())),
        c("97.7 古典音樂台（好家庭）", None, Source::Stream(s.classical_url.clone())),
        c("Coffee Shop（合成）", None, Source::Loop(Synth::Cafe)),
        c("Rain（合成）", None, Source::Loop(Synth::Rain)),
        c("白噪音", Some("Frequencies"), Source::Loop(Synth::White)),
        c("粉紅噪音", Some("Frequencies"), Source::Loop(Synth::Pink)),
        c("棕噪音", Some("Frequencies"), Source::Loop(Synth::Brown)),
        c("40 Hz 雙耳節拍（專注，需耳機）", Some("Frequencies"), Source::Loop(Synth::Binaural { carrier: 200, beat: 40 })),
        c("10 Hz 雙耳節拍（放鬆，需耳機）", Some("Frequencies"), Source::Loop(Synth::Binaural { carrier: 200, beat: 10 })),
        c("Feishin", None, Source::Feishin),
        c("Spotify", None, Source::Spotify(s.spotify_uri.clone())),
    ]
}

// ---------------------------------------------------------------------------
// External players
// ---------------------------------------------------------------------------

/// Run an AppleScript without waiting (keeps the menu responsive).
fn osascript_async(script: &str) {
    let _ = Command::new("osascript")
        .args(["-e", script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Pause Spotify only if it is already running (never launches it).
fn spotify_pause() {
    osascript_async("if application \"Spotify\" is running then tell application \"Spotify\" to pause");
}

fn spotify_play(uri: &str) -> Result<(), String> {
    if uri.is_empty() {
        osascript_async("tell application \"Spotify\" to play");
        return Ok(());
    }
    // Spotify URIs contain only [A-Za-z0-9:]; reject anything else so the
    // AppleScript string literal cannot be broken out of.
    if !uri.chars().all(|c| c.is_ascii_alphanumeric() || c == ':') {
        return Err("spotify_uri 格式不正確".into());
    }
    osascript_async(&format!("tell application \"Spotify\" to play track \"{uri}\""));
    Ok(())
}

fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Send one command (`"play"` / `"pause"`) to Feishin's remote server.
///
/// Protocol (from Feishin's source): WebSocket on `feishin_port`; the first
/// message must be `{"event":"authenticate","header":"Basic <b64 user:pass>"}`,
/// after which the server pushes its state; then `{"event":"pause"}` etc.
///
/// # Errors
/// Returns a user-facing message when the server is off or rejects the login.
pub fn feishin(cmd: &str, s: &SoundSettings) -> Result<(), String> {
    use tungstenite::Message;
    let addr = SocketAddr::from(([127, 0, 0, 1], s.feishin_port));
    let tcp = TcpStream::connect_timeout(&addr, Duration::from_millis(300))
        .map_err(|_| "Feishin 遠端控制沒開（Feishin → 設定 → 啟用遠端控制伺服器）".to_owned())?;
    tcp.set_read_timeout(Some(Duration::from_millis(1500))).map_err(|e| e.to_string())?;
    let (mut ws, _) = tungstenite::client(format!("ws://127.0.0.1:{}", s.feishin_port), tcp)
        .map_err(|e| format!("Feishin 連線失敗：{e}"))?;
    let creds = format!("{}:{}", s.feishin_username, s.feishin_password);
    let auth = serde_json::json!({ "event": "authenticate", "header": format!("Basic {}", base64(creds.as_bytes())) });
    ws.send(Message::text(auth.to_string())).map_err(|e| e.to_string())?;
    // A successful login is answered with the player state.
    match ws.read() {
        Ok(Message::Close(_)) | Err(_) => {
            return Err("Feishin 拒絕登入：請在 config.toml 的 [sound] 填入 feishin_username／feishin_password".into());
        }
        Ok(_) => {}
    }
    ws.send(Message::text(serde_json::json!({ "event": cmd }).to_string())).map_err(|e| e.to_string())?;
    let _ = ws.close(None);
    let _ = ws.flush();
    Ok(())
}

// ---------------------------------------------------------------------------
// Internet radio: HTTP → Symphonia → rodio
// ---------------------------------------------------------------------------

/// Decode one connection's worth of a stream into `player` until `stop`.
fn stream_once(url: &str, player: &rodio::Player, stop: &AtomicBool) -> Result<(), String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(8))
        .timeout_read(Duration::from_secs(15))
        .build();
    // No "Icy-MetaData" header, so the server sends plain audio frames.
    let resp = agent.get(url).call().map_err(|e| e.to_string())?;
    let mut hint = Hint::new();
    match resp.content_type() {
        "audio/aac" | "audio/aacp" => hint.with_extension("aac"),
        _ => hint.with_extension("mp3"),
    };
    let reader: Box<dyn Read + Send + Sync> = resp.into_reader();
    let mss = MediaSourceStream::new(Box::new(ReadOnlySource::new(reader)), Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&hint, mss, FormatOptions::default(), MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let track = format.default_track(TrackType::Audio).ok_or("no audio track")?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).ok_or("no audio params")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| e.to_string())?;

    // Batch decoded frames into ~0.5 s chunks before handing them to rodio.
    let mut pending: Vec<f32> = Vec::new();
    let mut scratch: Vec<f32> = Vec::new();
    let mut fmt: Option<(u16, u32)> = None;
    while !stop.load(Ordering::Relaxed) {
        let Some(packet) = format.next_packet().map_err(|e| e.to_string())? else { return Ok(()) };
        if packet.track_id != track_id {
            continue;
        }
        let buf = match decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymError::DecodeError(_)) => continue, // skip a corrupt frame
            Err(e) => return Err(e.to_string()),
        };
        let (ch, rate) = (buf.spec().channels().count() as u16, buf.spec().rate());
        scratch.resize(buf.samples_interleaved(), 0.0);
        buf.copy_to_slice_interleaved(&mut scratch);
        if fmt.is_some_and(|f| f != (ch, rate)) {
            pending.clear(); // format changed mid-stream; drop the partial chunk
        }
        fmt = Some((ch, rate));
        pending.extend_from_slice(&scratch);
        if pending.len() >= (rate as usize * ch as usize) / 2 {
            let (Some(c), Some(r)) = (NonZero::new(ch), NonZero::new(rate)) else { continue };
            player.append(SamplesBuffer::new(c, r, std::mem::take(&mut pending)));
            // Back-pressure: keep about 3 s queued, never an unbounded buffer.
            while player.len() > 6 && !stop.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Ok(())
}

/// A running radio stream: worker thread plus its rodio player.
struct RadioStream {
    stop: Arc<AtomicBool>,
    player: Arc<rodio::Player>,
}

impl RadioStream {
    fn start(url: String, player: Arc<rodio::Player>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (s, p) = (Arc::clone(&stop), Arc::clone(&player));
        // Detached worker: it notices `stop` at its next packet or read
        // timeout, so halting never blocks the main thread.
        std::thread::spawn(move || {
            // Reconnect on drops with a short back-off, until stopped.
            while !s.load(Ordering::Relaxed) {
                if let Err(e) = stream_once(&url, &p, &s) {
                    eprintln!("radio {url}: {e}");
                }
                for _ in 0..30 {
                    if s.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        });
        Self { stop, player }
    }
}

impl Drop for RadioStream {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.player.stop();
    }
}

// ---------------------------------------------------------------------------
// Player
// ---------------------------------------------------------------------------

fn last_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/aw-timer/last_sound")
}

/// Main-thread player state machine.
pub struct Player {
    settings: SoundSettings,
    choices: Vec<Choice>,
    /// Opened lazily so an idle timer never holds the audio device.
    device: Option<MixerDeviceSink>,
    loop_player: Option<rodio::Player>,
    radio: Option<RadioStream>,
    loops: HashMap<Synth, SamplesBuffer>,
    current: Option<Source>,
    paused: bool,
    /// Fixed gain for diagnostics only (`sound-check`). `None` = never
    /// touch the volume: unity gain, the system volume/mute decides.
    test_gain: Option<f32>,
}

impl Player {
    pub fn new(settings: SoundSettings) -> Self {
        Self {
            choices: catalog(&settings),
            test_gain: None,
            settings,
            device: None,
            loop_player: None,
            radio: None,
            loops: HashMap::new(),
            current: None,
            paused: false,
        }
    }

    pub fn choices(&self) -> &[Choice] {
        &self.choices
    }

    /// Source selected (playing or paused).
    pub fn current(&self) -> Option<&Source> {
        self.current.as_ref()
    }

    pub fn is_playing(&self) -> bool {
        self.current.is_some() && !self.paused
    }

    fn mixer(&mut self) -> Result<&rodio::mixer::Mixer, String> {
        if self.device.is_none() {
            let mut d = DeviceSinkBuilder::open_default_sink().map_err(|e| format!("無法開啟音訊輸出：{e}"))?;
            d.log_on_drop(false);
            self.device = Some(d);
        }
        Ok(self.device.as_ref().expect("just opened").mixer())
    }

    /// Stop in-process audio (radio / loop).
    fn stop_internal(&mut self) {
        self.radio = None; // Drop stops the worker and its player
        if let Some(p) = self.loop_player.take() {
            p.stop();
        }
    }

    /// Pause the external players, except `keep`.
    fn pause_external(&self, keep: Option<&Source>) {
        if !matches!(keep, Some(Source::Spotify(_))) {
            spotify_pause();
        }
        if !matches!(keep, Some(Source::Feishin)) {
            // Best effort: silently ignored when Feishin or its remote is off.
            let _ = feishin("pause", &self.settings);
        }
    }

    fn start(&mut self, source: &Source) -> Result<(), String> {
        match source {
            Source::Stream(url) => {
                let player = Arc::new(rodio::Player::connect_new(self.mixer()?));
                if let Some(g) = self.test_gain {
                    player.set_volume(g);
                }
                self.radio = Some(RadioStream::start(url.clone(), player));
            }
            Source::Loop(synth) => {
                let buf = self
                    .loops
                    .entry(*synth)
                    .or_insert_with(|| {
                        let data: Vec<f32> = soundsynth::render_loop(*synth).into_iter().flatten().collect();
                        let (ch, rate) = (NonZero::new(2).expect("2 > 0"), NonZero::new(soundsynth::RATE).expect("rate > 0"));
                        SamplesBuffer::new(ch, rate, data)
                    })
                    .clone();
                let player = rodio::Player::connect_new(self.mixer()?);
                if let Some(g) = self.test_gain {
                    player.set_volume(g);
                }
                player.append(buf.repeat_infinite());
                self.loop_player = Some(player);
            }
            Source::Spotify(uri) => spotify_play(uri)?,
            Source::Feishin => {
                if let Err(e) = feishin("play", &self.settings) {
                    let _ = Command::new("open").args(["-a", "Feishin"]).status();
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Switch to `source`: everything else (Spotify/Feishin included) pauses.
    ///
    /// # Errors
    /// Returns a user-facing message; the selection is kept either way.
    pub fn play(&mut self, source: &Source) -> Result<(), String> {
        self.stop_internal();
        self.pause_external(Some(source));
        self.current = Some(source.clone());
        self.paused = false;
        if let Some(c) = self.choices.iter().find(|c| &c.source == source) {
            if let Some(dir) = last_path().parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(last_path(), c.title);
        }
        self.start(source)
    }

    /// Pause without forgetting the selection.
    pub fn pause(&mut self) {
        let Some(cur) = self.current.clone() else { return };
        match cur {
            Source::Stream(_) => self.stop_internal(), // live radio: reconnect on resume
            Source::Loop(_) => {
                if let Some(p) = &self.loop_player {
                    p.pause();
                }
            }
            Source::Spotify(_) => spotify_pause(),
            Source::Feishin => {
                let _ = feishin("pause", &self.settings);
            }
        }
        self.paused = true;
    }

    /// Resume the paused selection.
    ///
    /// # Errors
    /// As for [`Player::play`].
    pub fn resume(&mut self) -> Result<(), String> {
        let Some(cur) = self.current.clone() else { return Ok(()) };
        self.paused = false;
        if let (Source::Loop(_), Some(p)) = (&cur, &self.loop_player) {
            p.play();
            return Ok(());
        }
        self.pause_external(Some(&cur));
        self.start(&cur)
    }

    /// Click on the menu-bar icon: pause if playing, otherwise resume the
    /// current or last-used selection (default: the first entry).
    ///
    /// # Errors
    /// As for [`Player::play`].
    pub fn toggle(&mut self) -> Result<(), String> {
        if self.is_playing() {
            self.pause();
            return Ok(());
        }
        if self.current.is_some() {
            return self.resume();
        }
        let last = std::fs::read_to_string(last_path()).unwrap_or_default();
        let source = self
            .choices
            .iter()
            .find(|c| c.title == last.trim())
            .or(self.choices.first())
            .map(|c| c.source.clone())
            .ok_or("沒有可播放的背景音")?;
        self.play(&source)
    }

    /// Stop everything, including Spotify and Feishin.
    pub fn stop(&mut self) {
        self.stop_internal();
        self.pause_external(None);
        self.current = None;
        self.paused = false;
    }

    /// Diagnostics only: play at a fixed gain (e.g. 0 for a silent test).
    pub fn with_test_gain(mut self, gain: f32) -> Self {
        self.test_gain = Some(gain.clamp(0.0, 1.0));
        self
    }

    /// Chunks (~0.5 s each) of radio audio decoded and queued (diagnostics).
    pub fn queued_chunks(&self) -> usize {
        self.radio.as_ref().map_or(0, |r| r.player.len())
    }

    /// Whether a loop is actively rendering (diagnostics).
    pub fn loop_active(&self) -> bool {
        self.loop_player.as_ref().is_some_and(|p| !p.is_paused() && !p.empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"feishin:abc123"), "ZmVpc2hpbjphYmMxMjM=");
    }
}
