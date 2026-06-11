//! FFI-based `FFmpeg` capability detection.
//!
//! Replaces subprocess probing (`ffmpeg -hwaccels`, `-encoders`, `-decoders`, `-filters`)
//! with direct iteration of `FFmpeg`'s internal codec/filter/format registries.
//! Zero process spawn overhead.

use rsmpeg::ffi;
use std::collections::HashSet;
use std::ffi::CStr;
use std::sync::OnceLock;

/// Hardware acceleration capabilities detected via FFI.
///
/// Field-compatible with `rust-hls::HwCapabilities` — consumers can
/// construct their own struct from this or use it directly.
#[derive(Debug, Clone)]
pub struct HwCapabilities {
    pub hwaccels: HashSet<String>,
    pub encoders: HashSet<String>,
    pub decoders: HashSet<String>,
    pub filters: HashSet<String>,

    // ── NVIDIA CUDA (Linux / Windows) ─────────────────────────────────────
    pub has_nvenc: bool,
    pub has_cuvid: bool,
    pub has_cuda_full: bool,
    pub has_bwdif_cuda: bool,
    pub has_bwdif: bool,
    /// `hevc_nvenc` encoder available.
    pub has_nvenc_hevc: bool,

    // ── VAAPI — Linux Intel / AMD ──────────────────────────────────────────
    /// vaapi hwaccel available + `h264_vaapi` encoder compiled in.
    pub has_vaapi: bool,
    /// Full VAAPI: + `scale_vaapi` + `deinterlace_vaapi` + `tonemap_vaapi` +
    /// `transpose_vaapi` + `hwupload_vaapi` (Jellyfin `IsVaapiFullSupported`).
    pub has_vaapi_full: bool,
    /// `hevc_vaapi` encoder available (requires `has_vaapi`).
    pub has_vaapi_hevc: bool,

    // ── Intel Quick Sync (QSV) — Linux / Windows ──────────────────────────
    /// qsv hwaccel + `h264_qsv` encoder.
    pub has_qsv: bool,
    /// Full QSV: + `scale_qsv` + `vpp_qsv` (Jellyfin `IsQsvFullSupported`).
    pub has_qsv_full: bool,
    /// `hevc_qsv` encoder available (requires `has_qsv`).
    pub has_qsv_hevc: bool,

    // ── Apple VideoToolbox — macOS ─────────────────────────────────────────
    /// videotoolbox hwaccel + `h264_videotoolbox` encoder.
    pub has_videotoolbox: bool,
    /// Full VT: + `yadif_videotoolbox` + `overlay_videotoolbox` + `scale_vt`
    /// (Jellyfin `IsVideoToolboxFullSupported`).
    pub has_videotoolbox_full: bool,
    /// `VideoToolbox` has tonemap support (`tonemap_videotoolbox` filter present).
    pub has_videotoolbox_tonemap: bool,
    /// `hevc_videotoolbox` encoder available (requires `has_videotoolbox`).
    pub has_videotoolbox_hevc: bool,

    // ── AMD AMF — Windows (+ experimental Linux via VAAPI bridge) ─────────
    /// `h264_amf` encoder available.
    pub has_amf: bool,
    /// `hevc_amf` encoder available.
    pub has_amf_hevc: bool,

    // ── Rockchip RKMPP — embedded ARM ─────────────────────────────────────
    /// rkmpp hwaccel + `h264_rkmpp` encoder.
    pub has_rkmpp: bool,
    /// `hevc_rkmpp` encoder available (requires `has_rkmpp`).
    pub has_rkmpp_hevc: bool,

    // ── SW filter extras ──────────────────────────────────────────────────
    pub has_tonemap: bool,
    pub has_tonemapx: bool,
    pub has_zscale: bool,
    pub has_tonemap_opencl: bool,
    /// libx265 software HEVC encoder available.
    pub has_libx265: bool,

    // ── CUVID per-codec hardware support (probed at startup) ───────────────
    /// Normalized codec names (e.g. "av1", "hevc", "h264") that have been
    /// confirmed to work with CUDA hardware decode on the current GPU.
    /// Populated by `rust_hls::hw_detect` after `detect_capabilities()`.
    /// Empty until that probe runs.
    pub cuvid_hw_codecs: HashSet<String>,
}

/// Codec metadata returned by listing functions.
#[derive(Debug, Clone)]
pub struct CodecInfo {
    pub name: String,
    pub long_name: String,
    pub codec_type: CodecType,
    pub is_encoder: bool,
    pub is_decoder: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecType {
    Video,
    Audio,
    Subtitle,
    Data,
    Unknown,
}

/// Filter metadata.
#[derive(Debug, Clone)]
pub struct FilterInfo {
    pub name: String,
    pub description: String,
}

/// Format (muxer/demuxer) metadata.
#[derive(Debug, Clone)]
pub struct FormatDesc {
    pub name: String,
    pub long_name: String,
}

// ── Public API ──────────────────────────────────────────────────────────────

static HW_CAPABILITIES: OnceLock<HwCapabilities> = OnceLock::new();

/// Detect hardware capabilities by querying `FFmpeg`'s internal registries.
/// No subprocess spawned — pure FFI iteration.
///
/// The result is computed once and cached for the lifetime of the process.
pub fn detect_capabilities() -> &'static HwCapabilities {
    HW_CAPABILITIES.get_or_init(detect_capabilities_inner)
}

fn detect_capabilities_inner() -> HwCapabilities {
    let hwaccels = list_hwaccels();
    let (encoders, decoders) = list_codecs_by_name();
    let filters = list_filter_names();

    // ── NVIDIA CUDA ──────────────────────────────────────────────────────
    let has_nvenc = encoders.contains("h264_nvenc");
    let has_nvenc_hevc = encoders.contains("hevc_nvenc");
    let has_cuvid = decoders.contains("hevc_cuvid");
    let has_cuda_full = hwaccels.contains("cuda")
        && filters.contains("scale_cuda")
        && filters.contains("yadif_cuda")
        && filters.contains("tonemap_cuda")
        && filters.contains("overlay_cuda")
        && filters.contains("hwupload_cuda");
    let has_bwdif_cuda = filters.contains("bwdif_cuda");
    let has_bwdif = filters.contains("bwdif");

    // ── VAAPI (Linux Intel/AMD) ───────────────────────────────────────────
    // Jellyfin: IsVaapiSupported = hwaccel present; IsVaapiFullSupported = all filters
    let has_vaapi = hwaccels.contains("vaapi") && encoders.contains("h264_vaapi");
    let has_vaapi_full = has_vaapi
        && filters.contains("scale_vaapi")
        && filters.contains("deinterlace_vaapi")
        && filters.contains("tonemap_vaapi")
        && filters.contains("transpose_vaapi")
        && filters.contains("hwupload_vaapi");
    let has_vaapi_hevc = has_vaapi && encoders.contains("hevc_vaapi");

    // ── Intel QSV ────────────────────────────────────────────────────────
    let has_qsv = hwaccels.contains("qsv") && encoders.contains("h264_qsv");
    let has_qsv_full = has_qsv && filters.contains("scale_qsv") && filters.contains("vpp_qsv");
    let has_qsv_hevc = has_qsv && encoders.contains("hevc_qsv");

    // ── Apple VideoToolbox (macOS) ────────────────────────────────────────
    // Jellyfin: IsVideoToolboxFullSupported = videotoolbox + yadif_videotoolbox + scale_vt
    let has_videotoolbox = hwaccels.contains("videotoolbox") && encoders.contains("h264_videotoolbox");
    let has_videotoolbox_full =
        has_videotoolbox && filters.contains("yadif_videotoolbox") && filters.contains("scale_vt");
    let has_videotoolbox_tonemap = has_videotoolbox && filters.contains("tonemap_videotoolbox");
    let has_videotoolbox_hevc = has_videotoolbox && encoders.contains("hevc_videotoolbox");

    // ── AMD AMF ──────────────────────────────────────────────────────────
    let has_amf = hwaccels.contains("amf") && encoders.contains("h264_amf");
    let has_amf_hevc = has_amf && encoders.contains("hevc_amf");

    // ── Rockchip RKMPP ────────────────────────────────────────────────────
    let has_rkmpp = hwaccels.contains("rkmpp") && encoders.contains("h264_rkmpp");
    let has_rkmpp_hevc = has_rkmpp && encoders.contains("hevc_rkmpp");

    // ── SW filter extras ─────────────────────────────────────────────────
    let has_tonemap = filters.contains("tonemap");
    let has_tonemapx = filters.contains("tonemapx");
    let has_zscale = filters.contains("zscale");
    let has_tonemap_opencl = filters.contains("tonemap_opencl");
    let has_libx265 = encoders.contains("libx265");

    HwCapabilities {
        hwaccels,
        encoders,
        decoders,
        filters,
        has_nvenc,
        has_cuvid,
        has_cuda_full,
        has_bwdif_cuda,
        has_bwdif,
        has_nvenc_hevc,
        has_vaapi,
        has_vaapi_full,
        has_vaapi_hevc,
        has_qsv,
        has_qsv_full,
        has_qsv_hevc,
        has_videotoolbox,
        has_videotoolbox_full,
        has_videotoolbox_tonemap,
        has_videotoolbox_hevc,
        has_amf,
        has_amf_hevc,
        has_rkmpp,
        has_rkmpp_hevc,
        has_tonemap,
        has_tonemapx,
        has_zscale,
        has_tonemap_opencl,
        has_libx265,
        cuvid_hw_codecs: HashSet::new(), // populated later by rust_hls::hw_detect
    }
}

/// Get the `FFmpeg` version string (e.g. "7.1").
pub fn ffmpeg_version() -> String {
    let ver = unsafe { ffi::avutil_version() };
    let major = (ver >> 16) & 0xFF;
    let minor = (ver >> 8) & 0xFF;
    let micro = ver & 0xFF;
    format!("{major}.{minor}.{micro}")
}

/// Full version string including libavcodec, libavformat, etc.
pub fn ffmpeg_version_full() -> String {
    let avutil = unsafe { ffi::avutil_version() };
    let avcodec = unsafe { ffi::avcodec_version() };
    let avformat = unsafe { ffi::avformat_version() };
    let avfilter = unsafe { ffi::avfilter_version() };
    let swscale = unsafe { ffi::swscale_version() };
    let swresample = unsafe { ffi::swresample_version() };

    fn fmt_ver(v: u32) -> String {
        format!("{}.{}.{}", (v >> 16) & 0xFF, (v >> 8) & 0xFF, v & 0xFF)
    }

    format!(
        "libavutil {} / libavcodec {} / libavformat {} / libavfilter {} / libswscale {} / libswresample {}",
        fmt_ver(avutil),
        fmt_ver(avcodec),
        fmt_ver(avformat),
        fmt_ver(avfilter),
        fmt_ver(swscale),
        fmt_ver(swresample),
    )
}

/// List all available hardware accelerator types.
pub fn list_hwaccels() -> HashSet<String> {
    let mut set = HashSet::new();
    unsafe {
        let mut hw_type = ffi::AV_HWDEVICE_TYPE_NONE;
        loop {
            hw_type = ffi::av_hwdevice_iterate_types(hw_type);
            if hw_type == ffi::AV_HWDEVICE_TYPE_NONE {
                break;
            }
            let name_ptr = ffi::av_hwdevice_get_type_name(hw_type);
            if !name_ptr.is_null() {
                let name = CStr::from_ptr(name_ptr).to_string_lossy().to_string();
                set.insert(name);
            }
        }
    }
    set
}

/// List all codecs, returning (encoders, decoders) as name sets.
fn list_codecs_by_name() -> (HashSet<String>, HashSet<String>) {
    let mut encoders = HashSet::new();
    let mut decoders = HashSet::new();
    unsafe {
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let codec = ffi::av_codec_iterate(&raw mut opaque);
            if codec.is_null() {
                break;
            }
            let name = CStr::from_ptr((*codec).name).to_string_lossy().to_string();
            if ffi::av_codec_is_encoder(codec) != 0 {
                encoders.insert(name.clone());
            }
            if ffi::av_codec_is_decoder(codec) != 0 {
                decoders.insert(name);
            }
        }
    }
    (encoders, decoders)
}

/// List all codecs with full metadata.
pub fn list_codecs() -> Vec<CodecInfo> {
    let mut result = Vec::new();
    unsafe {
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let codec = ffi::av_codec_iterate(&raw mut opaque);
            if codec.is_null() {
                break;
            }
            let name = CStr::from_ptr((*codec).name).to_string_lossy().to_string();
            let long_name = if (*codec).long_name.is_null() {
                String::new()
            } else {
                CStr::from_ptr((*codec).long_name).to_string_lossy().to_string()
            };
            let codec_type = match (*codec).type_ {
                ffi::AVMEDIA_TYPE_VIDEO => CodecType::Video,
                ffi::AVMEDIA_TYPE_AUDIO => CodecType::Audio,
                ffi::AVMEDIA_TYPE_SUBTITLE => CodecType::Subtitle,
                ffi::AVMEDIA_TYPE_DATA => CodecType::Data,
                _ => CodecType::Unknown,
            };
            let is_encoder = ffi::av_codec_is_encoder(codec) != 0;
            let is_decoder = ffi::av_codec_is_decoder(codec) != 0;

            result.push(CodecInfo {
                name,
                long_name,
                codec_type,
                is_encoder,
                is_decoder,
            });
        }
    }
    result
}

/// List all filter names.
pub fn list_filter_names() -> HashSet<String> {
    let mut set = HashSet::new();
    unsafe {
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let filter = ffi::av_filter_iterate(&raw mut opaque);
            if filter.is_null() {
                break;
            }
            let name = CStr::from_ptr((*filter).name).to_string_lossy().to_string();
            set.insert(name);
        }
    }
    set
}

/// List all filters with descriptions.
pub fn list_filters() -> Vec<FilterInfo> {
    let mut result = Vec::new();
    unsafe {
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let filter = ffi::av_filter_iterate(&raw mut opaque);
            if filter.is_null() {
                break;
            }
            let name = CStr::from_ptr((*filter).name).to_string_lossy().to_string();
            let description = if (*filter).description.is_null() {
                String::new()
            } else {
                CStr::from_ptr((*filter).description).to_string_lossy().to_string()
            };
            result.push(FilterInfo { name, description });
        }
    }
    result
}

/// List all output formats (muxers).
pub fn list_muxers() -> Vec<FormatDesc> {
    let mut result = Vec::new();
    unsafe {
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let fmt = ffi::av_muxer_iterate(&raw mut opaque);
            if fmt.is_null() {
                break;
            }
            let name = CStr::from_ptr((*fmt).name).to_string_lossy().to_string();
            let long_name = if (*fmt).long_name.is_null() {
                String::new()
            } else {
                CStr::from_ptr((*fmt).long_name).to_string_lossy().to_string()
            };
            result.push(FormatDesc { name, long_name });
        }
    }
    result
}

/// List all input formats (demuxers).
pub fn list_demuxers() -> Vec<FormatDesc> {
    let mut result = Vec::new();
    unsafe {
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let fmt = ffi::av_demuxer_iterate(&raw mut opaque);
            if fmt.is_null() {
                break;
            }
            let name = CStr::from_ptr((*fmt).name).to_string_lossy().to_string();
            let long_name = if (*fmt).long_name.is_null() {
                String::new()
            } else {
                CStr::from_ptr((*fmt).long_name).to_string_lossy().to_string()
            };
            result.push(FormatDesc { name, long_name });
        }
    }
    result
}

/// List available protocols (input + output).
pub fn list_protocols() -> Vec<String> {
    let mut result = Vec::new();
    unsafe {
        // Output protocols
        let mut opaque: *mut std::ffi::c_void = std::ptr::null_mut();
        loop {
            let name_ptr = ffi::avio_enum_protocols(&raw mut opaque, 1);
            if name_ptr.is_null() {
                break;
            }
            let name = CStr::from_ptr(name_ptr).to_string_lossy().to_string();
            result.push(name);
        }
        // Input protocols
        opaque = std::ptr::null_mut();
        loop {
            let name_ptr = ffi::avio_enum_protocols(&raw mut opaque, 0);
            if name_ptr.is_null() {
                break;
            }
            let name = CStr::from_ptr(name_ptr).to_string_lossy().to_string();
            if !result.contains(&name) {
                result.push(name);
            }
        }
    }
    result
}

/// Check if a specific filter is available.
pub fn has_filter(name: &str) -> bool {
    detect_capabilities().filters.contains(name)
}

/// Check if a specific encoder is available.
pub fn has_encoder(name: &str) -> bool {
    detect_capabilities().encoders.contains(name)
}

/// Return the best available AAC encoder name.
///
/// Prefers `libfdk_aac` (Fraunhofer reference implementation, superior quality
/// especially at low/mid bitrates) and falls back to native `aac` if not
/// compiled into the `FFmpeg` build.
pub fn best_aac_encoder() -> &'static str {
    if has_encoder("libfdk_aac") { "libfdk_aac" } else { "aac" }
}

/// Map a target audio codec name to the best available FFmpeg encoder for it.
///
/// Returns `None` if the codec is unknown or no suitable encoder is compiled in.
/// Callers should fall back to AAC when `None` is returned.
pub fn best_encoder_for_audio_codec(codec: &str) -> Option<&'static str> {
    let caps = detect_capabilities();
    match codec {
        "aac" => Some(if caps.encoders.contains("libfdk_aac") {
            "libfdk_aac"
        } else {
            "aac"
        }),
        "mp3" => {
            if caps.encoders.contains("libmp3lame") {
                Some("libmp3lame")
            } else if caps.encoders.contains("mp3") {
                Some("mp3")
            } else {
                None
            }
        }
        "opus" => {
            if caps.encoders.contains("libopus") {
                Some("libopus")
            } else if caps.encoders.contains("opus") {
                Some("opus")
            } else {
                None
            }
        }
        "flac" => {
            if caps.encoders.contains("flac") {
                Some("flac")
            } else {
                None
            }
        }
        "alac" => {
            if caps.encoders.contains("alac") {
                Some("alac")
            } else {
                None
            }
        }
        "ac3" => {
            if caps.encoders.contains("ac3") {
                Some("ac3")
            } else {
                None
            }
        }
        "eac3" => {
            if caps.encoders.contains("eac3") {
                Some("eac3")
            } else {
                None
            }
        }
        "vorbis" => {
            if caps.encoders.contains("libvorbis") {
                Some("libvorbis")
            } else if caps.encoders.contains("vorbis") {
                Some("vorbis")
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Check if a specific decoder is available.
pub fn has_decoder(name: &str) -> bool {
    detect_capabilities().decoders.contains(name)
}

/// Probe which CUVID decoders actually work on the current GPU hardware.
///
/// Returns a set of normalized codec names (e.g. `"av1"`, `"hevc"`, `"h264"`)
/// whose CUVID decoder successfully opened on the GPU.  Codecs that are compiled
/// into FFmpeg but not supported by the physical GPU (e.g. AV1 on Turing) are
/// excluded.
///
/// Mechanism: FFmpeg's `cuvid_decode_init()` calls `cuvidGetDecoderCaps()` during
/// `avcodec_open2`, which returns `bIsSupported=false` on GPUs that lack hardware
/// support for a particular codec, causing `open()` to fail.
///
/// FFmpeg log output is silenced to `AV_LOG_FATAL` during probing to suppress
/// expected error messages for unsupported codecs.
pub fn probe_cuvid_hw_codecs() -> HashSet<String> {
    use rsmpeg::avcodec::{AVCodec, AVCodecContext};
    use std::ffi::CString;

    let Some(device_ctx) = crate::transcode::hw::HwAccel::get_or_create_device_ctx(crate::transcode::hw::HwType::Cuda)
    else {
        return HashSet::new();
    };

    const CANDIDATES: &[(&str, &str)] = &[
        ("h264_cuvid", "h264"),
        ("hevc_cuvid", "hevc"),
        ("vp9_cuvid", "vp9"),
        ("av1_cuvid", "av1"),
        ("mpeg2_cuvid", "mpeg2video"),
        ("mpeg4_cuvid", "mpeg4"),
        ("vc1_cuvid", "vc1"),
        ("vp8_cuvid", "vp8"),
    ];

    let orig_level = unsafe { ffi::av_log_get_level() };
    unsafe { ffi::av_log_set_level(ffi::AV_LOG_FATAL as i32) };

    let mut supported = HashSet::new();

    for &(decoder_name, codec_key) in CANDIDATES {
        let c_name = CString::new(decoder_name).expect("decoder name contains no NUL");
        let Some(codec) = AVCodec::find_decoder_by_name(c_name.as_c_str()) else {
            continue;
        };
        let mut ctx = AVCodecContext::new(&codec);
        ctx.set_hw_device_ctx(device_ctx.clone());
        let ok = ctx.open(None).is_ok();
        if ok {
            supported.insert(codec_key.to_string());
        }
    }

    unsafe { ffi::av_log_set_level(orig_level) };

    supported
}

/// Get the CUVID decoder name for the given source video codec.
/// Returns None if the codec is not supported by CUVID.
/// Matches Jellyfin's `GetNvdecVidDecoder()`.
///
/// When `caps.cuvid_hw_codecs` is non-empty (populated after startup probe),
/// this also checks that the GPU actually supports the codec at runtime.
pub fn get_cuvid_decoder(video_codec: &str, caps: &HwCapabilities) -> Option<&'static str> {
    if !caps.has_cuvid {
        return None;
    }
    let normalized = super::codec::normalize_video_codec(video_codec);
    let (decoder, codec_key) = match normalized.as_str() {
        "h264" => ("h264_cuvid", "h264"),
        "hevc" => ("hevc_cuvid", "hevc"),
        "vp9" => ("vp9_cuvid", "vp9"),
        "av1" => ("av1_cuvid", "av1"),
        "mpeg2video" => ("mpeg2_cuvid", "mpeg2video"),
        "mpeg4" => ("mpeg4_cuvid", "mpeg4"),
        "vc1" => ("vc1_cuvid", "vc1"),
        "vp8" => ("vp8_cuvid", "vp8"),
        _ => return None,
    };
    if !caps.decoders.contains(decoder) {
        return None;
    }
    // If the startup hardware probe ran, check actual GPU support.
    // If it hasn't run yet (empty set), fall back to decoder-list check only.
    if !caps.cuvid_hw_codecs.is_empty() && !caps.cuvid_hw_codecs.contains(codec_key) {
        return None;
    }
    Some(decoder)
}

// ── Per-backend HW decode support checks ────────────────────────────────────
//
// These return `true` when the **source codec** can be decoded by the respective
// hardware accelerator.  Unlike CUVID, VAAPI/QSV/VideoToolbox use the generic
// `-hwaccel` path — there are no separate named decoders.
//
// Codec lists mirror Jellyfin's `GetVaapiVidDecoder`, `GetQsvHwVidDecoder`,
// `GetVideotoolboxVidDecoder`, and `GetAmfVidDecoder`.

/// Returns `true` if the source codec has VAAPI hardware acceleration support.
/// Jellyfin `GetVaapiVidDecoder` — subset safe for 8/10/12-bit yuv420.
pub fn vaapi_decode_supported(video_codec: &str) -> bool {
    let n = super::codec::normalize_video_codec(video_codec);
    matches!(
        n.as_str(),
        "h264" | "hevc" | "mpeg2video" | "vc1" | "vp8" | "vp9" | "av1"
    )
}

/// Returns `true` if the source codec has QSV hardware acceleration support.
/// Jellyfin `GetQsvHwVidDecoder`.
pub fn qsv_decode_supported(video_codec: &str) -> bool {
    let n = super::codec::normalize_video_codec(video_codec);
    matches!(
        n.as_str(),
        "h264" | "hevc" | "mpeg2video" | "vc1" | "vp8" | "vp9" | "av1"
    )
}

/// Returns `true` if the source codec has `VideoToolbox` hardware acceleration.
/// Jellyfin `GetVideotoolboxVidDecoder`.
pub fn videotoolbox_decode_supported(video_codec: &str) -> bool {
    let n = super::codec::normalize_video_codec(video_codec);
    matches!(n.as_str(), "h264" | "hevc" | "vp8" | "vp9" | "av1")
}

/// Returns `true` if the source codec can be HW-decoded for AMD AMF pipelines.
/// On Windows, AMF uses d3d11va for decode.  On Linux, SW decode is used.
/// Jellyfin `GetAmfVidDecoder` — only h264/mpeg2/vc1/hevc/vp9/av1.
pub fn amf_decode_supported(video_codec: &str) -> bool {
    let n = super::codec::normalize_video_codec(video_codec);
    matches!(n.as_str(), "h264" | "mpeg2video" | "vc1" | "hevc" | "vp9" | "av1")
}
