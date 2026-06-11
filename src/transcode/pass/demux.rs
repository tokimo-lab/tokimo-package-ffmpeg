#[allow(unused_imports)]
use crate::bail;
use crate::error::{Result, ResultExt};
use rsmpeg::{
    avcodec::AVPacket,
    avformat::{AVFormatContextInput, AVFormatContextOutput},
    avutil::AVFrame,
    error::RsmpegError,
    ffi,
};
use std::sync::atomic::Ordering;
use std::sync::{Arc, mpsc};
use std::time::Instant;

use super::super::dts::DtsEstimator;
use super::super::encode::filter_encode_write_frame;
use super::super::hw::HwAccel;
use super::super::types::{StreamMapping, TranscodeOptions};
use super::super::{pipeline, scheduler};
use super::analyze::{DecodeContexts, StreamAnalysis};
use super::config::PassConfig;
use super::encode::EncodePipeline;
use super::targets::StreamTargets;

/// Main demux→dispatch→mux loop with threaded video/audio pipelines.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn run_demux_loop(
    ifmt_ctx: &mut AVFormatContextInput,
    opts: &TranscodeOptions,
    cfg: &PassConfig,
    analysis: &StreamAnalysis,
    dec_ctxs: &mut DecodeContexts,
    targets: &StreamTargets,
    pipe: &mut EncodePipeline,
    ofmt_ctx: &mut AVFormatContextOutput,
    decode_accel: &Option<HwAccel>,
) -> Result<()> {
    let copy_video = cfg.copy_video;
    let copy_audio = cfg.copy_audio;

    // ── Pre-compute PTS thresholds ───────────────────────────────────────
    let duration_end_pts: Vec<i64> = if let (Some(dur), Some(seek)) = (opts.duration, opts.seek) {
        ifmt_ctx
            .streams()
            .iter()
            .map(|s| {
                let tb = s.time_base;
                if tb.den > 0 {
                    ((seek + dur + cfg.format_start_secs) * f64::from(tb.den) / f64::from(tb.num)) as i64
                } else {
                    i64::MAX
                }
            })
            .collect()
    } else if let Some(dur) = opts.duration {
        ifmt_ctx
            .streams()
            .iter()
            .map(|s| {
                let tb = s.time_base;
                if tb.den > 0 {
                    ((dur + cfg.format_start_secs) * f64::from(tb.den) / f64::from(tb.num)) as i64
                } else {
                    i64::MAX
                }
            })
            .collect()
    } else {
        vec![i64::MAX; analysis.nb_streams]
    };

    let seek_pts_per_stream: Vec<i64> = if let Some(seek_secs) = opts.seek {
        if copy_video || copy_audio {
            vec![0i64; analysis.nb_streams]
        } else {
            let seek_with_offset = seek_secs + cfg.format_start_secs;
            ifmt_ctx
                .streams()
                .iter()
                .map(|s| {
                    let tb = s.time_base;
                    if tb.den > 0 && seek_with_offset > 0.0 {
                        (seek_with_offset * f64::from(tb.den) / f64::from(tb.num)) as i64
                    } else {
                        0
                    }
                })
                .collect()
        }
    } else {
        vec![0i64; analysis.nb_streams]
    };

    // After seeking in copy mode, skip until the first video keyframe.
    let mut awaiting_copy_keyframe = false;
    let mut copy_video_stream_idx: Option<usize> = None;
    let mut copy_keyframe_pts: i64 = ffi::AV_NOPTS_VALUE;
    if opts.seek.is_some() {
        for (i, mapping) in analysis.stream_map.iter().enumerate() {
            if let StreamMapping::Copy { .. } = mapping
                && ifmt_ctx.streams()[i].codecpar().codec_type().is_video()
            {
                awaiting_copy_keyframe = true;
                copy_video_stream_idx = Some(i);
                break;
            }
        }
    }

    // ── Identify streams for threaded processing ─────────────────────────
    let audio_stream_idx = analysis
        .stream_map
        .iter()
        .position(|m| matches!(m, StreamMapping::Audio { .. }));

    let threaded_video_idx = analysis.stream_map.iter().enumerate().find_map(|(i, m)| {
        if let StreamMapping::Video { .. } = m
            && pipe.filter_pipelines[i].is_some()
            && pipe.enc_contexts[i].is_some()
        {
            return Some(i);
        }
        None
    });

    let decode_hw = decode_accel.as_ref().map(|h| h.hw_type);
    let nb_streams = analysis.nb_streams;
    let out_stream_count = analysis.out_stream_count;
    let cancel = cfg.cancel.clone();

    // ── Launch threaded pipelines + run main packet loop ─────────────────
    std::thread::scope(|scope| -> Result<()> {
        let sched = Arc::new(scheduler::MuxScheduler::new(out_stream_count));

        // Audio thread
        let mut audio_pkt_tx: Option<mpsc::SyncSender<AVPacket>> = None;
        let mut audio_enc_rx: Option<mpsc::Receiver<AVPacket>> = None;

        if let Some(audio_idx) = audio_stream_idx {
            let audio_out_idx = match &analysis.stream_map[audio_idx] {
                StreamMapping::Audio { out_idx, .. } => *out_idx,
                _ => unreachable!(),
            };
            let audio_dec = dec_ctxs[audio_idx].take().context("audio decode context missing")?;
            let audio_fp = pipe.filter_pipelines[audio_idx]
                .take()
                .context("audio filter pipeline missing")?;
            let audio_enc = pipe.enc_contexts[audio_idx]
                .take()
                .context("audio encoder context missing")?;
            let enc_tb = audio_enc.time_base;
            let out_tb = ofmt_ctx.streams()[audio_out_idx].time_base;
            let audio_seek_pts = seek_pts_per_stream.get(audio_idx).copied().unwrap_or(0);

            let (tx, rx) = mpsc::sync_channel(64);
            let (enc_tx, enc_rx) = mpsc::channel();
            audio_pkt_tx = Some(tx);
            audio_enc_rx = Some(enc_rx);

            let audio_sched = Arc::clone(&sched);
            let audio_cancel = cancel.clone();
            scope.spawn(move || {
                pipeline::audio_thread_loop(
                    audio_dec,
                    audio_fp,
                    audio_enc,
                    rx,
                    enc_tx,
                    audio_out_idx,
                    enc_tb,
                    out_tb,
                    audio_seek_pts,
                    audio_sched,
                    audio_cancel,
                )
            });
        }

        // Video decode + filter+encode threads
        let mut video_pkt_tx: Option<mpsc::SyncSender<AVPacket>> = None;
        let mut video_enc_rx: Option<mpsc::Receiver<AVPacket>> = None;
        let mut video_threaded_idx: Option<usize> = None;

        if let Some(vid_idx) = threaded_video_idx {
            let vid_out_idx = match &analysis.stream_map[vid_idx] {
                StreamMapping::Video { out_idx, .. } => *out_idx,
                _ => unreachable!(),
            };
            let video_dec = dec_ctxs[vid_idx].take().context("video decode context missing")?;
            let video_fp = pipe.filter_pipelines[vid_idx]
                .take()
                .context("video filter pipeline missing")?;
            let video_enc = pipe.enc_contexts[vid_idx]
                .take()
                .context("video encoder context missing")?;
            let enc_tb = video_enc.time_base;
            let out_tb = ofmt_ctx.streams()[vid_out_idx].time_base;
            let video_seek_pts = seek_pts_per_stream[vid_idx];

            let is_gpu = targets.video[vid_idx].as_ref().is_some_and(|t| t.gpu_pipeline);
            let (pkt_buf, frame_buf) = if is_gpu { (4, 2) } else { (16, 8) };
            let (pkt_s, pkt_r) = mpsc::sync_channel::<AVPacket>(pkt_buf);
            let (frame_s, frame_r) = mpsc::sync_channel::<AVFrame>(frame_buf);
            let (enc_s, enc_r) = mpsc::channel::<AVPacket>();

            video_pkt_tx = Some(pkt_s);
            video_enc_rx = Some(enc_r);
            video_threaded_idx = Some(vid_idx);

            let hw_download_fmt: Option<i32> = if is_gpu {
                None
            } else {
                decode_hw.map(super::super::hw::HwType::pix_fmt)
            };

            scope
                .spawn(move || pipeline::video_decode_loop(video_dec, pkt_r, frame_s, video_seek_pts, hw_download_fmt));

            let video_sched = Arc::clone(&sched);
            scope.spawn(move || {
                pipeline::video_filter_encode_loop(
                    video_fp,
                    video_enc,
                    frame_r,
                    enc_s,
                    vid_out_idx,
                    enc_tb,
                    out_tb,
                    video_sched,
                )
            });

            tracing::debug!(
                "[transcode] {} thread pipeline: demux → [decode] → [filter+encode] → mux",
                if is_gpu { "GPU" } else { "SW" }
            );
        }

        // Per-stream DTS estimators for Copy streams
        let mut dts_estimators: Vec<Option<DtsEstimator>> = (0..nb_streams)
            .map(|i| match &analysis.stream_map[i] {
                StreamMapping::Copy { .. } => {
                    let in_stream = unsafe { &*(*ifmt_ctx.as_ptr()).streams.add(i).read() };
                    let par = unsafe { &*in_stream.codecpar };
                    Some(DtsEstimator::new(in_stream, par))
                }
                _ => None,
            })
            .collect();

        let pause = opts.pause.clone();

        // ── Main demux + mux loop ────────────────────────────────────────
        let mut first_pkt_logged = false;
        let mut pkt_count: u64 = 0;
        let t_loop_start = Instant::now();
        let mut t_read_us: u64 = 0;
        let mut t_mux_us: u64 = 0;
        let mut t_dispatch_us: u64 = 0;
        let mut read_calls: u64 = 0;
        let mut copy_pkts: u64 = 0;
        let mut audio_pkts: u64 = 0;
        let mut audio_drained: u64 = 0;
        let mut first_seg_logged = false;

        loop {
            if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                tracing::info!("Transcode cancelled by caller");
                break;
            }
            while pause.as_ref().is_some_and(|p| p.load(Ordering::Relaxed)) {
                if cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed)) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }

            let t0 = Instant::now();
            let packet = match ifmt_ctx.read_packet() {
                Ok(Some(pkt)) => pkt,
                Ok(None) => break,
                Err(e) => bail!("Error reading packet: {:?}", e),
            };
            t_read_us += t0.elapsed().as_micros() as u64;
            read_calls += 1;

            if !first_pkt_logged {
                first_pkt_logged = true;
                tracing::info!(
                    "[transcode] First packet: {:.0}ms (stream={}, size={}B)",
                    t_loop_start.elapsed().as_secs_f64() * 1000.0,
                    packet.stream_index,
                    packet.size,
                );
            }
            pkt_count += 1;

            let in_stream_idx = packet.stream_index as usize;
            if in_stream_idx >= nb_streams {
                continue;
            }

            if packet.pts != ffi::AV_NOPTS_VALUE && packet.pts >= duration_end_pts[in_stream_idx] {
                break;
            }

            // Copy mode: skip until first keyframe after seek
            if awaiting_copy_keyframe {
                if copy_video_stream_idx == Some(in_stream_idx) && packet.flags & ffi::AV_PKT_FLAG_KEY as i32 != 0 {
                    awaiting_copy_keyframe = false;
                    copy_keyframe_pts = packet.pts;
                    tracing::debug!(
                        "[transcode] Copy keyframe gate cleared at PTS {} (skipped {} pkts)",
                        packet.pts,
                        pkt_count - 1
                    );
                } else {
                    continue;
                }
            } else if copy_keyframe_pts != ffi::AV_NOPTS_VALUE
                && packet.pts != ffi::AV_NOPTS_VALUE
                && packet.pts < copy_keyframe_pts
                && copy_video_stream_idx != Some(in_stream_idx)
            {
                continue;
            }

            match &analysis.stream_map[in_stream_idx] {
                // Threaded video → decode thread
                StreamMapping::Video { .. } if video_threaded_idx == Some(in_stream_idx) => {
                    if let Some(ref tx) = video_pkt_tx {
                        let _ = tx.send(packet);
                    }
                }
                // Audio → audio thread
                StreamMapping::Audio { .. } if pipe.filter_pipelines[in_stream_idx].is_none() => {
                    let audio_seek = seek_pts_per_stream[in_stream_idx];
                    if audio_seek > 0 && packet.pts != ffi::AV_NOPTS_VALUE && packet.pts < audio_seek {
                        continue;
                    }
                    let t0 = Instant::now();
                    if let Some(ref tx) = audio_pkt_tx {
                        let _ = tx.send(packet);
                    }
                    t_dispatch_us += t0.elapsed().as_micros() as u64;
                    audio_pkts += 1;
                }
                // Inline video/audio (non-GPU fallback)
                StreamMapping::Video { out_idx, .. } | StreamMapping::Audio { out_idx, .. } => {
                    let out_idx = *out_idx;
                    let is_gpu = targets.video[in_stream_idx].as_ref().is_some_and(|t| t.gpu_pipeline);
                    let video_seek_pts = seek_pts_per_stream[in_stream_idx];

                    let dec_ctx = dec_ctxs[in_stream_idx]
                        .as_mut()
                        .context("decode context missing for inline stream")?;
                    dec_ctx.send_packet(Some(&packet)).unwrap_or_else(|e| {
                        tracing::info!("Warning: send_packet failed for stream {}: {:?}", in_stream_idx, e);
                    });

                    let enc_ctx = pipe.enc_contexts[in_stream_idx]
                        .as_mut()
                        .context("encoder context missing for inline stream")?;
                    let fp = pipe.filter_pipelines[in_stream_idx]
                        .as_mut()
                        .context("filter pipeline missing for inline stream")?;

                    loop {
                        let frame = match dec_ctx.receive_frame() {
                            Ok(f) => f,
                            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => break,
                            Err(e) => {
                                tracing::info!("Warning: receive_frame error: {:?}", e);
                                break;
                            }
                        };
                        let pts = frame.best_effort_timestamp;
                        if video_seek_pts > 0 && pts != ffi::AV_NOPTS_VALUE && pts < video_seek_pts {
                            continue;
                        }
                        let processed =
                            if frame.format == decode_hw.map_or(-1, super::super::hw::HwType::pix_fmt) && !is_gpu {
                                let mut sw = AVFrame::new();
                                sw.hwframe_transfer_data(&frame)?;
                                sw.set_pts(frame.pts);
                                sw
                            } else {
                                frame
                            };
                        let mut f = processed;
                        let pts = f.best_effort_timestamp;
                        if pts != ffi::AV_NOPTS_VALUE {
                            f.set_pts(pts);
                        }
                        filter_encode_write_frame(Some(f), fp, enc_ctx, ofmt_ctx, out_idx)?;
                    }
                }
                StreamMapping::Copy { out_idx, .. } => {
                    let out_idx = *out_idx;
                    let in_stream = &ifmt_ctx.streams()[in_stream_idx];
                    let out_stream = &ofmt_ctx.streams()[out_idx];
                    let mut pkt = packet;
                    if let Some(ref mut est) = dts_estimators[in_stream_idx] {
                        est.fix_timestamps(&mut pkt);
                    }
                    pkt.rescale_ts(in_stream.time_base, out_stream.time_base);
                    let dts_us = scheduler::dts_to_us(pkt.dts, out_stream.time_base);
                    pkt.set_stream_index(out_idx as i32);
                    pkt.set_pos(-1);
                    let t0 = Instant::now();
                    ofmt_ctx.interleaved_write_frame(&mut pkt)?;
                    t_mux_us += t0.elapsed().as_micros() as u64;
                    sched.report_dts(out_idx, dts_us);
                    copy_pkts += 1;
                }
                StreamMapping::Ignore => {}
            }

            // Drain threaded outputs
            if let Some(ref rx) = video_enc_rx {
                while let Ok(mut pkt) = rx.try_recv() {
                    let t0 = Instant::now();
                    ofmt_ctx.interleaved_write_frame(&mut pkt)?;
                    t_mux_us += t0.elapsed().as_micros() as u64;
                }
            }
            if let Some(ref rx) = audio_enc_rx {
                while let Ok(mut pkt) = rx.try_recv() {
                    let t0 = Instant::now();
                    ofmt_ctx.interleaved_write_frame(&mut pkt)?;
                    t_mux_us += t0.elapsed().as_micros() as u64;
                    audio_drained += 1;
                }
            }

            if !first_seg_logged && pkt_count >= 50 {
                let elapsed = t_loop_start.elapsed().as_millis();
                if elapsed > 500 {
                    first_seg_logged = true;
                    tracing::info!(
                        "[transcode] Segment timing: total={}ms | read={}ms ({} calls) | mux={}ms | dispatch={}ms | copy={} audio={} drained={}",
                        elapsed,
                        t_read_us / 1000,
                        read_calls,
                        t_mux_us / 1000,
                        t_dispatch_us / 1000,
                        copy_pkts,
                        audio_pkts,
                        audio_drained,
                    );
                }
            }
        }

        // ── Signal threads to finish ─────────────────────────────────────
        let was_cancelled = cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
        if was_cancelled {
            for i in 0..out_stream_count {
                sched.mark_finished(i);
            }
            drop(video_enc_rx);
            drop(audio_enc_rx);
            drop(video_pkt_tx);
            drop(audio_pkt_tx);
        } else {
            drop(video_pkt_tx);
            if let Some(rx) = video_enc_rx {
                while let Ok(mut pkt) = rx.recv() {
                    ofmt_ctx.interleaved_write_frame(&mut pkt)?;
                }
            }
            drop(audio_pkt_tx);
            if let Some(rx) = audio_enc_rx {
                while let Ok(mut pkt) = rx.recv() {
                    ofmt_ctx.interleaved_write_frame(&mut pkt)?;
                }
            }
        }

        Ok(())
    })?;

    Ok(())
}
