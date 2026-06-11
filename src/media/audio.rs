//! Audio format conversion via FFI.
//!
//! Replaces subprocess calls like:
//!   ffmpeg -i pipe:0 -ar 16000 -ac 1 -`sample_fmt` s16 -f wav pipe:1
//!
//! Uses temp files for I/O (simpler than custom AVIO for this low-frequency use case).

use crate::error::{Error, Result, ResultExt};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avfilter::{AVFilter, AVFilterGraph, AVFilterInOut},
    avformat::{AVFormatContextInput, AVFormatContextOutput},
    avutil::{AVChannelLayout, AVFrame, get_sample_fmt_name},
    error::RsmpegError,
    ffi,
};
use std::ffi::CString;
use std::path::Path;

/// Options for audio conversion.
pub struct AudioConvertOptions {
    /// Target sample rate (e.g. 16000 for speech-to-text).
    pub sample_rate: u32,
    /// Number of output channels (e.g. 1 for mono).
    pub channels: u8,
    /// Output format name (e.g. "wav", "mp3", "aac").
    pub output_format: String,
}

/// Convert audio data to the specified format.
///
/// Reads from `input_data` bytes, decodes, resamples, and encodes to the
/// target format. Returns the encoded output bytes.
pub fn convert_audio(input_data: &[u8], opts: &AudioConvertOptions) -> Result<Vec<u8>> {
    // Write input to temp file
    let tmp_input = std::env::temp_dir().join(format!("tokimo_audio_in_{:x}", timestamp_nanos()));
    let tmp_output = std::env::temp_dir().join(format!(
        "tokimo_audio_out_{:x}.{}",
        timestamp_nanos(),
        &opts.output_format
    ));

    std::fs::write(&tmp_input, input_data)?;

    let result = convert_audio_file(&tmp_input, &tmp_output, opts);

    // Read output before cleanup
    let output = if result.is_ok() {
        std::fs::read(&tmp_output)?
    } else {
        Vec::new()
    };

    let _ = std::fs::remove_file(&tmp_input);
    let _ = std::fs::remove_file(&tmp_output);

    result?;

    if opts.output_format == "wav" {
        let mut wav = output;
        fix_wav_sizes(&mut wav);
        Ok(wav)
    } else {
        Ok(output)
    }
}

/// Convert an audio file on disk.
pub fn convert_audio_file(input_path: &Path, output_path: &Path, opts: &AudioConvertOptions) -> Result<()> {
    let c_input = CString::new(
        input_path
            .to_str()
            .ok_or_else(|| Error::Other("Invalid input path".into()))?,
    )?;
    let c_output = CString::new(
        output_path
            .to_str()
            .ok_or_else(|| Error::Other("Invalid output path".into()))?,
    )?;

    // Open input
    let mut input_ctx = AVFormatContextInput::open(&c_input)?;

    // Find audio stream and set up decoder
    let (stream_idx, mut dec_ctx) = {
        let mut found = None;
        for (i, stream) in input_ctx.streams().iter().enumerate() {
            if stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_AUDIO {
                let codec = AVCodec::find_decoder(stream.codecpar().codec_id)
                    .ok_or_else(|| Error::Other(format!("No decoder for audio stream {i}")))?;
                found = Some((i, codec));
                break;
            }
        }
        let (idx, decoder) = found.ok_or_else(|| Error::Other("No audio stream found".into()))?;
        let stream = &input_ctx.streams()[idx];
        let codecpar = stream.codecpar();

        let mut dec = AVCodecContext::new(&decoder);
        dec.apply_codecpar(&codecpar)?;
        dec.set_pkt_timebase(stream.time_base);
        dec.open(None)?;
        (idx, dec)
    };

    // Determine output encoder
    let enc_name = match opts.output_format.as_str() {
        "wav" => "pcm_s16le",
        "mp3" => "libmp3lame",
        "aac" | "m4a" => crate::common::capabilities::best_aac_encoder(),
        "flac" => "flac",
        "ogg" | "opus" => "libopus",
        other => return Err(Error::Other(format!("Unsupported output format: {other}"))),
    };
    let c_enc_name = CString::new(enc_name)?;
    let encoder =
        AVCodec::find_encoder_by_name(&c_enc_name).ok_or_else(|| Error::Other("Output encoder not found".into()))?;

    let mut enc_ctx = AVCodecContext::new(&encoder);
    enc_ctx.set_sample_rate(opts.sample_rate as i32);
    enc_ctx.set_ch_layout(AVChannelLayout::from_nb_channels(i32::from(opts.channels)).into_inner());

    // Set sample format
    let target_sample_fmt = {
        let sf = unsafe { (*encoder.as_ptr()).sample_fmts };
        if sf.is_null() {
            ffi::AV_SAMPLE_FMT_S16
        } else {
            unsafe { *sf }
        }
    };
    enc_ctx.set_sample_fmt(target_sample_fmt);
    enc_ctx.set_time_base(ffi::AVRational {
        num: 1,
        den: opts.sample_rate as i32,
    });
    unsafe {
        (*enc_ctx.as_mut_ptr()).strict_std_compliance = -2;
    }

    // Create output
    let mut output_ctx = AVFormatContextOutput::create(&c_output)?;

    if output_ctx.oformat().flags & ffi::AVFMT_GLOBALHEADER as i32 != 0 {
        enc_ctx.set_flags(enc_ctx.flags | ffi::AV_CODEC_FLAG_GLOBAL_HEADER as i32);
    }

    enc_ctx.open(None)?;

    {
        let mut out_stream = output_ctx.new_stream();
        out_stream.set_codecpar(enc_ctx.extract_codecpar());
        out_stream.set_time_base(enc_ctx.time_base);
    }

    output_ctx.write_header(&mut None)?;

    // Audio filter for resampling
    let target_ch_layout = AVChannelLayout::from_nb_channels(i32::from(opts.channels));
    let mut filter_graph = AVFilterGraph::new();
    let mut filter_pipeline = init_audio_resample_filter(
        &mut filter_graph,
        &mut dec_ctx,
        target_sample_fmt,
        opts.sample_rate as i32,
        &target_ch_layout,
    )?;

    // Capture time bases before entering mutable borrow loops
    let out_tb = {
        let st = &output_ctx.streams()[0];
        st.time_base
    };
    let enc_tb = enc_ctx.time_base;

    // Decode → filter → encode → write loop
    loop {
        let packet = match input_ctx.read_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => break,
            Err(e) => return Err(Error::Other(format!("Read error: {e:?}"))),
        };

        if packet.stream_index as usize != stream_idx {
            continue;
        }

        dec_ctx.send_packet(Some(&packet)).ok();
        drain_decode_filter_encode(
            &mut dec_ctx,
            &mut filter_pipeline,
            &mut enc_ctx,
            &mut output_ctx,
            enc_tb,
            out_tb,
        )?;
    }

    // Flush decoder
    dec_ctx.send_packet(None).ok();
    drain_decode_filter_encode(
        &mut dec_ctx,
        &mut filter_pipeline,
        &mut enc_ctx,
        &mut output_ctx,
        enc_tb,
        out_tb,
    )?;

    // Flush filter
    filter_pipeline.src.buffersrc_add_frame(None::<AVFrame>, None).ok();
    drain_filter_encode(&mut filter_pipeline, &mut enc_ctx, &mut output_ctx, enc_tb, out_tb)?;

    // Flush encoder
    enc_ctx.send_frame(None).ok();
    drain_encoder(&mut enc_ctx, &mut output_ctx, enc_tb, out_tb)?;

    output_ctx.write_trailer()?;

    Ok(())
}

// ── Internal helpers ────────────────────────────────────────────────────────

struct AudioFilterPipeline<'a> {
    src: rsmpeg::avfilter::AVFilterContextMut<'a>,
    sink: rsmpeg::avfilter::AVFilterContextMut<'a>,
}

fn init_audio_resample_filter<'a>(
    filter_graph: &'a mut AVFilterGraph,
    dec_ctx: &mut AVCodecContext,
    target_sample_fmt: i32,
    target_sample_rate: i32,
    target_ch_layout: &AVChannelLayout,
) -> Result<AudioFilterPipeline<'a>> {
    let buffersrc = AVFilter::get_by_name(c"abuffer").ok_or_else(|| Error::Other("abuffer not found".into()))?;
    let buffersink =
        AVFilter::get_by_name(c"abuffersink").ok_or_else(|| Error::Other("abuffersink not found".into()))?;

    if dec_ctx.ch_layout.order == ffi::AV_CHANNEL_ORDER_UNSPEC {
        dec_ctx.set_ch_layout(AVChannelLayout::from_nb_channels(dec_ctx.ch_layout.nb_channels).into_inner());
    }

    let src_args = format!(
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
    let src_args_c = CString::new(src_args)?;

    let mut src_ctx = filter_graph.create_filter_context(&buffersrc, c"in", Some(&src_args_c))?;

    let mut sink_ctx = filter_graph
        .alloc_filter_context(&buffersink, c"out")
        .ok_or_else(|| Error::Other("Cannot create audio buffer sink".into()))?;
    sink_ctx.opt_set_bin(c"sample_fmts", &target_sample_fmt)?;
    sink_ctx.opt_set(
        c"ch_layouts",
        &target_ch_layout
            .describe()
            .context("failed to describe target channel layout")?,
    )?;
    sink_ctx.opt_set_bin(c"sample_rates", &target_sample_rate)?;
    sink_ctx.init_dict(&mut None)?;

    let target_fmt_name = get_sample_fmt_name(target_sample_fmt)
        .context("invalid target sample format")?
        .to_string_lossy()
        .to_string();
    let target_ch_desc = target_ch_layout
        .describe()
        .context("failed to describe target channel layout")?
        .to_string_lossy()
        .to_string();
    let filter_spec = CString::new(format!(
        "aformat=sample_fmts={target_fmt_name}:channel_layouts={target_ch_desc}:sample_rates={target_sample_rate}"
    ))?;

    let outputs = AVFilterInOut::new(c"in", &mut src_ctx, 0);
    let inputs = AVFilterInOut::new(c"out", &mut sink_ctx, 0);
    let (_inputs, _outputs) = filter_graph.parse_ptr(&filter_spec, Some(inputs), Some(outputs))?;
    filter_graph.config()?;

    Ok(AudioFilterPipeline {
        src: src_ctx,
        sink: sink_ctx,
    })
}

fn drain_decode_filter_encode(
    dec_ctx: &mut AVCodecContext,
    filter: &mut AudioFilterPipeline,
    enc_ctx: &mut AVCodecContext,
    output_ctx: &mut AVFormatContextOutput,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
) -> Result<()> {
    loop {
        let frame = match dec_ctx.receive_frame() {
            Ok(f) => f,
            Err(RsmpegError::DecoderDrainError | RsmpegError::DecoderFlushedError) => {
                break;
            }
            Err(e) => return Err(Error::Other(format!("Decode error: {e:?}"))),
        };

        filter.src.buffersrc_add_frame(Some(frame), None)?;

        drain_filter_encode(filter, enc_ctx, output_ctx, enc_tb, out_tb)?;
    }
    Ok(())
}

fn drain_filter_encode(
    filter: &mut AudioFilterPipeline,
    enc_ctx: &mut AVCodecContext,
    output_ctx: &mut AVFormatContextOutput,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
) -> Result<()> {
    loop {
        let filtered = match filter.sink.buffersink_get_frame(None) {
            Ok(f) => f,
            Err(RsmpegError::BufferSinkDrainError | RsmpegError::BufferSinkEofError) => break,
            Err(_) => return Err(Error::Other("Failed to get audio frame from filter".into())),
        };

        enc_ctx.send_frame(Some(&filtered))?;

        drain_encoder(enc_ctx, output_ctx, enc_tb, out_tb)?;
    }
    Ok(())
}

fn drain_encoder(
    enc_ctx: &mut AVCodecContext,
    output_ctx: &mut AVFormatContextOutput,
    enc_tb: ffi::AVRational,
    out_tb: ffi::AVRational,
) -> Result<()> {
    loop {
        let mut pkt = match enc_ctx.receive_packet() {
            Ok(p) => p,
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => {
                break;
            }
            Err(e) => return Err(Error::Other(format!("Encode error: {e:?}"))),
        };
        pkt.set_stream_index(0);
        pkt.rescale_ts(enc_tb, out_tb);
        output_ctx.interleaved_write_frame(&mut pkt)?;
    }
    Ok(())
}

/// Fix WAV header sizes when writing to a file.
/// `FFmpeg` may write correct sizes, but if piped, it writes 0xFFFFFFFF.
fn fix_wav_sizes(wav: &mut [u8]) {
    if wav.len() < 44 {
        return;
    }
    if &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return;
    }

    // Fix RIFF chunk size (file_size - 8)
    let riff_size = (wav.len() - 8) as u32;
    wav[4..8].copy_from_slice(&riff_size.to_le_bytes());

    // Find "data" chunk and fix its size
    let mut pos = 12;
    while pos + 8 <= wav.len() {
        let chunk_id = &wav[pos..pos + 4];
        if chunk_id == b"data" {
            let data_size = (wav.len() - pos - 8) as u32;
            wav[pos + 4..pos + 8].copy_from_slice(&data_size.to_le_bytes());
            break;
        }
        let chunk_size = u32::from_le_bytes(wav[pos + 4..pos + 8].try_into().unwrap_or([0; 4])) as usize;
        pos += 8 + chunk_size;
    }
}

fn timestamp_nanos() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
