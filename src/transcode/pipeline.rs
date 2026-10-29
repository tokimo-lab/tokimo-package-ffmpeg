use crate::error::{Error, Result};
use rsmpeg::{
    avcodec::{AVCodecContext, AVPacket},
    avutil::AVFrame,
    error::RsmpegError,
    ffi,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use super::encode::KeyframeForcer;
use super::filter::{FilterPipeline, OwnedFilterPipeline};
use super::scheduler::{MuxScheduler, dts_to_us};

// ── Video decode thread ─────────────────────────────────────────────────────

/// Run in a spawned thread: receives packets, decodes frames, sends to filter.
///
/// When `hw_download_fmt` is `Some(pix_fmt)`, decoded frames in that HW pixel
/// format are downloaded to system memory (Tier 2: HW decode + SW filter).
pub fn video_decode_loop(
    mut dec_ctx: AVCodecContext,
    pkt_rx: mpsc::Receiver<AVPacket>,
    frame_tx: mpsc::SyncSender<AVFrame>,
    seek_pts: i64,
    hw_download_fmt: Option<i32>,
) -> Result<()> {
    while let Ok(pkt) = pkt_rx.recv() {
        dec_ctx.send_packet(Some(&pkt)).unwrap_or_else(|e| {
            tracing::info!("Warning: video send_packet failed: {:?}", e);
        });
        drain_decoder_frames(&mut dec_ctx, &frame_tx, seek_pts, hw_download_fmt)?;
    }
    // Flush decoder
    let _ = dec_ctx.send_packet(None);
    drain_decoder_frames(&mut dec_ctx, &frame_tx, seek_pts, hw_download_fmt)?;
    drop(frame_tx);
    Ok(())
}

fn drain_decoder_frames(
    dec_ctx: &mut AVCodecContext,
    frame_tx: &mpsc::SyncSender<AVFrame>,
    seek_pts: i64,
    hw_download_fmt: Option<i32>,
) -> Result<()> {
    loop {
        let Ok(frame) = dec_ctx.receive_frame() else { break };
        let pts = frame.best_effort_timestamp;
        if seek_pts > 0 && pts != ffi::AV_NOPTS_VALUE && pts < seek_pts {
            continue;
        }
        // Download from GPU if needed (Tier 2: HW decode + SW filter/encode)
        let mut f = if let Some(hw_fmt) = hw_download_fmt {
            if frame.format == hw_fmt {
                let mut sw = AVFrame::new();
                sw.hwframe_transfer_data(&frame)?;
                sw.set_pts(frame.pts);
                sw
            } else {
                frame
            }
        } else {
            frame
        };
        if pts != ffi::AV_NOPTS_VALUE {
            f.set_pts(pts);
        }
        if frame_tx.send(f).is_err() {
            break;
        }
    }
    Ok(())
}

// ── Video filter+encode thread ──────────────────────────────────────────────

/// Run in a spawned thread: receives decoded frames, filters, encodes, sends packets.
#[allow(clippy::too_many_arguments)]
pub fn video_filter_encode_loop(
    mut pipeline: OwnedFilterPipeline,
    mut enc_ctx: AVCodecContext,
    frame_rx: mpsc::Receiver<AVFrame>,
    pkt_tx: mpsc::Sender<AVPacket>,
    out_stream_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    scheduler: Arc<MuxScheduler>,
    mut kf_forcer: Option<KeyframeForcer>,
) -> Result<()> {
    let sched = Some(scheduler.as_ref());
    while let Ok(frame) = frame_rx.recv() {
        push_filter_encode(
            &mut pipeline,
            &mut enc_ctx,
            Some(frame),
            &pkt_tx,
            out_stream_idx,
            enc_tb,
            out_tb,
            sched,
            kf_forcer.as_mut(),
        )?;
    }

    // Flush filter graph
    pipeline.buffersrc_ctx.buffersrc_add_frame(None::<AVFrame>, None)?;
    drain_filter_encode(
        &mut pipeline,
        &mut enc_ctx,
        &pkt_tx,
        out_stream_idx,
        enc_tb,
        out_tb,
        sched,
        kf_forcer.as_mut(),
    )?;

    // Flush encoder
    if enc_ctx.codec().capabilities & ffi::AV_CODEC_CAP_DELAY as i32 != 0 {
        let _ = enc_ctx.send_frame(None);
        drain_encoder(&mut enc_ctx, &pkt_tx, out_stream_idx, enc_tb, out_tb, sched)?;
    }

    scheduler.mark_finished(out_stream_idx);
    drop(pkt_tx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn push_filter_encode(
    pipeline: &mut FilterPipeline,
    enc_ctx: &mut AVCodecContext,
    frame: Option<AVFrame>,
    pkt_tx: &mpsc::Sender<AVPacket>,
    out_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    scheduler: Option<&MuxScheduler>,
    kf_forcer: Option<&mut KeyframeForcer>,
) -> Result<()> {
    pipeline.buffersrc_ctx.buffersrc_add_frame(frame, None)?;
    drain_filter_encode(pipeline, enc_ctx, pkt_tx, out_idx, enc_tb, out_tb, scheduler, kf_forcer)
}

#[allow(clippy::too_many_arguments)]
fn drain_filter_encode(
    pipeline: &mut FilterPipeline,
    enc_ctx: &mut AVCodecContext,
    pkt_tx: &mpsc::Sender<AVPacket>,
    out_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    scheduler: Option<&MuxScheduler>,
    mut kf_forcer: Option<&mut KeyframeForcer>,
) -> Result<()> {
    loop {
        let mut filtered = match pipeline.buffersink_ctx.buffersink_get_frame(None) {
            Ok(f) => f,
            Err(RsmpegError::BufferSinkDrainError | RsmpegError::BufferSinkEofError) => break,
            Err(_) => return Err(Error::Other("Failed to get frame from filter".into())),
        };
        let filter_tb = pipeline.buffersink_ctx.get_time_base();
        filtered.set_time_base(filter_tb);
        filtered.set_pict_type(ffi::AV_PICTURE_TYPE_NONE);

        if filtered.pts != ffi::AV_NOPTS_VALUE && (filter_tb.num != enc_tb.num || filter_tb.den != enc_tb.den) {
            filtered.set_pts(unsafe { ffi::av_rescale_q(filtered.pts, filter_tb, enc_tb) });
        }
        if let Some(forcer) = kf_forcer.as_deref_mut() {
            forcer.apply(&mut filtered);
        }

        enc_ctx.send_frame(Some(&filtered))?;
        drain_encoder(enc_ctx, pkt_tx, out_idx, enc_tb, out_tb, scheduler)?;
    }
    Ok(())
}

fn drain_encoder(
    enc_ctx: &mut AVCodecContext,
    pkt_tx: &mpsc::Sender<AVPacket>,
    out_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    scheduler: Option<&MuxScheduler>,
) -> Result<()> {
    loop {
        let mut pkt = match enc_ctx.receive_packet() {
            Ok(p) => p,
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => break,
            Err(e) => return Err(e.into()),
        };
        pkt.set_stream_index(out_idx as i32);
        pkt.rescale_ts(enc_tb, out_tb);
        // Report DTS to scheduler so fast streams can see our progress.
        if let Some(sched) = scheduler {
            sched.report_dts(out_idx, dts_to_us(pkt.dts, out_tb));
        }
        if pkt_tx.send(pkt).is_err() {
            break;
        }
    }
    Ok(())
}

// ── Audio thread ────────────────────────────────────────────────────────────

/// Run in a spawned thread: receives packets, decodes, filters, encodes audio.
#[allow(clippy::too_many_arguments)]
pub fn audio_thread_loop(
    mut dec_ctx: AVCodecContext,
    mut pipeline: OwnedFilterPipeline,
    mut enc_ctx: AVCodecContext,
    pkt_rx: mpsc::Receiver<AVPacket>,
    pkt_tx: mpsc::Sender<AVPacket>,
    out_stream_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    seek_pts: i64,
    scheduler: Arc<MuxScheduler>,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<()> {
    let mut preroll_skipped = 0u64;
    let sched = Some(scheduler.as_ref());

    while let Ok(pkt) = pkt_rx.recv() {
        // Exit immediately when cancelled — dropping pkt_rx unblocks the main
        // loop's SyncSender::send() so it can reach its own cancel check.
        if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
            break;
        }
        dec_ctx.send_packet(Some(&pkt)).unwrap_or_else(|e| {
            tracing::info!("Warning: audio send_packet failed: {:?}", e);
        });
        loop {
            let Ok(frame) = dec_ctx.receive_frame() else { break };
            let mut f = frame;
            let pts = f.best_effort_timestamp;
            if pts != ffi::AV_NOPTS_VALUE {
                f.set_pts(pts);
            }
            if seek_pts > 0 && pts < seek_pts {
                preroll_skipped += 1;
                continue;
            }
            audio_filter_encode(
                &mut pipeline,
                &mut enc_ctx,
                Some(f),
                &pkt_tx,
                out_stream_idx,
                enc_tb,
                out_tb,
                sched,
            )?;
        }
    }

    // Skip flush when cancelled — segments will be discarded anyway.
    if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
        scheduler.mark_finished(out_stream_idx);
        return Ok(());
    }

    // Flush decoder
    let _ = dec_ctx.send_packet(None);
    loop {
        let Ok(frame) = dec_ctx.receive_frame() else { break };
        let mut f = frame;
        f.set_pts(f.best_effort_timestamp);
        audio_filter_encode(
            &mut pipeline,
            &mut enc_ctx,
            Some(f),
            &pkt_tx,
            out_stream_idx,
            enc_tb,
            out_tb,
            sched,
        )?;
    }

    // Flush filter
    let _ = pipeline.buffersrc_ctx.buffersrc_add_frame(None::<AVFrame>, None);
    audio_drain_filter_encode(
        &mut pipeline,
        &mut enc_ctx,
        &pkt_tx,
        out_stream_idx,
        enc_tb,
        out_tb,
        sched,
    )?;

    // Flush encoder
    if enc_ctx.codec().capabilities & ffi::AV_CODEC_CAP_DELAY as i32 != 0 {
        let _ = enc_ctx.send_frame(None);
        drain_encoder(&mut enc_ctx, &pkt_tx, out_stream_idx, enc_tb, out_tb, sched)?;
    }

    if preroll_skipped > 0 {
        tracing::info!("Audio: skipped {} preroll frames before seek point", preroll_skipped);
    }

    scheduler.mark_finished(out_stream_idx);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn audio_filter_encode(
    pipeline: &mut FilterPipeline,
    enc_ctx: &mut AVCodecContext,
    frame: Option<AVFrame>,
    pkt_tx: &mpsc::Sender<AVPacket>,
    out_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    scheduler: Option<&MuxScheduler>,
) -> Result<()> {
    pipeline.buffersrc_ctx.buffersrc_add_frame(frame, None)?;
    audio_drain_filter_encode(pipeline, enc_ctx, pkt_tx, out_idx, enc_tb, out_tb, scheduler)
}

fn audio_drain_filter_encode(
    pipeline: &mut FilterPipeline,
    enc_ctx: &mut AVCodecContext,
    pkt_tx: &mpsc::Sender<AVPacket>,
    out_idx: usize,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
    scheduler: Option<&MuxScheduler>,
) -> Result<()> {
    loop {
        let mut filtered = match pipeline.buffersink_ctx.buffersink_get_frame(None) {
            Ok(f) => f,
            Err(RsmpegError::BufferSinkDrainError | RsmpegError::BufferSinkEofError) => break,
            Err(_) => return Err(Error::Other("Failed to get audio frame from filter".into())),
        };
        let filter_tb = pipeline.buffersink_ctx.get_time_base();
        filtered.set_time_base(filter_tb);

        // Match FFmpeg CLI do_audio_out(): rescale frame PTS from filter to encoder timebase
        if filtered.pts != ffi::AV_NOPTS_VALUE && (filter_tb.num != enc_tb.num || filter_tb.den != enc_tb.den) {
            filtered.set_pts(unsafe { ffi::av_rescale_q(filtered.pts, filter_tb, enc_tb) });
        }

        enc_ctx.send_frame(Some(&filtered))?;
        drain_encoder(enc_ctx, pkt_tx, out_idx, enc_tb, out_tb, scheduler)?;
    }
    Ok(())
}
