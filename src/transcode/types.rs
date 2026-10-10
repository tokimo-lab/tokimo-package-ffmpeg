use rsmpeg::avutil::{AVChannelLayout, AVHWDeviceContext};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

// ── Options ─────────────────────────────────────────────────────────────────

/// A cloneable handle to cancel a running transcode.
/// Set to `true` from any thread to request clean shutdown.
///
/// ```rust,no_run
/// use tokimo_package_ffmpeg::transcode::{CancellationToken, cancellation_token, TranscodeOptions, transcode};
/// let cancel = cancellation_token();
/// let opts = TranscodeOptions { cancel: Some(cancel.clone()), ..Default::default() };
/// std::thread::spawn(move || transcode(&opts));
/// // later, from any thread:
/// cancel.store(true, std::sync::atomic::Ordering::Relaxed);
/// ```
pub type CancellationToken = Arc<AtomicBool>;

/// A cloneable handle to pause/resume a running transcode.
/// Set to `true` from any thread to pause; set to `false` to resume.
/// Replaces SIGSTOP/SIGCONT for in-process throttling.
pub type PauseToken = Arc<AtomicBool>;

/// Create a new cancellation token (not cancelled).
pub fn cancellation_token() -> CancellationToken {
    Arc::new(AtomicBool::new(false))
}

/// Create a new pause token (not paused).
pub fn pause_token() -> PauseToken {
    Arc::new(AtomicBool::new(false))
}

/// Command sent to a persistent transcode worker for seek-restart.
pub enum SeekCommand {
    /// Seek to a new position and start a new transcode pass.
    Seek {
        seek_secs: f64,
        start_segment: u32,
        cancel: CancellationToken,
        pause: PauseToken,
    },
    /// Shut down the worker thread (session stopped).
    Stop,
}

/// HLS segment container format.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HlsSegmentType {
    /// Fragmented MP4 (fMP4) — requires init.mp4 + `#EXT-X-MAP`.
    #[default]
    Fmp4,
    /// MPEG-TS — self-contained segments, no init segment needed.
    /// Used for copy mode to avoid hls.js passthrough-remuxer timestamp issues.
    Mpegts,
}

impl HlsSegmentType {
    /// File extension for segments of this type.
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Fmp4 => "tokimo",
            Self::Mpegts => "ts",
        }
    }
}

/// HLS output configuration.
#[derive(Clone, Debug)]
pub struct HlsOptions {
    /// Segment duration in seconds (default: 6).
    pub segment_duration: u32,
    /// Segment filename pattern (e.g. "/tmp/hls/%05d.tokimo").
    pub segment_pattern: String,
    /// HLS init segment filename (e.g. "init.mp4") — only used for Fmp4.
    pub init_filename: String,
    /// Playlist type: "event" or "vod".
    pub playlist_type: String,
    /// Starting segment number.
    pub start_number: u32,
    /// Segment container format (fMP4 or mpegts).
    pub segment_type: HlsSegmentType,
}

/// Direct VFS input for custom AVIO — bypasses HTTP for `FFmpeg` reads.
///
/// When provided in `TranscodeOptions`, `FFmpeg` reads through a custom AVIO
/// context that calls `read_at` directly instead of going through HTTP range
/// requests. This eliminates ~500ms of HTTP→SMB round-trip overhead per seek.
pub struct DirectInput {
    /// Read up to `size` bytes from `offset`. Returns the owned Vec<u8>
    /// (possibly shorter than `size`), or an empty Vec for EOF.
    /// Returning the Vec directly avoids a copy vs. the old fill-buffer signature.
    pub read_at: crate::ReadAt,
    /// Total file size in bytes.
    pub size: u64,
    /// Optional filename hint for format detection (e.g. "movie.mkv").
    pub filename_hint: Option<String>,
    /// Read-ahead fetch size per VFS call.
    ///
    /// Tune based on access pattern:
    /// - `None` — use the conservative default (4 MB); safe for probe, random access, etc.
    /// - `Some(32 * 1024 * 1024)` — HLS / transcode (sequential; maximises SMB throughput)
    /// - `Some(2 * 1024 * 1024)` — probe-only (libavformat needs a few seeks, not GBs)
    pub readahead_bytes: Option<u64>,
}

impl DirectInput {
    /// Construct from a local file path using a positional read syscall
    /// (`pread` on Unix via [`std::os::unix::fs::FileExt::read_at`],
    /// `ReadFile` with `OVERLAPPED.Offset` on Windows via
    /// [`std::os::windows::fs::FileExt::seek_read`]). Both avoid the
    /// shared cursor races of `seek` + `read`.
    pub fn from_local(
        path: impl AsRef<str>,
        size: u64,
        filename_hint: Option<String>,
        readahead_bytes: Option<u64>,
    ) -> std::io::Result<Arc<Self>> {
        use std::fs::File;
        #[cfg(unix)]
        use std::os::unix::fs::FileExt;
        #[cfg(windows)]
        use std::os::windows::fs::FileExt;
        let file = Arc::new(File::open(path.as_ref())?);
        let read_at: crate::ReadAt = Arc::new(move |offset, max| {
            let mut buf = vec![0u8; max];
            #[cfg(unix)]
            let n = file.read_at(&mut buf, offset)?;
            #[cfg(windows)]
            let n = file.seek_read(&mut buf, offset)?;
            buf.truncate(n);
            Ok(buf)
        });
        Ok(Arc::new(Self {
            read_at,
            size,
            filename_hint,
            readahead_bytes,
        }))
    }

    /// Construct from any [`ReadAt`] closure (local or VFS).
    pub fn from_read_at(
        read_at: crate::ReadAt,
        size: u64,
        filename_hint: Option<String>,
        readahead_bytes: Option<u64>,
    ) -> Arc<Self> {
        Arc::new(Self {
            read_at,
            size,
            filename_hint,
            readahead_bytes,
        })
    }
}

impl Clone for DirectInput {
    fn clone(&self) -> Self {
        Self {
            read_at: self.read_at.clone(),
            size: self.size,
            filename_hint: self.filename_hint.clone(),
            readahead_bytes: self.readahead_bytes,
        }
    }
}

impl std::fmt::Debug for DirectInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectInput")
            .field("size", &self.size)
            .field("filename_hint", &self.filename_hint)
            .field("readahead_bytes", &self.readahead_bytes)
            .finish_non_exhaustive()
    }
}

#[derive(Clone)]
pub struct TranscodeOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    pub video_codec: String,
    pub audio_codec: String,
    /// Ignore video streams, including embedded cover images.
    pub audio_only: bool,
    /// Raw input stream index for audio; None selects the first audio stream.
    pub audio_stream_index: Option<u32>,
    pub decode: Option<String>,
    pub filter_backend: Option<String>,
    pub preset: String,
    pub crf: Option<u32>,
    pub bitrate: Option<String>,
    pub resolution: Option<String>,
    pub duration: Option<f64>,
    #[allow(dead_code)]
    pub progress: bool,
    pub seek: Option<f64>,
    pub video_filter: Option<String>,
    pub video_profile: Option<String>,
    pub maxrate: Option<String>,
    pub bufsize: Option<String>,
    pub gop: Option<i32>,
    pub keyint_min: Option<i32>,
    pub audio_bitrate: Option<String>,
    pub audio_channels: Option<i32>,
    /// Audio sample rate override (e.g. None = keep source rate).
    pub audio_sample_rate: Option<i32>,
    /// Optional cancellation token. Store `true` from any thread to request
    /// a clean shutdown. `transcode()` drains in-flight frames, closes the
    /// output file, and returns `Ok(())`.
    pub cancel: Option<CancellationToken>,
    /// Optional pause token. Store `true` to throttle (pause) the transcode
    /// loop; `false` to resume. Used for HLS backpressure.
    pub pause: Option<PauseToken>,
    /// HLS output configuration. When Some, output is HLS segments instead
    /// of a single file.
    pub hls: Option<HlsOptions>,
    /// Force IDR keyframes at this interval (seconds). Used for HLS with
    /// libx264. Equivalent to ffmpeg's -`force_key_frames` expr:gte(t,n*X).
    pub force_key_frames_interval: Option<f64>,
    /// If true, do not apply `noaccurate_seek` (always use accurate seek).
    pub accurate_seek: bool,
    /// Cached CUDA device context from a previous transcode, to avoid
    /// re-creating the GPU device on every seek-restart (~130ms savings).
    pub cached_device_ctx: Option<AVHWDeviceContext>,
    /// Direct VFS input — when set, `FFmpeg` reads through a custom AVIO
    /// context instead of opening the `input` path.
    pub direct_input: Option<Arc<DirectInput>>,
}

impl Default for TranscodeOptions {
    fn default() -> Self {
        Self {
            input: PathBuf::new(),
            output: PathBuf::new(),
            video_codec: "libx264".into(),
            audio_codec: crate::common::capabilities::best_aac_encoder().into(),
            audio_only: false,
            audio_stream_index: None,
            decode: None,
            filter_backend: None,
            preset: "medium".into(),
            crf: None,
            bitrate: None,
            resolution: None,
            duration: None,
            progress: false,
            seek: None,
            video_filter: None,
            video_profile: None,
            maxrate: None,
            bufsize: None,
            gop: None,
            keyint_min: None,
            audio_bitrate: None,
            audio_channels: None,
            audio_sample_rate: None,
            cancel: None,
            pause: None,
            hls: None,
            force_key_frames_interval: None,
            accurate_seek: true,
            cached_device_ctx: None,
            direct_input: None,
        }
    }
}

// ── Stream mapping (crate-internal) ─────────────────────────────────────────

#[allow(dead_code)]
pub(crate) enum StreamMapping {
    Video {
        in_idx: usize,
        out_idx: usize,
        gpu_pipeline: bool,
    },
    Audio {
        in_idx: usize,
        out_idx: usize,
    },
    Copy {
        in_idx: usize,
        out_idx: usize,
    },
    Ignore,
}

// ── Target format descriptions (crate-internal) ─────────────────────────────

pub(crate) struct VideoTargets {
    pub codec_name: String,
    pub pix_fmt: i32,
    pub sw_pix_fmt: i32,
    pub gpu_pipeline: bool,
    pub width: i32,
    pub height: i32,
}

pub(crate) struct AudioTargets {
    pub codec_name: String,
    pub sample_fmt: i32,
    pub sample_rate: i32,
    pub ch_layout: AVChannelLayout,
}
