use crate::error::{Error, Result, ResultExt};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avfilter::AVFilterGraph,
    avformat::{AVFormatContextInput, AVFormatContextOutput},
    avutil::{AVDictionary, AVHWFramesContext},
    ffi,
};
use std::ffi::CString;

use super::super::encode::apply_video_encoder_options;
use super::super::filter::{self, FilterPipeline, OwnedFilterPipeline};
use super::super::hw::{HwAccel, HwPipeline, codec_id_for_encoder, format_name};
use super::super::types::{StreamMapping, TranscodeOptions};
use super::analyze::{DecodeContexts, StreamAnalysis};
use super::config::PassConfig;
use super::targets::StreamTargets;

/// Initialise GPU decode→filter→encode pipelines for all streams flagged with
/// `gpu_pipeline: true` in the stream mapping.
///
/// Fills `filter_pipelines[i]` and `enc_contexts[i]` for each GPU stream, and
/// updates the placeholder output stream's `codecpar` in `ofmt_ctx`.
#[allow(clippy::too_many_arguments)]
pub(super) fn init_gpu_pipelines(
    analysis: &StreamAnalysis,
    dec_ctxs: &DecodeContexts,
    targets: &StreamTargets,
    cfg: &PassConfig,
    opts: &TranscodeOptions,
    ifmt_ctx: &AVFormatContextInput,
    ofmt_ctx: &mut AVFormatContextOutput,
    decode_accel: &Option<HwAccel>,
    encode_accel: &Option<HwAccel>,
    pipeline_cfg: &HwPipeline,
    filter_pipelines: &mut [Option<OwnedFilterPipeline>],
    enc_contexts: &mut [Option<AVCodecContext>],
) -> Result<()> {
    let Some(decode_accel) = decode_accel else {
        return Ok(());
    };
    let encode_accel = encode_accel.as_ref().unwrap_or(decode_accel);

    for i in 0..analysis.nb_streams {
        let out_idx = match &analysis.stream_map[i] {
            StreamMapping::Video {
                out_idx,
                gpu_pipeline: true,
                ..
            } => *out_idx,
            _ => continue,
        };
        let vtargets = match targets.video[i].as_ref() {
            Some(t) if t.gpu_pipeline => t,
            _ => continue,
        };
        let Some(dec_ctx) = dec_ctxs[i].as_ref() else { continue };

        let src_sw_fmt = decode_accel.hw_type.sw_format(ifmt_ctx.streams()[i].codecpar().format);

        let mut manual_hw_fc = decode_accel.device_ctx.hwframe_ctx_alloc();
        {
            let data = manual_hw_fc.data();
            data.format = decode_accel.hw_type.pix_fmt();
            data.sw_format = src_sw_fmt;
            data.width = dec_ctx.width;
            data.height = dec_ctx.height;
            data.initial_pool_size = 25;
        }
        manual_hw_fc.init()?;

        let mut fg = AVFilterGraph::new();

        let need_format_convert = src_sw_fmt != vtargets.sw_pix_fmt || cfg.resolution.is_some();
        let custom_filter_str = opts.video_filter.as_deref();

        let hw_filter_params = filter::HwFilterParams {
            decode_device: &decode_accel.device_ctx,
            decode_hw: decode_accel.hw_type,
            decode_frames: &manual_hw_fc,
            filter_backend: pipeline_cfg.filter,
            filter_device: None,
            encode_device: Some(&encode_accel.device_ctx),
            encode_hw: Some(encode_accel.hw_type),
            target_sw_fmt: vtargets.sw_pix_fmt,
            resolution: cfg.resolution,
            need_format_convert,
            custom_filter: custom_filter_str,
        };

        let pipeline = filter::init_video_filter_hw(&mut fg, dec_ctx, &hw_filter_params)?;
        // SAFETY: `pipeline` borrows from `fg`; `OwnedFilterPipeline::new` ties
        // their lifetimes so `pipeline` is always dropped before `fg`.
        let pipeline_static: FilterPipeline<'static> = unsafe { std::mem::transmute(pipeline) };
        let filter_tb = pipeline_static.buffersink_ctx.get_time_base();
        let owned = unsafe { OwnedFilterPipeline::new(pipeline_static, fg) };

        // ── Build GPU encoder ─────────────────────────────────────────────
        let codec_name = &vtargets.codec_name;
        let c_codec_name = CString::new(codec_name.as_str())?;
        let encoder = AVCodec::find_encoder_by_name(&c_codec_name)
            .ok_or_else(|| Error::Other(format!("Encoder '{codec_name}' not found")))?;
        let mut enc_ctx = AVCodecContext::new(&encoder);

        enc_ctx.set_width(vtargets.width);
        enc_ctx.set_height(vtargets.height);
        enc_ctx.set_sample_aspect_ratio(dec_ctx.sample_aspect_ratio);
        enc_ctx.set_pix_fmt(encode_accel.hw_type.pix_fmt());
        enc_ctx.set_framerate(dec_ctx.framerate);
        enc_ctx.set_time_base(filter_tb);

        // Color properties
        unsafe {
            let enc_ptr = enc_ctx.as_mut_ptr();
            let has_tonemap_bt709 = custom_filter_str.is_some_and(|f| f.contains("tonemap") && f.contains("bt709"));
            if has_tonemap_bt709 {
                (*enc_ptr).color_primaries = ffi::AVCOL_PRI_BT709;
                (*enc_ptr).color_trc = ffi::AVCOL_TRC_BT709;
                (*enc_ptr).colorspace = ffi::AVCOL_SPC_BT709;
                (*enc_ptr).color_range = ffi::AVCOL_RANGE_MPEG;
            }
            if (*enc_ptr).color_primaries == ffi::AVCOL_PRI_UNSPECIFIED && opts.video_filter.is_some() {
                (*enc_ptr).color_primaries = ffi::AVCOL_PRI_BT709;
                (*enc_ptr).color_trc = ffi::AVCOL_TRC_BT709;
                (*enc_ptr).colorspace = ffi::AVCOL_SPC_BT709;
                (*enc_ptr).color_range = ffi::AVCOL_RANGE_MPEG;
            }
        }

        // hw_frames_ctx from filter output for encoder
        let sink_hw_frames_ptr = unsafe { ffi::av_buffersink_get_hw_frames_ctx(owned.buffersink_ctx.as_ptr()) };
        if sink_hw_frames_ptr.is_null() {
            let mut enc_frames_ctx = encode_accel.device_ctx.hwframe_ctx_alloc();
            {
                let data = enc_frames_ctx.data();
                data.format = encode_accel.hw_type.pix_fmt();
                data.sw_format = vtargets.sw_pix_fmt;
                data.width = vtargets.width;
                data.height = vtargets.height;
                data.initial_pool_size = 25;
            }
            enc_frames_ctx.init()?;
            enc_ctx.set_hw_frames_ctx(enc_frames_ctx);
        } else {
            let hw_fc = unsafe {
                AVHWFramesContext::from_raw(
                    std::ptr::NonNull::new(ffi::av_buffer_ref(sink_hw_frames_ptr))
                        .context("av_buffer_ref returned null")?,
                )
            };
            enc_ctx.set_hw_frames_ctx(hw_fc);
        }

        if let Some(bitrate) = cfg.target_bitrate {
            enc_ctx.set_bit_rate(bitrate);
        }
        let preset_cstr = CString::new(opts.preset.clone())?;
        let enc_opts = Some(AVDictionary::new(c"preset", &preset_cstr, 0));
        enc_ctx.set_gop_size(opts.gop.unwrap_or(250));
        unsafe {
            (*enc_ctx.as_mut_ptr()).thread_count = 1;
        }
        apply_video_encoder_options(
            &mut enc_ctx,
            opts.maxrate.as_deref(),
            opts.bufsize.as_deref(),
            opts.gop,
            opts.keyint_min,
            opts.video_profile.as_deref(),
        )?;
        enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_FRAME_DURATION as i32);
        if ofmt_ctx.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0 {
            enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
        }
        enc_ctx.open(enc_opts)?;

        tracing::info!(
            "[transcode] Video encoder: {} (GPU) {}x{} preset={} bitrate={}",
            vtargets.codec_name,
            enc_ctx.width,
            enc_ctx.height,
            opts.preset,
            enc_ctx.bit_rate
        );
        tracing::debug!(
            "[transcode]   pix_fmt={}, sw_pix_fmt={}, time_base={}/{}, framerate={}/{}",
            format_name(enc_ctx.pix_fmt),
            format_name(vtargets.sw_pix_fmt),
            enc_ctx.time_base.num,
            enc_ctx.time_base.den,
            enc_ctx.framerate.num,
            enc_ctx.framerate.den
        );
        tracing::debug!(
            "[transcode]   gop={}, max_b_frames={}, maxrate={}, bufsize={}, profile={:?}",
            enc_ctx.gop_size,
            enc_ctx.max_b_frames,
            unsafe { (*enc_ctx.as_ptr()).rc_max_rate },
            unsafe { (*enc_ctx.as_ptr()).rc_buffer_size },
            opts.video_profile
        );

        // Update placeholder stream codecpar
        let extracted = enc_ctx.extract_codecpar();
        unsafe {
            let ofmt_ptr = ofmt_ctx.as_mut_ptr();
            let stream_ptr = *(*ofmt_ptr).streams.add(out_idx);
            ffi::avcodec_parameters_copy((*stream_ptr).codecpar, extracted.as_ptr());
            (*stream_ptr).time_base = enc_ctx.time_base;
        }

        let filter_desc = if custom_filter_str.is_some() {
            "custom_filter"
        } else if need_format_convert {
            pipeline_cfg.scale_filter()
        } else {
            "passthrough"
        };
        tracing::debug!(
            "[transcode] GPU pipeline: {} | filter={} | sw_fmt: {}→{}",
            pipeline_cfg.describe(),
            filter_desc,
            format_name(src_sw_fmt),
            format_name(vtargets.sw_pix_fmt)
        );

        filter_pipelines[i] = Some(owned);
        enc_contexts[i] = Some(enc_ctx);
    }

    Ok(())
}

/// Return the `FFmpeg` codec ID for a named encoder, for placeholder stream setup.
///
/// Returns the native `ffi::AVCodecID` type (which differs between platforms:
/// `u32` on Linux bindgen, `i32` on MSVC bindgen). Callers assign it directly
/// to `AVCodecContext::codec_id` so type compatibility is preserved.
pub(super) fn placeholder_codec_id(codec_name: &str) -> ffi::AVCodecID {
    codec_id_for_encoder(codec_name)
}
