use crate::error::Result;
use rsmpeg::{avformat::AVFormatContextInput, ffi};

use super::super::encode::{parse_bitrate, parse_resolution};
use super::super::hw::{self, HwAccel, is_any_hw_encoder, software_fallback};
use super::super::types::{CancellationToken, TranscodeOptions};

/// Immutable configuration derived from [`TranscodeOptions`] + input context
/// metadata.  Computed once per pass, read by all subsequent phases.
///
/// Every field is fully initialized — no `Option` wrappers for "will be set
/// later" state.  Fields that are genuinely optional in the domain (e.g.
/// resolution override) use `Option` to express that.
pub(super) struct PassConfig {
    pub resolution: Option<(i32, i32)>,
    pub target_bitrate: Option<i64>,
    pub copy_video: bool,
    pub copy_audio: bool,
    pub decode_hw: Option<hw::HwType>,
    pub encode_hw: Option<hw::HwType>,
    pub video_codec_name: Option<String>,
    pub format_start_time: i64,
    pub format_start_secs: f64,
    pub cancel: Option<CancellationToken>,
}

impl PassConfig {
    pub fn new(
        ifmt_ctx: &AVFormatContextInput,
        opts: &TranscodeOptions,
        decode_accel: &Option<HwAccel>,
        encode_accel: &Option<HwAccel>,
    ) -> Result<Self> {
        let cancel = opts.cancel.clone();
        let resolution = opts.resolution.as_deref().map(parse_resolution).transpose()?;
        let target_bitrate = opts.bitrate.as_deref().map(parse_bitrate).transpose()?;
        let copy_video = opts.audio_only || opts.video_codec == "copy";
        let copy_audio = opts.audio_codec == "copy";
        let decode_hw = decode_accel.as_ref().map(|h| h.hw_type);
        let encode_hw = encode_accel.as_ref().map(|h| h.hw_type);

        let video_codec_name = if copy_video {
            None
        } else if is_any_hw_encoder(&opts.video_codec) {
            if encode_accel.is_some() {
                Some(opts.video_codec.clone())
            } else {
                let fallback = software_fallback(&opts.video_codec);
                tracing::info!("[transcode] HW encoder not available, falling back to {}", fallback);
                Some(fallback.to_string())
            }
        } else {
            Some(opts.video_codec.clone())
        };

        let format_start_time = if ifmt_ctx.start_time == ffi::AV_NOPTS_VALUE {
            0
        } else {
            ifmt_ctx.start_time
        };
        let format_start_secs = format_start_time as f64 / f64::from(ffi::AV_TIME_BASE);

        Ok(Self {
            resolution,
            target_bitrate,
            copy_video,
            copy_audio,
            decode_hw,
            encode_hw,
            video_codec_name,
            format_start_time,
            format_start_secs,
            cancel,
        })
    }
}
