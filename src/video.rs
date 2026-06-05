use eframe::egui::{self, ColorImage};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::frame::Video;
use russh_sftp::client::SftpSession;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

static FFMPEG_INIT: Once = Once::new();

pub fn is_video(uri: &str) -> bool {
    matches!(
        uri.to_ascii_lowercase().rsplit('.').next(),
        Some(
            "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "wmv" | "flv" | "mpg" | "mpeg" | "ts"
        )
    )
}

/// One decoded video frame as RGBA, with its presentation time in seconds.
pub struct Frame {
    pub image: ColorImage,
    pub pts: f64,
}

/// Pull-based decoder for a single video file: reads packets, decodes the video
/// stream, and scales each frame to RGBA. Frames come out in decode order with a
/// presentation timestamp the player uses to pace playback.
pub struct VideoDecoder {
    input: ffmpeg::format::context::Input,
    decoder: ffmpeg::decoder::Video,
    scaler: Scaler,
    stream_index: usize,
    time_base: f64,
    pending: VecDeque<Frame>,
    drained: bool,
}

impl VideoDecoder {
    pub fn open(path: &Path) -> Result<Self, ffmpeg::Error> {
        FFMPEG_INIT.call_once(|| {
            let _ = ffmpeg::init();
        });
        let input = ffmpeg::format::input(&path)?;
        let stream = input
            .streams()
            .best(Type::Video)
            .ok_or(ffmpeg::Error::StreamNotFound)?;
        let stream_index = stream.index();
        let time_base = f64::from(stream.time_base());
        let decoder = ffmpeg::codec::context::Context::from_parameters(stream.parameters())?
            .decoder()
            .video()?;
        let (width, height) = (decoder.width(), decoder.height());
        let scaler = Scaler::get(
            decoder.format(),
            width,
            height,
            Pixel::RGBA,
            width,
            height,
            Flags::BILINEAR,
        )?;
        Ok(Self {
            input,
            decoder,
            scaler,
            stream_index,
            time_base,
            pending: VecDeque::new(),
            drained: false,
        })
    }

    /// Next decoded frame, or `None` once the stream is exhausted.
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ffmpeg::Error> {
        loop {
            if let Some(frame) = self.pending.pop_front() {
                return Ok(Some(frame));
            }
            if self.drained {
                return Ok(None);
            }
            let mut packet = ffmpeg::Packet::empty();
            match packet.read(&mut self.input) {
                Ok(()) => {
                    if packet.stream() == self.stream_index {
                        self.decoder.send_packet(&packet)?;
                        self.receive_frames()?;
                    }
                }
                Err(ffmpeg::Error::Eof) => {
                    self.decoder.send_eof()?;
                    self.receive_frames()?;
                    self.drained = true;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn receive_frames(&mut self) -> Result<(), ffmpeg::Error> {
        let mut decoded = Video::empty();
        while self.decoder.receive_frame(&mut decoded).is_ok() {
            let mut rgba = Video::empty();
            self.scaler.run(&decoded, &mut rgba)?;
            let pts = decoded.pts().unwrap_or(0) as f64 * self.time_base;
            self.pending.push_back(Frame {
                image: to_color_image(&rgba),
                pts,
            });
        }
        Ok(())
    }

    /// Video duration in seconds, or `0.0` if unknown.
    fn duration(&self) -> f64 {
        let d = self.input.duration();
        if d > 0 { d as f64 / AV_TIME_BASE } else { 0.0 }
    }

    /// Seek to `secs` (snapping to the keyframe at or before it) and flush decoder
    /// state so decoding resumes from there.
    fn seek_to(&mut self, secs: f64) -> Result<(), ffmpeg::Error> {
        let ts = (secs.max(0.0) * AV_TIME_BASE) as i64;
        self.input.seek(ts, ..ts)?;
        self.decoder.flush();
        self.pending.clear();
        self.drained = false;
        Ok(())
    }
}

/// ffmpeg's `AV_TIME_BASE`: durations and seek timestamps are in microseconds.
const AV_TIME_BASE: f64 = 1_000_000.0;

/// How many decoded frames to buffer ahead; bounds the decoder's lead and the
/// memory it holds for a looping (otherwise unbounded) stream.
const FRAME_BUFFER: usize = 6;

struct TimedFrame {
    image: ColorImage,
    /// In-file presentation time in seconds.
    position: f64,
    /// Seek generation; frames decoded before a seek carry an older value.
    generation: u64,
}

/// State shared with the worker: pending seek requests, the current seek
/// generation, and the duration the worker fills in once the file is open.
#[derive(Default)]
struct Shared {
    seek_request: Option<f64>,
    generation: u64,
    duration: f64,
}

/// Plays a video: a worker thread decodes (looping, and honoring seeks) into a
/// bounded channel, and `frame` hands the UI the frame due now, uploaded as a
/// texture. Playback is paced by anchoring a wall-clock to a frame position and
/// re-anchoring on any position discontinuity (loop or seek).
pub struct VideoPlayer {
    pub uri: String,
    rx: Receiver<TimedFrame>,
    shared: Arc<Mutex<Shared>>,
    /// The seek generation the UI expects; frames with an older value are dropped.
    generation: u64,
    /// (wall instant, playback position then); `None` until (re)anchored.
    anchor: Option<(Instant, f64)>,
    paused: bool,
    position: f64,
    next: Option<TimedFrame>,
    texture: Option<egui::TextureHandle>,
}

impl VideoPlayer {
    pub fn open(uri: String, path: PathBuf) -> Self {
        Self::spawn(uri, move |tx, shared| match VideoDecoder::open(&path) {
            Ok(decoder) => decode_loop(decoder, &tx, &shared),
            Err(e) => crate::log!("video open failed for {}: {e}", path.display()),
        })
    }

    /// Like `open`, but for a remote (SFTP) video: the worker thread downloads
    /// the file to a temp path, then decodes it. The temp file is held on the
    /// thread for the player's lifetime and removed when the player is dropped.
    pub fn open_remote(
        uri: String,
        session: Arc<SftpSession>,
        handle: tokio::runtime::Handle,
        remote_path: String,
    ) -> Self {
        let suffix = Path::new(&uri)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        Self::spawn(uri, move |tx, shared| {
            let bytes = match handle.block_on(session.read(remote_path.clone())) {
                Ok(bytes) => bytes,
                Err(e) => {
                    crate::log!("remote video fetch failed for {remote_path}: {e}");
                    return;
                }
            };
            let mut temp = match tempfile::Builder::new().suffix(&suffix).tempfile() {
                Ok(temp) => temp,
                Err(e) => {
                    crate::log!("temp file for {remote_path} failed: {e}");
                    return;
                }
            };
            if let Err(e) = temp.write_all(&bytes) {
                crate::log!("temp write for {remote_path} failed: {e}");
                return;
            }
            // `temp` stays alive (and on disk) until this thread returns.
            match VideoDecoder::open(temp.path()) {
                Ok(decoder) => decode_loop(decoder, &tx, &shared),
                Err(e) => crate::log!("remote video decode failed for {remote_path}: {e}"),
            }
        })
    }

    /// Spawn the worker (whose body produces frames into `tx`) and build the player.
    /// The decoder and its non-Send scaler live entirely on the worker thread.
    fn spawn(
        uri: String,
        body: impl FnOnce(SyncSender<TimedFrame>, Arc<Mutex<Shared>>) + Send + 'static,
    ) -> Self {
        let (tx, rx) = sync_channel(FRAME_BUFFER);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let worker_shared = shared.clone();
        thread::spawn(move || body(tx, worker_shared));
        Self {
            uri,
            rx,
            shared,
            generation: 0,
            anchor: None,
            paused: false,
            position: 0.0,
            next: None,
            texture: None,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    /// Toggle play/pause. Resuming re-anchors the clock to the current position so
    /// playback continues from there instead of jumping ahead.
    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused {
            self.anchor = Some((Instant::now(), self.position));
        }
    }

    pub fn duration(&self) -> f64 {
        self.shared.lock().unwrap().duration
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    /// Request a seek to `secs`. The worker performs it; stale frames are dropped
    /// and pacing re-anchors when the seeked frame arrives.
    pub fn seek(&mut self, secs: f64) {
        {
            let mut shared = self.shared.lock().unwrap();
            shared.generation += 1;
            shared.seek_request = Some(secs);
            self.generation = shared.generation;
        }
        self.next = None;
        self.anchor = None;
        self.position = secs;
    }

    /// Upload and return the frame due now plus when to repaint, or `None` until
    /// the first frame has been decoded.
    pub fn frame(&mut self, ctx: &egui::Context) -> Option<(egui::load::SizedTexture, Duration)> {
        let options = egui::TextureOptions::LINEAR;
        loop {
            if self.next.is_none() {
                self.next = self.rx.try_recv().ok();
            }
            let Some(frame) = self.next.as_ref() else { break };
            if frame.generation != self.generation {
                self.next = None; // stale pre-seek frame
                continue;
            }
            // Re-anchor on the first frame, a seek, or a loop-back to an earlier position.
            let reanchor = match self.anchor {
                None => true,
                Some((_, p0)) => frame.position + 1e-3 < p0,
            };
            // While paused, only land a re-anchoring frame (first/seek/loop), then hold.
            let due = reanchor
                || (!self.paused
                    && matches!(self.anchor, Some((t0, p0)) if frame.position - p0 <= t0.elapsed().as_secs_f64()));
            if !due {
                break;
            }
            let frame = self.next.take().unwrap();
            match &mut self.texture {
                Some(handle) => handle.set(frame.image, options),
                None => self.texture = Some(ctx.load_texture(&self.uri, frame.image, options)),
            }
            self.position = frame.position;
            if reanchor {
                self.anchor = Some((Instant::now(), frame.position));
            }
        }
        let handle = self.texture.as_ref()?;
        let texture = egui::load::SizedTexture::from_handle(handle);
        if self.paused {
            // Poll while still awaiting a seeked frame; otherwise hold without repainting.
            let delay = if self.anchor.is_none() {
                Duration::from_millis(5)
            } else {
                Duration::from_secs(3600)
            };
            return Some((texture, delay));
        }
        let delay = match (self.anchor, self.next.as_ref()) {
            (Some((t0, p0)), Some(frame)) if frame.generation == self.generation => {
                Duration::from_secs_f64(((frame.position - p0) - t0.elapsed().as_secs_f64()).max(0.0))
            }
            _ => Duration::from_millis(5), // waiting on the decoder; poll again soon
        };
        Some((texture, delay))
    }
}

/// Decode frames forever — looping at EOF and honoring seek requests — pushing
/// each onto the channel with its position and seek generation. Returns when the
/// player is dropped (send fails) or decoding errors.
fn decode_loop(mut decoder: VideoDecoder, tx: &SyncSender<TimedFrame>, shared: &Arc<Mutex<Shared>>) {
    shared.lock().unwrap().duration = decoder.duration();
    let mut generation = 0;
    loop {
        // Apply a pending seek before producing the next frame.
        let target = {
            let mut s = shared.lock().unwrap();
            s.seek_request.take().inspect(|_| generation = s.generation)
        };
        if let Some(target) = target
            && decoder.seek_to(target).is_err()
        {
            return;
        }
        match decoder.next_frame() {
            Ok(Some(frame)) => {
                let mut item = TimedFrame {
                    image: frame.image,
                    position: frame.pts,
                    generation,
                };
                loop {
                    match tx.try_send(item) {
                        Ok(()) => break,
                        Err(TrySendError::Full(returned)) => {
                            // Stay responsive to seeks while the channel is full (paused).
                            if shared.lock().unwrap().seek_request.is_some() {
                                break; // drop this now-stale frame; the seek runs next
                            }
                            thread::sleep(Duration::from_millis(5));
                            item = returned;
                        }
                        Err(TrySendError::Disconnected(_)) => return, // player dropped
                    }
                }
            }
            Ok(None) => {
                if decoder.seek_to(0.0).is_err() {
                    return; // loop back to the start
                }
            }
            Err(_) => return,
        }
    }
}

/// Copy a scaled RGBA frame into a `ColorImage`, dropping the row padding ffmpeg
/// adds (stride is usually wider than `width * 4`).
fn to_color_image(frame: &Video) -> ColorImage {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let stride = frame.stride(0);
    let data = frame.data(0);
    let row_bytes = width * 4;
    let mut pixels = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        let start = y * stride;
        pixels.extend_from_slice(&data[start..start + row_bytes]);
    }
    ColorImage::from_rgba_unmultiplied([width, height], &pixels)
}
