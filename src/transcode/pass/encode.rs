use crate::error::{Error, Result, ResultExt};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avformat::{AVFormatContextInput, AVFormatContextOutput},
    avutil::AVDictionary,
    ffi,
};
use std::ffi::CString;

use super::super::encode::{apply_video_encoder_options, parse_bitrate};
use super::super::filter::{self, OwnedFilterPipeline};
use super::super::hw::{HwAccel, HwPipeline, is_any_hw_encoder};
use super::super::types::{StreamMapping, TranscodeOptions};
use super::analyze::{DecodeContexts, StreamAnalysis};
use super::config::PassConfig;
use super::targets::StreamTargets;

/// Owns all filter and encoder resources for the transcode pipeline.
///
/// Both `filter_pipelines` and `enc_contexts` are indexed by input stream index.
/// Each `OwnedFilterPipeline` carries its own backing `AVFilterGraph`, so drop
/// order is guaranteed per-element (pipeline drops before graph inside the
/// wrapper).
pub(super) struct EncodePipeline {
    pub filter_pipelines: Vec<Option<OwnedFilterPipeline>>,
    pub enc_contexts: Vec<Option<AVCodecContext>>,
}

/// Build the complete encode pipeline: SW filters → encoders → GPU pipelines.
///
/// Output streams are added to `ofmt_ctx` as a side effect (one per mapped
/// stream).
#[allow(clippy::too_many_arguments)]
pub(super) fn create_pipeline(
    analysis: &StreamAnalysis,
    dec_ctxs: &mut DecodeContexts,
    targets: &StreamTargets,
    cfg: &PassConfig,
    opts: &TranscodeOptions,
    ifmt_ctx: &AVFormatContextInput,
    ofmt_ctx: &mut AVFormatContextOutput,
    decode_accel: &Option<HwAccel>,
    encode_accel: &Option<HwAccel>,
    pipeline_cfg: &HwPipeline,
) -> Result<EncodePipeline> {
    let nb_streams = analysis.nb_streams;
    let mut filter_pipelines: Vec<Option<OwnedFilterPipeline>> = (0..nb_streams).map(|_| None).collect();
    let mut enc_contexts: Vec<Option<AVCodecContext>> = (0..nb_streams).map(|_| None).collect();

    // ── Phase A: SW filter pipelines ─────────────────────────────────────
    init_sw_filters(analysis, dec_ctxs, targets, cfg, opts, &mut filter_pipelines)?;

    // ── Phase B: SW + copy encoder contexts + output streams ─────────────
    init_sw_encoders(
        analysis,
        dec_ctxs,
        targets,
        cfg,
        opts,
        ifmt_ctx,
        ofmt_ctx,
        encode_accel,
        &filter_pipelines,
        &mut enc_contexts,
    )?;

    // ── Phase C: GPU filter + encoder pipelines (override placeholders) ───
    super::gpu::init_gpu_pipelines(
        analysis,
        dec_ctxs,
        targets,
        cfg,
        opts,
        ifmt_ctx,
        ofmt_ctx,
        decode_accel,
        encode_accel,
        pipeline_cfg,
        &mut filter_pipelines,
        &mut enc_contexts,
    )?;

    Ok(EncodePipeline {
        filter_pipelines,
        enc_contexts,
    })
}

// ── SW filter init ────────────────────────────────────────────────────────────

fn init_sw_filters(
    analysis: &StreamAnalysis,
    dec_ctxs: &mut DecodeContexts,
    targets: &StreamTargets,
    cfg: &PassConfig,
    opts: &TranscodeOptions,
    filter_pipelines: &mut [Option<OwnedFilterPipeline>],
) -> Result<()> {
    use super::super::filter::FilterPipeline;
    use rsmpeg::avfilter::AVFilterGraph;

    let nb_streams = analysis.nb_streams;
    for i in 0..nb_streams {
        match &analysis.stream_map[i] {
            StreamMapping::Video { gpu_pipeline: true, .. } => {
                // Placeholder — GPU init (Phase C) will fill this slot.
            }
            StreamMapping::Video { .. } => {
                let vtargets = targets.video[i].as_ref().context("video targets missing")?;
                let dec_ctx = dec_ctxs[i].as_ref().context("decode context missing for video")?;
                let custom = opts.video_filter.as_deref();
                tracing::debug!(
                    "[transcode]   SW video filter init: target_pix_fmt={}, resolution={:?}, custom={:?}",
                    vtargets.pix_fmt,
                    cfg.resolution,
                    custom
                );
                let mut fg = AVFilterGraph::new();
                let pipeline = filter::init_video_filter(&mut fg, dec_ctx, vtargets.pix_fmt, cfg.resolution, custom)?;
                // SAFETY: `pipeline` borrows from `fg`; OwnedFilterPipeline ties
                // their lifetimes — pipeline is always dropped before fg.
                let pipeline_static: FilterPipeline<'static> = unsafe { std::mem::transmute(pipeline) };
                let owned = unsafe { OwnedFilterPipeline::new(pipeline_static, fg) };
                filter_pipelines[i] = Some(owned);
            }
            StreamMapping::Audio { .. } => {
                let atargets = targets.audio[i].as_ref().context("audio targets missing")?;
                let dec_ctx_mut = dec_ctxs[i].as_mut().context("decode context missing for audio")?;
                let mut fg = AVFilterGraph::new();
                let pipeline = filter::init_audio_filter(
                    &mut fg,
                    dec_ctx_mut,
                    atargets.sample_fmt,
                    atargets.sample_rate,
                    &atargets.ch_layout,
                )?;
                let pipeline_static: FilterPipeline<'static> = unsafe { std::mem::transmute(pipeline) };
                let owned = unsafe { OwnedFilterPipeline::new(pipeline_static, fg) };
                filter_pipelines[i] = Some(owned);
            }
            _ => {}
        }
    }
    Ok(())
}

// ── SW encoder + copy stream init ─────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn init_sw_encoders(
    analysis: &StreamAnalysis,
    dec_ctxs: &DecodeContexts,
    targets: &StreamTargets,
    cfg: &PassConfig,
    opts: &TranscodeOptions,
    ifmt_ctx: &AVFormatContextInput,
    ofmt_ctx: &mut AVFormatContextOutput,
    encode_accel: &Option<HwAccel>,
    filter_pipelines: &[Option<OwnedFilterPipeline>],
    enc_contexts: &mut [Option<AVCodecContext>],
) -> Result<()> {
    use super::super::hw::format_name;

    let nb_streams = analysis.nb_streams;
    for i in 0..nb_streams {
        let in_stream = &ifmt_ctx.streams()[i];
        let codecpar = in_stream.codecpar();

        match &analysis.stream_map[i] {
            StreamMapping::Video {
                out_idx,
                gpu_pipeline: true,
                ..
            } => {
                // GPU placeholder stream — codecpar filled by gpu::init_gpu_pipelines
                let out_idx = *out_idx;
                let vtargets = targets.video[i]
                    .as_ref()
                    .context("video targets missing for GPU stream")?;
                let mut out_stream = ofmt_ctx.new_stream();
                unsafe {
                    let cp = (*out_stream.as_mut_ptr()).codecpar;
                    (*cp).codec_type = ffi::AVMEDIA_TYPE_VIDEO;
                    (*cp).codec_id = super::gpu::placeholder_codec_id(&vtargets.codec_name);
                    (*cp).width = vtargets.width;
                    (*cp).height = vtargets.height;
                }
                assert_eq!(out_stream.index as usize, out_idx);
            }
            StreamMapping::Video { out_idx, .. } => {
                let out_idx = *out_idx;
                let vtargets = targets.video[i].as_ref().context("video targets missing")?;
                let codec_name = &vtargets.codec_name;
                let c_codec_name = CString::new(codec_name.as_str())?;
                let encoder = AVCodec::find_encoder_by_name(&c_codec_name)
                    .ok_or_else(|| Error::Other(format!("Encoder '{codec_name}' not found")))?;
                let dec_ctx = dec_ctxs[i]
                    .as_ref()
                    .context("decode context missing for video encoder")?;
                let mut enc_ctx = AVCodecContext::new(&encoder);

                enc_ctx.set_width(vtargets.width);
                enc_ctx.set_height(vtargets.height);
                enc_ctx.set_sample_aspect_ratio(dec_ctx.sample_aspect_ratio);
                enc_ctx.set_pix_fmt(vtargets.pix_fmt);
                enc_ctx.set_framerate(dec_ctx.framerate);

                let filter_tb = filter_pipelines[i]
                    .as_ref()
                    .context("filter pipeline missing for video encoder")?
                    .buffersink_ctx
                    .get_time_base();
                enc_ctx.set_time_base(filter_tb);

                if let Some(hw) = encode_accel
                    && hw.hw_type.is_hw_encoder(codec_name)
                {
                    enc_ctx.set_hw_device_ctx(hw.device_ctx.clone());
                }
                if let Some(bitrate) = cfg.target_bitrate {
                    enc_ctx.set_bit_rate(bitrate);
                }

                let preset_cstr = CString::new(opts.preset.clone())?;
                let mut enc_opts = None;
                if is_any_hw_encoder(codec_name) || codec_name == "libx264" || codec_name == "libx265" {
                    enc_opts = Some(AVDictionary::new(c"preset", &preset_cstr, 0));
                }
                if (codec_name == "libx264" || codec_name == "libx265") && cfg.target_bitrate.is_none() {
                    let crf = opts.crf.unwrap_or(23);
                    let crf_str = CString::new(crf.to_string())?;
                    enc_opts = Some(
                        enc_opts
                            .unwrap_or_else(|| AVDictionary::new(c"crf", &crf_str, 0))
                            .set(c"crf", &crf_str, 0),
                    );
                }

                enc_ctx.set_gop_size(opts.gop.unwrap_or(250));
                unsafe {
                    (*enc_ctx.as_mut_ptr()).thread_count = i32::from(is_any_hw_encoder(codec_name));
                }
                apply_video_encoder_options(
                    &mut enc_ctx,
                    opts.maxrate.as_deref(),
                    opts.bufsize.as_deref(),
                    opts.gop,
                    opts.keyint_min,
                    opts.video_profile.as_deref(),
                )?;

                unsafe {
                    let enc_ptr = enc_ctx.as_mut_ptr();
                    let has_tonemap_bt709 = opts
                        .video_filter
                        .as_deref()
                        .is_some_and(|f| f.contains("tonemap") && f.contains("bt709"));
                    if has_tonemap_bt709 {
                        (*enc_ptr).color_primaries = ffi::AVCOL_PRI_BT709;
                        (*enc_ptr).color_trc = ffi::AVCOL_TRC_BT709;
                        (*enc_ptr).colorspace = ffi::AVCOL_SPC_BT709;
                        (*enc_ptr).color_range = ffi::AVCOL_RANGE_MPEG;
                    }
                }

                enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_FRAME_DURATION as i32);
                if ofmt_ctx.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0 {
                    enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
                }
                enc_ctx.open(enc_opts)?;

                tracing::info!(
                    "[transcode] Video encoder: {} {}x{} preset={} bitrate={}",
                    vtargets.codec_name,
                    enc_ctx.width,
                    enc_ctx.height,
                    opts.preset,
                    enc_ctx.bit_rate
                );
                tracing::debug!(
                    "[transcode]   pix_fmt={} time_base={}/{} gop={} profile={:?} maxrate={} bufsize={}",
                    format_name(enc_ctx.pix_fmt),
                    enc_ctx.time_base.num,
                    enc_ctx.time_base.den,
                    enc_ctx.gop_size,
                    opts.video_profile,
                    unsafe { (*enc_ctx.as_ptr()).rc_max_rate },
                    unsafe { (*enc_ctx.as_ptr()).rc_buffer_size }
                );

                let mut out_stream = ofmt_ctx.new_stream();
                out_stream.set_codecpar(enc_ctx.extract_codecpar());
                out_stream.set_time_base(enc_ctx.time_base);
                assert_eq!(out_stream.index as usize, out_idx);
                enc_contexts[i] = Some(enc_ctx);
            }
            StreamMapping::Audio { out_idx, .. } => {
                let out_idx = *out_idx;
                let atargets = targets.audio[i].as_ref().context("audio targets missing")?;
                let c_codec_name = CString::new(atargets.codec_name.as_str())?;
                let encoder = AVCodec::find_encoder_by_name(&c_codec_name)
                    .ok_or_else(|| Error::Other(format!("Audio encoder '{}' not found", atargets.codec_name)))?;
                let mut enc_ctx = AVCodecContext::new(&encoder);

                enc_ctx.set_sample_rate(atargets.sample_rate);
                enc_ctx.set_ch_layout(atargets.ch_layout.clone().into_inner());
                enc_ctx.set_sample_fmt(atargets.sample_fmt);
                let filter_tb = filter_pipelines[i]
                    .as_ref()
                    .context("filter pipeline missing for audio encoder")?
                    .buffersink_ctx
                    .get_time_base();
                enc_ctx.set_time_base(filter_tb);

                if let Some(ref ab) = opts.audio_bitrate {
                    enc_ctx.set_bit_rate(parse_bitrate(ab)?);
                } else if enc_ctx.bit_rate == 0 {
                    enc_ctx.set_bit_rate(128_000);
                }

                enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_FRAME_DURATION as i32);
                if ofmt_ctx.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0 {
                    enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
                }
                enc_ctx.set_strict_std_compliance(-2);
                unsafe {
                    (*enc_ctx.as_mut_ptr()).thread_count = 1;
                }
                enc_ctx.open(None)?;

                tracing::info!(
                    "[transcode] Audio encoder: {} {}ch {}Hz {}kbps",
                    atargets.codec_name,
                    enc_ctx.ch_layout.nb_channels,
                    enc_ctx.sample_rate,
                    enc_ctx.bit_rate / 1000
                );

                let mut out_stream = ofmt_ctx.new_stream();
                out_stream.set_codecpar(enc_ctx.extract_codecpar());
                out_stream.set_time_base(enc_ctx.time_base);
                assert_eq!(out_stream.index as usize, out_idx);
                enc_contexts[i] = Some(enc_ctx);
            }
            StreamMapping::Copy { out_idx, .. } => {
                let out_idx = *out_idx;
                let mut out_stream = ofmt_ctx.new_stream();
                out_stream.set_codecpar(codecpar.clone());
                out_stream.set_time_base(in_stream.time_base);
                assert_eq!(out_stream.index as usize, out_idx);
            }
            StreamMapping::Ignore => {}
        }
    }
    Ok(())
}
