use crate::error::{Result, ResultExt};
use rsmpeg::{avformat::AVFormatContextOutput, avutil::AVFrame};
use std::sync::atomic::Ordering;

use super::super::encode::{filter_encode_write_frame, flush_encoder};
use super::super::hw::HwAccel;
use super::super::types::StreamMapping;
use super::analyze::{DecodeContexts, StreamAnalysis};
use super::config::PassConfig;
use super::encode::EncodePipeline;
use super::targets::StreamTargets;

/// Flush remaining inline streams, write trailer, and release all resources
/// in the correct order.
///
/// This is called after [`run_demux_loop`][super::demux::run_demux_loop]
/// completes (normally or cancelled).  When cancelled, only resource cleanup
/// is performed; the trailer is skipped.
#[allow(clippy::too_many_arguments)]
pub(super) fn flush_and_finalize(
    cfg: &PassConfig,
    analysis: &StreamAnalysis,
    dec_ctxs: &mut DecodeContexts,
    targets: &StreamTargets,
    pipeline: &mut EncodePipeline,
    ofmt_ctx: &mut AVFormatContextOutput,
    decode_accel: &Option<HwAccel>,
) -> Result<()> {
    let was_cancelled = cfg.cancel.as_ref().is_some_and(|c| c.load(Ordering::Relaxed));
    let decode_hw = decode_accel.as_ref().map(|h| h.hw_type);

    if !was_cancelled {
        // ── Flush remaining inline streams ───────────────────────────────
        for i in 0..analysis.nb_streams {
            match &analysis.stream_map[i] {
                StreamMapping::Video { out_idx, .. } | StreamMapping::Audio { out_idx, .. } => {
                    let out_idx = *out_idx;
                    if dec_ctxs[i].is_none() {
                        continue; // already handled by threads
                    }
                    let is_gpu = targets.video[i].as_ref().is_some_and(|t| t.gpu_pipeline);
                    let dec_ctx = dec_ctxs[i].as_mut().context("decode context missing during flush")?;
                    let enc_ctx = pipeline.enc_contexts[i]
                        .as_mut()
                        .context("encoder context missing during flush")?;
                    let fp = pipeline.filter_pipelines[i]
                        .as_mut()
                        .context("filter pipeline missing during flush")?;

                    let _ = dec_ctx.send_packet(None);
                    loop {
                        let Ok(frame) = dec_ctx.receive_frame() else { break };
                        let processed =
                            if frame.format == decode_hw.map_or(-1, super::super::hw::HwType::pix_fmt) && !is_gpu {
                                let mut sw = AVFrame::new();
                                if sw.hwframe_transfer_data(&frame).is_err() {
                                    continue;
                                }
                                sw.set_pts(frame.pts);
                                sw
                            } else {
                                frame
                            };
                        let mut f = processed;
                        f.set_pts(f.best_effort_timestamp);
                        filter_encode_write_frame(Some(f), fp, enc_ctx, ofmt_ctx, out_idx)?;
                    }
                    filter_encode_write_frame(None, fp, enc_ctx, ofmt_ctx, out_idx)?;
                    flush_encoder(enc_ctx, ofmt_ctx, out_idx)?;
                }
                _ => {}
            }
        }

        ofmt_ctx.write_trailer()?;
    }

    // ── Cleanup ───────────────────────────────────────────────────────────
    // Drop pipelines (and their backing graphs) before encoder contexts.
    // Each OwnedFilterPipeline handles its own internal drop order.
    for p in &mut pipeline.filter_pipelines {
        *p = None;
    }
    for e in &mut pipeline.enc_contexts {
        *e = None;
    }
    for d in dec_ctxs.iter_mut() {
        *d = None;
    }

    Ok(())
}
