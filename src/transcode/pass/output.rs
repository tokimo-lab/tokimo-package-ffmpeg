use crate::error::Result;
use rsmpeg::{avformat::AVFormatContextOutput, avutil::AVDictionary, ffi};
use std::ffi::CString;

use super::super::types::{HlsSegmentType, TranscodeOptions};
use super::config::PassConfig;

/// Configure muxer options, HLS parameters, PTS normalisation, and write the
/// format header.
pub(super) fn configure_output(
    ofmt_ctx: &mut AVFormatContextOutput,
    opts: &TranscodeOptions,
    cfg: &PassConfig,
) -> Result<()> {
    // ── Output options (CLI parity) ──────────────────────────────────────
    // NOTE: Jellyfin CLI uses `-copyts -avoid_negative_ts disabled`, but in FFI
    // mode there is no `-copyts` equivalent.  Without it the muxer applies its
    // own timestamp logic.  If we also disable negative-ts adjustment, the AAC
    // encoder priming delay (-2048 samples) produces a negative PTS that ends
    // up as a huge unsigned value in the fMP4 tfdt box, breaking hls.js.
    //
    // The safe default (AVFMT_AVOID_NEG_TS_MAKE_NON_NEGATIVE = 1) auto-shifts
    // all streams so the first PTS ≥ 0.  This is the FFmpeg default and works
    // correctly for both copy and transcode paths.

    // Match Jellyfin: -max_delay 5000000
    unsafe {
        (*ofmt_ctx.as_mut_ptr()).max_delay = 5_000_000;
    }

    // Match CLI: -map_metadata -1 -map_chapters -1 (strip metadata)
    unsafe {
        let ofmt_ptr = ofmt_ctx.as_mut_ptr();
        if !(*ofmt_ptr).metadata.is_null() {
            ffi::av_dict_free(&raw mut (*ofmt_ptr).metadata);
        }
        (*ofmt_ptr).nb_chapters = 0;
    }

    // ── Muxer options ────────────────────────────────────────────────────
    let queue_size = if opts.hls.is_some() { "2048" } else { "128" };
    let queue_size_c = CString::new(queue_size)?;
    let mut mux_dict = AVDictionary::new(c"max_muxing_queue_size", &queue_size_c, 0);

    // HLS muxer options
    if let Some(ref hls) = opts.hls {
        let seg_dur = CString::new(hls.segment_duration.to_string())?;
        let start_num = CString::new(hls.start_number.to_string())?;
        let playlist_type = CString::new(hls.playlist_type.as_str())?;
        let seg_pattern = CString::new(hls.segment_pattern.as_str())?;

        mux_dict = mux_dict
            .set(c"hls_time", &seg_dur, 0)
            .set(c"hls_list_size", c"0", 0)
            .set(c"hls_playlist_type", &playlist_type, 0)
            .set(c"hls_segment_filename", &seg_pattern, 0)
            .set(c"start_number", &start_num, 0);

        match hls.segment_type {
            HlsSegmentType::Fmp4 => {
                let init_fn = CString::new(hls.init_filename.as_str())?;
                mux_dict = mux_dict
                    .set(c"hls_segment_type", c"fmp4", 0)
                    .set(c"hls_fmp4_init_filename", &init_fn, 0)
                    .set(c"hls_segment_options", c"movflags=+frag_discont", 0);
            }
            HlsSegmentType::Mpegts => {
                mux_dict = mux_dict.set(c"hls_segment_type", c"mpegts", 0);
            }
        }
    }

    let mut mux_opts = Some(mux_dict);
    if let Some(ref hls) = opts.hls {
        let seg_type_str = match hls.segment_type {
            HlsSegmentType::Fmp4 => "fmp4",
            HlsSegmentType::Mpegts => "mpegts",
        };
        tracing::info!(
            "[transcode] Output: HLS {} {}s segments, start={}",
            seg_type_str,
            hls.segment_duration,
            hls.start_number
        );
    } else {
        tracing::debug!("[transcode] Output: mux_queue={}", queue_size);
    }

    // ── Normalize output PTS ─────────────────────────────────────────────
    // BDMV m2ts files have large start_time (e.g. 600 s).  Without
    // normalisation the output MPEG-TS PES timestamps carry this offset,
    // which causes HLS.js's MPEG-TS→fMP4 remuxer to set a large initPTS.
    // After a seek the remuxed fMP4 baseMediaDecodeTime is nominally
    // correct, but Chrome's SourceBuffer silently refuses to extend the
    // buffered range.  Subtracting start_time makes output PTS start
    // near 0 (initial pass) or near seek_secs (after seek), which keeps
    // HLS.js and MSE happy.
    if cfg.format_start_time > 0 {
        unsafe {
            (*ofmt_ctx.as_mut_ptr()).output_ts_offset = -cfg.format_start_time;
        }
        tracing::info!(
            "[transcode] PTS normalisation: output_ts_offset={:.3}s",
            -(cfg.format_start_time as f64 / f64::from(ffi::AV_TIME_BASE)),
        );
    }

    ofmt_ctx.write_header(&mut mux_opts)?;

    Ok(())
}
