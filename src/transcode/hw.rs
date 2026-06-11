use rsmpeg::{avutil::AVHWDeviceContext, ffi};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

// ── Hardware device types ───────────────────────────────────────────────────
//
// HwType identifies a GPU/accelerator family. Shared across decode/filter/encode.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum HwType {
    Cuda,
    Vaapi,
    Qsv,
    Amf,
    Videotoolbox,
    Rkmpp,
    V4l2m2m,
}

impl HwType {
    pub fn device_type(self) -> ffi::AVHWDeviceType {
        match self {
            HwType::Cuda => ffi::AV_HWDEVICE_TYPE_CUDA,
            HwType::Vaapi => ffi::AV_HWDEVICE_TYPE_VAAPI,
            HwType::Qsv => ffi::AV_HWDEVICE_TYPE_QSV,
            HwType::Amf => ffi::AV_HWDEVICE_TYPE_D3D11VA,
            HwType::Videotoolbox => ffi::AV_HWDEVICE_TYPE_VIDEOTOOLBOX,
            HwType::Rkmpp | HwType::V4l2m2m => ffi::AV_HWDEVICE_TYPE_DRM,
        }
    }

    pub fn pix_fmt(self) -> ffi::AVPixelFormat {
        match self {
            HwType::Cuda => ffi::AV_PIX_FMT_CUDA,
            HwType::Vaapi => ffi::AV_PIX_FMT_VAAPI,
            HwType::Qsv => ffi::AV_PIX_FMT_QSV,
            HwType::Amf => ffi::AV_PIX_FMT_D3D11,
            HwType::Videotoolbox => ffi::AV_PIX_FMT_VIDEOTOOLBOX,
            HwType::Rkmpp | HwType::V4l2m2m => ffi::AV_PIX_FMT_DRM_PRIME,
        }
    }

    pub fn sw_format(self, src_pix_fmt: ffi::AVPixelFormat) -> ffi::AVPixelFormat {
        match self {
            HwType::Cuda => match src_pix_fmt {
                ffi::AV_PIX_FMT_YUV420P10LE | ffi::AV_PIX_FMT_YUV420P10BE => ffi::AV_PIX_FMT_P010LE,
                ffi::AV_PIX_FMT_YUV444P => ffi::AV_PIX_FMT_YUV444P,
                ffi::AV_PIX_FMT_YUV444P10LE => ffi::AV_PIX_FMT_YUV444P16LE,
                _ => ffi::AV_PIX_FMT_NV12,
            },
            _ => match src_pix_fmt {
                ffi::AV_PIX_FMT_YUV420P10LE | ffi::AV_PIX_FMT_YUV420P10BE => ffi::AV_PIX_FMT_P010LE,
                _ => ffi::AV_PIX_FMT_NV12,
            },
        }
    }

    pub fn accepts_p010(self) -> bool {
        !matches!(self, HwType::Rkmpp | HwType::V4l2m2m)
    }

    pub fn display_name(self) -> &'static str {
        match self {
            HwType::Cuda => "NVIDIA CUDA",
            HwType::Vaapi => "VAAPI",
            HwType::Qsv => "Intel QSV",
            HwType::Amf => "AMD AMF",
            HwType::Videotoolbox => "Apple VideoToolbox",
            HwType::Rkmpp => "Rockchip RKMPP",
            HwType::V4l2m2m => "V4L2M2M",
        }
    }

    /// Check if a codec name corresponds to a HW encoder for this device type.
    pub fn is_hw_encoder(self, codec: &str) -> bool {
        match self {
            HwType::Cuda => codec.contains("nvenc") || codec.contains("cuvid"),
            HwType::Vaapi => codec.contains("vaapi"),
            HwType::Qsv => codec.contains("qsv"),
            HwType::Amf => codec.contains("amf"),
            HwType::Videotoolbox => codec.contains("videotoolbox"),
            HwType::Rkmpp => codec.contains("rkmpp"),
            HwType::V4l2m2m => codec.contains("v4l2m2m"),
        }
    }

    /// Convenience: native (same-device) scale filter for this `HwType`.
    pub fn scale_filter(self) -> &'static str {
        FilterBackend::Native.scale_filter(Some(self))
    }
}

// ── Three-stage composable pipeline ─────────────────────────────────────────
//
// Modeled after Jellyfin: decode/filter/encode can each use a different backend.
//   e.g. VAAPI decode → OpenCL filter → QSV encode

/// Filter (scale/tonemap/overlay) backend — may differ from decode/encode device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterBackend {
    /// Same device as decode backend (`scale_cuda`, `scale_vaapi`, etc.)
    Native,
    /// Cross-device `OpenCL` filters (`tonemap_opencl`, `scale_opencl`)
    OpenCL,
    /// Cross-device Vulkan filters (libplacebo, `scale_vulkan`)
    Vulkan,
    /// CPU filters (scale, format)
    Software,
}

impl FilterBackend {
    pub fn scale_filter(self, decode_hw: Option<HwType>) -> &'static str {
        match self {
            FilterBackend::Native => match decode_hw {
                Some(HwType::Cuda) => "scale_cuda",
                Some(HwType::Vaapi) => "scale_vaapi",
                Some(HwType::Qsv) => "scale_qsv",
                Some(HwType::Videotoolbox) => "scale_vt",
                Some(HwType::Rkmpp) => "scale_rkrga",
                Some(HwType::Amf | HwType::V4l2m2m) | None => "scale",
            },
            FilterBackend::OpenCL => "scale_opencl",
            FilterBackend::Vulkan => "scale_vulkan",
            FilterBackend::Software => "scale",
        }
    }

    pub fn tonemap_filter(self, decode_hw: Option<HwType>) -> Option<&'static str> {
        match self {
            FilterBackend::Native => match decode_hw {
                Some(HwType::Cuda) => Some("tonemap_cuda"),
                Some(HwType::Vaapi) => Some("tonemap_vaapi"),
                Some(HwType::Videotoolbox) => Some("tonemap_videotoolbox"),
                Some(HwType::Qsv) => Some("vpp_qsv"),
                _ => None,
            },
            FilterBackend::OpenCL => Some("tonemap_opencl"),
            FilterBackend::Vulkan => Some("libplacebo"),
            FilterBackend::Software => None,
        }
    }

    pub fn deinterlace_filter(self, decode_hw: Option<HwType>) -> Option<&'static str> {
        match self {
            FilterBackend::Native => match decode_hw {
                Some(HwType::Cuda) => Some("yadif_cuda"),
                Some(HwType::Vaapi) => Some("deinterlace_vaapi"),
                Some(HwType::Qsv) => Some("deinterlace_qsv"),
                Some(HwType::Videotoolbox) => Some("yadif_videotoolbox"),
                _ => None,
            },
            FilterBackend::OpenCL => Some("yadif_opencl"),
            FilterBackend::Vulkan => None,
            FilterBackend::Software => Some("yadif"),
        }
    }

    pub fn overlay_filter(self, decode_hw: Option<HwType>) -> &'static str {
        match self {
            FilterBackend::Native => match decode_hw {
                Some(HwType::Cuda) => "overlay_cuda",
                Some(HwType::Vaapi) => "overlay_vaapi",
                Some(HwType::Qsv) => "overlay_qsv",
                Some(HwType::Videotoolbox) => "overlay_videotoolbox",
                Some(HwType::Rkmpp) => "overlay_rkrga",
                Some(HwType::Amf | HwType::V4l2m2m) | None => "overlay",
            },
            FilterBackend::OpenCL => "overlay_opencl",
            FilterBackend::Vulkan => "overlay_vulkan",
            FilterBackend::Software => "overlay",
        }
    }

    pub fn transpose_filter(self, decode_hw: Option<HwType>) -> &'static str {
        match self {
            FilterBackend::Native => match decode_hw {
                Some(HwType::Cuda) => "transpose_cuda",
                Some(HwType::Vaapi) => "transpose_vaapi",
                Some(HwType::Videotoolbox) => "transpose_vt",
                _ => "transpose",
            },
            FilterBackend::OpenCL => "transpose_opencl",
            FilterBackend::Vulkan => "transpose_vulkan",
            FilterBackend::Software => "transpose",
        }
    }

    /// `FFmpeg` hwaccel device type needed for this filter backend (for hwmap/derive).
    pub fn device_type(self) -> Option<ffi::AVHWDeviceType> {
        match self {
            FilterBackend::OpenCL => Some(ffi::AV_HWDEVICE_TYPE_OPENCL),
            FilterBackend::Vulkan => Some(ffi::AV_HWDEVICE_TYPE_VULKAN),
            FilterBackend::Native | FilterBackend::Software => None,
        }
    }

    /// Whether this backend requires hwmap bridge between decode device and filter device.
    pub fn needs_hwmap(self) -> bool {
        matches!(self, FilterBackend::OpenCL | FilterBackend::Vulkan)
    }
}

/// Fully resolved 3-stage pipeline configuration.
///
/// Created once from CLI args or programmatically; consumed by transcode engine.
#[derive(Clone)]
pub struct HwPipeline {
    /// Decode backend (None = software decode)
    pub decode: Option<HwType>,
    /// Filter backend
    pub filter: FilterBackend,
    /// Encode backend (None = software encode, e.g. libx264)
    pub encode: Option<HwType>,
    /// Video encoder name (e.g. "`hevc_nvenc`", "libx264")
    pub encoder_name: String,
    /// Fallback level used to reach this configuration
    pub fallback: FallbackLevel,
}

impl HwPipeline {
    /// True if decode and encode use the same HW device (zero-copy path).
    pub fn is_unified(&self) -> bool {
        match (self.decode, self.encode) {
            (Some(d), Some(e)) => d == e && self.filter == FilterBackend::Native,
            _ => false,
        }
    }

    /// True if any stage uses hardware.
    pub fn has_hw(&self) -> bool {
        self.decode.is_some() || self.encode.is_some()
    }

    /// The `HwType` whose `pix_fmt` the decoder outputs (for filter graph input).
    pub fn decode_pix_fmt(&self) -> Option<ffi::AVPixelFormat> {
        self.decode.map(HwType::pix_fmt)
    }

    /// The `HwType` whose `pix_fmt` the encoder expects (for filter graph output).
    pub fn encode_pix_fmt(&self) -> Option<ffi::AVPixelFormat> {
        self.encode.map(HwType::pix_fmt)
    }

    /// Scale filter name for this pipeline.
    pub fn scale_filter(&self) -> &'static str {
        self.filter.scale_filter(self.decode)
    }

    /// Whether cross-device hwmap is needed between decode and filter.
    pub fn needs_decode_to_filter_map(&self) -> bool {
        match self.filter {
            FilterBackend::Native | FilterBackend::Software => false,
            FilterBackend::OpenCL | FilterBackend::Vulkan => self.decode.is_some(),
        }
    }

    /// Whether cross-device hwmap is needed between filter and encode.
    pub fn needs_filter_to_encode_map(&self) -> bool {
        match self.filter {
            FilterBackend::Native | FilterBackend::Software => false,
            FilterBackend::OpenCL | FilterBackend::Vulkan => self.encode.is_some(),
        }
    }

    pub fn describe(&self) -> String {
        let dec = self.decode.map_or("SW".to_string(), |d| format!("{d:?}"));
        let flt = format!("{:?}", self.filter);
        let enc = self
            .encode
            .map_or(self.encoder_name.clone(), |e| format!("{:?}({})", e, self.encoder_name));
        format!("{} → {} → {} [{}]", dec, flt, enc, self.fallback.label())
    }
}

// ── Global hardware device context pool ─────────────────────────────────────
//
// Creating a hardware device context is expensive:
//   • CUDA  — spawns persistent cuda-EvtHandlr threads; ~200–500ms
//   • VAAPI — opens /dev/dri/renderD* and loads the VA driver; ~80–200ms
//   • QSV   — initialises the Intel MFX runtime on top of VAAPI/D3D11; ~200–400ms
//   • AMF   — initialises the DirectX11 runtime; ~100–300ms
//
// All of these costs compound directly into HLS segment latency when sessions
// are created or restarted (e.g. seek, audio-track toggle, HDR toggle).
//
// The pool stores one context per HwType for the lifetime of the process.
// Callers receive a refcounted clone (`AVBufferRef`); the pool retains the
// original.  Clone cost is a single atomic increment.

static GLOBAL_HW_DEVICE_POOL: LazyLock<Mutex<HashMap<HwType, AVHWDeviceContext>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get (or lazily create) the process-wide device context for `hw_type`.
///
/// On first call for a given backend, allocates and initialises the context.
/// Subsequent calls return a cheap refcounted clone.
/// Returns `None` if the hardware is not present or the driver fails to load.
fn get_pooled_device_ctx(hw_type: HwType) -> Option<AVHWDeviceContext> {
    let mut pool = GLOBAL_HW_DEVICE_POOL.lock().unwrap_or_else(|e| {
        tracing::warn!("hw device pool mutex poisoned, recovering: {e}");
        e.into_inner()
    });
    if let Some(ctx) = pool.get(&hw_type) {
        return Some(ctx.clone());
    }
    let ctx = AVHWDeviceContext::create(hw_type.device_type(), None, None, 0).ok()?;
    let clone = ctx.clone();
    pool.insert(hw_type, ctx);
    tracing::info!(
        "[transcode] Created persistent {} device context (shared across all sessions)",
        hw_type.display_name()
    );
    Some(clone)
}

// ── Initialized hardware context ─────────────────────────────────────────────

pub struct HwAccel {
    pub hw_type: HwType,
    pub device_ctx: AVHWDeviceContext,
}

impl HwAccel {
    /// Probe whether `hw_type` is available on this system and return an
    /// initialised `HwAccel`.
    ///
    /// The underlying device context is stored in the process-wide pool on
    /// first success, so repeated calls are cheap (one atomic clone).
    pub fn try_init(hw_type: HwType) -> Option<Self> {
        let device_ctx = get_pooled_device_ctx(hw_type)?;
        Some(HwAccel { hw_type, device_ctx })
    }

    /// Get a persistent device context for `hw_type` from the process-wide pool.
    ///
    /// Equivalent to `try_init(hw_type).map(|a| a.device_ctx)`.
    /// Use this when only the bare context is needed without a full `HwAccel`.
    pub fn get_or_create_device_ctx(hw_type: HwType) -> Option<AVHWDeviceContext> {
        get_pooled_device_ctx(hw_type)
    }

    #[allow(dead_code)]
    pub fn auto_detect() -> Option<Self> {
        let candidates = [
            HwType::Cuda,
            HwType::Vaapi,
            HwType::Qsv,
            HwType::Amf,
            HwType::Videotoolbox,
            HwType::Rkmpp,
            HwType::V4l2m2m,
        ];
        for hw in candidates {
            if let Some(accel) = Self::try_init(hw) {
                return Some(accel);
            }
        }
        None
    }
}

// ── Pipeline resolution ──────────────────────────────────────────────────────

/// Fallback level used during pipeline resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackLevel {
    /// Full HW: decode + native filter + encode on same device
    FullHw,
    /// Cross-device: decode on one device, filter via OpenCL/Vulkan bridge, encode on another
    CrossDevice,
    /// Mixed A: SW decode → hwupload → HW filter + HW encode
    MixedSwDecode,
    /// Mixed B: HW decode → hwdownload → SW filter + HW encode
    MixedHwDownload,
    /// Pure software
    Software,
}

impl FallbackLevel {
    pub fn label(self) -> &'static str {
        match self {
            FallbackLevel::FullHw => "full-hw",
            FallbackLevel::CrossDevice => "cross-device",
            FallbackLevel::MixedSwDecode => "sw-decode",
            FallbackLevel::MixedHwDownload => "hw-download",
            FallbackLevel::Software => "software",
        }
    }
}

/// Build a `HwPipeline` from CLI arguments.
///
/// `video_codec`:    encoder name ("`hevc_nvenc`", "libx264", etc.)
/// `decode_opt`:     explicit decode backend ("cuda", "vaapi", etc.).
///                   `None` means SW decode — callers that want HW decode must
///                   pass an explicit backend string.  This avoids silently
///                   enabling CUDA decode for codecs that NVDEC doesn't support
///                   (e.g. rv40/RealVideo), which would crash `scale_cuda`.
/// `filter_opt`:     explicit filter backend ("native", "opencl", "vulkan", "software")
pub fn resolve_pipeline(video_codec: &str, decode_opt: Option<&str>, filter_opt: Option<&str>) -> HwPipeline {
    let encode_hw = infer_hw_from_codec(video_codec);

    // Treat None as "no HW decode" rather than "match encoder".
    // Unified HW decode+encode is only safe when the caller explicitly confirms
    // the source codec is supported by the hardware decoder (e.g. via CUVID list).
    let decode_hw = decode_opt.and_then(parse_hw_type);

    let filter = if let Some(f) = filter_opt {
        parse_filter_backend(f)
    } else {
        FilterBackend::Native
    };

    HwPipeline {
        decode: decode_hw,
        filter,
        encode: encode_hw,
        encoder_name: video_codec.to_string(),
        fallback: infer_fallback_level(decode_hw, filter, encode_hw),
    }
}

/// Build a `HwPipeline` programmatically (for lib callers, no string parsing).
pub fn build_pipeline(
    decode: Option<HwType>,
    filter: FilterBackend,
    encode: Option<HwType>,
    encoder_name: &str,
) -> HwPipeline {
    HwPipeline {
        fallback: infer_fallback_level(decode, filter, encode),
        decode,
        filter,
        encode,
        encoder_name: encoder_name.to_string(),
    }
}

/// Try to build a pipeline with automatic fallback.
/// Returns the highest-available pipeline for the given encoder.
pub fn resolve_pipeline_with_fallback(video_codec: &str) -> HwPipeline {
    let encode_hw = infer_hw_from_codec(video_codec);

    if let Some(enc_hw) = encode_hw {
        // Level 1: Full HW (unified)
        if HwAccel::try_init(enc_hw).is_some() {
            return build_pipeline(Some(enc_hw), FilterBackend::Native, Some(enc_hw), video_codec);
        }
        // Level 2: Cross-device (try OpenCL filter bridge)
        // (would need a different decode device — platform specific, skip for now)

        // Level 3: Mixed A — SW decode + HW encode
        if HwAccel::try_init(enc_hw).is_some() {
            return build_pipeline(None, FilterBackend::Software, Some(enc_hw), video_codec);
        }
    }

    // Level 5: Pure software
    let sw_codec = software_fallback(video_codec);
    build_pipeline(None, FilterBackend::Software, None, sw_codec)
}

fn infer_fallback_level(decode: Option<HwType>, filter: FilterBackend, encode: Option<HwType>) -> FallbackLevel {
    match (decode, filter, encode) {
        (Some(d), FilterBackend::Native, Some(e)) if d == e => FallbackLevel::FullHw,
        (Some(_), FilterBackend::OpenCL | FilterBackend::Vulkan | FilterBackend::Native, Some(_)) => {
            FallbackLevel::CrossDevice
        }
        (None, _, Some(_)) => FallbackLevel::MixedSwDecode,
        (Some(_), FilterBackend::Software, Some(_)) => FallbackLevel::MixedHwDownload,
        _ => FallbackLevel::Software,
    }
}

fn parse_filter_backend(s: &str) -> FilterBackend {
    match s {
        "opencl" | "ocl" => FilterBackend::OpenCL,
        "vulkan" | "vk" => FilterBackend::Vulkan,
        "software" | "sw" | "cpu" => FilterBackend::Software,
        _ => FilterBackend::Native,
    }
}

/// Parse a `HwType` from a string.
pub fn parse_hw_type(s: &str) -> Option<HwType> {
    match s {
        "cuda" | "nvenc" | "nvidia" => Some(HwType::Cuda),
        "vaapi" => Some(HwType::Vaapi),
        "qsv" | "quicksync" => Some(HwType::Qsv),
        "amf" | "amd" => Some(HwType::Amf),
        "videotoolbox" | "vt" => Some(HwType::Videotoolbox),
        "rkmpp" | "rockchip" => Some(HwType::Rkmpp),
        "v4l2m2m" | "v4l2" => Some(HwType::V4l2m2m),
        _ => None,
    }
}

/// Infer `HwType` from encoder codec name.
pub fn infer_hw_from_codec(codec: &str) -> Option<HwType> {
    if codec.contains("nvenc") || codec.contains("cuvid") {
        Some(HwType::Cuda)
    } else if codec.contains("vaapi") {
        Some(HwType::Vaapi)
    } else if codec.contains("qsv") {
        Some(HwType::Qsv)
    } else if codec.contains("amf") {
        Some(HwType::Amf)
    } else if codec.contains("videotoolbox") {
        Some(HwType::Videotoolbox)
    } else if codec.contains("rkmpp") {
        Some(HwType::Rkmpp)
    } else if codec.contains("v4l2m2m") {
        Some(HwType::V4l2m2m)
    } else {
        None
    }
}

// ── Convenience helpers (used by mod.rs) ────────────────────────────────────

pub fn is_any_hw_encoder(codec: &str) -> bool {
    infer_hw_from_codec(codec).is_some() || codec.contains("v4l2m2m")
}

pub fn software_fallback(codec: &str) -> &'static str {
    if codec.contains("hevc") || codec.contains("h265") {
        "libx265"
    } else if codec.contains("av1") {
        "libsvtav1"
    } else {
        "libx264"
    }
}

pub fn codec_id_for_encoder(codec_name: &str) -> ffi::AVCodecID {
    if codec_name.contains("hevc") || codec_name.contains("h265") || codec_name == "libx265" {
        ffi::AV_CODEC_ID_HEVC
    } else if codec_name.contains("av1") || codec_name == "libsvtav1" || codec_name == "libaom-av1" {
        ffi::AV_CODEC_ID_AV1
    } else {
        ffi::AV_CODEC_ID_H264
    }
}

/// Format spec string for filter arguments.
pub fn format_name(fmt: ffi::AVPixelFormat) -> &'static str {
    match fmt {
        ffi::AV_PIX_FMT_P010LE => "p010le",
        ffi::AV_PIX_FMT_YUV420P => "yuv420p",
        ffi::AV_PIX_FMT_YUV420P10LE => "yuv420p10le",
        _ => "nv12",
    }
}
