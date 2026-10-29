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
    mut kf_forcer: Option<&mut KeyframeForcer>,
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
        if let Some(forcer) = kf_forcer.as_deref_mut() {
            forcer.apply(&mut filtered_frame);
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

// ── Keyframe forcing ────────────────────────────────────────────────────────

/// Forces a keyframe every `interval` seconds, measured from the first frame
/// of the pass (FFI equivalent of CLI `-force_key_frames expr:gte(t,n_forced*N)`).
///
/// The HLS muxer can only cut segments on keyframes, and it cuts once
/// `pts - start_pts >= hls_time * n`.  Without forced keyframes the encoder's
/// natural GOP (250 frames + scenecut) produces irregular ~8–10 s segments
/// that drift away from the pre-generated 6 s VOD playlist, so segment N no
/// longer covers `N * hls_time` and players stall after a seek.
pub struct KeyframeForcer {
    interval: i64,
    next: Option<i64>,
}

impl KeyframeForcer {
    /// `enc_tb` is the time base of frames handed to [`Self::apply`].
    pub fn new(interval_secs: f64, enc_tb: ffi::AVRational) -> Option<Self> {
        if interval_secs <= 0.0 || enc_tb.num <= 0 || enc_tb.den <= 0 {
            return None;
        }
        let interval = (interval_secs * f64::from(enc_tb.den) / f64::from(enc_tb.num)).round() as i64;
        (interval > 0).then_some(Self { interval, next: None })
    }

    /// Mark `frame` as a forced I-frame when it crosses the next boundary.
    pub fn apply(&mut self, frame: &mut AVFrame) {
        let pts = frame.pts;
        if pts == ffi::AV_NOPTS_VALUE {
            return;
        }
        let next = match self.next {
            None => pts,
            Some(next) if pts >= next => next,
            Some(_) => return,
        };
        frame.set_pict_type(ffi::AV_PICTURE_TYPE_I);
        // Skip over any boundaries a timestamp jump may have crossed.
        self.next = Some(next + self.interval * ((pts - next) / self.interval + 1));
    }
}

/// Make forced I-frames real IDR frames so every HLS segment starts with an
/// independently decodable picture (libx264/libx265/nvenc use `forced-idr`,
/// qsv/amf use `forced_idr`; VAAPI and VideoToolbox already treat them as IDR).
pub fn enable_forced_idr(enc_ctx: &mut AVCodecContext) {
    for key in [c"forced-idr", c"forced_idr"] {
        unsafe {
            ffi::av_opt_set(
                enc_ctx.as_mut_ptr().cast(),
                key.as_ptr(),
                c"1".as_ptr(),
                ffi::AV_OPT_SEARCH_CHILDREN as i32,
            );
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn forced_at(forcer: &mut KeyframeForcer, pts: i64) -> bool {
        let mut frame = AVFrame::new();
        frame.set_pts(pts);
        forcer.apply(&mut frame);
        frame.pict_type == ffi::AV_PICTURE_TYPE_I
    }

    #[test]
    fn keyframe_forcer_aligns_to_first_frame() {
        // 23.976 fps in a 1/24000 time base; 6 s segments starting at 7026 s.
        let tb = ffi::AVRational { num: 1, den: 24000 };
        let mut forcer = KeyframeForcer::new(6.0, tb).unwrap();
        let start = 7026 * 24000;
        let forced: Vec<i64> = (0..400)
            .map(|i| start + i * 1001)
            .filter(|&pts| forced_at(&mut forcer, pts))
            .collect();
        assert_eq!(forced.len(), 3);
        assert_eq!(forced[0], start);
        for (k, pts) in forced.iter().enumerate() {
            let boundary = start + k as i64 * 6 * 24000;
            assert!(*pts >= boundary && *pts < boundary + 1001, "kf {k} at {pts}");
        }
    }

    #[test]
    fn keyframe_forcer_skips_crossed_boundaries() {
        let tb = ffi::AVRational { num: 1, den: 1000 };
        let mut forcer = KeyframeForcer::new(6.0, tb).unwrap();
        assert!(forced_at(&mut forcer, 0));
        assert!(forced_at(&mut forcer, 20_000));
        assert!(!forced_at(&mut forcer, 23_000));
        assert!(forced_at(&mut forcer, 24_000));
    }
}
