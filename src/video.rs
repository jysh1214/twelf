use eframe::egui::{self, ColorImage};
use ffmpeg_next as ffmpeg;
use ffmpeg::format::Pixel;
use ffmpeg::media::Type;
use ffmpeg::software::scaling::{context::Context as Scaler, flag::Flags};
use ffmpeg::frame::Video;
use russh_sftp::client::SftpSession;
use russh_sftp::client::fs::File;
use std::collections::VecDeque;
use std::io::SeekFrom;
use std::os::raw::{c_int, c_void};
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

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

/// Consecutive `send_packet` rejections tolerated before decoding is declared
/// dead: isolated corruption is skipped, a stream that no longer decodes ends
/// (reported) instead of looping silently.
const MAX_DECODE_ERRORS: u32 = 100;

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
    /// Consecutive rejected packets; decoding is declared dead past the cap.
    decode_errors: u32,
    /// Backing for a custom-AVIO (remote) input; `None` for a path-based one.
    /// Declared last so it is freed only after `input` has been closed.
    _avio: Option<AvioGuard>,
}

impl VideoDecoder {
    pub fn open(path: &Path) -> Result<Self, ffmpeg::Error> {
        FFMPEG_INIT.call_once(|| {
            let _ = ffmpeg::init();
        });
        let input = ffmpeg::format::input(&path)?;
        Self::from_input(None, input)
    }

    /// Open a decoder that reads `file` (an SFTP file of `size` bytes) through a
    /// custom AVIO doing on-demand range reads, so the demuxer can read any
    /// offset — including a trailing MP4 index — without a whole-file download.
    fn open_sftp(
        handle: tokio::runtime::Handle,
        file: File,
        size: u64,
    ) -> Result<Self, ffmpeg::Error> {
        FFMPEG_INIT.call_once(|| {
            let _ = ffmpeg::init();
        });
        unsafe {
            let reader = Box::into_raw(Box::new(SftpReader {
                handle,
                file,
                pos: 0,
                size,
            }));
            let buffer_size: c_int = 1 << 16;
            let buffer = ffmpeg::ffi::av_malloc(buffer_size as usize) as *mut u8;
            if buffer.is_null() {
                drop(Box::from_raw(reader));
                return Err(ffmpeg::Error::Unknown);
            }
            let avio = ffmpeg::ffi::avio_alloc_context(
                buffer,
                buffer_size,
                0,
                reader as *mut c_void,
                Some(avio_read),
                None,
                Some(avio_seek),
            );
            if avio.is_null() {
                ffmpeg::ffi::av_free(buffer as *mut c_void);
                drop(Box::from_raw(reader));
                return Err(ffmpeg::Error::Unknown);
            }
            let mut ctx = ffmpeg::ffi::avformat_alloc_context();
            if ctx.is_null() {
                free_avio(avio, reader);
                return Err(ffmpeg::Error::Unknown);
            }
            (*ctx).pb = avio;
            (*ctx).flags |= ffmpeg::ffi::AVFMT_FLAG_CUSTOM_IO as c_int;
            let ret =
                ffmpeg::ffi::avformat_open_input(&mut ctx, ptr::null(), ptr::null(), ptr::null_mut());
            if ret < 0 {
                // open_input frees `ctx` on failure but leaves our custom pb.
                free_avio(avio, reader);
                return Err(ffmpeg::Error::from(ret));
            }
            if ffmpeg::ffi::avformat_find_stream_info(ctx, ptr::null_mut()) < 0 {
                ffmpeg::ffi::avformat_close_input(&mut ctx);
                free_avio(avio, reader);
                return Err(ffmpeg::Error::Unknown);
            }
            let input = ffmpeg::format::context::Input::wrap(ctx);
            Self::from_input(Some(AvioGuard { avio, opaque: reader }), input)
        }
    }

    /// `avio` is declared before `input` on purpose: parameters drop in reverse
    /// declaration order, so the early `?` returns below close the format context
    /// (whose `pb` points into the AVIO) before the guard frees it — the same
    /// invariant the struct's field order keeps on the success path.
    fn from_input(
        avio: Option<AvioGuard>,
        input: ffmpeg::format::context::Input,
    ) -> Result<Self, ffmpeg::Error> {
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
            decode_errors: 0,
            _avio: avio,
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
                        match self.decoder.send_packet(&packet) {
                            Ok(()) => {
                                self.decode_errors = 0;
                                self.receive_frames()?;
                            }
                            // Skip packets the decoder rejects (isolated corruption)
                            // until nothing decodes anymore.
                            Err(e) => {
                                self.decode_errors += 1;
                                if self.decode_errors > MAX_DECODE_ERRORS {
                                    return Err(e);
                                }
                            }
                        }
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
            let pts = decoded.pts().or_else(|| decoded.timestamp()).unwrap_or(0) as f64 * self.time_base;
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
        self.decode_errors = 0;
        Ok(())
    }
}

/// Opaque behind a remote input's custom AVIO: the runtime handle used to drive
/// blocking SFTP reads, the open remote file, the current read position, and the
/// total size (for `AVSEEK_SIZE`/`SEEK_END`).
struct SftpReader {
    handle: tokio::runtime::Handle,
    file: File,
    pos: u64,
    size: u64,
}

/// Owns a remote input's `AVIOContext` and its `SftpReader`, freeing both when
/// the decoder is dropped — after its `Input` has closed the format context.
struct AvioGuard {
    avio: *mut ffmpeg::ffi::AVIOContext,
    opaque: *mut SftpReader,
}

impl Drop for AvioGuard {
    fn drop(&mut self) {
        unsafe { free_avio(self.avio, self.opaque) }
    }
}

unsafe fn free_avio(avio: *mut ffmpeg::ffi::AVIOContext, reader: *mut SftpReader) {
    unsafe {
        if !avio.is_null() {
            ffmpeg::ffi::av_freep(&mut (*avio).buffer as *mut _ as *mut c_void);
            let mut avio = avio;
            ffmpeg::ffi::avio_context_free(&mut avio);
        }
        drop(Box::from_raw(reader));
    }
}

/// Custom AVIO read: range-read `buf_size` bytes at the current position straight
/// from the remote file. Reports an error (not EOF) on an SFTP failure so the
/// demuxer stops instead of looping.
unsafe extern "C" fn avio_read(opaque: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let reader = unsafe { &mut *(opaque as *mut SftpReader) };
    let want = buf_size.max(0) as usize;
    if want == 0 {
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(buf, want) };
    let pos = reader.pos;
    let handle = reader.handle.clone();
    let file = &mut reader.file;
    let result = handle.block_on(async move {
        file.seek(SeekFrom::Start(pos)).await?;
        file.read(slice).await
    });
    match result {
        Ok(0) => ffmpeg::ffi::AVERROR_EOF,
        Ok(n) => {
            reader.pos += n as u64;
            n as c_int
        }
        Err(_) => ffmpeg::ffi::AVERROR_EXTERNAL,
    }
}

/// Custom AVIO seek. `AVSEEK_SIZE` and `SEEK_END` need the total size and fail if
/// it is unknown; otherwise the position is just recorded for the next read.
unsafe extern "C" fn avio_seek(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    const SEEK_SET: c_int = 0;
    const SEEK_CUR: c_int = 1;
    const SEEK_END: c_int = 2;
    const AVSEEK_SIZE: c_int = 0x10000;
    const AVSEEK_FORCE: c_int = 0x20000;
    let reader = unsafe { &mut *(opaque as *mut SftpReader) };
    let whence = whence & !AVSEEK_FORCE;
    if whence & AVSEEK_SIZE != 0 {
        return if reader.size > 0 { reader.size as i64 } else { -1 };
    }
    let new = match whence {
        SEEK_SET => offset,
        SEEK_CUR => reader.pos as i64 + offset,
        SEEK_END if reader.size > 0 => reader.size as i64 + offset,
        _ => return -1,
    };
    if new < 0 {
        return -1;
    }
    reader.pos = new as u64;
    new
}

/// ffmpeg's `AV_TIME_BASE`: durations and seek timestamps are in microseconds.
const AV_TIME_BASE: f64 = 1_000_000.0;

/// How many decoded frames to buffer ahead; bounds the decoder's lead and the
/// memory it holds for a looping (otherwise unbounded) stream.
const FRAME_BUFFER: usize = 6;

/// Forward position jump (seconds) treated as a stream discontinuity rather than a
/// wait until its wall-clock time: ffplay's no-sync threshold. Below it, playback
/// waits the gap out in real time, which keeps low-fps content paced.
const NOSYNC_THRESHOLD: f64 = 10.0;

struct TimedFrame {
    image: ColorImage,
    /// In-file presentation time in seconds.
    position: f64,
    /// Seek generation; frames decoded before a seek carry an older value.
    generation: u64,
}

/// State shared with the worker: pending seek requests, the current seek
/// generation, the duration the worker fills in once the file is open, and the
/// failure a dying worker reports.
#[derive(Default)]
struct Shared {
    seek_request: Option<f64>,
    generation: u64,
    duration: f64,
    error: Option<String>,
}

/// Record a worker failure for the player to surface; also logged for debugging.
fn report_error(shared: &Mutex<Shared>, msg: String) {
    crate::log!("{msg}");
    shared.lock().unwrap().error = Some(msg);
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
    /// Set once the worker is gone: the failure to show instead of the video.
    error: Option<String>,
}

impl VideoPlayer {
    pub fn open(uri: String, path: PathBuf) -> Self {
        Self::spawn(uri, move |tx, shared| {
            // ffmpeg-next unwraps to_str(), so a non-UTF-8 path would panic the worker.
            if path.to_str().is_none() {
                report_error(&shared, format!("video path is not UTF-8: {}", path.display()));
                return;
            }
            match VideoDecoder::open(&path) {
                Ok(decoder) => decode_loop(decoder, &tx, &shared),
                Err(e) => {
                    report_error(&shared, format!("video open failed for {}: {e}", path.display()))
                }
            }
        })
    }

    /// Like `open`, but for a remote (SFTP) video: the worker thread opens the
    /// remote file and decodes straight from it through a custom AVIO doing
    /// on-demand range reads, so playback starts after only the header and index
    /// are read rather than after a whole-file download.
    pub fn open_remote(
        uri: String,
        session: Arc<SftpSession>,
        handle: tokio::runtime::Handle,
        remote_path: String,
    ) -> Self {
        Self::spawn(uri, move |tx, shared| {
            let size = handle
                .block_on(session.metadata(remote_path.clone()))
                .ok()
                .and_then(|meta| meta.size)
                .unwrap_or(0);
            let file = match handle.block_on(session.open(remote_path.clone())) {
                Ok(file) => file,
                Err(e) => {
                    report_error(&shared, format!("remote video open failed for {remote_path}: {e}"));
                    return;
                }
            };
            match VideoDecoder::open_sftp(handle, file, size) {
                Ok(decoder) => decode_loop(decoder, &tx, &shared),
                Err(e) => {
                    report_error(&shared, format!("remote video decode failed for {remote_path}: {e}"))
                }
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
            error: None,
        }
    }

    /// The failure that ended playback, if the worker has died.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
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
                match self.rx.try_recv() {
                    Ok(frame) => self.next = Some(frame),
                    Err(TryRecvError::Empty) => {}
                    // Buffered frames drain first, so this is the worker's true end.
                    Err(TryRecvError::Disconnected) => {
                        if self.error.is_none() {
                            self.error = Some(self.shared.lock().unwrap().error.clone().unwrap_or_else(
                                || "playback stopped unexpectedly".to_string(),
                            ));
                        }
                    }
                }
            }
            let Some(frame) = self.next.as_ref() else { break };
            if frame.generation != self.generation {
                self.next = None; // stale pre-seek frame
                continue;
            }
            // Re-anchor on the first frame, a seek, or a discontinuity against the
            // playing position: a jump back (loop) or past the no-sync threshold.
            let reanchor = match self.anchor {
                None => true,
                Some(_) => {
                    let delta = frame.position - self.position;
                    delta < -1e-3 || delta > NOSYNC_THRESHOLD
                }
            };
            // While paused, only land a re-anchoring frame (first/seek/jump), then hold.
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
            && let Err(e) = decoder.seek_to(target)
        {
            report_error(shared, format!("video seek failed: {e}"));
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
                // Loop back to the start.
                if let Err(e) = decoder.seek_to(0.0) {
                    report_error(shared, format!("video restart failed: {e}"));
                    return;
                }
            }
            Err(e) => {
                report_error(shared, format!("video decode failed: {e}"));
                return;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Player whose worker sends one frame per position, then exits.
    fn player_with(positions: &[f64]) -> VideoPlayer {
        let positions = positions.to_vec();
        VideoPlayer::spawn("test://video".to_string(), move |tx, _shared| {
            for position in positions {
                let frame = TimedFrame {
                    image: ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 255]),
                    position,
                    generation: 0,
                };
                if tx.send(frame).is_err() {
                    return;
                }
            }
        })
    }

    /// Poll until the first frame has landed (`frame` yields a texture).
    fn wait_for_first_frame(player: &mut VideoPlayer, ctx: &egui::Context) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while player.frame(ctx).is_none() {
            assert!(Instant::now() < deadline, "no frame landed within 2s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Poll until a frame at `expected` lands. Discontinuous frames must land far
    /// inside the deadline; a frame paced that far ahead would miss it.
    fn wait_for_position(player: &mut VideoPlayer, ctx: &egui::Context, expected: f64) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while (player.position() - expected).abs() > 1e-9 {
            assert!(
                Instant::now() < deadline,
                "frame at {expected} not landed within 2s (position {})",
                player.position()
            );
            player.frame(ctx);
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Poll for `hold`, asserting the position stays at `expected` throughout.
    fn assert_position_holds(player: &mut VideoPlayer, ctx: &egui::Context, expected: f64, hold: Duration) {
        let until = Instant::now() + hold;
        while Instant::now() < until {
            player.frame(ctx);
            assert!(
                (player.position() - expected).abs() < 1e-9,
                "position moved to {} before its time",
                player.position()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn loop_restart_reanchors_instead_of_fast_forwarding() {
        let ctx = egui::Context::default();
        // The bug shape: the loop restarts at the same pts the clock was anchored
        // to, which the old anchor-relative test could never classify as a jump.
        let mut player = player_with(&[10.0, 10.4, 10.8, 10.0, 10.4, 10.8]);
        wait_for_first_frame(&mut player, &ctx);
        wait_for_position(&mut player, &ctx, 10.4);
        // The loop-back frame re-anchors and lands (10.8 cascades into it, so
        // 10.0 is the next stable position)...
        wait_for_position(&mut player, &ctx, 10.0);
        // ...and the frames behind it are paced from the new anchor instead of
        // being instantly due against the stale one (the fast-forward bug).
        assert_position_holds(&mut player, &ctx, 10.0, Duration::from_millis(200));
        wait_for_position(&mut player, &ctx, 10.4);
    }

    #[test]
    fn forward_gap_reanchors_immediately() {
        let ctx = egui::Context::default();
        let mut player = player_with(&[0.0, 60.0]);
        wait_for_first_frame(&mut player, &ctx);
        // 60s exceeds the no-sync threshold: landing must not wait for the gap.
        wait_for_position(&mut player, &ctx, 60.0);
    }

    #[test]
    fn continuous_frame_waits_for_presentation_time() {
        let ctx = egui::Context::default();
        let mut player = player_with(&[0.0, 2.0]);
        wait_for_first_frame(&mut player, &ctx);
        // 2s ahead is below the threshold: still paced, not landed early.
        assert_position_holds(&mut player, &ctx, 0.0, Duration::from_millis(300));
    }

    /// Poll until the player surfaces an error, or panic after a timeout.
    fn wait_for_error(player: &mut VideoPlayer, ctx: &egui::Context) -> String {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            player.frame(ctx);
            if let Some(error) = player.error() {
                return error.to_string();
            }
            assert!(Instant::now() < deadline, "no error surfaced within 2s");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn reported_worker_failure_surfaces() {
        let ctx = egui::Context::default();
        let mut player = VideoPlayer::spawn("test://video".to_string(), |_tx, shared| {
            report_error(&shared, "boom".to_string());
        });
        assert_eq!(wait_for_error(&mut player, &ctx), "boom");
    }

    #[test]
    fn silent_worker_death_gets_fallback_message() {
        let ctx = egui::Context::default();
        let mut player = VideoPlayer::spawn("test://video".to_string(), |_tx, _shared| {});
        assert_eq!(wait_for_error(&mut player, &ctx), "playback stopped unexpectedly");
    }

    #[test]
    fn buffered_frame_lands_before_the_error() {
        let ctx = egui::Context::default();
        let mut player = VideoPlayer::spawn("test://video".to_string(), |tx, shared| {
            let frame = TimedFrame {
                image: ColorImage::from_rgba_unmultiplied([1, 1], &[0, 0, 0, 255]),
                position: 1.5,
                generation: 0,
            };
            let _ = tx.send(frame);
            report_error(&shared, "died after one frame".to_string());
        });
        wait_for_first_frame(&mut player, &ctx);
        assert_eq!(player.position(), 1.5);
        assert_eq!(wait_for_error(&mut player, &ctx), "died after one frame");
    }

    #[test]
    fn non_utf8_path_fails_without_panicking() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let ctx = egui::Context::default();
        let path = PathBuf::from(OsStr::from_bytes(b"/tmp/f\xFFlm.mp4"));
        let mut player = VideoPlayer::open("file:///tmp/film.mp4".to_string(), path);
        let error = wait_for_error(&mut player, &ctx);
        assert!(error.contains("not UTF-8"), "unexpected error: {error}");
    }
}
