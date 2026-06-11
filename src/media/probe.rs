use crate::ResultExt;
use crate::error::Result;
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avformat::AVFormatContextInput,
    avutil::{AVDictionary, AVPixFmtDescriptorRef, av_q2d, get_pix_fmt_name, get_sample_fmt_name},
    ffi,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fmt::Write as _;

// ── Data model (matches CLI ffprobe JSON output) ────────────────────────────

#[derive(Debug, Serialize)]
pub struct MediaInfo {
    pub streams: Vec<StreamInfo>,
    pub format: FormatInfo,
    pub chapters: Vec<ChapterInfo>,
}

#[derive(Debug, Serialize)]
pub struct FormatInfo {
    pub filename: String,
    pub nb_streams: i32,
    pub nb_programs: i32,
    pub format_name: String,
    pub format_long_name: String,
    pub start_time: String,
    pub duration: String,
    pub size: String,
    pub bit_rate: String,
    pub probe_score: i32,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

impl FormatInfo {
    pub fn duration_secs(&self) -> f64 {
        self.duration.parse().unwrap_or(0.0)
    }
}

#[derive(Debug, Serialize)]
pub struct StreamInfo {
    pub index: i32,
    pub codec_name: String,
    pub codec_long_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub codec_type: String,
    pub codec_tag_string: String,
    pub codec_tag: String,

    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub video: Option<VideoFields>,

    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioFields>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    pub r_frame_rate: String,
    pub avg_frame_rate: String,
    pub time_base: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_pts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ts: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub bit_rate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nb_frames: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub extradata_size: Option<i32>,

    pub disposition: BTreeMap<String, i32>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub side_data_list: Option<Vec<SideDataInfo>>,

    #[serde(skip)]
    pub has_hdr10_plus: bool,
}

#[derive(Debug, Serialize)]
pub struct VideoFields {
    pub width: i32,
    pub height: i32,
    pub coded_width: i32,
    pub coded_height: i32,
    #[serde(rename = "pix_fmt")]
    pub pixel_format: String,
    pub level: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_space: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_transfer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color_primaries: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_aspect_ratio: Option<String>,
    pub sample_aspect_ratio: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_order: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chroma_location: Option<String>,
    pub closed_captions: i32,
    pub film_grain: i32,
    pub has_b_frames: i32,
    pub refs: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_ids_available: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_pos_available: Option<String>,

    #[serde(skip)]
    pub bit_depth: u8,
}

#[derive(Debug, Serialize)]
pub struct AudioFields {
    #[serde(rename = "sample_fmt")]
    pub sample_format: String,
    pub sample_rate: String,
    pub channels: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_layout: Option<String>,
    pub bits_per_sample: i32,
}

#[derive(Debug, Serialize)]
pub struct ChapterInfo {
    pub id: i64,
    pub time_base: String,
    pub start: i64,
    pub start_time: String,
    pub end: i64,
    pub end_time: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub tags: BTreeMap<String, String>,
}

#[derive(Debug, Serialize)]
pub struct SideDataInfo {
    pub side_data_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_version_major: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_version_minor: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_profile: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dv_level: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpu_present_flag: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub el_present_flag: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bl_present_flag: Option<i32>,
    #[serde(rename = "dv_bl_signal_compatibility_id")]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bl_signal_compatibility_id: Option<i32>,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn format_duration(seconds: f64) -> String {
    if seconds < 0.0 {
        return "N/A".to_string();
    }
    let total_secs = seconds as u64;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    let millis = ((seconds - total_secs as f64) * 1000.0) as u64;
    format!("{hours:02}:{mins:02}:{secs:02}.{millis:03}")
}

fn media_type_name(codec_type: ffi::AVMediaType) -> &'static str {
    match codec_type {
        x if x == ffi::AVMEDIA_TYPE_VIDEO => "video",
        x if x == ffi::AVMEDIA_TYPE_AUDIO => "audio",
        x if x == ffi::AVMEDIA_TYPE_SUBTITLE => "subtitle",
        x if x == ffi::AVMEDIA_TYPE_DATA => "data",
        x if x == ffi::AVMEDIA_TYPE_ATTACHMENT => "attachment",
        _ => "unknown",
    }
}

fn color_space_name(cs: i32) -> Option<String> {
    let name = match cs {
        1 => "bt709",
        5 => "bt470bg",
        6 => "smpte170m",
        7 => "smpte240m",
        8 => "ycgco",
        9 => "bt2020nc",
        10 => "bt2020c",
        12 => "ictcp",
        _ => return None,
    };
    Some(name.to_string())
}

fn color_range_name(cr: i32) -> Option<String> {
    let name = match cr {
        1 => "tv",
        2 => "pc",
        _ => return None,
    };
    Some(name.to_string())
}

fn color_primaries_name(cp: i32) -> Option<String> {
    let name = match cp {
        1 => "bt709",
        4 => "bt470m",
        5 => "bt470bg",
        6 => "smpte170m",
        7 => "smpte240m",
        9 => "bt2020",
        11 => "smpte428",
        12 => "smpte431",
        13 => "smpte432",
        _ => return None,
    };
    Some(name.to_string())
}

fn color_transfer_name(ct: i32) -> Option<String> {
    let name = match ct {
        1 => "bt709",
        4 => "gamma22",
        5 => "gamma28",
        6 => "smpte170m",
        7 => "smpte240m",
        8 => "linear",
        14 => "bt2020-10",
        15 => "bt2020-12",
        16 => "smpte2084",
        18 => "arib-std-b67",
        _ => return None,
    };
    Some(name.to_string())
}

fn field_order_name(order: i32) -> Option<String> {
    let name = match order {
        1 => "progressive",
        2 => "tt",
        3 => "bb",
        4 => "tb",
        5 => "bt",
        _ => return None,
    };
    Some(name.to_string())
}

fn chroma_location_name(loc: i32) -> Option<String> {
    let name = match loc {
        1 => "left",
        2 => "center",
        3 => "topleft",
        4 => "top",
        5 => "bottomleft",
        6 => "bottom",
        _ => return None,
    };
    Some(name.to_string())
}

fn decode_disposition(flags: i32) -> BTreeMap<String, i32> {
    let checks: &[(i32, &str)] = &[
        (0x0001, "default"),
        (0x0002, "dub"),
        (0x0004, "original"),
        (0x0008, "comment"),
        (0x0010, "lyrics"),
        (0x0020, "karaoke"),
        (0x0040, "forced"),
        (0x0080, "hearing_impaired"),
        (0x0100, "visual_impaired"),
        (0x0200, "clean_effects"),
        (0x0400, "attached_pic"),
        (0x0800, "timed_thumbnails"),
        (0x1000, "non_diegetic"),
        (0x10000, "captions"),
        (0x20000, "descriptions"),
        (0x40000, "metadata"),
        (0x80000, "dependent"),
        (0x0010_0000, "still_image"),
        (0x0020_0000, "multilayer"),
    ];
    let mut map = BTreeMap::new();
    for &(flag, name) in checks {
        map.insert(name.to_string(), i32::from(flags & flag != 0));
    }
    map
}

fn get_bit_depth(pix_fmt: i32) -> u8 {
    AVPixFmtDescriptorRef::get(pix_fmt).map_or(8, |desc| desc.comp[0].depth as u8)
}

fn rational_to_string(r: ffi::AVRational) -> String {
    if r.den == 0 {
        "0/0".to_string()
    } else {
        format!("{}/{}", r.num, r.den)
    }
}

fn compute_dar(w: i32, h: i32, sar: ffi::AVRational) -> Option<String> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let sar_num = if sar.num > 0 { sar.num } else { 1 };
    let sar_den = if sar.den > 0 { sar.den } else { 1 };
    let dar_num = i64::from(w) * i64::from(sar_num);
    let dar_den = i64::from(h) * i64::from(sar_den);
    let g = gcd(dar_num.unsigned_abs(), dar_den.unsigned_abs()) as i64;
    if g == 0 {
        return None;
    }
    Some(format!("{}:{}", dar_num / g, dar_den / g))
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

fn fourcc_to_string(tag: u32) -> String {
    let bytes = tag.to_le_bytes();
    let mut result = String::new();
    for b in bytes {
        if b > 0x20 && b < 0x7f {
            result.push(b as char);
        } else {
            write!(result, "[{b}]").expect("write to String");
        }
    }
    result
}

/// Read a string-type `AVOption` from the decoder's private data.
/// Returns `Some(value)` if the option exists, `None` otherwise.
fn read_decoder_opt_string(ctx: &AVCodecContext, name: &str) -> Option<String> {
    let c_name = CString::new(name).ok()?;
    let mut out: *mut u8 = std::ptr::null_mut();
    unsafe {
        // Search the codec context and its children (including priv_data)
        // using AV_OPT_SEARCH_CHILDREN. This avoids passing raw priv_data
        // to av_opt_get, which can segfault if the codec's priv_data doesn't
        // start with a valid AVClass*.
        let ret = ffi::av_opt_get(
            ctx.as_ptr() as *mut _,
            c_name.as_ptr(),
            ffi::AV_OPT_SEARCH_CHILDREN as i32,
            &raw mut out,
        );
        if ret < 0 || out.is_null() {
            return None;
        }
        let s = CStr::from_ptr(out as *const _).to_string_lossy().into_owned();
        ffi::av_free(out.cast());
        Some(s)
    }
}

// ── Probing ─────────────────────────────────────────────────────────────────

pub fn probe_file(input: &str) -> Result<MediaInfo> {
    let c_path = CString::new(input).context("Input contains null bytes")?;

    // Use larger probesize/analyzeduration so streams with late codec parameters
    // (e.g. PGS/pgssub in MKV files with many subtitle tracks) are fully detected.
    let mut probe_opts = Some(AVDictionary::new(c"probesize", c"50000000", 0).set(c"analyzeduration", c"50000000", 0));
    let input_ctx = AVFormatContextInput::builder()
        .url(&c_path)
        .options(&mut probe_opts)
        .open()
        .context("Failed to open input")?;

    let size_str = if input.contains("://") {
        "0".to_string()
    } else {
        std::fs::metadata(input)
            .ok()
            .map_or_else(|| "0".to_string(), |m| m.len().to_string())
    };

    Ok(probe_format_ctx(input_ctx, input, &size_str))
}

/// Core probing logic shared by `probe_file` and `probe_direct`.
/// `filename_hint` is stored in `FormatInfo.filename`; `size_str` is the
/// file-size string (pass `"0"` when probing over AVIO).
#[allow(clippy::too_many_lines)]
pub(crate) fn probe_format_ctx(input_ctx: AVFormatContextInput, filename_hint: &str, size_str: &str) -> MediaInfo {
    let (format_info, mut streams, chapters) = {
        // Format-level info
        let (format_name, format_long_name) = {
            let iformat = input_ctx.iformat();
            (
                iformat.name().to_string_lossy().into_owned(),
                iformat.long_name().to_string_lossy().into_owned(),
            )
        };
        let duration_us = input_ctx.duration;
        let duration_secs = if duration_us > 0 {
            duration_us as f64 / f64::from(ffi::AV_TIME_BASE)
        } else {
            0.0
        };
        let start_time_raw = input_ctx.start_time;
        let start_time_secs = if start_time_raw == ffi::AV_NOPTS_VALUE {
            0.0
        } else {
            start_time_raw as f64 / f64::from(ffi::AV_TIME_BASE)
        };
        let bit_rate = input_ctx.bit_rate;

        let (nb_programs, probe_score) = unsafe {
            let raw = input_ctx.as_ptr();
            ((*raw).nb_programs as i32, (*raw).probe_score)
        };

        let format_tags = extract_tags(input_ctx.metadata());

        // Stream info
        let mut streams = Vec::new();
        for (i, stream) in input_ctx.streams().iter().enumerate() {
            let codecpar = stream.codecpar();
            let codec_type_raw = codecpar.codec_type;
            let codec_type = codecpar.codec_type();
            let codec_id = codecpar.codec_id;
            let codec_type_str = media_type_name(codec_type_raw).to_string();

            let codec_tag = codecpar.codec_tag;
            let codec_tag_hex = format!("0x{codec_tag:04x}");
            let codec_tag_str = fourcc_to_string(codec_tag);

            // Open decoder context (mirrors CLI ffprobe's open_input_file).
            // codec_name uses avcodec_descriptor_get()->name, identical to CLI ffprobe.
            // decoder is only used for opening the decode context to extract profile/refs/hbf.
            let decoder = AVCodec::find_decoder(codec_id);

            let (codec_name, codec_long_name) = unsafe {
                let desc = ffi::avcodec_descriptor_get(codec_id);
                if desc.is_null() {
                    (format!("unknown(0x{codec_id:x})"), "Unknown codec".to_string())
                } else {
                    let name = CStr::from_ptr((*desc).name).to_string_lossy().into_owned();
                    let long_name = if (*desc).long_name.is_null() {
                        String::new()
                    } else {
                        CStr::from_ptr((*desc).long_name).to_string_lossy().into_owned()
                    };
                    (name, long_name)
                }
            };

            // Open decoder: avcodec_alloc_context3 + parameters_to_context + avcodec_open2
            // Used only for coded_width/coded_height and refs (decoder-private values).
            // profile, has_b_frames use codecpar directly — same as CLI ffprobe.
            let mut dec_ctx_opt: Option<AVCodecContext> = None;
            let mut rv = 1i32;

            // profile: avcodec_profile_name(par->codec_id, par->profile) — CLI ffprobe line 3314
            let profile_str = unsafe {
                if codecpar.profile >= 0 {
                    let ptr = ffi::avcodec_profile_name(codec_id, codecpar.profile);
                    if ptr.is_null() {
                        None
                    } else {
                        Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
                    }
                } else {
                    None
                }
            };

            // has_b_frames: par->video_delay — CLI ffprobe line 3343
            let hbf = unsafe { (*codecpar.as_ptr()).video_delay };

            if let Some(codec) = &decoder {
                let mut dec_ctx = AVCodecContext::new(codec);
                if dec_ctx.apply_codecpar(&codecpar).is_ok() {
                    unsafe {
                        (*dec_ctx.as_mut_ptr()).pkt_timebase = stream.time_base;
                        (*dec_ctx.as_mut_ptr()).thread_count = 1;
                    }
                    if dec_ctx.open(None).is_ok() {
                        unsafe {
                            rv = (*dec_ctx.as_ptr()).refs;
                        }
                        dec_ctx_opt = Some(dec_ctx);
                    } else {
                        unsafe {
                            rv = (*dec_ctx.as_ptr()).refs;
                        }
                    }
                }
            }

            let video = if codec_type.is_video() {
                let pix_fmt_name = get_pix_fmt_name(codecpar.format).map_or_else(
                    || format!("unknown({})", codecpar.format),
                    |s| s.to_string_lossy().into_owned(),
                );
                let sar = codecpar.sample_aspect_ratio;
                let sar_str = if sar.num > 0 && sar.den > 0 {
                    format!("{}:{}", sar.num, sar.den)
                } else {
                    "1:1".to_string()
                };
                let dar_str = compute_dar(codecpar.width, codecpar.height, sar);
                #[allow(clippy::unnecessary_cast)] // cast required on Windows-MSVC, redundant on Linux
                let field_order = unsafe { field_order_name((*codecpar.as_ptr()).field_order as i32) };
                #[allow(clippy::unnecessary_cast)]
                let chroma_loc = unsafe { chroma_location_name((*codecpar.as_ptr()).chroma_location as i32) };

                // coded_width/coded_height from opened decoder (more accurate with cropping)
                let (cw, ch) = dec_ctx_opt
                    .as_ref()
                    .map_or((codecpar.width, codecpar.height), |ctx| unsafe {
                        let raw = ctx.as_ptr();
                        ((*raw).coded_width, (*raw).coded_height)
                    });

                // view_ids_available / view_pos_available: decoder private options
                // Only present on codecs that support multiview (HEVC, H.264)
                let (view_ids, view_pos) = if let Some(ref ctx) = dec_ctx_opt {
                    (
                        read_decoder_opt_string(ctx, "view_ids_available"),
                        read_decoder_opt_string(ctx, "view_pos_available"),
                    )
                } else {
                    (None, None)
                };

                Some(VideoFields {
                    width: codecpar.width,
                    height: codecpar.height,
                    coded_width: cw,
                    coded_height: ch,
                    pixel_format: pix_fmt_name,
                    level: codecpar.level,
                    #[allow(clippy::unnecessary_cast)] // cast required on Windows-MSVC, redundant on Linux
                    color_range: color_range_name(codecpar.color_range as i32),
                    #[allow(clippy::unnecessary_cast)]
                    color_space: color_space_name(codecpar.color_space as i32),
                    #[allow(clippy::unnecessary_cast)]
                    color_primaries: color_primaries_name(codecpar.color_primaries as i32),
                    #[allow(clippy::unnecessary_cast)]
                    color_transfer: color_transfer_name(codecpar.color_trc as i32),
                    display_aspect_ratio: dar_str,
                    sample_aspect_ratio: sar_str,
                    field_order,
                    chroma_location: chroma_loc,
                    closed_captions: 0,
                    film_grain: 0,
                    has_b_frames: hbf,
                    refs: rv,
                    view_ids_available: view_ids,
                    view_pos_available: view_pos,
                    bit_depth: get_bit_depth(codecpar.format),
                })
            } else {
                None
            };

            let audio = if codec_type.is_audio() {
                let ch_layout = codecpar.ch_layout();
                let nb_channels = codecpar.ch_layout.nb_channels;
                let channel_layout_str = ch_layout
                    .describe()
                    .map(|s| s.to_string_lossy().into_owned())
                    .ok()
                    .filter(|s| !s.is_empty());
                let sample_fmt_name = get_sample_fmt_name(codecpar.format).map_or_else(
                    || format!("unknown({})", codecpar.format),
                    |s| s.to_string_lossy().into_owned(),
                );
                Some(AudioFields {
                    sample_format: sample_fmt_name,
                    sample_rate: codecpar.sample_rate.to_string(),
                    channels: nb_channels,
                    channel_layout: channel_layout_str,
                    bits_per_sample: codecpar.bits_per_raw_sample,
                })
            } else {
                None
            };

            // Stream-level timing
            let start_pts = if stream.start_time == ffi::AV_NOPTS_VALUE {
                None
            } else {
                Some(stream.start_time)
            };
            let stream_start_time = start_pts.map(|pts| format!("{:.6}", pts as f64 * av_q2d(stream.time_base)));

            let duration_ts = if stream.duration > 0 {
                Some(stream.duration)
            } else {
                None
            };
            let stream_duration = duration_ts.map(|d| format!("{:.6}", d as f64 * av_q2d(stream.time_base)));

            let stream_bit_rate = if codecpar.bit_rate > 0 {
                Some(codecpar.bit_rate.to_string())
            } else {
                None
            };

            let nb_frames = if stream.nb_frames > 0 {
                Some(stream.nb_frames.to_string())
            } else {
                None
            };

            let stream_id = if stream.id != 0 {
                Some(format!("0x{:x}", stream.id))
            } else {
                None
            };

            let extradata_sz = unsafe { (*codecpar.as_ptr()).extradata_size };
            let extradata_size = if extradata_sz > 0 { Some(extradata_sz) } else { None };

            let stream_tags = extract_tags(stream.metadata());
            let disp = decode_disposition(stream.disposition);
            let side_data = extract_coded_side_data(&codecpar);
            let side_data_list = if side_data.is_empty() { None } else { Some(side_data) };

            streams.push(StreamInfo {
                index: i as i32,
                codec_name,
                codec_long_name,
                profile: profile_str,
                codec_type: codec_type_str,
                codec_tag_string: codec_tag_str,
                codec_tag: codec_tag_hex,
                video,
                audio,
                id: stream_id,
                r_frame_rate: rational_to_string(stream.r_frame_rate),
                avg_frame_rate: rational_to_string(stream.avg_frame_rate),
                time_base: rational_to_string(stream.time_base),
                start_pts,
                start_time: stream_start_time,
                duration_ts,
                duration: stream_duration,
                bit_rate: stream_bit_rate,
                nb_frames,
                extradata_size,
                disposition: disp,
                tags: stream_tags,
                side_data_list,
                has_hdr10_plus: false,
            });
        }

        let chapters = extract_chapters(&input_ctx);
        let nb_streams = streams.len() as i32;

        let format_info = FormatInfo {
            filename: filename_hint.to_string(),
            nb_streams,
            nb_programs,
            format_name,
            format_long_name,
            start_time: format!("{start_time_secs:.6}"),
            duration: format!("{duration_secs:.6}"),
            size: size_str.to_string(),
            bit_rate: bit_rate.to_string(),
            probe_score,
            tags: format_tags,
        };

        (format_info, streams, chapters)
    };

    // HDR10+ detection: scan first video packets
    detect_hdr10_plus(&mut streams, input_ctx);

    MediaInfo {
        streams,
        format: format_info,
        chapters,
    }
}

fn extract_coded_side_data(codecpar: &rsmpeg::avcodec::AVCodecParametersRef) -> Vec<SideDataInfo> {
    let mut result = Vec::new();
    unsafe {
        let raw = codecpar.as_ptr();
        let nb = (*raw).nb_coded_side_data;
        let ptr = (*raw).coded_side_data;
        if ptr.is_null() || nb <= 0 {
            return result;
        }

        for i in 0..nb as isize {
            let sd = &*ptr.offset(i);
            let type_name = {
                let name_ptr = ffi::av_packet_side_data_name(sd.type_);
                if name_ptr.is_null() {
                    format!("unknown({})", sd.type_)
                } else {
                    CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
                }
            };

            let mut info = SideDataInfo {
                side_data_type: type_name,
                dv_version_major: None,
                dv_version_minor: None,
                dv_profile: None,
                dv_level: None,
                rpu_present_flag: None,
                el_present_flag: None,
                bl_present_flag: None,
                bl_signal_compatibility_id: None,
            };

            if sd.type_ == ffi::AV_PKT_DATA_DOVI_CONF
                && sd.size >= std::mem::size_of::<ffi::AVDOVIDecoderConfigurationRecord>()
            {
                let dovi = &*(sd.data as *const ffi::AVDOVIDecoderConfigurationRecord);
                info.dv_version_major = Some(i32::from(dovi.dv_version_major));
                info.dv_version_minor = Some(i32::from(dovi.dv_version_minor));
                info.dv_profile = Some(i32::from(dovi.dv_profile));
                info.dv_level = Some(i32::from(dovi.dv_level));
                info.rpu_present_flag = Some(i32::from(dovi.rpu_present_flag));
                info.el_present_flag = Some(i32::from(dovi.el_present_flag));
                info.bl_present_flag = Some(i32::from(dovi.bl_present_flag));
                info.bl_signal_compatibility_id = Some(i32::from(dovi.dv_bl_signal_compatibility_id));
            }

            result.push(info);
        }
    }
    result
}

/// Scan first video packets for HDR10+ dynamic metadata (type 31).
fn detect_hdr10_plus(streams: &mut [StreamInfo], mut input_ctx: AVFormatContextInput) {
    let video_stream_idx = streams.iter().position(|s| s.video.is_some());
    let Some(video_idx) = video_stream_idx else {
        return;
    };

    const AV_PKT_DATA_DYNAMIC_HDR10_PLUS: ffi::AVPacketSideDataType = 31;
    const MAX_PACKETS: usize = 32;

    let mut found = false;
    for _ in 0..MAX_PACKETS {
        let Ok(Some(pkt)) = input_ctx.read_packet() else { break };

        if pkt.stream_index as usize != video_idx {
            continue;
        }

        unsafe {
            let raw = pkt.as_ptr();
            let sd_count = (*raw).side_data_elems;
            let sd_ptr = (*raw).side_data;
            if !sd_ptr.is_null() && sd_count > 0 {
                for i in 0..sd_count as isize {
                    let sd = &*sd_ptr.offset(i);
                    if sd.type_ == AV_PKT_DATA_DYNAMIC_HDR10_PLUS {
                        found = true;
                        break;
                    }
                }
            }
        }

        if found {
            break;
        }
    }

    if found {
        streams[video_idx].has_hdr10_plus = true;
    }
}

/// Decode a CStr to a Rust String, handling non-UTF-8 byte sequences.
///
/// FFmpeg's AVDictionary stores tag values as C strings. For ID3v1 tags and
/// some ID3v2 tags, the bytes may be Latin-1, GBK, or other legacy encodings
/// rather than UTF-8. We try encodings in order of likelihood:
/// 1. UTF-8 (fast path — most modern tags)
/// 2. GBK (common for Chinese ID3v1 tags)
/// 3. Latin-1 (ID3v1 default, FFmpeg fallback)
fn decode_cstr_lossy(cstr: &CStr) -> String {
    // Fast path: valid UTF-8
    if let Ok(s) = cstr.to_str() {
        // Even valid UTF-8 may contain Latin-1 chars from FFmpeg's ID3v1 handling.
        // If all non-ASCII chars are in the Latin-1 range (U+0080..U+00FF) and the
        // string looks like mojibake (Latin-1 interpretation of GBK bytes), try
        // re-interpreting as GBK.
        if s.chars().any(|c| (c as u32) >= 0x80 && (c as u32) <= 0xFF) {
            let latin1_bytes: Vec<u8> = s.chars().map(|c| c as u32 as u8).collect();
            let (gbk_decoded, _, gbk_had_errors) = encoding_rs::GBK.decode(&latin1_bytes);
            if !gbk_had_errors && gbk_decoded.chars().all(|c| !c.is_control() || c == '\t' || c == '\n') {
                return gbk_decoded.into_owned();
            }
        }
        return s.to_owned();
    }
    let bytes = cstr.to_bytes();
    if bytes.is_empty() {
        return String::new();
    }
    // Try GBK decoding (superset of GB2312, common for Chinese MP3 tags).
    // GBK produces replacement character U+FFFD for invalid byte sequences,
    // so if the result contains no replacements, it's likely correct.
    let (gbk_decoded, _, gbk_had_errors) = encoding_rs::GBK.decode(bytes);
    if !gbk_had_errors {
        return gbk_decoded.into_owned();
    }
    // Fallback: Latin-1 (ISO-8859-1) — every byte 0x00-0xFF is valid.
    // This is the default for ID3v1 and FFmpeg's demuxer fallback.
    let mut output = String::with_capacity(bytes.len());
    for &b in bytes {
        output.push(b as char);
    }
    output
}

fn extract_tags(metadata: Option<rsmpeg::avutil::AVDictionaryRef>) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    if let Some(dict) = metadata {
        for entry in dict.iter() {
            tags.insert(decode_cstr_lossy(entry.key()), decode_cstr_lossy(entry.value()));
        }
    }
    tags
}

fn extract_chapters(input_ctx: &AVFormatContextInput) -> Vec<ChapterInfo> {
    let mut chapters = Vec::new();
    let nb_chapters = input_ctx.nb_chapters as usize;
    if nb_chapters == 0 {
        return chapters;
    }

    unsafe {
        let raw = input_ctx.as_ptr();
        let ch_ptr = (*raw).chapters;
        if ch_ptr.is_null() {
            return chapters;
        }
        for i in 0..nb_chapters {
            let chapter = *ch_ptr.add(i);
            if chapter.is_null() {
                continue;
            }
            let ch = &*chapter;
            let tb = ch.time_base;
            let time_base_str = format!("{}/{}", tb.num, tb.den);
            let start_secs = ch.start as f64 * av_q2d(tb);
            let end_secs = ch.end as f64 * av_q2d(tb);

            // Extract all chapter tags (title, etc.)
            let mut tags = BTreeMap::new();
            if !ch.metadata.is_null() {
                let key = c"";
                let mut prev: *const ffi::AVDictionaryEntry = std::ptr::null();
                loop {
                    prev = ffi::av_dict_get(ch.metadata, key.as_ptr(), prev, 2);
                    if prev.is_null() {
                        break;
                    }
                    let k = decode_cstr_lossy(CStr::from_ptr((*prev).key));
                    let v = decode_cstr_lossy(CStr::from_ptr((*prev).value));
                    tags.insert(k, v);
                }
            }

            chapters.push(ChapterInfo {
                id: ch.id,
                time_base: time_base_str,
                start: ch.start,
                start_time: format!("{start_secs:.6}"),
                end: ch.end,
                end_time: format!("{end_secs:.6}"),
                tags,
            });
        }
    }
    chapters
}

// ── Display ─────────────────────────────────────────────────────────────────

pub fn print_info(info: &MediaInfo) {
    let fmt = &info.format;
    println!("Input: {}", fmt.filename);
    println!("  Format:     {} ({})", fmt.format_name, fmt.format_long_name);
    let dur = fmt.duration_secs();
    if dur > 0.0 {
        println!("  Duration:   {}", format_duration(dur));
    }
    let br: i64 = fmt.bit_rate.parse().unwrap_or(0);
    if br > 0 {
        println!("  Bitrate:    {} kb/s", br / 1000);
    }
    let size: u64 = fmt.size.parse().unwrap_or(0);
    if size > 0 {
        let size_mb = size as f64 / (1024.0 * 1024.0);
        if size_mb >= 1024.0 {
            println!("  Size:       {:.2} GB", size_mb / 1024.0);
        } else {
            println!("  Size:       {size_mb:.2} MB");
        }
    }
    println!("  Streams:    {}", fmt.nb_streams);

    if !fmt.tags.is_empty() {
        println!("  Metadata:");
        for (key, value) in &fmt.tags {
            println!("    {key}: {value}");
        }
    }

    println!();

    for stream in &info.streams {
        print!(
            "  Stream #{} ({}): {} - {}",
            stream.index, stream.codec_type, stream.codec_name, stream.codec_long_name
        );
        if let Some(ref profile) = stream.profile {
            print!(" ({profile})");
        }
        println!();

        if let Some(ref v) = stream.video {
            println!("    Resolution:    {}x{}", v.width, v.height);
            println!("    Pixel format:  {} ({}-bit)", v.pixel_format, v.bit_depth);
            if let Some(ref dar) = v.display_aspect_ratio {
                println!("    Aspect ratio:  {dar}");
            }
            if let Some(ref cs) = v.color_space {
                println!("    Color space:   {cs}");
            }
            if let Some(ref cr) = v.color_range {
                println!("    Color range:   {cr}");
            }
            if let Some(ref cp) = v.color_primaries {
                println!("    Color prims:   {cp}");
            }
            if let Some(ref ct) = v.color_transfer {
                println!("    Color TRC:     {ct}");
            }
        }

        if let Some(ref a) = stream.audio {
            println!("    Sample rate:   {} Hz", a.sample_rate);
            println!(
                "    Channels:      {}{}",
                a.channels,
                a.channel_layout
                    .as_deref()
                    .map(|l| format!(" ({l})"))
                    .unwrap_or_default()
            );
            println!("    Sample format: {}", a.sample_format);
            if a.bits_per_sample > 0 {
                println!("    Bit depth:     {}", a.bits_per_sample);
            }
        }

        if let Some(ref br) = stream.bit_rate {
            let br_val: i64 = br.parse().unwrap_or(0);
            if br_val > 0 {
                println!("    Bitrate:       {} kb/s", br_val / 1000);
            }
        }
        if let Some(ref dur) = stream.duration {
            let dur_val: f64 = dur.parse().unwrap_or(0.0);
            if dur_val > 0.0 {
                println!("    Duration:      {}", format_duration(dur_val));
            }
        }

        if !stream.tags.is_empty() {
            println!("    Tags:");
            for (key, value) in &stream.tags {
                println!("      {key}: {value}");
            }
        }
    }

    if !info.chapters.is_empty() {
        println!("\n  Chapters:");
        for ch in &info.chapters {
            let title = ch.tags.get("title").map_or("(no title)", std::string::String::as_str);
            println!("    #{}: {} → {} [{}]", ch.id, ch.start_time, ch.end_time, title);
        }
    }
}
