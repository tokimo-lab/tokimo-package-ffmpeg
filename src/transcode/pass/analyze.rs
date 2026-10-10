#[allow(unused_imports)]
use crate::bail;
use crate::error::{Error, Result};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avformat::AVFormatContextInput,
    ffi,
};

use super::super::hw::{HwAccel, format_name};
use super::super::types::{StreamMapping, TranscodeOptions};
use super::config::PassConfig;

/// Decoder contexts for each input stream, indexed by input stream index.
/// `None` for streams that are copied, ignored, or have already been taken
/// by a background thread.
///
/// Lives separately from [`StreamAnalysis`] so that `StreamAnalysis` can be
/// borrowed immutably while decoder contexts are mutably consumed.
pub(super) struct DecodeContexts(pub Vec<Option<AVCodecContext>>);

impl std::ops::Deref for DecodeContexts {
    type Target = Vec<Option<AVCodecContext>>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for DecodeContexts {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Stream mapping and output metadata produced by stream analysis.
///
/// Immutable after construction.  Decoder contexts live in the companion
/// [`DecodeContexts`] so that these fields can be read without a mutable
/// borrow during the encode / demux phases.
pub(super) struct StreamAnalysis {
    pub nb_streams: usize,
    pub stream_map: Vec<StreamMapping>,
    pub out_stream_count: usize,
}

/// Seek the input to the requested position.
pub(super) fn seek(ifmt_ctx: &mut AVFormatContextInput, opts: &TranscodeOptions, cfg: &PassConfig) -> Result<()> {
    if let Some(seek_secs) = opts.seek {
        let seek_ts = (seek_secs * f64::from(ffi::AV_TIME_BASE)) as i64 + cfg.format_start_time;
        unsafe {
            let ret = ffi::avformat_seek_file(ifmt_ctx.as_mut_ptr(), -1, i64::MIN, seek_ts, seek_ts, 0);
            if ret < 0 {
                bail!("Failed to seek to {}s (error {})", seek_secs, ret);
            }
        }
        tracing::info!(
            "[transcode] Seeked to {}s (start_time={:.3}s, target_ts={})",
            seek_secs,
            cfg.format_start_secs,
            seek_ts
        );
    }
    Ok(())
}

/// Iterate input streams, create decoders, and build the stream mapping.
pub(super) fn analyze_streams(
    ifmt_ctx: &mut AVFormatContextInput,
    opts: &TranscodeOptions,
    cfg: &PassConfig,
    decode_accel: &Option<HwAccel>,
) -> Result<(StreamAnalysis, DecodeContexts)> {
    let nb_streams = ifmt_ctx.nb_streams as usize;
    let mut dec_contexts: Vec<Option<AVCodecContext>> = Vec::with_capacity(nb_streams);
    let mut stream_map: Vec<StreamMapping> = Vec::with_capacity(nb_streams);
    let mut out_stream_count: usize = 0;

    let mut first_video_selected = false;
    let mut first_audio_selected = false;

    for (i, in_stream) in ifmt_ctx.streams().iter().enumerate() {
        let codecpar = in_stream.codecpar();
        let codec_type = codecpar.codec_type();

        if (codec_type.is_video() && opts.audio_only)
            || (codec_type.is_audio() && opts.audio_stream_index.is_some_and(|index| index as usize != i))
        {
            dec_contexts.push(None);
            stream_map.push(StreamMapping::Ignore);
            continue;
        }

        let disposition = in_stream.disposition;
        if disposition & ffi::AV_DISPOSITION_ATTACHED_PIC as i32 != 0 {
            dec_contexts.push(None);
            stream_map.push(StreamMapping::Ignore);
            continue;
        }

        if codec_type.is_subtitle() || codecpar.codec_type == ffi::AVMEDIA_TYPE_ATTACHMENT {
            dec_contexts.push(None);
            stream_map.push(StreamMapping::Ignore);
            continue;
        }

        if codec_type.is_video() && !cfg.copy_video {
            if first_video_selected {
                dec_contexts.push(None);
                stream_map.push(StreamMapping::Ignore);
                continue;
            }
            first_video_selected = true;

            let gpu_pipeline = decode_accel.is_some()
                && cfg
                    .encode_hw
                    .is_some_and(|ht| ht.is_hw_encoder(cfg.video_codec_name.as_deref().unwrap_or("")));

            let decoder = AVCodec::find_decoder(codecpar.codec_id)
                .ok_or_else(|| Error::Other(format!("No decoder for video stream #{i}")))?;
            let mut dec_ctx = AVCodecContext::new(&decoder);
            dec_ctx.apply_codecpar(&codecpar)?;
            dec_ctx.set_pkt_timebase(in_stream.time_base);
            if let Some(framerate) = in_stream.guess_framerate() {
                dec_ctx.set_framerate(framerate);
            }

            if let Some(hw) = decode_accel {
                dec_ctx.set_hw_device_ctx(hw.device_ctx.clone());
                unsafe {
                    (*dec_ctx.as_mut_ptr()).pix_fmt = hw.hw_type.pix_fmt();
                    // Match Jellyfin: -hwaccel_flags +unsafe_output
                    (*dec_ctx.as_mut_ptr()).hwaccel_flags |= ffi::AV_HWACCEL_FLAG_UNSAFE_OUTPUT as i32;
                }
                tracing::debug!(
                    "[transcode] video decoder: HW accel={}, pix_fmt={}",
                    hw.hw_type.display_name(),
                    format_name(hw.hw_type.pix_fmt())
                );
            } else {
                tracing::debug!("[transcode] video decoder: SW");
            }
            // GPU handles its own parallelism (thread_count=1).
            // SW benefits from FFmpeg's frame/slice threading (thread_count=0 → auto).
            unsafe {
                (*dec_ctx.as_mut_ptr()).thread_count = i32::from(decode_accel.is_some());
            }

            dec_ctx.open(None)?;

            dec_contexts.push(Some(dec_ctx));
            stream_map.push(StreamMapping::Video {
                in_idx: i,
                out_idx: out_stream_count,
                gpu_pipeline,
            });
            out_stream_count += 1;
        } else if codec_type.is_audio() && !cfg.copy_audio {
            if first_audio_selected {
                dec_contexts.push(None);
                stream_map.push(StreamMapping::Ignore);
                continue;
            }
            first_audio_selected = true;

            let decoder = AVCodec::find_decoder(codecpar.codec_id)
                .ok_or_else(|| Error::Other(format!("No decoder for audio stream #{i}")))?;
            let mut dec_ctx = AVCodecContext::new(&decoder);
            dec_ctx.apply_codecpar(&codecpar)?;
            dec_ctx.set_pkt_timebase(in_stream.time_base);
            unsafe {
                // thread_count=0 → FFmpeg auto-selects thread count.
                // Helps with complex audio formats (DTS-HD MA, TrueHD) where
                // decoding is non-trivial. Audio decoders avoid the race
                // conditions that plague video frame threading.
                (*dec_ctx.as_mut_ptr()).thread_count = 0;
            }
            dec_ctx.open(None)?;

            dec_contexts.push(Some(dec_ctx));
            stream_map.push(StreamMapping::Audio {
                in_idx: i,
                out_idx: out_stream_count,
            });
            out_stream_count += 1;
        } else if codec_type.is_video() || codec_type.is_audio() {
            if codec_type.is_video() && first_video_selected {
                dec_contexts.push(None);
                stream_map.push(StreamMapping::Ignore);
                continue;
            }
            if codec_type.is_audio() && first_audio_selected {
                dec_contexts.push(None);
                stream_map.push(StreamMapping::Ignore);
                continue;
            }
            if codec_type.is_video() {
                first_video_selected = true;
            }
            if codec_type.is_audio() {
                first_audio_selected = true;
            }

            dec_contexts.push(None);
            stream_map.push(StreamMapping::Copy {
                in_idx: i,
                out_idx: out_stream_count,
            });
            out_stream_count += 1;
        } else {
            dec_contexts.push(None);
            stream_map.push(StreamMapping::Ignore);
        }
    }

    // Log stream mapping summary
    {
        let mut video_count = 0u32;
        let mut audio_count = 0u32;
        let mut copy_count = 0u32;
        let mut ignore_count = 0u32;
        for mapping in &stream_map {
            match mapping {
                StreamMapping::Video { .. } => video_count += 1,
                StreamMapping::Audio { .. } => audio_count += 1,
                StreamMapping::Copy { .. } => copy_count += 1,
                StreamMapping::Ignore => ignore_count += 1,
            }
        }
        let mut parts = Vec::new();
        if video_count > 0 {
            parts.push(format!("{video_count}×video(transcode)"));
        }
        if copy_count > 0 {
            parts.push(format!("{copy_count}×video(copy)"));
        }
        if audio_count > 0 {
            parts.push(format!("{audio_count}×audio(transcode)"));
        }
        if ignore_count > 0 {
            parts.push(format!("{ignore_count}×ignore"));
        }
        tracing::info!("[transcode] Streams: {}", parts.join(", "));
        for (i, mapping) in stream_map.iter().enumerate() {
            match mapping {
                StreamMapping::Video {
                    in_idx,
                    out_idx,
                    gpu_pipeline,
                } => tracing::debug!(
                    "[transcode]   stream[{}] → Video (in={}, out={}, gpu={})",
                    i,
                    in_idx,
                    out_idx,
                    gpu_pipeline
                ),
                StreamMapping::Audio { in_idx, out_idx } => {
                    tracing::debug!("[transcode]   stream[{}] → Audio (in={}, out={})", i, in_idx, out_idx);
                }
                StreamMapping::Copy { in_idx, out_idx } => {
                    tracing::debug!("[transcode]   stream[{}] → Copy (in={}, out={})", i, in_idx, out_idx);
                }
                StreamMapping::Ignore => {}
            }
        }
    }

    // Discard packets for ignored streams
    for (i, mapping) in stream_map.iter().enumerate() {
        if matches!(mapping, StreamMapping::Ignore) {
            unsafe {
                let stream_ptr = *(*ifmt_ctx.as_mut_ptr()).streams.add(i);
                (*stream_ptr).discard = ffi::AVDISCARD_ALL;
            }
        }
    }

    Ok((
        StreamAnalysis {
            nb_streams,
            stream_map,
            out_stream_count,
        },
        DecodeContexts(dec_contexts),
    ))
}
