//! HTTP Live Streaming (RFC 8216): playlists, segments and the reader that turns
//! them into one elementary audio stream for the existing decoder.
//!
//! The web stream worker fetches the URL; when the answer is a playlist with
//! `#EXT-X-` tags, this reader takes over: it picks an audio-capable variant, keeps a
//! media playlist's window, downloads segments in order (AES-128 encrypted ones too)
//! and hands their audio elementary stream - ADTS AAC or MPEG audio, from a plain
//! `.aac`/`.mp3` segment or demultiplexed out of MPEG-TS - to [`super::decode`], which
//! sniffs it like any other stream. The fragments are joined into one continuous
//! stream: the ID3v2 tag some servers put at the start of a segment is dropped.
//!
//! fMP4 segments (`#EXT-X-MAP`), `SAMPLE-AES` and low-latency HLS are recognised and
//! refused with a message (see [`HlsReader::new`] and [`parse_media`]).

use super::http::{self, Url};
use super::ts;
use super::Shared;
use std::io::{self, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Segments to keep between the live edge and the one played first (0: the last one).
const START_BACK: usize = 1;

/// The largest playlist, segment or key accepted, bytes.
const MAX_PLAYLIST: u64 = 4 << 20;
const MAX_SEGMENT: u64 = 64 << 20;
const MAX_KEY: u64 = 4 << 10;

/// AES keys kept for the segments that follow (a stream may rotate them).
const MAX_KEYS: usize = 8;

/// Whether `text` is an HLS playlist: it has `#EXT-X-` tags.
pub(crate) fn is_hls(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().to_ascii_uppercase().starts_with("#EXT-X-"))
}

/// Whether `text` is a master playlist: it lists variants.
fn is_master(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().to_ascii_uppercase().starts_with("#EXT-X-STREAM-INF"))
}

/// A variant of a master playlist (`#EXT-X-STREAM-INF`) or an audio rendition
/// (`#EXT-X-MEDIA:TYPE=AUDIO`).
#[derive(Debug, Clone)]
struct Variant {
    uri: String,
    bandwidth: u64,
    /// An audio rendition (`#EXT-X-MEDIA`), with its selection attributes.
    rendition: Option<Rendition>,
    /// It names a video codec or a resolution: its audio is muxed with video.
    video: bool,
}

/// The selection attributes of an audio rendition.
#[derive(Debug, Clone)]
struct Rendition {
    default: bool,
    autoselect: bool,
}

/// A master playlist: the variants, in the order of the file.
#[derive(Debug, Default)]
struct Master {
    variants: Vec<Variant>,
}

/// The key of a segment (`#EXT-X-KEY`), if the segments are encrypted.
#[derive(Debug, Clone, PartialEq)]
struct Key {
    uri: String,
    /// The initialisation vector, 16 bytes; absent: the segment's media sequence.
    iv: Option<[u8; 16]>,
}

/// One segment of a media playlist.
#[derive(Debug, Clone)]
struct Segment {
    /// The media sequence number (its position in the live stream).
    seq: u64,
    uri: String,
    duration: f64,
    /// A byte range of the URI (`#EXT-X-BYTERANGE`), inclusive.
    range: Option<(u64, u64)>,
    key: Option<Key>,
    discontinuity: bool,
    /// `#EXT-X-GAP`: the segment is missing by the broadcaster's choice.
    gap: bool,
}

/// A media playlist at one moment.
#[derive(Debug, Default)]
struct Media {
    target_s: f64,
    /// In order, oldest first.
    segments: Vec<Segment>,
    /// `#EXT-X-ENDLIST`: the stream is complete.
    ended: bool,
}

/// The HLS reader: a `Read` of the audio elementary stream assembled from segments.
pub(crate) struct HlsReader {
    shared: Arc<Shared>,
    req: http::Request,
    /// The URL the user gave (the master): fetched again when a session expires.
    entry: Url,
    /// The chosen variant's URL (the media playlist to fetch).
    variant: Url,
    /// Where the media playlist was last served (segment URIs resolve against it).
    media_url: Url,
    bitrate: Option<u32>,
    media: Media,
    /// The media sequence number of the segment to fetch next.
    next_seq: u64,
    /// Elementary stream bytes ready to be read.
    pending: Vec<u8>,
    pos: usize,
    ended: bool,
    /// AES keys by resolved URL.
    keys: Vec<(String, [u8; 16])>,
    /// When the playlist last moved (a segment appeared or a session changed).
    last_progress: Instant,
    /// Consecutive failures of a segment fetch or a playlist refresh.
    failures: usize,
}

impl HlsReader {
    /// Pick a variant of the master `text` (or take `text` as the media playlist),
    /// fetch the media playlist and position at the live edge.
    pub(crate) fn new(text: &str, entry: Url, mut req: http::Request, shared: Arc<Shared>) -> Result<Self, String> {
        // The segments are audio files: ICY metadata would be interleaved into them.
        req.icy_metadata = false;
        let mut variant = entry.clone();
        let mut bitrate = None;
        if is_master(text) {
            let master = parse_master(text)?;
            let chosen = choose(&master).ok_or("the master playlist lists no audio stream")?;
            variant = entry.join(&chosen.uri).map_err(|e| format!("variant {}: {e}", chosen.uri))?;
            bitrate = bitrate_of(&chosen);
        }
        let mut reader = HlsReader {
            shared,
            req,
            entry,
            variant: variant.clone(),
            media_url: variant,
            bitrate,
            media: Media::default(),
            next_seq: 0,
            pending: Vec::new(),
            pos: 0,
            ended: false,
            keys: Vec::new(),
            last_progress: Instant::now(),
            failures: 0,
        };
        let (media, _) = reader.fetch_media()?;
        reader.next_seq = start_seq(&media);
        let complete = media.ended;
        reader.media = media;
        let kbit = reader.bitrate.map(|b| format!(" ({b} kbit/s)")).unwrap_or_default();
        reader.shared.log(format!("HLS: {}{kbit}{}", reader.variant, if complete { ", complete" } else { "" }));
        Ok(reader)
    }

    /// The variant being played (the media playlist's URL).
    pub(crate) fn variant_url(&self) -> &Url {
        &self.variant
    }

    /// The variant's declared bit rate, kbit/s (only an audio-only variant has one).
    pub(crate) fn bitrate(&self) -> Option<u32> {
        self.bitrate
    }

    /// Whether the playlist is complete (`#EXT-X-ENDLIST`): a file, not a live stream.
    pub(crate) fn is_a_file(&self) -> bool {
        self.media.ended
    }

    /// Fetch the media playlist, re-resolving the variant through the entry URL when
    /// the session has expired (the server answers 403/404). True when the variant had
    /// to be re-resolved: its media sequence then has nothing to do with the old one.
    fn fetch_media(&mut self) -> Result<(Media, bool), String> {
        match self.get_text(&self.variant.clone()) {
            Ok((text, url)) => {
                if is_master(&text) {
                    return Err("the media playlist is a master playlist".into());
                }
                let media = parse_media(&text)?;
                self.media_url = url;
                Ok((media, false))
            }
            Err(e) => {
                // A session (the `hlssid` in the URL) or the variant may have changed.
                let (text, _) = self
                    .get_text(&self.entry.clone())
                    .map_err(|e2| format!("{e}; and the entry URL: {e2}"))?;
                let Some((variant, bitrate)) = variant_url(&text, &self.entry) else {
                    return Err(format!("{e}; the entry URL no longer names an audio variant"));
                };
                self.variant = variant;
                self.bitrate = bitrate;
                self.keys.clear();
                {
                    let mut inner = self.shared.lock();
                    inner.status.stream_url = Some(self.variant.to_string());
                    inner.status.bitrate = bitrate;
                }
                self.shared.log(format!("HLS: the session changed, now {}", self.variant));
                let (text, url) = self.get_text(&self.variant.clone())?;
                if is_master(&text) {
                    return Err("the media playlist is a master playlist".into());
                }
                let media = parse_media(&text)?;
                self.media_url = url;
                Ok((media, true))
            }
        }
    }

    /// Refetch the media playlist after waiting for it to advance.
    fn refresh(&mut self) -> Result<(), String> {
        let before = window(&self.media);
        let (media, changed) = self.fetch_media()?;
        if changed {
            // The new session numbers its segments anew: start at its live edge.
            self.next_seq = start_seq(&media);
        }
        if changed || window(&media) != before {
            self.last_progress = Instant::now();
        }
        self.media = media;
        Ok(())
    }

    fn get_text(&self, url: &Url) -> Result<(String, Url), String> {
        let resp = http::get(url, &self.req).map_err(|e| e.to_string())?;
        let answered = resp.url.clone();
        let mut body = Vec::new();
        resp.body
            .take(MAX_PLAYLIST + 1)
            .read_to_end(&mut body)
            .map_err(|e| format!("{answered}: {}", http::describe_io(&e)))?;
        if body.len() as u64 > MAX_PLAYLIST {
            return Err(format!("{answered}: the playlist is larger than {MAX_PLAYLIST} bytes"));
        }
        Ok((http::text(&body), answered))
    }

    fn get_bytes(&self, url: &Url, range: Option<(u64, u64)>) -> Result<Vec<u8>, String> {
        let mut req = self.req.clone();
        req.range = range;
        let resp = http::get(url, &req).map_err(|e| e.to_string())?;
        if let Some((start, end)) = range
            && resp.status != 206
        {
            // A 200 is only the segment when it is the first range of the whole file.
            let asked = end.saturating_sub(start) + 1;
            let answered = resp.header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
            if start != 0 || answered != Some(asked) {
                return Err(format!("{url}: the server ignored the byte range (HTTP {})", resp.status));
            }
        }
        let answered = resp.url.clone();
        let mut body = Vec::new();
        resp.body
            .take(MAX_SEGMENT + 1)
            .read_to_end(&mut body)
            .map_err(|e| format!("{answered}: {}", http::describe_io(&e)))?;
        if body.len() as u64 > MAX_SEGMENT {
            return Err(format!("{answered}: the segment is larger than {MAX_SEGMENT} bytes"));
        }
        Ok(body)
    }

    /// The AES key of `key`, fetched once per URL.
    fn key(&mut self, key: &Key, seq: u64) -> Result<([u8; 16], [u8; 16]), String> {
        let url = self.media_url.join(&key.uri).map_err(|e| format!("key {}: {e}", key.uri))?;
        let id = url.to_string();
        let bytes = match self.keys.iter().find(|(u, _)| *u == id) {
            Some((_, b)) => *b,
            None => {
                let body = self.get_bytes(&url, None)?;
                if body.len() as u64 > MAX_KEY {
                    return Err(format!("{url}: the key is larger than {MAX_KEY} bytes"));
                }
                let b: [u8; 16] = body
                    .try_into()
                    .map_err(|b: Vec<u8>| format!("{url}: the key is {} bytes, not 16", b.len()))?;
                self.keys.push((id, b));
                if self.keys.len() > MAX_KEYS {
                    self.keys.remove(0);
                }
                b
            }
        };
        let iv = key.iv.unwrap_or_else(|| {
            let mut iv = [0u8; 16];
            iv[8..].copy_from_slice(&seq.to_be_bytes());
            iv
        });
        Ok((bytes, iv))
    }

    /// Download a segment and return its audio elementary stream.
    fn load_segment(&mut self, seg: &Segment) -> Result<Vec<u8>, String> {
        let url = self.media_url.join(&seg.uri).map_err(|e| format!("segment {}: {e}", seg.uri))?;
        let mut bytes = self.get_bytes(&url, seg.range)?;
        if let Some(key) = seg.key.clone() {
            let (key, iv) = self.key(&key, seg.seq)?;
            decrypt(&key, &iv, &mut bytes)?;
        }
        // Some servers put an ID3v2 tag (the Apple transport stream timestamp) at the
        // start of every segment; it is not audio.
        while let Some(len) = super::framing::id3v2_len(&bytes) {
            if len >= bytes.len() {
                bytes.clear();
                break;
            }
            bytes.drain(..len);
        }
        if bytes.is_empty() {
            return Ok(bytes);
        }
        if looks_like_fmp4(&bytes) {
            return Err(format!("{url}: fMP4 segments (EXT-X-MAP) are not supported yet"));
        }
        if looks_like_ts(&bytes) {
            return ts::audio_es(&bytes).map_err(|e| format!("{url}: {e}"));
        }
        Ok(bytes)
    }

    /// Wait for the next segment to appear in the playlist (live), or finish (VOD).
    fn fill(&mut self) -> io::Result<()> {
        loop {
            if self.shared.stopped() {
                return Err(http::stop_error());
            }
            if let Some(seg) = self.media.segments.iter().find(|s| s.seq == self.next_seq).cloned() {
                if seg.discontinuity {
                    self.shared.log(format!("HLS: a discontinuity before segment {}", seg.seq));
                }
                if seg.gap {
                    self.shared.log(format!("HLS: segment {} is a gap ({:.1} s), skipped", seg.seq, seg.duration));
                    self.next_seq += 1;
                    continue;
                }
                match self.load_segment(&seg) {
                    Ok(bytes) => {
                        self.next_seq += 1;
                        self.failures = 0;
                        if !bytes.is_empty() {
                            self.pending.extend_from_slice(&bytes);
                            return Ok(());
                        }
                        continue;
                    }
                    Err(e) => {
                        self.failures += 1;
                        self.shared.log(format!("HLS: {e}"));
                        // The playlist may have moved past the segment (a normal race at
                        // the live edge): then it is skipped, not retried.
                        if let Err(e) = self.refresh() {
                            self.shared.log(format!("HLS: {e}"));
                        }
                        if self.media.segments.first().is_none_or(|s| s.seq > seg.seq) {
                            self.shared.log(format!("HLS: segment {} is gone, skipped", seg.seq));
                            self.next_seq += 1;
                            self.failures = 0;
                            continue;
                        }
                        if self.failures >= 3 {
                            return Err(io::Error::other(e));
                        }
                        self.sleep(Duration::from_millis(500))?;
                        continue;
                    }
                }
            }
            let (first, last) = (
                self.media.segments.first().map(|s| s.seq),
                self.media.segments.last().map(|s| s.seq),
            );
            let (Some(first), Some(last)) = (first, last) else {
                // An empty playlist: wait for the broadcaster to add the first segment.
                self.wait_and_refresh()?;
                continue;
            };
            if self.next_seq < first {
                // The window moved past us: skip to the oldest segment still there.
                self.shared.log(format!("HLS: skipped {} segment(s); the playlist moved on", first - self.next_seq));
                self.next_seq = first;
                continue;
            }
            if self.next_seq > last + 1 {
                // The media sequence went back: the broadcaster restarted the stream.
                self.shared.log(format!(
                    "HLS: the media sequence went back ({} is past {}), starting at the live edge",
                    self.next_seq, last
                ));
                self.next_seq = start_seq(&self.media);
                self.keys.clear();
                continue;
            }
            // A hole in the window (no segment with this number): take the next one.
            if let Some(next) = self.media.segments.iter().map(|s| s.seq).find(|&s| s >= self.next_seq) {
                self.shared.log(format!("HLS: segment {} is missing, skipped to {next}", self.next_seq));
                self.next_seq = next;
                continue;
            }
            if self.media.ended {
                self.ended = true;
                return Ok(());
            }
            // Live: wait a little, then look at the playlist again.
            self.wait_and_refresh()?;
        }
    }

    /// Wait for the playlist to advance and refetch it; errors are retried.
    fn wait_and_refresh(&mut self) -> io::Result<()> {
        let limit = self.stall_limit();
        if self.last_progress.elapsed() > limit {
            return Err(io::Error::other(format!(
                "the playlist has not advanced for {:.0} s",
                limit.as_secs_f64()
            )));
        }
        self.sleep(self.reload_wait())?;
        if let Err(e) = self.refresh() {
            self.failures += 1;
            self.shared.log(format!("HLS: {e}"));
            if self.failures >= 3 {
                return Err(io::Error::other(e));
            }
            self.sleep(Duration::from_millis(500))?;
        }
        Ok(())
    }

    /// The playlist's target duration, s (6 when it does not say).
    fn target_s(&self) -> f64 {
        if self.media.target_s > 0.0 { self.media.target_s } else { 6.0 }
    }

    /// How long a live playlist may stand still before the stream counts as stalled.
    fn stall_limit(&self) -> Duration {
        Duration::from_secs_f64((self.target_s() * 5.0).max(30.0))
    }

    /// How long to wait before asking for the playlist again (half the target, 0.2-3 s).
    fn reload_wait(&self) -> Duration {
        Duration::from_secs_f64((self.target_s() / 2.0).clamp(0.2, 3.0))
    }

    fn sleep(&self, at_least: Duration) -> io::Result<()> {
        let until = Instant::now() + at_least;
        loop {
            if self.shared.stopped() {
                return Err(http::stop_error());
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.pending.len() {
            self.pending.clear();
            self.pos = 0;
            if self.ended {
                return Ok(0);
            }
            self.fill()?;
        }
        let n = buf.len().min(self.pending.len() - self.pos);
        buf[..n].copy_from_slice(&self.pending[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// Whether the bytes are a fragmented MP4 segment (a box size, then an fMP4 box type).
fn looks_like_fmp4(b: &[u8]) -> bool {
    b.len() >= 8
        && matches!(&b[4..8], b"styp" | b"moof" | b"ftyp" | b"sidx" | b"moov")
        && b[..4].iter().any(|&x| x != 0)
}

/// Whether the bytes are MPEG-TS packets (0x47 every 188 bytes).
fn looks_like_ts(b: &[u8]) -> bool {
    b.len() >= 188 && b[0] == 0x47 && (b.len() < 377 || b[188] == 0x47)
}

/// What the playlist says about its window: it advances when this changes.
fn window(media: &Media) -> (Option<u64>, Option<u64>, bool) {
    (media.segments.first().map(|s| s.seq), media.segments.last().map(|s| s.seq), media.ended)
}

/// The media sequence number to start at: the live edge (a segment or two back), or
/// the beginning of a complete playlist.
fn start_seq(media: &Media) -> u64 {
    let Some(last) = media.segments.last().map(|s| s.seq) else { return 0 };
    if media.ended {
        return media.segments.first().map_or(0, |s| s.seq);
    }
    last.saturating_sub(START_BACK as u64)
}

/// The variant to play: an audio rendition (the default one), else an audio-only
/// variant with the best bit rate (its quality is what goes on the air), else the
/// variant with video that costs the least (its video is thrown away).
fn choose(master: &Master) -> Option<Variant> {
    let renditions: Vec<&Variant> = master.variants.iter().filter(|v| v.rendition.is_some()).collect();
    let pick = renditions
        .iter()
        .find(|v| v.rendition.as_ref().is_some_and(|r| r.default))
        .or_else(|| renditions.iter().find(|v| v.rendition.as_ref().is_some_and(|r| r.autoselect)))
        .or_else(|| renditions.first());
    if let Some(v) = pick {
        return Some((*v).clone());
    }
    if let Some(v) = master.variants.iter().filter(|v| !v.video).max_by_key(|v| v.bandwidth) {
        return Some(v.clone());
    }
    master.variants.iter().min_by_key(|v| v.bandwidth).cloned()
}

/// The bit rate to show for `v`, kbit/s: only an audio-only stream variant declares a
/// meaningful one (a rendition has no BANDWIDTH, a muxed variant's is mostly video).
fn bitrate_of(v: &Variant) -> Option<u32> {
    (v.rendition.is_none() && !v.video && v.bandwidth > 0).then_some((v.bandwidth / 1000) as u32)
}

/// The variant URL of a master `text` and its bit rate (after a session change).
fn variant_url(text: &str, entry: &Url) -> Option<(Url, Option<u32>)> {
    let master = parse_master(text).ok()?;
    let chosen = choose(&master)?;
    let url = entry.join(&chosen.uri).ok()?;
    Some((url, bitrate_of(&chosen)))
}

/// Parse a master playlist.
fn parse_master(text: &str) -> Result<Master, String> {
    let mut master = Master::default();
    let mut pending: Option<Variant> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = attrs(rest);
            let codecs = value(&attrs, "CODECS").unwrap_or_default().to_ascii_lowercase();
            let video = ["avc1", "avc3", "hvc1", "hev1", "dvh1", "dvhe", "av01", "vp09", "vp8"]
                .iter()
                .any(|c| codecs.contains(c))
                || value(&attrs, "RESOLUTION").is_some();
            pending = Some(Variant {
                uri: String::new(),
                bandwidth: number(&attrs, "BANDWIDTH").unwrap_or(0),
                rendition: None,
                video,
            });
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attrs(rest);
            let is_audio = value(&attrs, "TYPE").is_some_and(|t| t.eq_ignore_ascii_case("AUDIO"));
            if is_audio && let Some(uri) = value(&attrs, "URI") {
                let yes = |name: &str| value(&attrs, name).is_some_and(|v| v.eq_ignore_ascii_case("YES"));
                master.variants.push(Variant {
                    uri,
                    bandwidth: 0,
                    rendition: Some(Rendition { default: yes("DEFAULT"), autoselect: yes("AUTOSELECT") }),
                    video: false,
                });
            }
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if let Some(mut v) = pending.take() {
            v.uri = line.to_string();
            master.variants.push(v);
        }
    }
    Ok(master)
}

/// Parse a media playlist.
fn parse_media(text: &str) -> Result<Media, String> {
    let mut media = Media { target_s: 0.0, segments: Vec::new(), ended: false };
    let mut sequence = 0u64;
    let mut duration: Option<f64> = None;
    let mut range: Option<(u64, u64)> = None;
    let mut key: Option<Key> = None;
    let mut discontinuity = false;
    let mut gap = false;
    let mut last_range: Option<(String, u64)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            media.target_s = rest.trim().parse().unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let attrs = attrs(rest);
            let method = value(&attrs, "METHOD").unwrap_or_default();
            if method.eq_ignore_ascii_case("NONE") {
                key = None;
            } else if method.eq_ignore_ascii_case("AES-128") {
                let uri = value(&attrs, "URI").ok_or("an AES-128 key without a URI")?;
                let iv = match value(&attrs, "IV") {
                    Some(v) => Some(parse_iv(&v)?),
                    None => None,
                };
                key = Some(Key { uri, iv });
            } else {
                return Err(format!("the segments use {method} encryption, which is not supported (AES-128 only)"));
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let _ = rest;
            return Err("the segments are fragmented MP4 (EXT-X-MAP), which is not supported yet".into());
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            let length = |s: &str| s.parse::<u64>().map_err(|_| format!("byte range \"{rest}\""));
            let (len, offset) = match rest.split_once('@') {
                Some((l, o)) => (length(l)?, Some(length(o)?)),
                None => (length(rest)?, None),
            };
            let start = offset.or_else(|| last_range.as_ref().map(|(_, end)| *end)).unwrap_or(0);
            range = Some((start, start + len.saturating_sub(1)));
            last_range = Some((String::new(), start + len));
        } else if line.eq_ignore_ascii_case("#EXT-X-DISCONTINUITY") {
            discontinuity = true;
        } else if line.eq_ignore_ascii_case("#EXT-X-GAP") {
            gap = true;
        } else if line.eq_ignore_ascii_case("#EXT-X-ENDLIST") {
            media.ended = true;
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = Some(rest.split(',').next().unwrap_or_default().trim().parse().unwrap_or(0.0));
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if let Some(d) = duration.take() {
            let seq = sequence + media.segments.len() as u64;
            if let Some((_, end)) = last_range.take() {
                last_range = Some((line.to_string(), end));
            }
            media.segments.push(Segment {
                seq,
                uri: line.to_string(),
                duration: d,
                range: range.take(),
                key: key.clone(),
                discontinuity,
                gap,
            });
            discontinuity = false;
            gap = false;
        }
    }
    if media.segments.is_empty() && !media.ended {
        return Err("the media playlist lists no segments".into());
    }
    Ok(media)
}

/// `IV=0x…` (32 hex digits) as 16 bytes.
fn parse_iv(v: &str) -> Result<[u8; 16], String> {
    let hex = v.trim().trim_start_matches("0x").trim_start_matches("0X");
    let bytes = hex.as_bytes();
    let nibble = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    // Bytes, not characters: a playlist from the network must not panic the worker.
    if bytes.len() != 32 || !bytes.iter().all(|b| nibble(*b).is_some()) {
        return Err(format!("the IV \"{v}\" is not 32 hex digits"));
    }
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        let (hi, lo) = (nibble(bytes[2 * i]), nibble(bytes[2 * i + 1]));
        *byte = (hi.expect("checked above") << 4) | lo.expect("checked above");
    }
    Ok(iv)
}

/// `NAME=value` pairs of an attribute list (quoted values may contain commas).
fn attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(eq) = rest.find('=') {
        let name = rest[..eq].trim().trim_start_matches(',').to_string();
        rest = &rest[eq + 1..];
        let (value, after) = if let Some(stripped) = rest.strip_prefix('"') {
            match stripped.find('"') {
                Some(end) => (stripped[..end].to_string(), &stripped[end + 1..]),
                None => (stripped.to_string(), ""),
            }
        } else {
            match rest.find(',') {
                Some(end) => (rest[..end].to_string(), &rest[end..]),
                None => (rest.to_string(), ""),
            }
        };
        out.push((name, value));
        rest = after.trim_start_matches(',');
    }
    out
}

fn value(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone())
}

fn number(attrs: &[(String, String)], name: &str) -> Option<u64> {
    value(attrs, name).and_then(|v| v.trim().parse().ok())
}

/// AES-128-CBC decryption and PKCS#7 unpadding (RFC 8216 5.2).
fn decrypt(key: &[u8; 16], iv: &[u8; 16], data: &mut Vec<u8>) -> Result<(), String> {
    use aes::cipher::{Array, BlockCipherDecrypt, KeyInit};
    if data.is_empty() {
        return Ok(());
    }
    if !data.len().is_multiple_of(16) {
        return Err(format!("the encrypted segment is {} bytes, not whole AES blocks", data.len()));
    }
    let cipher = aes::Aes128::new(&Array::from(*key));
    let mut previous = *iv;
    let (blocks, _) = data.as_chunks_mut::<16>();
    for chunk in blocks {
        let mut ciphertext = [0u8; 16];
        ciphertext.copy_from_slice(chunk);
        let mut block = Array::from(ciphertext);
        cipher.decrypt_block(&mut block);
        let plain: [u8; 16] = block.into();
        for (i, byte) in chunk.iter_mut().enumerate() {
            *byte = plain[i] ^ previous[i];
        }
        previous = ciphertext;
    }
    let pad = usize::from(*data.last().unwrap_or(&0));
    if (1..=16).contains(&pad) && data.len() >= pad && data[data.len() - pad..].iter().all(|&b| usize::from(b) == pad) {
        data.truncate(data.len() - pad);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=113945,AVERAGE-BANDWIDTH=112190,CODECS=\"mp4a.40.2\"
playlist.m3u8?hlssid=abc
";

    const MEDIA: &str = "\
#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-MEDIA-SEQUENCE:41
#EXT-X-DISCONTINUITY
#EXTINF:0.185760,
l1_aaa.aac
#EXTINF:5.990748,
l0_bbb.aac
#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090A0B0C0D0E0F
#EXT-X-BYTERANGE:1024@0
#EXTINF:5.990748,
l0_ccc.aac
#EXTINF:5.990748,
l0_ddd.aac
";

    #[test]
    fn a_master_lists_variants_with_their_query() {
        assert!(is_hls(MASTER) && is_master(MASTER));
        let master = parse_master(MASTER).unwrap();
        assert_eq!(master.variants.len(), 1);
        assert_eq!(master.variants[0].uri, "playlist.m3u8?hlssid=abc");
        assert!(!master.variants[0].video && master.variants[0].rendition.is_none());
        let entry = Url::parse("https://example.com/rbc/112/playlist.m3u8").unwrap();
        let chosen = choose(&master).unwrap();
        assert_eq!(entry.join(&chosen.uri).unwrap().to_string(), "https://example.com/rbc/112/playlist.m3u8?hlssid=abc");
        assert_eq!(bitrate_of(&chosen), Some(113));
    }

    #[test]
    fn a_video_variant_is_not_taken_for_the_radio() {
        // A video master: the audio-only variant wins, and a muxed one is the last
        // resort (the cheapest: its video is thrown away).
        let text = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.4d401f,mp4a.40.2\"
high.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\"
audio.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=192000,CODECS=\"mp4a.40.2\"
better.m3u8
";
        let master = parse_master(text).unwrap();
        let chosen = choose(&master).unwrap();
        assert_eq!(chosen.uri, "better.m3u8", "the best audio-only variant");
        assert_eq!(bitrate_of(&chosen), Some(192));

        let muxed = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.4d401f,mp4a.40.2\"
high.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"
low.m3u8
";
        let chosen = choose(&parse_master(muxed).unwrap()).unwrap();
        assert_eq!(chosen.uri, "low.m3u8", "the least video traffic");
        assert_eq!(bitrate_of(&chosen), None, "a muxed bit rate is mostly video");
    }

    #[test]
    fn a_default_audio_rendition_wins() {
        let text = "\
#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"English\",DEFAULT=YES,URI=\"en.m3u8\"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"German\",AUTOSELECT=YES,URI=\"de.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=128000,CODECS=\"mp4a.40.2\",AUDIO=\"a\"
stream.m3u8
";
        let master = parse_master(text).unwrap();
        let chosen = choose(&master).unwrap();
        assert_eq!(chosen.uri, "en.m3u8");
        assert!(chosen.rendition.as_ref().is_some_and(|r| r.default));
        assert_eq!(bitrate_of(&chosen), None, "a rendition declares no bit rate");
    }

    #[test]
    fn a_broken_iv_is_an_error_not_a_panic() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0x\u{20ac}000102030405060708090A0B0C0D0E\n#EXTINF:6,\na.aac\n";
        let e = parse_media(text).unwrap_err();
        assert!(e.contains("not 32 hex digits"), "{e}");
        assert_eq!(parse_iv("0x000102030405060708090A0B0C0D0E0F").unwrap()[..4], [0, 1, 2, 3]);
    }

    #[test]
    fn a_media_playlist_names_segments_and_keys() {
        assert!(is_hls(MEDIA));
        let media = parse_media(MEDIA).unwrap();
        assert_eq!(media.target_s, 6.0);
        assert_eq!(media.segments.len(), 4);
        assert_eq!(media.segments[0].seq, 41);
        assert!(media.segments[0].discontinuity);
        assert_eq!(media.segments[1].uri, "l0_bbb.aac");
        assert!(media.segments[1].key.is_none(), "the key comes after it");
        let key = media.segments[2].key.clone().unwrap();
        assert_eq!(key.uri, "key.bin");
        assert_eq!(key.iv.unwrap()[..4], [0, 1, 2, 3]);
        assert_eq!(media.segments[2].range, Some((0, 1023)));
        assert!(!media.ended);
    }

    #[test]
    fn a_live_playlist_starts_near_its_end() {
        let media = parse_media(MEDIA).unwrap();
        assert_eq!(start_seq(&media), 43, "the second-to-last of 41..44");
        let mut vod = parse_media(&format!("{MEDIA}#EXT-X-ENDLIST\n")).unwrap();
        vod.ended = true;
        assert_eq!(start_seq(&vod), 41, "a complete playlist plays from the start");
    }

    #[test]
    fn sample_aes_is_refused() {
        let text = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"k\"\n#EXTINF:6,\na.ts\n";
        let e = parse_media(text).unwrap_err();
        assert!(e.contains("SAMPLE-AES"), "{e}");
    }

    #[test]
    fn fmp4_is_refused_by_name() {
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6,\na.m4s\n";
        let e = parse_media(text).unwrap_err();
        assert!(e.contains("fragmented MP4"), "{e}");
    }

    #[test]
    fn aes_128_cbc_decrypts_a_known_vector() {
        // NIST SP 800-38A, F.2.2 CBC-AES128.Decrypt, the first block.
        let key: [u8; 16] = [0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf, 0x4f, 0x3c];
        let iv: [u8; 16] = [0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
        let mut data = vec![
            0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9, 0x19, 0x7d,
            0x50, 0x86, 0xcb, 0x9b, 0x50, 0x72, 0x19, 0xee, 0x95, 0xdb, 0x11, 0x3a, 0x91, 0x76, 0x78, 0xb2,
        ];
        decrypt(&key, &iv, &mut data).unwrap();
        // The NIST plaintext, with the padding our unpad removes (none here: 0xb2 is
        // not a valid padding byte for an unpadded vector, so it stays).
        let mut plain = vec![
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93, 0x17, 0x2a,
            0xae, 0x2d, 0x8a, 0x57, 0x1e, 0x03, 0xac, 0x9c, 0x9e, 0xb7, 0x6f, 0xac, 0x45, 0xaf, 0x8e, 0x51,
        ];
        assert_eq!(data, plain);
        // And a padded segment: encrypt a block, then decrypt it back.
        plain = plain[..20].to_vec();
        let pad = 12u8;
        plain.extend(std::iter::repeat_n(pad, pad.into()));
        let mut encrypted = plain.clone();
        {
            use aes::cipher::{Array, BlockCipherEncrypt, KeyInit};
            let cipher = aes::Aes128::new(&Array::from([0x42; 16]));
            let mut previous = [0u8; 16];
            let (blocks, _) = encrypted.as_chunks_mut::<16>();
            for chunk in blocks {
                let mut xored = [0u8; 16];
                for (i, x) in xored.iter_mut().enumerate() {
                    *x = chunk[i] ^ previous[i];
                }
                let mut block = Array::from(xored);
                cipher.encrypt_block(&mut block);
                let out: [u8; 16] = block.into();
                chunk.copy_from_slice(&out);
                previous = out;
            }
        }
        decrypt(&[0x42; 16], &[0u8; 16], &mut encrypted).unwrap();
        assert_eq!(encrypted, plain[..20].to_vec());
    }
}
