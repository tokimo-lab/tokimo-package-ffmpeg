//! Session setup: hardware acceleration init + input file opening.
//!
//! This is the expensive part (~600ms over network) — done once per session
//! and reused across seek-restarts.

#[allow(unused_imports)]
use crate::bail;
use crate::error::{Error, Result};
use rsmpeg::{avformat::AVFormatContextInput, avutil::AVDictionary, ffi};
use std::ffi::CString;
use std::time::Instant;

use super::hw::{FilterBackend, HwAccel, HwPipeline};
use super::types::TranscodeOptions;
use super::vfs_io;

/// Shared state created once per session and reused across seek-restarts.
pub(crate) struct SessionSetup {
    pub ifmt_ctx: AVFormatContextInput,
    pub decode_accel: Option<HwAccel>,
    pub encode_accel: Option<HwAccel>,
    pub pipeline_cfg: HwPipeline,
    pub hw_init_ms: f64,
}

/// Open the input file and initialize HW acceleration.
/// This is the expensive part (~600ms over network) — done only once per session.
pub(crate) fn setup_session(opts: &TranscodeOptions) -> Result<SessionSetup> {
    let input_path = opts
        .input
        .to_str()
        .ok_or_else(|| Error::Other("Input path contains invalid UTF-8".into()))?;
    let c_input = CString::new(input_path)?;

    let copy_video = opts.audio_only || opts.video_codec == "copy";
    let copy_audio = opts.audio_codec == "copy";

    // ── Hardware acceleration ────────────────────────────────────────────
    let t_cuda_start = Instant::now();

    let pipeline_cfg = if copy_video {
        HwPipeline {
            decode: None,
            filter: FilterBackend::Software,
            encode: None,
            encoder_name: "copy".into(),
            fallback: super::hw::FallbackLevel::Software,
        }
    } else {
        super::hw::resolve_pipeline(
            &opts.video_codec,
            opts.decode.as_deref(),
            opts.filter_backend.as_deref(),
        )
    };

    let decode_accel = if let Some(dec_hw) = pipeline_cfg.decode {
        if let Some(ref cached_ctx) = opts.cached_device_ctx {
            Some(HwAccel {
                hw_type: dec_hw,
                device_ctx: cached_ctx.clone(),
            })
        } else {
            match HwAccel::try_init(dec_hw) {
                Some(hw) => Some(hw),
                None => {
                    bail!("{} decode device not available", dec_hw.display_name());
                }
            }
        }
    } else {
        None
    };

    let encode_accel = match pipeline_cfg.encode {
        Some(enc_hw) if pipeline_cfg.decode == Some(enc_hw) => decode_accel.as_ref().map(|d| HwAccel {
            hw_type: d.hw_type,
            device_ctx: d.device_ctx.clone(),
        }),
        Some(enc_hw) => {
            if let Some(ref cached_ctx) = opts.cached_device_ctx {
                Some(HwAccel {
                    hw_type: enc_hw,
                    device_ctx: cached_ctx.clone(),
                })
            } else {
                match HwAccel::try_init(enc_hw) {
                    Some(hw) => Some(hw),
                    None => {
                        bail!("{} encode device not available", enc_hw.display_name());
                    }
                }
            }
        }
        None => None,
    };

    let hw_init_ms = t_cuda_start.elapsed().as_secs_f64() * 1000.0;

    // ── Set FFmpeg log level ─────────────────────────────────────────────
    unsafe {
        ffi::av_log_set_level(ffi::AV_LOG_WARNING as i32);
    }

    // ── Open input ───────────────────────────────────────────────────────
    let mut probe_opts = if copy_video {
        None
    } else {
        Some(AVDictionary::new(c"probesize", c"1048576", 0).set(c"analyzeduration", c"1000000", 0))
    };
    let mut ifmt_ctx = if let Some(ref direct) = opts.direct_input {
        tracing::info!("[transcode] Input: AVIO direct");
        vfs_io::open_direct_input(direct.clone(), &mut probe_opts)?
    } else {
        tracing::info!("[transcode] Input: file {}", input_path);
        AVFormatContextInput::builder()
            .url(&c_input)
            .options(&mut probe_opts)
            .open()?
    };

    if copy_video || copy_audio {
        unsafe {
            (*ifmt_ctx.as_mut_ptr()).flags |= ffi::AVFMT_FLAG_GENPTS as i32;
        }
    }

    Ok(SessionSetup {
        ifmt_ctx,
        decode_accel,
        encode_accel,
        pipeline_cfg,
        hw_init_ms,
    })
}
