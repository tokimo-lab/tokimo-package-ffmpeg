use crate::error::{Error, Result, ResultExt};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avformat::AVFormatContextInput,
    avutil::AVChannelLayout,
    ffi,
};
use std::ffi::CString;

use super::super::types::{AudioTargets, StreamMapping, TranscodeOptions, VideoTargets};
use super::analyze::{DecodeContexts, StreamAnalysis};
use super::config::PassConfig;

/// Per-stream encode targets computed from analysis + configuration.
pub(super) struct StreamTargets {
    pub video: Vec<Option<VideoTargets>>,
    pub audio: Vec<Option<AudioTargets>>,
}

/// Compute video/audio target formats for each mapped stream.
pub(super) fn determine_targets(
    analysis: &StreamAnalysis,
    dec_ctxs: &DecodeContexts,
    cfg: &PassConfig,
    ifmt_ctx: &AVFormatContextInput,
    opts: &TranscodeOptions,
) -> Result<StreamTargets> {
    let mut video: Vec<Option<VideoTargets>> = (0..analysis.nb_streams).map(|_| None).collect();
    let mut audio: Vec<Option<AudioTargets>> = (0..analysis.nb_streams).map(|_| None).collect();

    for (i, mapping) in analysis.stream_map.iter().enumerate() {
        match mapping {
            StreamMapping::Video {
                gpu_pipeline: stream_gpu_pipeline,
                ..
            } => {
                let codec_name = cfg.video_codec_name.as_deref().context("video_codec_name not set")?;
                let c_codec_name = CString::new(codec_name)?;
                let encoder = AVCodec::find_encoder_by_name(&c_codec_name)
                    .ok_or_else(|| Error::Other(format!("Encoder '{codec_name}' not found")))?;
                let dec_ctx = dec_ctxs[i]
                    .as_ref()
                    .context("decode context missing for video stream")?;
                let tmp_enc = AVCodecContext::new(&encoder);

                let (pix_fmt, sw_pix_fmt, gpu_pipeline) =
                    if cfg.encode_hw.is_some_and(|ht| ht.is_hw_encoder(codec_name)) && *stream_gpu_pipeline {
                        let eht = cfg.encode_hw.context("encode_hw not set for GPU pipeline")?;
                        let src_sw = cfg.decode_hw.unwrap_or(eht).sw_format(
                            ifmt_ctx
                                .streams()
                                .get(i)
                                .context("stream index out of bounds")?
                                .codecpar()
                                .format,
                        );
                        let sw =
                            if codec_name.contains("hevc") || codec_name.contains("h265") || codec_name.contains("av1")
                            {
                                if eht.accepts_p010() {
                                    src_sw
                                } else {
                                    ffi::AV_PIX_FMT_NV12
                                }
                            } else {
                                ffi::AV_PIX_FMT_NV12
                            };
                        tracing::debug!(
                            "[transcode]   video[{}]: GPU pipeline, pix_fmt={}, sw_pix_fmt={}",
                            i,
                            eht.pix_fmt(),
                            sw
                        );
                        (eht.pix_fmt(), sw, true)
                    } else if cfg.encode_hw.is_some_and(|ht| ht.is_hw_encoder(codec_name)) && cfg.encode_hw.is_some() {
                        tracing::debug!("[transcode]   video[{}]: HW encode, SW decode, nv12", i);
                        (ffi::AV_PIX_FMT_NV12, ffi::AV_PIX_FMT_NV12, false)
                    } else {
                        let fmt = {
                            let pf = unsafe { (*tmp_enc.codec().as_ptr()).pix_fmts };
                            if pf.is_null() { dec_ctx.pix_fmt } else { unsafe { *pf } }
                        };
                        tracing::debug!("[transcode]   video[{}]: SW pipeline, pix_fmt={}", i, fmt);
                        (fmt, fmt, false)
                    };
                let (out_w, out_h) = cfg.resolution.unwrap_or((dec_ctx.width, dec_ctx.height));
                video[i] = Some(VideoTargets {
                    codec_name: codec_name.to_string(),
                    pix_fmt,
                    sw_pix_fmt,
                    gpu_pipeline,
                    width: out_w,
                    height: out_h,
                });
            }
            StreamMapping::Audio { .. } => {
                let c_codec_name = CString::new(opts.audio_codec.clone())?;
                let encoder = AVCodec::find_encoder_by_name(&c_codec_name)
                    .ok_or_else(|| Error::Other(format!("Audio encoder '{}' not found", opts.audio_codec)))?;
                let dec_ctx = dec_ctxs[i]
                    .as_ref()
                    .context("decode context missing for audio stream")?;
                let tmp_enc = AVCodecContext::new(&encoder);

                let sample_fmt = {
                    let sf = unsafe { (*tmp_enc.codec().as_ptr()).sample_fmts };
                    if sf.is_null() {
                        dec_ctx.sample_fmt
                    } else {
                        unsafe { *sf }
                    }
                };
                let target_ch_layout = if let Some(ac) = opts.audio_channels {
                    AVChannelLayout::from_nb_channels(ac)
                } else {
                    AVChannelLayout::from_nb_channels(dec_ctx.ch_layout.nb_channels)
                };
                audio[i] = Some(AudioTargets {
                    codec_name: opts.audio_codec.clone(),
                    sample_fmt,
                    sample_rate: dec_ctx.sample_rate,
                    ch_layout: target_ch_layout,
                });
            }
            _ => {}
        }
    }

    Ok(StreamTargets { video, audio })
}
