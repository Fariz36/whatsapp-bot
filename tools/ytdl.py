"""Simple YouTube downloader + sound-sticker asset builder.

Usage:
  uv run tools/ytdl.py download <url> [-o media/source.mp4]
  uv run tools/ytdl.py sticker <video> [--start 0] [--duration 8] [--fps 12] [-o media/sticker]
      -> media/sticker.webp (animated, 512x512) + media/sticker.ogg (opus)
  uv run tools/ytdl.py job (--input FILE | --url URL [--item N]) --start S --duration D [--fps F] --out DIR
      Bot entry point. Writes DIR/sticker.webp, DIR/sticker.secs (+ DIR/sticker.ogg if the clip has audio).
      Exit 2 + one-line message on stderr = user-facing rejection (too long, bad link, ...).
"""

import argparse
import json
import re
import urllib.request
import subprocess
import sys
from pathlib import Path

import imageio_ffmpeg
import yt_dlp
from yt_dlp.utils import download_range_func

FFMPEG = imageio_ffmpeg.get_ffmpeg_exe()

# Abuse limits for YouTube jobs.
MAX_SOURCE_SECONDS = 15 * 60   # refuse videos longer than this (checked from metadata, before download)
MAX_DOWNLOAD_BYTES = 50 << 20  # hard backstop on what yt-dlp may write
MAX_CLIP_SECONDS = 60  # hard cap even for `full` requests
YOUTUBE_RE = re.compile(
    r"^https?://(www\.|m\.|music\.)?(youtube\.com/(watch\?|shorts/|live/)|youtu\.be/)[\w\-?=&%./]+$"
)
# x.com / twitter.com posts, plus common embed-fixer mirrors (normalized back to x.com).
X_RE = re.compile(
    r"^https?://(?:www\.|mobile\.)?(?:x|twitter|fxtwitter|vxtwitter|fixupx|fixvx)\.com/"
    r"(?P<user>\w+)/status/(?P<id>\d+)(?:/(?:video|photo)/(?P<index>\d+))?/?(?:[?#].*)?$"
)


# instagram.com posts / reels (optionally /<user>/p/ID, share query like ?igsh=..., ?img_index=N).
IG_RE = re.compile(
    r"^https?://(?:www\.|m\.)?instagram\.com/(?:[\w.]+/)?(?P<kind>p|reels?|tv)/(?P<id>[\w-]+)/?"
    r"(?:\?(?P<q>[^#]*))?(?:#.*)?$"
)


def normalize_url(url: str, item: int | None) -> tuple[str, str, int | None]:
    """Validate the link -> (canonical url, site, 1-based media index or None)."""
    if YOUTUBE_RE.match(url):
        return url, "youtube", None
    if m := X_RE.match(url):
        item = item or (int(m["index"]) if m["index"] else None)
        base = f"https://x.com/{m['user']}/status/{m['id']}"
        # /video/N selects the Nth media of the post, counting photos (yt-dlp semantics).
        return (f"{base}/video/{item}" if item else base), "x", item
    if m := IG_RE.match(url):
        if item is None and (q := re.search(r"(?:^|&)img_index=(\d+)", m["q"] or "")):
            item = int(q[1])
        kind = "reel" if m["kind"].startswith("reel") else m["kind"]
        return f"https://www.instagram.com/{kind}/{m['id']}/", "instagram", item
    raise Reject("only youtube.com / youtu.be / x.com / instagram.com links are supported")


class Reject(Exception):
    """A problem the WhatsApp user should be told about."""


def download(url: str, out: Path) -> Path:
    out.parent.mkdir(parents=True, exist_ok=True)
    opts = {
        "format": "bv*[ext=mp4]+ba[ext=m4a]/b[ext=mp4]/b",
        "merge_output_format": "mp4",
        "outtmpl": str(out),
        "ffmpeg_location": FFMPEG,
        "noplaylist": True,
    }
    with yt_dlp.YoutubeDL(opts) as ydl:
        ydl.download([url])
    print(f"saved {out}")
    return out


IMAGE_EXTS = {".jpg", ".jpeg", ".png", ".webp"}
UA = {"User-Agent": "Mozilla/5.0"}


def fetch_file(url: str, out: Path, headers: dict | None = None) -> Path:
    """Plain HTTP download with the same size backstop as yt-dlp."""
    req = urllib.request.Request(url, headers={**UA, **(headers or {})})
    with urllib.request.urlopen(req, timeout=30) as r, open(out, "wb") as f:
        total = 0
        while chunk := r.read(1 << 16):
            total += len(chunk)
            if total > MAX_DOWNLOAD_BYTES:
                f.close()
                out.unlink()
                raise Reject("media is too big")
            f.write(chunk)
    return out


def x_media(tweet_id: str) -> list[dict] | None:
    """Media list of a post from X's public embed endpoint (None if it can't be read)."""
    try:
        req = urllib.request.Request(
            f"https://cdn.syndication.twimg.com/tweet-result?id={tweet_id}&token=a", headers=UA)
        with urllib.request.urlopen(req, timeout=15) as r:
            return json.load(r).get("mediaDetails") or []
    except Exception:
        return None


def fetch_media(url: str, item: int | None, start: float, duration: float, out_dir: Path) -> tuple[Path, float]:
    """Download the requested media -> (file, where the clip starts inside it).

    yt-dlp videos are range-downloaded (start 0); direct files (X GIFs) are whole (start as given).
    Photos come back as .jpg.
    """
    canon, site, item = normalize_url(url, item)
    if site == "x":
        media = x_media(X_RE.match(canon)["id"])
        if media is not None:  # else: fall back to yt-dlp alone
            if not media:
                raise Reject("no media found in that post")
            idx = item or 1
            if idx > len(media):
                raise Reject(f"that post only has {len(media)} media")
            m = media[idx - 1]
            if m.get("type") == "photo":
                return fetch_file(m["media_url_https"] + "?name=large", out_dir / "source.jpg"), 0.0
            if m.get("type") == "animated_gif":
                mp4s = [v for v in m.get("video_info", {}).get("variants", [])
                        if v.get("content_type") == "video/mp4"]
                if not mp4s:
                    raise Reject("can't read that GIF")
                best = max(mp4s, key=lambda v: v.get("bitrate") or 0)
                return fetch_file(best["url"], out_dir / "source.mp4"), start
            item = idx  # video: let yt-dlp fetch /video/N (same numbering, photos counted)
    return download_clip(url, item, start, duration, out_dir), 0.0


def download_clip(url: str, item: int | None, start: float, duration: float, out_dir: Path) -> Path:
    """Download only [start, start+duration] of one video at <=480p, after checking limits.

    X / Instagram posts with several media: `item` (1-based, counting photos) picks one;
    default is the first video.
    """
    url, site, item = normalize_url(url, item)
    is_x = site == "x"
    base = {"quiet": True, "no_warnings": True, "ffmpeg_location": FFMPEG,
            # YouTube: never expand playlists. X: a multi-video post is a "playlist"; without a
            # number take its first video, with a number let yt-dlp pick /video/N (needs noplaylist).
            "noplaylist": site == "youtube" or (is_x and item is not None), "playlist_items": "1",
            # Instagram single-photo posts: don't fail, we turn the photo into a sticker.
            "ignore_no_formats_error": site == "instagram"}
    with yt_dlp.YoutubeDL(base) as ydl:
        try:
            # Instagram: inspect the raw carousel first so photos can be skipped / reported by number.
            info = ydl.extract_info(url, download=False, process=site != "instagram")
        except yt_dlp.utils.DownloadError as e:
            msg = re.sub(r"^ERROR: (\[\w+\] \w+: )?", "", str(e).splitlines()[0])
            if "is not a video" in msg:
                msg += " (photos and GIFs can't be picked by number)"
            if site == "instagram" and re.search(r"log(ged.)?in|rate.limit|not available|empty media", msg, re.I):
                msg += " (Instagram may be blocking anonymous downloads)"
            raise Reject(f"can't read that media: {msg[:150]}")
    if site == "instagram":
        if info.get("_type") == "playlist":
            entries = list(info.get("entries") or [])
            if not entries:
                raise Reject("no media found in that post")
            idx = item or 1
            if idx > len(entries):
                raise Reject(f"that post only has {len(entries)} media")
            info = entries[idx - 1]
            base["playlist_items"] = str(idx)
        elif item and item != 1:
            raise Reject("that post only has 1 media")
        if not info.get("formats"):  # a photo
            thumbs = [t for t in info.get("thumbnails") or [] if t.get("url")]
            if not thumbs:
                raise Reject(f"media #{item or 1} has no video or photo")
            best = max(thumbs, key=lambda t: (t.get("width") or 0) * (t.get("height") or 0))
            return fetch_file(best["url"], out_dir / "source.jpg", {"Referer": "https://www.instagram.com/"})
    elif info.get("_type") == "playlist":
        entries = [e for e in info.get("entries") or [] if e]
        if not is_x or not entries:
            raise Reject("no video found in that post" if is_x else "playlists are not supported")
        info = entries[0]
    if info.get("is_live") or info.get("live_status") in ("is_live", "is_upcoming"):
        raise Reject("live streams are not supported")
    length = info.get("duration")
    if not length:
        if site == "youtube":
            raise Reject("unknown video length; refusing")
        # X GIFs / Instagram media often lack a duration; they are short by platform limits,
        # and we only download the requested range under the size cap anyway.
        length = start + duration
    if length > MAX_SOURCE_SECONDS:
        raise Reject(f"video is {int(length) // 60} min long; max is {MAX_SOURCE_SECONDS // 60} min")
    if start >= length:
        raise Reject(f"start {start:.0f}s is past the end of the video ({length:.0f}s)")

    opts = {
        **base,
        "format": "bv*[height<=480]+ba/b[height<=480]/wv*+ba/w",
        "merge_output_format": "mp4",
        "outtmpl": str(out_dir / "source.%(ext)s"),
        "download_ranges": download_range_func(None, [(start, min(start + duration, length))]),
        "max_filesize": MAX_DOWNLOAD_BYTES,
    }
    with yt_dlp.YoutubeDL(opts) as ydl:
        ydl.download([url])
    files = sorted(out_dir.glob("source.*"))
    if not files:
        raise Reject("download failed or exceeded the size limit")
    return files[0]


def run(*args: str) -> None:
    subprocess.run([FFMPEG, "-hide_banner", "-loglevel", "error", "-y", *args], check=True)


# WhatsApp's servers accepted 36 s / 7.3 MB animated stickers, so the 500 KB / 10 s "limits" are
# app-side guidance only. Keep a backstop so a sticker stays sendable and loads on phones.
# Servers accept far bigger stickers, but phones refuse to play ones over ~1 MB (tested: 946 KB plays,
# 1046 KB doesn't). WhatsApp Web has no limit. Stay just under both readings of "1 MB".
MAX_STICKER_BYTES = 1_000_000
DEFAULT_FPS = 12
AUTO_FPS = (12, 10, 8, 6, 4)        # tried in order when the user didn't ask for an fps
QUALITY_STEPS = (80, 70, 60, 50, 40, 30)  # "good" range; encoded in parallel per fps
LOW_QUALITY = (20, 10)              # last resort before dropping fps further / giving up
DESKTOP_MAX_BYTES = 16 << 20        # fallback when a clip can't fit the phone limit at all


def encode_webp(prep: Path, fps: int, quality: int, out: Path) -> int:
    # libwebp_anim stores inter-frame diffs (big win on calm footage, ~neutral on high motion).
    # rgba BEFORE pad: padding in yuva420p came out opaque black (libwebp_anim drops alpha on frame 1).
    vf = f"fps={fps},format=rgba,pad=512:512:(ow-iw)/2:(oh-ih)/2:color=0x00000000"
    run("-i", str(prep), "-an", "-vf", vf, "-c:v", "libwebp_anim", "-lossless", "0",
        "-compression_level", "4", "-q:v", str(quality), "-loop", "0", str(out))
    return out.stat().st_size


def image_sticker(image: Path, webp: Path) -> None:
    """Static 512x512 sticker (transparent padding), quality stepped down to stay small."""
    vf = ("scale=512:512:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,"
          "pad=512:512:(ow-iw)/2:(oh-ih)/2:color=0x00000000,format=rgba")
    for q in (85, 70, 50, 30):
        run("-i", str(image), "-frames:v", "1", "-vf", vf, "-c:v", "libwebp", "-q:v", str(q), str(webp))
        if webp.stat().st_size <= 300 * 1024:
            break
    print(f"saved {webp} (static, {webp.stat().st_size / 1024:.0f} KB)")


def pick_encode(prep: Path, workdir: Path, fps_list: tuple[int, ...]) -> tuple[int, int, Path, bool]:
    """Best (fps, quality) under MAX_STICKER_BYTES -> (fps, q, file, fits_phone).

    Round 1: every fps at the lowest good quality, in parallel -> highest fps that fits.
    Round 2: that fps at higher qualities, in parallel -> highest quality that fits.
    If no fps fits even at good quality, try LOW_QUALITY. If still nothing fits, return a
    desktop-only encode (first fps, quality 50) and fits_phone=False.
    """
    from concurrent.futures import ThreadPoolExecutor

    def enc_many(pairs):
        def enc(fq):
            fps, q = fq
            out = workdir / f"v{fps}_q{q}.webp"
            return fps, q, out, encode_webp(prep, fps, q, out)
        with ThreadPoolExecutor(len(pairs)) as pool:
            res = list(pool.map(enc, pairs))
        print(" ".join(f"{f}fps/q{q}={size // 1024}KB" for f, q, _, size in res))
        return [r for r in res if r[3] <= MAX_STICKER_BYTES]

    low_good = min(QUALITY_STEPS)
    if fits := enc_many([(f, low_good) for f in fps_list]):
        fps = max(r[0] for r in fits)
        best = [r for r in fits if r[0] == fps][0]
        better = enc_many([(fps, q) for q in (*QUALITY_STEPS[:-1], low_good + 5)])
        if better:
            best = max(better, key=lambda r: r[1])
        return best[0], best[1], best[2], True
    if fits := enc_many([(f, q) for f in fps_list for q in LOW_QUALITY]):
        best = max(fits, key=lambda r: (r[0], r[1]))
        return best[0], best[1], best[2], True
    # Can't be made phone-playable: give the desktop / WhatsApp Web version.
    fps = fps_list[0]
    out = workdir / "desktop.webp"
    size = encode_webp(prep, fps, 50, out)
    if size > DESKTOP_MAX_BYTES:
        raise Reject("clip is too heavy for a sticker; try a shorter range or lower fps")
    return fps, 50, out, False


def sticker(video: Path, start: float, duration: float, prefix: Path, fps: int | None = None) -> None:
    prefix.parent.mkdir(parents=True, exist_ok=True)
    if video.suffix.lower() in IMAGE_EXTS:
        return image_sticker(video, prefix.with_suffix(".webp"))
    webp, ogg = prefix.with_suffix(".webp"), prefix.with_suffix(".ogg")
    prep = prefix.parent / "prep.mkv"
    # 1) Cut + scale + light denoise once into a near-lossless intermediate (denoise helps WebP a lot).
    fps_list = (fps,) if fps else AUTO_FPS
    run("-ss", str(start), "-t", str(duration), "-i", str(video), "-an",
        "-vf", f"fps={fps_list[0]},scale=512:512:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos,"
               "hqdn3d=1.5:1.5:3:3",
        "-c:v", "libx264", "-crf", "10", "-preset", "veryfast", str(prep))
    _, secs = imageio_ffmpeg.count_frames_and_secs(str(prep))
    # 2) Best fps/quality under the phone limit.
    workdir = prefix.parent / "variants"
    workdir.mkdir(exist_ok=True)
    try:
        fps, q, out, phone_ok = pick_encode(prep, workdir, fps_list)
        out.replace(webp)
        size = webp.stat().st_size
        print(f"picked fps={fps} q={q} ({size / 1024:.0f} KB)" + ("" if phone_ok else " DESKTOP ONLY"))
        # sticker.note: a line the bot sends after the sticker.
        if not phone_ok:
            (prefix.parent / "sticker.note").write_text(
                f"⚠️ this clip is {size / 1e6:.1f} MB, over the 1 MB phones can play; "
                "it only works on WhatsApp Web / desktop. Use a shorter range for a phone-friendly one.\n")
    finally:
        for f in workdir.iterdir():
            f.unlink()
        workdir.rmdir()
        prep.unlink()
    (prefix.parent / "sticker.secs").write_text(f"{secs:.2f}\n")
    # Voice-note style audio: mono 48 kHz Opus in OGG. Clips without audio just get no .ogg.
    try:
        run("-ss", str(start), "-t", str(duration), "-i", str(video), "-vn", "-af", "asetpts=N/SR/TB", "-ac", "1",
            "-ar", "48000", "-c:a", "libopus", "-b:a", "48k", str(ogg))
    except subprocess.CalledProcessError:
        ogg.unlink(missing_ok=True)
    if ogg.exists() and ogg.stat().st_size < 1024:  # empty/no audio stream
        ogg.unlink()
    print(f"saved {webp} ({secs:.1f}s)" + (f" and {ogg}" if ogg.exists() else " (no audio)"))


def job(a: argparse.Namespace) -> None:
    duration = max(0.5, min(a.duration, MAX_CLIP_SECONDS))
    a.out.mkdir(parents=True, exist_ok=True)
    if a.url:
        src, start = fetch_media(a.url, a.item, a.start, duration, a.out)
    else:
        src, start = a.input, a.start
    sticker(src, start, duration, a.out / "sticker", max(1, min(a.fps, 30)) if a.fps else None)
    if not (a.out / "sticker.webp").exists():
        raise Reject("could not make a sticker from that clip")


def main() -> int:
    p = argparse.ArgumentParser()
    sub = p.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("download")
    d.add_argument("url")
    d.add_argument("-o", "--out", type=Path, default=Path("media/source.mp4"))
    s = sub.add_parser("sticker")
    s.add_argument("video", type=Path)
    s.add_argument("--start", type=float, default=0)
    s.add_argument("--duration", type=float, default=8)
    s.add_argument("--fps", type=int, help="fixed fps (default: best that fits 1 MB, max 12)")
    s.add_argument("-o", "--out", type=Path, default=Path("media/sticker"))
    j = sub.add_parser("job")
    src = j.add_mutually_exclusive_group(required=True)
    src.add_argument("--input", type=Path)
    src.add_argument("--url")
    j.add_argument("--item", type=int, help="1-based media index within an X / Instagram post")
    j.add_argument("--start", type=float, default=0)
    j.add_argument("--duration", type=float, default=8)
    j.add_argument("--fps", type=int, help="fixed fps (default: best that fits 1 MB, max 12)")
    j.add_argument("--out", type=Path, required=True)
    a = p.parse_args()
    try:
        if a.cmd == "download":
            download(a.url, a.out)
        elif a.cmd == "sticker":
            sticker(a.video, a.start, a.duration, a.out, a.fps)
        else:
            job(a)
    except Reject as e:
        print(str(e), file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
