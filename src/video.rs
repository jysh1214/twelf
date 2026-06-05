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
use std::sync::Arc;
use std::sync::Once;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
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

    /// Seek back to the start and flush decoder state so playback can loop.
    fn seek_to_start(&mut self) -> Result<(), ffmpeg::Error> {
        self.input.seek(0, ..)?;
        self.decoder.flush();
        self.pending.clear();
        self.drained = false;
        Ok(())
    }
}

/// How many decoded frames to buffer ahead; bounds the decoder's lead and the
/// memory it holds for a looping (otherwise unbounded) stream.
const FRAME_BUFFER: usize = 6;
/// Seam spacing used to continue the timeline across a loop when the per-frame
/// gap is unknown.
const FALLBACK_GAP: f64 = 1.0 / 30.0;

struct TimedFrame {
    image: ColorImage,
    /// Monotonically increasing presentation time (seconds) across loops.
    timeline: f64,
}

/// Plays a local video: a worker thread decodes (and loops) into a bounded
/// channel, and `frame` hands the UI the frame due at the current wall-clock
/// time, uploaded as a texture.
pub struct VideoPlayer {
    pub uri: String,
    rx: Receiver<TimedFrame>,
    start: Option<Instant>,
    /// Playback seconds captured when paused; `None` while playing.
    paused_at: Option<f64>,
    next: Option<TimedFrame>,
    texture: Option<egui::TextureHandle>,
}

impl VideoPlayer {
    pub fn open(uri: String, path: PathBuf) -> Self {
        let (tx, rx) = sync_channel(FRAME_BUFFER);
        // The decoder (and its non-Send scaler) is built and used entirely on
        // the worker thread; only the path crosses in and frames cross out.
        thread::spawn(move || match VideoDecoder::open(&path) {
            Ok(mut decoder) => decode_loop(&mut decoder, &tx),
            Err(e) => crate::log!("video open failed for {}: {e}", path.display()),
        });
        Self {
            uri,
            rx,
            start: None,
            paused_at: None,
            next: None,
            texture: None,
        }
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
        let (tx, rx) = sync_channel(FRAME_BUFFER);
        let suffix = Path::new(&uri)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        thread::spawn(move || {
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
                Ok(mut decoder) => decode_loop(&mut decoder, &tx),
                Err(e) => crate::log!("remote video decode failed for {remote_path}: {e}"),
            }
        });
        Self {
            uri,
            rx,
            start: None,
            paused_at: None,
            next: None,
            texture: None,
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused_at.is_some()
    }

    /// Toggle play/pause. Pausing captures the playback position; resuming rebases
    /// the clock so frames continue from there instead of jumping ahead.
    pub fn toggle_pause(&mut self) {
        match self.paused_at.take() {
            Some(elapsed) => self.start = Some(Instant::now() - Duration::from_secs_f64(elapsed)),
            None => {
                self.paused_at = Some(self.start.map_or(0.0, |s| s.elapsed().as_secs_f64()));
            }
        }
    }

    /// Upload and return the frame due now plus when to repaint, or `None` until
    /// the first frame has been decoded.
    pub fn frame(&mut self, ctx: &egui::Context) -> Option<(egui::load::SizedTexture, Duration)> {
        if self.paused_at.is_some() {
            // Hold the current frame; no repaint until the user resumes.
            let handle = self.texture.as_ref()?;
            return Some((egui::load::SizedTexture::from_handle(handle), Duration::from_secs(3600)));
        }
        let options = egui::TextureOptions::LINEAR;
        loop {
            if self.next.is_none() {
                self.next = self.rx.try_recv().ok();
            }
            let due = match (self.start, self.next.as_ref()) {
                (_, None) => break,
                (None, Some(_)) => true, // first frame: show it and start the clock
                (Some(start), Some(frame)) => frame.timeline <= start.elapsed().as_secs_f64(),
            };
            if !due {
                break;
            }
            let frame = self.next.take().unwrap();
            match &mut self.texture {
                Some(handle) => handle.set(frame.image, options),
                None => self.texture = Some(ctx.load_texture(&self.uri, frame.image, options)),
            }
            self.start.get_or_insert_with(Instant::now);
        }
        let handle = self.texture.as_ref()?;
        let delay = match (self.start, self.next.as_ref()) {
            (Some(start), Some(frame)) => {
                Duration::from_secs_f64((frame.timeline - start.elapsed().as_secs_f64()).max(0.0))
            }
            _ => Duration::from_millis(5), // waiting on the decoder; poll again soon
        };
        Some((egui::load::SizedTexture::from_handle(handle), delay))
    }
}

/// Decode frames forever, looping at EOF, pushing each onto a monotonic
/// timeline. Returns when the player is dropped (send fails) or decoding errors.
fn decode_loop(decoder: &mut VideoDecoder, tx: &SyncSender<TimedFrame>) {
    let mut offset = 0.0;
    let mut last_timeline = 0.0;
    let mut prev_pts = 0.0;
    let mut gap = FALLBACK_GAP;
    loop {
        match decoder.next_frame() {
            Ok(Some(frame)) => {
                let delta = frame.pts - prev_pts;
                if delta > 0.0 {
                    gap = delta;
                }
                prev_pts = frame.pts;
                let timeline = offset + frame.pts;
                last_timeline = timeline;
                if tx
                    .send(TimedFrame {
                        image: frame.image,
                        timeline,
                    })
                    .is_err()
                {
                    return; // player dropped
                }
            }
            Ok(None) => {
                offset = last_timeline + gap;
                prev_pts = 0.0;
                if decoder.seek_to_start().is_err() {
                    return;
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
