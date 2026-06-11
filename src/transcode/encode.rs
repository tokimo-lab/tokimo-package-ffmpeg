use crate::error::{Error, Result};
use rsmpeg::{avcodec::AVCodecContext, avformat::AVFormatContextOutput, avutil::AVFrame, error::RsmpegError, ffi};
use std::ffi::CString;

use super::filter::FilterPipeline;

// ── Encode + write helpers ──────────────────────────────────────────────────

/// Encode a single frame (or flush with None) and write all resulting packets.
pub fn encode_write_frame(
    frame: Option<AVFrame>,
    enc_ctx: &mut AVCodecContext,
    ofmt_ctx: &mut AVFormatContextOutput,
    out_stream_idx: usize,
) -> Result<()> {
    let frame_ref = frame.as_ref();
    enc_ctx.send_frame(frame_ref)?;

    loop {
        let mut pkt = match enc_ctx.receive_packet() {
            Ok(p) => p,
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(e) => return Err(e.into()),
        };

        pkt.set_stream_index(out_stream_idx as i32);
        pkt.rescale_ts(enc_ctx.time_base, ofmt_ctx.streams()[out_stream_idx].time_base);

        ofmt_ctx.interleaved_write_frame(&mut pkt)?;
    }

    Ok(())
}

/// Push frame through filter graph, then encode and write all output frames.
pub fn filter_encode_write_frame(
    frame: Option<AVFrame>,
    pipeline: &mut FilterPipeline,
    enc_ctx: &mut AVCodecContext,
    ofmt_ctx: &mut AVFormatContextOutput,
    out_stream_idx: usize,
) -> Result<()> {
    pipeline.buffersrc_ctx.buffersrc_add_frame(frame, None)?;

    loop {
        let mut filtered_frame = match pipeline.buffersink_ctx.buffersink_get_frame(None) {
            Ok(f) => f,
            Err(RsmpegError::BufferSinkDrainError | RsmpegError::BufferSinkEofError) => break,
            Err(_) => return Err(Error::Other("Failed to get frame from buffer sink".into())),
        };

        let filter_tb = pipeline.buffersink_ctx.get_time_base();
        filtered_frame.set_time_base(filter_tb);
        filtered_frame.set_pict_type(ffi::AV_PICTURE_TYPE_NONE);

        let enc_tb = enc_ctx.time_base;
        if filtered_frame.pts != ffi::AV_NOPTS_VALUE && (filter_tb.num != enc_tb.num || filter_tb.den != enc_tb.den) {
            filtered_frame.set_pts(unsafe { ffi::av_rescale_q(filtered_frame.pts, filter_tb, enc_tb) });
        }

        encode_write_frame(Some(filtered_frame), enc_ctx, ofmt_ctx, out_stream_idx)?;
    }

    Ok(())
}

/// Flush encoder of any buffered frames.
pub fn flush_encoder(
    enc_ctx: &mut AVCodecContext,
    ofmt_ctx: &mut AVFormatContextOutput,
    out_stream_idx: usize,
) -> Result<()> {
    if enc_ctx.codec().capabilities & ffi::AV_CODEC_CAP_DELAY as i32 == 0 {
        return Ok(());
    }
    encode_write_frame(None, enc_ctx, ofmt_ctx, out_stream_idx)?;
    Ok(())
}

// ── Encoder option helpers ──────────────────────────────────────────────────

/// Apply common video encoder options (maxrate, bufsize, gop, `keyint_min`, profile).
pub fn apply_video_encoder_options(
    enc_ctx: &mut AVCodecContext,
    maxrate: Option<&str>,
    bufsize: Option<&str>,
    gop: Option<i32>,
    keyint_min: Option<i32>,
    profile: Option<&str>,
) -> Result<()> {
    if let Some(maxrate) = maxrate {
        let val = parse_bitrate(maxrate)?;
        unsafe {
            (*enc_ctx.as_mut_ptr()).rc_max_rate = val;
        }
    }
    if let Some(bufsize) = bufsize {
        let val = parse_bitrate(bufsize)?;
        unsafe {
            (*enc_ctx.as_mut_ptr()).rc_buffer_size = val as i32;
        }
    }
    if let Some(gop) = gop {
        enc_ctx.set_gop_size(gop);
    }
    if let Some(keyint_min) = keyint_min {
        unsafe {
            (*enc_ctx.as_mut_ptr()).keyint_min = keyint_min;
        }
    }
    if let Some(profile) = profile {
        let profile_cstr = CString::new(profile)?;
        let key = CString::new("profile")?;
        unsafe {
            ffi::av_opt_set(
                enc_ctx.as_mut_ptr().cast(),
                key.as_ptr(),
                profile_cstr.as_ptr(),
                ffi::AV_OPT_SEARCH_CHILDREN as i32,
            );
        }
    }
    Ok(())
}

// ── Parsing utilities ───────────────────────────────────────────────────────

pub fn parse_resolution(s: &str) -> Result<(i32, i32)> {
    let parts: Vec<&str> = s.split('x').collect();
    if parts.len() != 2 {
        return Err(Error::Other(format!(
            "Invalid resolution format '{s}', expected WxH (e.g. 1920x1080)"
        )));
    }
    let w: i32 = parts[0]
        .parse()
        .map_err(|e| Error::Other(format!("Invalid width: {e}")))?;
    let h: i32 = parts[1]
        .parse()
        .map_err(|e| Error::Other(format!("Invalid height: {e}")))?;
    if w <= 0 || h <= 0 {
        return Err(Error::Other("Resolution dimensions must be positive".into()));
    }
    Ok((w, h))
}

pub fn parse_bitrate(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Some(rest) = s.strip_suffix('k').or_else(|| s.strip_suffix('K')) {
        let val: f64 = rest
            .parse()
            .map_err(|e| Error::Other(format!("Invalid bitrate: {e}")))?;
        Ok((val * 1000.0) as i64)
    } else if let Some(rest) = s.strip_suffix('M').or_else(|| s.strip_suffix('m')) {
        let val: f64 = rest
            .parse()
            .map_err(|e| Error::Other(format!("Invalid bitrate: {e}")))?;
        Ok((val * 1_000_000.0) as i64)
    } else {
        let val: i64 = s.parse().map_err(|e| Error::Other(format!("Invalid bitrate: {e}")))?;
        Ok(val)
    }
}
