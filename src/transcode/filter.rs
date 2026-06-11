use crate::error::{Error, Result, ResultExt};
use rsmpeg::{
    avcodec::AVCodecContext,
    avfilter::{AVFilter, AVFilterContextMut, AVFilterGraph, AVFilterInOut},
    avutil::{AVChannelLayout, AVHWDeviceContext, AVHWFramesContext, get_sample_fmt_name},
    ffi,
};
use std::ffi::CString;

use super::hw::{FilterBackend, HwType, format_name};

// ── Filter pipeline ─────────────────────────────────────────────────────────

pub struct FilterPipeline<'graph> {
    pub buffersrc_ctx: AVFilterContextMut<'graph>,
    pub buffersink_ctx: AVFilterContextMut<'graph>,
}

/// Owns a [`FilterPipeline<'static>`] together with its backing [`AVFilterGraph`],
/// guaranteeing correct drop order (pipeline drops before graph).
///
/// `FilterPipeline<'graph>` borrows from `AVFilterGraph`; Rust can't express
/// self-referential structs with a real lifetime.  The transmute to `'static`
/// erases the lifetime so the two can live together.  Drop order is enforced
/// by field declaration order: `pipeline` is declared first and therefore
/// dropped first, releasing all borrows before `_graph` is freed.
///
/// # Usage
/// Build with [`OwnedFilterPipeline::new`] immediately after initialising a
/// filter graph.  Pass by value to thread functions; the graph travels with
/// the pipeline and is freed when the thread finishes.
pub struct OwnedFilterPipeline {
    /// Dropped first (declared first) — releases borrows before `_graph`.
    pipeline: FilterPipeline<'static>,
    _graph: AVFilterGraph,
}

impl OwnedFilterPipeline {
    /// Wrap a transmuted `FilterPipeline<'static>` together with its backing
    /// graph.
    ///
    /// # Safety
    /// `pipeline` must have been obtained by transmuting a
    /// `FilterPipeline<'graph>` that borrows exclusively from `graph`.
    /// No other live borrows from `graph` may exist after this call.
    pub unsafe fn new(pipeline: FilterPipeline<'static>, graph: AVFilterGraph) -> Self {
        Self {
            pipeline,
            _graph: graph,
        }
    }
}

impl std::ops::Deref for OwnedFilterPipeline {
    type Target = FilterPipeline<'static>;
    fn deref(&self) -> &Self::Target {
        &self.pipeline
    }
}

impl std::ops::DerefMut for OwnedFilterPipeline {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.pipeline
    }
}

// ── CPU video filter ────────────────────────────────────────────────────────

pub fn init_video_filter<'graph>(
    filter_graph: &'graph mut AVFilterGraph,
    dec_ctx: &AVCodecContext,
    target_pix_fmt: i32,
    resolution: Option<(i32, i32)>,
    custom_filter: Option<&str>,
) -> Result<FilterPipeline<'graph>> {
    let buffersrc = AVFilter::get_by_name(c"buffer").ok_or_else(|| Error::Other("buffer filter not found".into()))?;
    let buffersink =
        AVFilter::get_by_name(c"buffersink").ok_or_else(|| Error::Other("buffersink filter not found".into()))?;

    let args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}",
        dec_ctx.width,
        dec_ctx.height,
        dec_ctx.pix_fmt,
        dec_ctx.pkt_timebase.num,
        dec_ctx.pkt_timebase.den,
        dec_ctx.sample_aspect_ratio.num.max(1),
        dec_ctx.sample_aspect_ratio.den.max(1),
    );
    let args = CString::new(args)?;

    let mut buffersrc_ctx = filter_graph.create_filter_context(&buffersrc, c"in", Some(&args))?;

    let mut buffersink_ctx = filter_graph
        .alloc_filter_context(&buffersink, c"out")
        .ok_or_else(|| Error::Other("Cannot create buffer sink".into()))?;

    buffersink_ctx.opt_set_bin(c"pix_fmts", &target_pix_fmt)?;
    buffersink_ctx.init_dict(&mut None)?;

    let filter_spec = if let Some(custom) = custom_filter {
        CString::new(custom)?
    } else if let Some((w, h)) = resolution {
        CString::new(format!("scale={w}:{h}"))?
    } else {
        CString::new("null")?
    };
    tracing::debug!("[filter] SW filter spec: {:?}", filter_spec);

    let outputs = AVFilterInOut::new(c"in", &mut buffersrc_ctx, 0);
    let inputs = AVFilterInOut::new(c"out", &mut buffersink_ctx, 0);

    let (_inputs, _outputs) = filter_graph.parse_ptr(&filter_spec, Some(inputs), Some(outputs))?;
    filter_graph.config()?;

    Ok(FilterPipeline {
        buffersrc_ctx,
        buffersink_ctx,
    })
}

// ── GPU video filter (unified + cross-device) ───────────────────────────────

/// Filter graph parameters for the 3-stage pipeline.
pub struct HwFilterParams<'a> {
    /// Decode device context (owns the frames coming in)
    pub decode_device: &'a AVHWDeviceContext,
    /// Decode HW type
    pub decode_hw: HwType,
    /// HW frames context from decoder
    pub decode_frames: &'a AVHWFramesContext,
    /// Filter backend to use
    pub filter_backend: FilterBackend,
    /// Filter device context (only needed for cross-device: `OpenCL`, Vulkan)
    pub filter_device: Option<&'a AVHWDeviceContext>,
    /// Encode device context (may differ from decode)
    pub encode_device: Option<&'a AVHWDeviceContext>,
    /// Encode HW type (may differ from decode)
    pub encode_hw: Option<HwType>,
    /// Target software pixel format for the encoder
    pub target_sw_fmt: ffi::AVPixelFormat,
    /// Output resolution (None = keep input)
    pub resolution: Option<(i32, i32)>,
    /// Whether pixel format conversion is needed
    pub need_format_convert: bool,
    /// User-provided custom filter string (e.g. `tonemap_cuda` pipeline)
    pub custom_filter: Option<&'a str>,
}

/// Create a hardware-accelerated filter graph supporting all Jellyfin pipeline types:
///
/// 1. **Unified** (Native): buffer(HW) → [`scale_cuda/scale_vaapi`/...] → buffersink(HW)
/// 2. **Cross-device** (OpenCL/Vulkan): buffer(HW) → hwmap → [`scale_opencl/libplacebo`] → hwmap → buffersink(HW)
/// 3. **Mixed A** (SW decode → HW): handled by caller with hwupload before this function
/// 4. **Mixed B** (HW → SW encode): buffer(HW) → [filters] → hwdownload → format → buffersink(SW)
pub fn init_video_filter_hw<'graph>(
    filter_graph: &'graph mut AVFilterGraph,
    dec_ctx: &AVCodecContext,
    params: &HwFilterParams,
) -> Result<FilterPipeline<'graph>> {
    let buffersrc = AVFilter::get_by_name(c"buffer").ok_or_else(|| Error::Other("buffer filter not found".into()))?;
    let buffersink =
        AVFilter::get_by_name(c"buffersink").ok_or_else(|| Error::Other("buffersink filter not found".into()))?;

    let hw_pix_fmt = params.decode_hw.pix_fmt();

    let args = format!(
        "video_size={}x{}:pix_fmt={}:time_base={}/{}:pixel_aspect={}/{}",
        dec_ctx.width,
        dec_ctx.height,
        hw_pix_fmt,
        dec_ctx.pkt_timebase.num,
        dec_ctx.pkt_timebase.den,
        dec_ctx.sample_aspect_ratio.num.max(1),
        dec_ctx.sample_aspect_ratio.den.max(1),
    );
    let args = CString::new(args)?;

    let mut buffersrc_ctx = filter_graph.create_filter_context(&buffersrc, c"in", Some(&args))?;

    // Set hw_frames_ctx on buffer source
    unsafe {
        let p = ffi::av_buffersrc_parameters_alloc();
        if p.is_null() {
            return Err(Error::Other("Failed to allocate buffersrc parameters".into()));
        }
        (*p).hw_frames_ctx = ffi::av_buffer_ref(params.decode_frames.as_ptr());
        let ret = ffi::av_buffersrc_parameters_set(buffersrc_ctx.as_mut_ptr(), p);
        ffi::av_free(p.cast());
        if ret < 0 {
            return Err(Error::Other(format!(
                "Failed to set buffersrc hw_frames_ctx (error {ret})"
            )));
        }
    }

    let mut buffersink_ctx = filter_graph
        .alloc_filter_context(&buffersink, c"out")
        .ok_or_else(|| Error::Other("Cannot create buffer sink".into()))?;
    buffersink_ctx.init_dict(&mut None)?;

    // Build the filter spec string based on backend type
    let filter_spec = build_filter_spec(params);
    tracing::debug!("[filter] HW filter spec: {}", filter_spec);
    let filter_spec_c = CString::new(filter_spec.as_str())?;

    let outputs = AVFilterInOut::new(c"in", &mut buffersrc_ctx, 0);
    let inputs = AVFilterInOut::new(c"out", &mut buffersink_ctx, 0);
    let (_inputs, _outputs) = filter_graph.parse_ptr(&filter_spec_c, Some(inputs), Some(outputs))?;

    // Set hw_device_ctx on all HW filters
    // For cross-device: set the filter device (OpenCL/Vulkan) on bridge filters,
    // and the decode device on native filters.
    if let Some(filter_dev) = params.filter_device {
        set_hw_device_on_filters(filter_graph, filter_dev);
    }
    set_hw_device_on_filters(filter_graph, params.decode_device);

    filter_graph.config()?;

    Ok(FilterPipeline {
        buffersrc_ctx,
        buffersink_ctx,
    })
}

/// Rewrite bare `format=xxx` segments in a comma-separated filter chain to use
/// the HW scale filter (e.g. `scale_cuda=format=yuv420p`).
///
/// Chains containing `hwdownload` are left unchanged — after hwdownload, frames
/// are on CPU and the CPU `format` filter is correct.
fn rewrite_cpu_format_for_hw(chain: &str, hw_scale: &str) -> String {
    if chain.contains("hwdownload") {
        return chain.to_string();
    }
    chain
        .split(',')
        .map(|seg| {
            let trimmed = seg.trim();
            if trimmed.starts_with("format=") {
                format!("{hw_scale}={trimmed}")
            } else {
                seg.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Build the `FFmpeg` filter spec string for the given pipeline configuration.
fn build_filter_spec(params: &HwFilterParams) -> String {
    // User-provided custom filter takes priority
    if let Some(custom) = params.custom_filter {
        return match params.filter_backend {
            FilterBackend::OpenCL => {
                // Wrap with hwmap bridge: decode_hw → opencl → [custom] → hwmap back
                format!(
                    "hwmap=derive_device=opencl,{},hwmap=derive_device={},format={}",
                    custom,
                    hwdevice_name(params.decode_hw),
                    format_name(params.decode_hw.pix_fmt())
                )
            }
            FilterBackend::Vulkan => {
                format!(
                    "hwmap=derive_device=vulkan,{},hwmap=derive_device={},format={}",
                    custom,
                    hwdevice_name(params.decode_hw),
                    format_name(params.decode_hw.pix_fmt())
                )
            }
            FilterBackend::Native => {
                // In a HW-native pipeline (e.g. CUDA), bare CPU "format=xxx" filters
                // can't process GPU frames. Rewrite to the HW scale equivalent
                // (e.g. scale_cuda=format=yuv420p).
                let scale = params.filter_backend.scale_filter(Some(params.decode_hw));
                rewrite_cpu_format_for_hw(custom, scale)
            }
            FilterBackend::Software => custom.to_string(),
        };
    }

    // No custom filter — build based on need_format_convert and backend
    if !params.need_format_convert {
        return "null".to_string();
    }

    let fmt_name = format_name(params.target_sw_fmt);

    match params.filter_backend {
        FilterBackend::Native => {
            let scale = params.filter_backend.scale_filter(Some(params.decode_hw));
            if let Some((w, h)) = params.resolution {
                format!("{scale}=w={w}:h={h}:format={fmt_name}")
            } else {
                format!("{scale}=format={fmt_name}")
            }
        }
        FilterBackend::OpenCL => {
            let scale = "scale_opencl";
            let inner = if let Some((w, h)) = params.resolution {
                format!("{scale}={w}:{h}:format={fmt_name}")
            } else {
                format!("{scale}=format={fmt_name}")
            };
            // hwmap in → OpenCL filter → hwmap out
            format!(
                "hwmap=derive_device=opencl,{},hwmap=derive_device={},format={}",
                inner,
                hwdevice_name(params.decode_hw),
                format_name(params.decode_hw.pix_fmt())
            )
        }
        FilterBackend::Vulkan => {
            let inner = if let Some((w, h)) = params.resolution {
                format!("libplacebo=w={w}:h={h}:format={fmt_name}")
            } else {
                format!("libplacebo=format={fmt_name}")
            };
            format!(
                "hwmap=derive_device=vulkan,{},hwmap=derive_device={},format={}",
                inner,
                hwdevice_name(params.decode_hw),
                format_name(params.decode_hw.pix_fmt())
            )
        }
        FilterBackend::Software => {
            // hwdownload → format → scale
            let scale_part = if let Some((w, h)) = params.resolution {
                format!(",scale={w}:{h}")
            } else {
                String::new()
            };
            format!("hwdownload,format={fmt_name}{scale_part}")
        }
    }
}

/// `FFmpeg` device type name for hwmap `derive_device`= parameter.
fn hwdevice_name(hw: HwType) -> &'static str {
    match hw {
        HwType::Cuda => "cuda",
        HwType::Vaapi => "vaapi",
        HwType::Qsv => "qsv",
        HwType::Amf => "d3d11va",
        HwType::Videotoolbox => "videotoolbox",
        HwType::Rkmpp | HwType::V4l2m2m => "drm",
    }
}

/// Set `hw_device_ctx` on all HWDEVICE-flagged filters in the graph.
fn set_hw_device_on_filters(filter_graph: &AVFilterGraph, hw_device_ctx: &AVHWDeviceContext) {
    unsafe {
        let graph_ptr = filter_graph.as_ptr().cast_mut();
        let nb_filters = (*graph_ptr).nb_filters as usize;
        let filters = (*graph_ptr).filters;
        for i in 0..nb_filters {
            let f = *filters.add(i);
            if (*(*f).filter).flags & ffi::AVFILTER_FLAG_HWDEVICE as i32 != 0 && (*f).hw_device_ctx.is_null() {
                (*f).hw_device_ctx = ffi::av_buffer_ref(hw_device_ctx.as_ptr());
            }
        }
    }
}

// ── Audio filter ────────────────────────────────────────────────────────────

pub fn init_audio_filter<'graph>(
    filter_graph: &'graph mut AVFilterGraph,
    dec_ctx: &mut AVCodecContext,
    target_sample_fmt: i32,
    target_sample_rate: i32,
    target_ch_layout: &AVChannelLayout,
) -> Result<FilterPipeline<'graph>> {
    let buffersrc = AVFilter::get_by_name(c"abuffer").ok_or_else(|| Error::Other("abuffer not found".into()))?;
    let buffersink =
        AVFilter::get_by_name(c"abuffersink").ok_or_else(|| Error::Other("abuffersink not found".into()))?;

    if dec_ctx.ch_layout.order == ffi::AV_CHANNEL_ORDER_UNSPEC {
        dec_ctx.set_ch_layout(AVChannelLayout::from_nb_channels(dec_ctx.ch_layout.nb_channels).into_inner());
    }

    let args = format!(
        "time_base={}/{}:sample_rate={}:sample_fmt={}:channel_layout={}",
        dec_ctx.pkt_timebase.num,
        dec_ctx.pkt_timebase.den,
        dec_ctx.sample_rate,
        get_sample_fmt_name(dec_ctx.sample_fmt)
            .context("invalid sample format")?
            .to_string_lossy(),
        dec_ctx
            .ch_layout()
            .describe()
            .context("failed to describe channel layout")?
            .to_string_lossy(),
    );
    let args = CString::new(args)?;

    let mut buffersrc_ctx = filter_graph.create_filter_context(&buffersrc, c"in", Some(&args))?;

    let mut buffersink_ctx = filter_graph
        .alloc_filter_context(&buffersink, c"out")
        .ok_or_else(|| Error::Other("Cannot create buffer sink".into()))?;

    buffersink_ctx.opt_set_bin(c"sample_fmts", &target_sample_fmt)?;
    buffersink_ctx.opt_set(
        c"ch_layouts",
        &target_ch_layout
            .describe()
            .context("failed to describe target channel layout")?,
    )?;
    buffersink_ctx.opt_set_bin(c"sample_rates", &target_sample_rate)?;
    buffersink_ctx.init_dict(&mut None)?;

    let outputs = AVFilterInOut::new(c"in", &mut buffersrc_ctx, 0);
    let inputs = AVFilterInOut::new(c"out", &mut buffersink_ctx, 0);

    let target_fmt_name = get_sample_fmt_name(target_sample_fmt)
        .context("invalid target sample format")?
        .to_string_lossy()
        .to_string();
    let target_ch_desc = target_ch_layout
        .describe()
        .context("failed to describe target channel layout")?
        .to_string_lossy()
        .to_string();
    let filter_spec = format!(
        "aformat=sample_fmts={target_fmt_name}:channel_layouts={target_ch_desc}:sample_rates={target_sample_rate},asetnsamples=n=1024:p=1"
    );
    let filter_spec_c = CString::new(filter_spec)?;

    let (_inputs, _outputs) = filter_graph.parse_ptr(&filter_spec_c, Some(inputs), Some(outputs))?;
    filter_graph.config()?;

    Ok(FilterPipeline {
        buffersrc_ctx,
        buffersink_ctx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rewrite_bare_format() {
        assert_eq!(
            rewrite_cpu_format_for_hw("format=yuv420p", "scale_cuda"),
            "scale_cuda=format=yuv420p"
        );
    }

    #[test]
    fn test_rewrite_deinterlace_plus_format() {
        assert_eq!(
            rewrite_cpu_format_for_hw("bwdif_cuda=0:-1:0,format=yuv420p", "scale_cuda"),
            "bwdif_cuda=0:-1:0,scale_cuda=format=yuv420p"
        );
    }

    #[test]
    fn test_no_rewrite_tonemap_cuda() {
        // tonemap_cuda has format= as a parameter, not a standalone filter
        let chain = "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,tonemap_cuda=format=yuv420p:p=bt709:t=bt709:m=bt709:tonemap=hable:peak=100:desat=0";
        assert_eq!(rewrite_cpu_format_for_hw(chain, "scale_cuda"), chain);
    }

    #[test]
    fn test_no_rewrite_hwdownload_chain() {
        let chain =
            "hwdownload,format=p010le,tonemapx=tonemap=hable:desat=0:peak=100:t=bt709:m=bt709:p=bt709:format=yuv420p";
        assert_eq!(rewrite_cpu_format_for_hw(chain, "scale_cuda"), chain);
    }

    #[test]
    fn test_rewrite_vaapi() {
        assert_eq!(
            rewrite_cpu_format_for_hw("format=nv12", "scale_vaapi"),
            "scale_vaapi=format=nv12"
        );
    }
}
