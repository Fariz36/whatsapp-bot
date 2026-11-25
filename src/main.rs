//! `!plz` sound-sticker bot on top of whatsapp-rust.
//!
//! Triggers (anything else is ignored):
//!   * a video / GIF / image whose caption starts with `!plz [start] [end]`
//!   * a `!plz [start] [end]` text that replies to a video / GIF / image
//!   * `!plz <youtube, x.com or instagram link> [#N] [start] [end]`  (`#N` = Nth media of a post, default: first)
//! Images become static stickers; videos/GIFs animated ones (with sound if the clip has audio).
//! Times: `12`, `1:05`, `0:01:05`, or a range `1:05-1:12`. Clip length defaults to/maxes at 15 s;
//! `full` takes the whole video (up to 60 s). `8fps` fixes the frame rate (default: best that fits the
//! 1 MB phone playback limit, max 12). `!plz help` = usage.
//! Messages timestamped before `--since` (default: bot start) are ignored.
//!
//! Wire shape of a sound sticker (verified against one sent by the official Android app):
//!   Message.audioStickerMessage = FutureProofMessage {
//!     message: Message { stickerMessage: StickerMessage { ..., audio: AudioMessage } }
//!   }
//!
//! Usage:
//!   cargo run --release -- [--since 2026-11-24T20:00:00+07:00] [--mode audio-sticker|fallback|both]
//!                          [--pair-phone 62xxxx]
//!   cargo run --release -- --test-send 62xxxx   # one-off send of media/sticker.{webp,ogg}

#![recursion_limit = "512"]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use log::{error, info, warn};
use tokio::sync::Semaphore;
use whatsapp_rust::download::{DownloadLimits, Downloadable, MediaType};
use whatsapp_rust::pair_code::PairCodeOptions;
use whatsapp_rust::prelude::*;
use whatsapp_rust::upload::UploadResponse;

const TRIGGER: &str = "!plz";
const MAX_CLIP_SECS: f64 = 15.0;
/// Cap for `full` requests (also enforced in ytdl.py).
const MAX_FULL_SECS: f64 = 60.0;
const MAX_FPS: u32 = 20;

const HELP: &str = "\
*!plz* turns a video, GIF or image into a sticker 🔊
(videos with sound become sound stickers)

*Sources*
• send a video / GIF / image with caption `!plz`
• reply `!plz` to a video / GIF / image
• `!plz <link>`: YouTube, X/Twitter, Instagram
  posts with several media: `#2` = 2nd media

*Options* (any order)
• `0:05` start at 5 s
• `0:05-0:12` or `0:05 0:12` clip range
• `full` whole video instead of 15 s (max 60 s)
• `8fps` fixed frame rate, 1-20

Default: first 15 s, best fps (up to 12) and quality that fit 1 MB, the most a phone will play. Longer clips get lower fps/quality; if it still can't fit, you get a desktop/WhatsApp Web-only sticker.

*Examples*
`!plz https://youtu.be/abc 1:05-1:15`
`!plz https://x.com/u/status/123 #2 full`
`!plz 0:03 8fps` (as video caption)

Times, `full` and fps are ignored for images.";
/// Max WhatsApp video we are willing to download (bytes).
const MAX_WA_VIDEO_BYTES: u64 = 64 << 20;
/// Max time for the python job (download + encode).
const JOB_TIMEOUT: Duration = Duration::from_secs(300);
const WIB_OFFSET_SECS: i64 = 7 * 3600;

#[derive(Clone)]
struct Config {
    since: i64,
    mode: String,
    root: PathBuf,
}

/// Jobs processed in parallel.
const MAX_CONCURRENT: usize = 10;
/// Requests allowed to wait for a free worker before we answer "busy".
const MAX_QUEUED: usize = 10;

struct State {
    cfg: Config,
    busy: Semaphore,
    queued: std::sync::atomic::AtomicUsize,
}

// ---------------------------------------------------------------- args

fn arg(v: &[String], k: &str) -> Option<String> {
    v.iter().position(|a| a == k).and_then(|i| v.get(i + 1)).cloned()
}

/// Parse `2026-11-24T20:00:00+07:00`, `2026-11-24 20:00` (assumed UTC+7) or unix seconds.
fn parse_since(s: &str) -> Result<i64> {
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    let s = s.trim().replace(' ', "T");
    let (dt, off) = if let Some(p) = s.rfind(['+', '-']).filter(|&p| p > 10) {
        let (d, o) = s.split_at(p);
        let sign = if o.starts_with('-') { -1 } else { 1 };
        let mut it = o[1..].split(':');
        let h: i64 = it.next().unwrap_or("0").parse()?;
        let m: i64 = it.next().unwrap_or("0").parse()?;
        (d.to_string(), sign * (h * 3600 + m * 60))
    } else if let Some(d) = s.strip_suffix('Z') {
        (d.to_string(), 0)
    } else {
        (s.clone(), WIB_OFFSET_SECS)
    };
    let (date, time) = dt.split_once('T').unwrap_or((&dt, "00:00:00"));
    let d: Vec<i64> = date.split('-').map(str::parse).collect::<Result<_, _>>()?;
    let t: Vec<i64> = time.split(':').map(str::parse).collect::<Result<_, _>>()?;
    if d.len() != 3 || t.is_empty() {
        bail!("bad --since {s}");
    }
    let days = days_from_civil(d[0], d[1], d[2]);
    let secs = days * 86400 + t[0] * 3600 + t.get(1).copied().unwrap_or(0) * 60 + t.get(2).copied().unwrap_or(0);
    Ok(secs - off)
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn fmt_wib(unix: i64) -> String {
    let t = unix + WIB_OFFSET_SECS;
    let (days, rem) = (t.div_euclid(86400), t.rem_euclid(86400));
    // civil_from_days
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC+7",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------- command parsing

#[derive(Debug, PartialEq)]
struct Request {
    url: Option<String>,
    /// 1-based media number inside an X post (`#2`).
    item: Option<u32>,
    start: f64,
    duration: f64,
    /// None = let ytdl.py pick the best fps that fits the 1 MB phone limit.
    fps: Option<u32>,
    help: bool,
}

fn parse_time(s: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.parse::<f64>().ok().filter(|x| *x >= 0.0)?;
    }
    Some(total)
}

/// `rest` is the text after `!plz`.
fn parse_request(rest: &str) -> Result<Request, String> {
    let mut url = None;
    let mut item = None;
    let mut times = Vec::new();
    let (mut full, mut fps, mut help) = (false, None, false);
    for tok in rest.split_whitespace() {
        let low = tok.to_ascii_lowercase();
        if low == "help" || low == "?" {
            help = true;
        } else if low == "full" {
            full = true;
        } else if let Some(n) = low.strip_suffix("fps") {
            fps = Some(n.parse().ok().filter(|n| (1..=MAX_FPS).contains(n)).ok_or(format!("bad fps `{tok}` (use 1fps to {MAX_FPS}fps)"))?);
        } else if tok.starts_with("http://") || tok.starts_with("https://") {
            url = Some(tok.to_string());
        } else if let Some(n) = tok.strip_prefix('#') {
            item = Some(n.parse().ok().filter(|n| (1..=20).contains(n)).ok_or(format!("bad media number `{tok}` (use #1, #2, ...)"))?);
        } else if let Some((a, b)) = tok.split_once('-') {
            times.push(parse_time(a).ok_or(format!("bad time `{a}`"))?);
            times.push(parse_time(b).ok_or(format!("bad time `{b}`"))?);
        } else {
            times.push(parse_time(tok).ok_or(format!("bad time `{tok}` (use 12, 1:05 or 1:05-1:12)"))?);
        }
    }
    let start = times.first().copied().unwrap_or(0.0);
    let cap = if full { MAX_FULL_SECS } else { MAX_CLIP_SECS };
    let duration = match times.get(1) {
        Some(end) if *end > start => (end - start).min(cap),
        Some(_) => return Err("end time must be after start time".into()),
        None => cap,
    };
    if times.len() > 2 {
        return Err("too many times; use `!plz [start] [end]`".into());
    }
    Ok(Request { url, item, start, duration, fps, help })
}

// ---------------------------------------------------------------- message building

fn sticker_msg(up: &UploadResponse, animated: bool) -> wa::message::StickerMessage {
    let mut s = wa::message::StickerMessage::default();
    s.url = Some(up.url.clone());
    s.direct_path = Some(up.direct_path.clone());
    s.media_key = Some(up.media_key.to_vec());
    s.file_sha256 = Some(up.file_sha256.to_vec());
    s.file_enc_sha256 = Some(up.file_enc_sha256.to_vec());
    s.file_length = Some(up.file_length);
    s.media_key_timestamp = Some(up.media_key_timestamp);
    s.mimetype = Some("image/webp".into());
    s.width = Some(512);
    s.height = Some(512);
    s.is_animated = Some(animated);
    s
}

/// Animated WebP files carry an `ANIM` chunk in their header area.
fn is_animated_webp(b: &[u8]) -> bool {
    b.len() > 40 && b[..b.len().min(64)].windows(4).any(|w| w == b"ANIM")
}

fn audio_msg(up: &UploadResponse, seconds: u32, ptt: bool) -> wa::message::AudioMessage {
    let mut a = wa::message::AudioMessage::default();
    a.url = Some(up.url.clone());
    a.direct_path = Some(up.direct_path.clone());
    a.media_key = Some(up.media_key.to_vec());
    a.file_sha256 = Some(up.file_sha256.to_vec());
    a.file_enc_sha256 = Some(up.file_enc_sha256.to_vec());
    a.file_length = Some(up.file_length);
    a.media_key_timestamp = Some(up.media_key_timestamp);
    a.streaming_sidecar = up.streaming_sidecar.clone();
    a.mimetype = Some("audio/ogg; codecs=opus".into());
    a.seconds = Some(seconds);
    a.ptt = Some(ptt);
    a
}

/// Upload `webp` (+ optional `ogg`) and send per `mode` to `to`.
async fn send_sound_sticker(
    client: &Client,
    to: &Jid,
    webp: Vec<u8>,
    ogg: Option<Vec<u8>>,
    seconds: u32,
    mode: &str,
) -> Result<()> {
    let animated = is_animated_webp(&webp);
    let ogg = if animated { ogg } else { None };
    let st_up = client.upload(webp, MediaType::Sticker, Default::default()).await?;
    let au_up = match ogg {
        Some(b) => Some(client.upload(b, MediaType::Audio, Default::default()).await?),
        None => None,
    };

    let Some(au_up) = au_up else {
        // No audio track: a plain animated sticker is all we can do.
        let mut m = wa::Message::default();
        m.sticker_message = MessageField::some(sticker_msg(&st_up, animated));
        client.send_message(to.clone(), m).await?;
        return Ok(());
    };

    if mode == "audio-sticker" || mode == "both" {
        let mut sticker = sticker_msg(&st_up, animated);
        sticker.audio = Some(wa::message::sticker_message::Audio::AudioMessage(Box::new(
            audio_msg(&au_up, seconds, false),
        )));
        let mut inner = wa::Message::default();
        inner.sticker_message = MessageField::some(sticker);
        let mut fpm = wa::message::FutureProofMessage::default();
        fpm.message = MessageField::some(inner);
        let mut msg = wa::Message::default();
        msg.audio_sticker_message = MessageField::some(fpm);
        client.send_message(to.clone(), msg).await?;
    }
    if mode == "fallback" || mode == "both" {
        let mut m1 = wa::Message::default();
        m1.sticker_message = MessageField::some(sticker_msg(&st_up, animated));
        client.send_message(to.clone(), m1).await?;
        let mut m2 = wa::Message::default();
        m2.audio_message = MessageField::some(audio_msg(&au_up, seconds, true));
        client.send_message(to.clone(), m2).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------- job pipeline

enum Outcome {
    Done,
    Rejected(String),
    Reply(String),
}

struct Sticker {
    webp: Vec<u8>,
    ogg: Option<Vec<u8>>,
    /// Real clip length (may be shorter than requested near the end of a video).
    secs: u32,
    /// Text to send after the sticker (e.g. "desktop only" warning).
    note: Option<String>,
}

/// Runs `tools/ytdl.py job` and returns the sticker files. Exit 2 = user-facing rejection.
async fn run_job(root: &Path, dir: &Path, input: Option<&Path>, url: Option<&str>, req: &Request) -> Result<Result<Sticker, String>> {
    let mut cmd = tokio::process::Command::new(root.join(".venv/bin/python"));
    cmd.arg(root.join("tools/ytdl.py")).arg("job");
    match (input, url) {
        (Some(p), _) => cmd.arg("--input").arg(p),
        (None, Some(u)) => {
            if let Some(n) = req.item {
                cmd.arg("--item").arg(n.to_string());
            }
            cmd.arg("--url").arg(u)
        }
        _ => bail!("no source"),
    };
    if let Some(f) = req.fps {
        cmd.arg("--fps").arg(f.to_string());
    }
    cmd.arg("--start").arg(req.start.to_string())
        .arg("--duration").arg(req.duration.to_string())
        .arg("--out").arg(dir)
        .current_dir(root)
        .kill_on_drop(true);
    let out = tokio::time::timeout(JOB_TIMEOUT, cmd.output())
        .await
        .map_err(|_| anyhow!("job timed out"))??;
    if out.status.code() == Some(2) {
        let msg = String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("rejected").to_string();
        return Ok(Err(msg));
    }
    if !out.status.success() {
        bail!("job failed: {}", String::from_utf8_lossy(&out.stderr).chars().rev().take(2500).collect::<String>().chars().rev().collect::<String>());
    }
    let webp = std::fs::read(dir.join("sticker.webp"))?;
    let ogg = std::fs::read(dir.join("sticker.ogg")).ok();
    let secs = std::fs::read_to_string(dir.join("sticker.secs"))
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(req.duration);
    let note = std::fs::read_to_string(dir.join("sticker.note")).ok().map(|s| s.trim().to_string());
    Ok(Ok(Sticker { webp, ogg, secs: secs.ceil().max(1.0) as u32, note }))
}

/// WhatsApp media we can turn into a sticker. GIFs arrive as videos (`gif_playback`).
enum Media {
    Video(wa::message::VideoMessage),
    Image(wa::message::ImageMessage),
}

impl Media {
    fn from_message(m: &wa::Message) -> Option<Media> {
        let b = m.get_base_message();
        if let Some(v) = b.video_message.as_option() {
            return Some(Media::Video(v.clone()));
        }
        b.image_message.as_option().map(|i| Media::Image(i.clone()))
    }
    fn downloadable(&self) -> &dyn Downloadable {
        match self {
            Media::Video(v) => v,
            Media::Image(i) => i,
        }
    }
    fn size(&self) -> u64 {
        match self {
            Media::Video(v) => v.file_length.unwrap_or(0),
            Media::Image(i) => i.file_length.unwrap_or(0),
        }
    }
    /// ytdl.py decides image vs video by extension; ffmpeg probes the real format anyway.
    fn file_name(&self) -> &'static str {
        match self {
            Media::Video(_) => "input.mp4",
            Media::Image(_) => "input.jpg",
        }
    }
}

/// The message's own media, or the media of the message it replies to.
fn find_media(msg: &wa::Message) -> Option<Media> {
    Media::from_message(msg).or_else(|| {
        let ctx = msg.get_base_message().extended_text_message.as_option()?.context_info.as_option()?;
        Media::from_message(ctx.quoted_message.as_option()?)
    })
}

async fn handle(state: Arc<State>, ctx: MessageContext) -> Result<Outcome> {
    let msg = &ctx.message;
    let text = msg.get_caption().or_else(|| msg.text_content()).unwrap_or("").trim();
    let Some(rest) = text.strip_prefix(TRIGGER) else { return Ok(Outcome::Done) };
    if !(rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        return Ok(Outcome::Done); // e.g. "!plzzz"
    }
    let req = match parse_request(rest) {
        Ok(r) => r,
        Err(e) => return Ok(Outcome::Rejected(format!("{e}\n\nsend `!plz help` for usage"))),
    };
    let video = find_media(msg);
    if req.help || (video.is_none() && req.url.is_none()) {
        return Ok(Outcome::Reply(HELP.into()));
    }

    let sender = ctx.info.source.sender.user.to_string();

    // Up to MAX_CONCURRENT jobs at once; up to MAX_QUEUED more wait their turn, beyond that we refuse.
    use std::sync::atomic::Ordering::SeqCst;
    let _permit = match state.busy.try_acquire() {
        Ok(p) => p,
        Err(_) => {
            if state.queued.fetch_add(1, SeqCst) >= MAX_QUEUED {
                state.queued.fetch_sub(1, SeqCst);
                return Ok(Outcome::Rejected("too many requests queued, try again in a minute".into()));
            }
            info!("queued request from {sender}");
            let p = state.busy.acquire().await;
            state.queued.fetch_sub(1, SeqCst);
            p?
        }
    };

    let dir = state.cfg.root.join("jobs").join(ctx.info.id.to_string());
    std::fs::create_dir_all(&dir)?;
    let result = async {
        let input = if let Some(v) = &video {
            let size = v.size();
            if size > MAX_WA_VIDEO_BYTES {
                return Ok(Err(format!("file is {} MB; max is {} MB", size >> 20, MAX_WA_VIDEO_BYTES >> 20)));
            }
            let bytes = ctx
                .client
                .download(v.downloadable(), DownloadLimits::new(MAX_WA_VIDEO_BYTES + (1 << 20), MAX_WA_VIDEO_BYTES))
                .await
                .map_err(|e| anyhow!("download: {e}"))?;
            let p = dir.join(v.file_name());
            std::fs::write(&p, bytes)?;
            Some(p)
        } else {
            None
        };
        let url = if input.is_some() { None } else { req.url.as_deref() };
        run_job(&state.cfg.root, &dir, input.as_deref(), url, &req).await
    }
    .await;
    // Always delete the downloaded video and intermediates.
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        warn!("cleanup {dir:?}: {e}");
    }

    match result? {
        Err(reason) => Ok(Outcome::Rejected(reason)),
        Ok(st) => {
            send_sound_sticker(&ctx.client, &ctx.info.source.chat, st.webp, st.ogg, st.secs, &state.cfg.mode).await?;
            if let Some(n) = st.note {
                let _ = ctx.reply(n).await;
            }
            Ok(Outcome::Done)
        }
    }
}

/// Re-run a link request (`!plz <url> ...`) for `chat` outside the normal message flow.
async fn replay(client: &Client, root: &Path, mode: &str, i: usize, chat: &str, text: &str) -> Result<()> {
    let jid: Jid = chat.parse().context("bad chat jid")?;
    let rest = text.trim().strip_prefix(TRIGGER).context("text must start with !plz")?;
    let req = parse_request(rest).map_err(|e| anyhow!(e))?;
    let url = req.url.clone().context("replay needs a link")?;
    let dir = root.join("jobs").join(format!("replay-{i}"));
    std::fs::create_dir_all(&dir)?;
    let r = run_job(root, &dir, None, Some(&url), &req).await;
    let _ = std::fs::remove_dir_all(&dir);
    match r? {
        Ok(st) => send_sound_sticker(client, &jid, st.webp, st.ogg, st.secs, mode).await,
        Err(reason) => bail!("rejected: {reason}"),
    }
}

// ---------------------------------------------------------------- main

fn print_qr(code: &str) {
    match qrcode::QrCode::new(code.as_bytes()) {
        Ok(qr) => {
            let s = qr.render::<qrcode::render::unicode::Dense1x2>().quiet_zone(true).build();
            println!("\n{s}\n");
            let _ = std::fs::write("qr.txt", format!("{s}\n\nraw: {code}\n"));
        }
        Err(e) => warn!("qr render failed: {e}; raw code: {code}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let v: Vec<String> = std::env::args().collect();
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let cfg = Config {
        since: arg(&v, "--since").map(|s| parse_since(&s)).transpose()?.unwrap_or(now),
        mode: arg(&v, "--mode").unwrap_or_else(|| "audio-sticker".into()),
        root: std::env::current_dir()?,
    };
    let test_send = arg(&v, "--test-send");
    // `--replay "CHAT_JID|!plz <link> ..."` (repeatable): process link requests once after connecting.
    let replays: Vec<(String, String)> = v
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--replay")
        .filter_map(|(i, _)| v.get(i + 1)?.split_once('|').map(|(c, t)| (c.to_string(), t.to_string())))
        .collect();
    // `--send "JID|file.webp|file.ogg|seconds"` (repeatable): send prepared files once after connecting.
    let sends: Vec<Vec<String>> = v
        .iter()
        .enumerate()
        .filter(|(_, a)| *a == "--send")
        .filter_map(|(i, _)| Some(v.get(i + 1)?.split('|').map(str::to_string).collect::<Vec<_>>()))
        .filter(|p| p.len() == 4)
        .collect();
    let replayed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let db = arg(&v, "--db").unwrap_or_else(|| "whatsapp.db".into());
    info!("ignoring messages sent before {} (unix {})", fmt_wib(cfg.since), cfg.since);
    info!("mode={}", cfg.mode);

    let state = Arc::new(State { cfg: cfg.clone(), busy: Semaphore::new(MAX_CONCURRENT), queued: Default::default() });
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<()>(1);

    let st = state.clone();
    let mut builder = Bot::builder()
        .with_backend(SqliteStore::open(&db).await?)
        .on_qr_code(|code, timeout| async move {
            info!("QR code (valid {timeout:?}). WhatsApp > Linked devices > Link a device:");
            print_qr(&code);
        })
        .on_pair_code(|code, timeout| async move {
            info!("PAIR CODE: {code}  (valid {timeout:?}) -> WhatsApp > Linked devices > Link with phone number");
            let _ = std::fs::write("paircode.txt", format!("{code}\n"));
        })
        .on_pair_code_error(|e, _c| async move { error!("pair code error: {e:?}") })
        .on_logged_out(|i| async move { error!("logged out: {i:?}") })
        .on_message(move |ctx| {
            let st = st.clone();
            async move {
                let ts = ctx.info.timestamp.as_secs();
                if ts < st.cfg.since {
                    return;
                }
                let text = ctx.message.get_caption().or_else(|| ctx.message.text_content()).unwrap_or("");
                if !text.trim_start().starts_with(TRIGGER) {
                    return;
                }
                let who = format!("{} ({})", ctx.info.push_name, ctx.info.source.sender);
                info!("request from {who} at {}: {text:?}", fmt_wib(ts));
                let started = Instant::now();
                match handle(st, ctx.clone()).await {
                    Ok(Outcome::Done) => info!("done for {who} in {:.1}s", started.elapsed().as_secs_f32()),
                    Ok(Outcome::Reply(r)) => {
                        info!("help sent to {who}");
                        let _ = ctx.reply(r).await;
                    }
                    Ok(Outcome::Rejected(r)) => {
                        info!("rejected {who}: {r}");
                        let _ = ctx.reply(format!("❌ {r}")).await;
                    }
                    Err(e) => {
                        error!("failed for {who}: {e:#}");
                        let _ = ctx.reply("❌ something went wrong processing that").await;
                    }
                }
            }
        })
        .on_connected(move |client| {
            let done = done_tx.clone();
            let to = test_send.clone();
            let mode = cfg.mode.clone();
            if (!replays.is_empty() || !sends.is_empty()) && !replayed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                let (client2, mode2, sends) = (client.clone(), mode.clone(), sends.clone());
                tokio::spawn(async move {
                    // Sequential, in the given order.
                    for p in sends {
                        let r = async {
                            let jid: Jid = p[0].parse().context("bad jid")?;
                            let webp = std::fs::read(&p[1])?;
                            let ogg = std::fs::read(&p[2]).ok();
                            let secs: u32 = p[3].parse().unwrap_or(8);
                            send_sound_sticker(&client2, &jid, webp, ogg, secs, &mode2).await
                        }
                        .await;
                        match r {
                            Ok(()) => info!("send ok: {}", p[1]),
                            Err(e) => error!("send failed: {}: {e:#}", p[1]),
                        }
                    }
                });
                for (i, (chat, text)) in replays.clone().into_iter().enumerate() {
                    let (client, root, mode) = (client.clone(), cfg.root.clone(), mode.clone());
                    tokio::spawn(async move {
                        let r = replay(&client, &root, &mode, i, &chat, &text).await;
                        match r {
                            Ok(()) => info!("replay ok: {chat} {text:?}"),
                            Err(e) => error!("replay failed: {chat} {text:?}: {e:#}"),
                        }
                    });
                }
            }
            async move {
                info!("connected; listening for `{TRIGGER}`");
                let Some(to) = to else { return };
                let r = async {
                    let jid: Jid = format!("{to}@s.whatsapp.net").parse().context("bad jid")?;
                    let webp = std::fs::read("media/sticker.webp")?;
                    let ogg = std::fs::read("media/sticker.ogg").ok();
                    send_sound_sticker(&client, &jid, webp, ogg, 8, &mode).await
                }
                .await;
                match r {
                    Ok(()) => info!("test send ok"),
                    Err(e) => error!("test send failed: {e:#}"),
                }
                let _ = done.send(()).await;
            }
        });

    if let Some(phone) = arg(&v, "--pair-phone") {
        builder = builder.with_pair_code(PairCodeOptions { phone_number: phone, ..Default::default() });
    }

    let bot = builder.build().await?;
    let mut handle = bot.spawn();
    tokio::select! {
        _ = &mut handle => {}
        _ = shutdown_signal() => info!("signal; shutting down"),
        _ = done_rx.recv() => {
            tokio::time::sleep(Duration::from_secs(5)).await;
            info!("test send finished; shutting down");
        }
    }
    let report = handle.shutdown().await;
    info!("shutdown: {report:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_requests() {
        let d = |start, duration| Request { url: None, item: None, start, duration, fps: None, help: false };
        assert_eq!(parse_request("").unwrap(), d(0.0, 15.0));
        assert_eq!(parse_request(" 5").unwrap(), d(5.0, 15.0));
        assert_eq!(parse_request(" 1:05-1:09").unwrap(), d(65.0, 4.0));
        assert_eq!(parse_request(" full").unwrap().duration, 60.0);
        assert_eq!(parse_request(" 0:10-0:50 FULL").unwrap().duration, 40.0);
        assert_eq!(parse_request(" 0:10-0:50").unwrap().duration, 15.0);
        assert_eq!(parse_request(" 8fps").unwrap().fps, Some(8));
        assert!(parse_request(" 0fps").is_err() && parse_request(" 60fps").is_err());
        assert!(parse_request(" help").unwrap().help);
        let r = parse_request(" https://x.com/a/status/1 #2 0:03").unwrap();
        assert_eq!((r.item, r.start), (Some(2), 3.0));
        assert!(parse_request(" https://x.com/a/status/1 #0").is_err());
        assert_eq!(parse_request(" 1:00 1:30").unwrap().duration, 15.0);
        let r = parse_request(" https://youtu.be/x 0:10").unwrap();
        assert_eq!((r.url.as_deref(), r.start), (Some("https://youtu.be/x"), 10.0));
        assert!(parse_request(" abc").is_err());
        assert!(parse_request(" 10-5").is_err());
    }

    #[test]
    fn parses_since() {
        let a = parse_since("2026-11-24T20:00:00+07:00").unwrap();
        assert_eq!(a, parse_since("2026-11-24 20:00").unwrap());
        assert_eq!(a, parse_since("2026-11-24T13:00:00Z").unwrap());
        assert_eq!(fmt_wib(a), "2026-11-24 20:00:00 UTC+7");
    }
}
