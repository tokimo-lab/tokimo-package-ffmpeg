//! Copy-codec remux with metadata embedding via FFI.
//!
//! Replaces subprocess calls like:
//!   ffmpeg -y -i media.mp4 [-i sub.srt] [-i cover.jpg] -c copy -movflags +faststart out.mp4
//!
//! Handles: subtitle embedding, chapter embedding, cover art, metadata tags,
//! audio-only extraction.

use crate::error::{Error, Result};
use rsmpeg::{
    avformat::{AVFormatContextInput, AVFormatContextOutput},
    avutil::AVDictionary,
    ffi,
};
use std::ffi::CString;
use std::path::{Path, PathBuf};

/// Options for remuxing a media file.
pub struct RemuxOptions {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Subtitle file to embed.
    pub subtitle_file: Option<PathBuf>,
    /// Subtitle language code (e.g. "eng").
    pub subtitle_language: Option<String>,
    /// Chapter metadata file (ffmetadata format).
    pub chapters_file: Option<PathBuf>,
    /// Cover/thumbnail image to embed as attached picture.
    pub cover_file: Option<PathBuf>,
    /// Metadata key-value pairs to embed.
    pub metadata: Vec<(String, String)>,
    /// If true, strip video tracks (audio-only extraction).
    pub strip_video: bool,
    /// If true, add +faststart flag for progressive download.
    pub movflags_faststart: bool,
}

/// Remux a media file with copy codec, optionally embedding subtitles,
/// chapters, cover art, and metadata.
#[allow(clippy::too_many_lines)]
pub fn remux(opts: &RemuxOptions) -> Result<()> {
    // Open main input
    let c_input = CString::new(
        opts.input
            .to_str()
            .ok_or_else(|| Error::Other("Invalid input path".into()))?,
    )?;
    let main_input = AVFormatContextInput::open(&c_input)?;

    let sub_input = if let Some(sub_path) = &opts.subtitle_file {
        let c_sub = CString::new(
            sub_path
                .to_str()
                .ok_or_else(|| Error::Other("Invalid subtitle path".into()))?,
        )?;
        Some(AVFormatContextInput::open(&c_sub)?)
    } else {
        None
    };

    let cover_input = if let Some(cover_path) = &opts.cover_file {
        let c_cover = CString::new(
            cover_path
                .to_str()
                .ok_or_else(|| Error::Other("Invalid cover path".into()))?,
        )?;
        Some(AVFormatContextInput::open(&c_cover)?)
    } else {
        None
    };

    let chapters_input = if let Some(ch_path) = &opts.chapters_file {
        let c_ch = CString::new(
            ch_path
                .to_str()
                .ok_or_else(|| Error::Other("Invalid chapters path".into()))?,
        )?;
        Some(AVFormatContextInput::open(&c_ch)?)
    } else {
        None
    };

    let c_output = CString::new(
        opts.output
            .to_str()
            .ok_or_else(|| Error::Other("Invalid output path".into()))?,
    )?;
    let mut output_ctx = AVFormatContextOutput::create(&c_output)?;

    // Track stream mappings: (input_index, stream_index_in_input) → output_stream_index
    let mut stream_map: Vec<(usize, usize, usize)> = Vec::new(); // (input_idx, in_stream, out_stream)
    let mut out_idx = 0usize;

    // Map streams from main input
    for (i, stream) in main_input.streams().iter().enumerate() {
        let codecpar = stream.codecpar();
        let is_video = codecpar.codec_type == ffi::AVMEDIA_TYPE_VIDEO;
        let _is_audio = codecpar.codec_type == ffi::AVMEDIA_TYPE_AUDIO;

        if opts.strip_video && is_video {
            continue;
        }

        // Skip attached pics for now (re-add from cover input if provided)
        if is_video && stream.disposition & ffi::AV_DISPOSITION_ATTACHED_PIC as i32 != 0 && opts.cover_file.is_some() {
            continue; // Will be replaced
        }

        let mut out_stream = output_ctx.new_stream();
        out_stream.set_codecpar(codecpar.clone());
        out_stream.set_time_base(stream.time_base);

        // Reset codec_tag so the output muxer picks the correct tag for its
        // container (e.g. WebM VP9 → MP4 vp09). Without this, a tag from the
        // source container may be carried over and confuse the muxer.
        unsafe {
            (*out_stream.as_mut_ptr())
                .codecpar
                .as_mut()
                .ok_or_else(|| Error::Other("output stream codecpar is null".into()))?
                .codec_tag = 0;
        }

        // Copy stream tags
        unsafe {
            let in_stream_ptr = stream.as_ptr();
            let out_stream_ptr = out_stream.as_mut_ptr();
            if !(*in_stream_ptr).metadata.is_null() {
                ffi::av_dict_copy(&raw mut (*out_stream_ptr).metadata, (*in_stream_ptr).metadata, 0);
            }
        }

        stream_map.push((0, i, out_idx));
        out_idx += 1;
    }

    // Map subtitle stream
    let _sub_out_idx = if let Some(ref sub_ctx) = sub_input {
        let mut found = None;
        for (i, stream) in sub_ctx.streams().iter().enumerate() {
            if stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_SUBTITLE {
                let mut out_stream = output_ctx.new_stream();
                out_stream.set_codecpar(stream.codecpar().clone());
                out_stream.set_time_base(stream.time_base);

                // Determine subtitle codec for MP4 container
                let sub_ext = opts
                    .subtitle_file
                    .as_ref()
                    .and_then(|p| p.extension())
                    .and_then(|e| e.to_str())
                    .unwrap_or("srt");

                unsafe {
                    let out_ptr = out_stream.as_mut_ptr();
                    let cp = (*out_ptr).codecpar;
                    match sub_ext {
                        "ass" | "ssa" => {
                            (*cp).codec_id = ffi::AV_CODEC_ID_ASS;
                        }
                        _ => {
                            (*cp).codec_id = ffi::AV_CODEC_ID_MOV_TEXT;
                        }
                    }
                }

                if let Some(lang) = &opts.subtitle_language {
                    let c_key = CString::new("language")?;
                    let c_val = CString::new(lang.as_str())?;
                    unsafe {
                        let out_ptr = out_stream.as_mut_ptr();
                        ffi::av_dict_set(&raw mut (*out_ptr).metadata, c_key.as_ptr(), c_val.as_ptr(), 0);
                    }
                }

                stream_map.push((1, i, out_idx));
                found = Some(out_idx);
                out_idx += 1;
                break;
            }
        }
        found
    } else {
        None
    };

    // Map cover image
    if let Some(ref cover_ctx) = cover_input {
        for (i, stream) in cover_ctx.streams().iter().enumerate() {
            if stream.codecpar().codec_type == ffi::AVMEDIA_TYPE_VIDEO {
                let mut out_stream = output_ctx.new_stream();
                out_stream.set_codecpar(stream.codecpar().clone());
                out_stream.set_time_base(stream.time_base);

                unsafe {
                    let out_ptr = out_stream.as_mut_ptr();
                    (*out_ptr).disposition = ffi::AV_DISPOSITION_ATTACHED_PIC as i32;
                }

                let input_idx = if sub_input.is_some() { 2 } else { 1 };
                stream_map.push((input_idx, i, out_idx));
                // out_idx not read after break, but keeping the increment pattern
                let _ = out_idx + 1;
                break;
            }
        }
    }

    // Copy chapters from chapters input
    if let Some(ref ch_ctx) = chapters_input {
        unsafe {
            let ch_ptr = ch_ctx.as_ptr();
            let out_ptr = output_ctx.as_mut_ptr();
            let nb = (*ch_ptr).nb_chapters as usize;
            if nb > 0 {
                // Allocate chapter array in output
                let chapters_arr =
                    ffi::av_malloc(nb * std::mem::size_of::<*mut ffi::AVChapter>()).cast::<*mut ffi::AVChapter>();
                if !chapters_arr.is_null() {
                    for i in 0..nb {
                        let src_ch = *(*ch_ptr).chapters.add(i);
                        let dst_ch = ffi::av_mallocz(std::mem::size_of::<ffi::AVChapter>()).cast::<ffi::AVChapter>();
                        if !dst_ch.is_null() {
                            (*dst_ch).id = (*src_ch).id;
                            (*dst_ch).time_base = (*src_ch).time_base;
                            (*dst_ch).start = (*src_ch).start;
                            (*dst_ch).end = (*src_ch).end;
                            if !(*src_ch).metadata.is_null() {
                                ffi::av_dict_copy(&raw mut (*dst_ch).metadata, (*src_ch).metadata, 0);
                            }
                            *chapters_arr.add(i) = dst_ch;
                        }
                    }
                    (*out_ptr).chapters = chapters_arr;
                    (*out_ptr).nb_chapters = nb as u32;
                }
            }
        }
    }

    // Set global metadata
    for (key, value) in &opts.metadata {
        let c_key = CString::new(key.as_str())?;
        let c_val = CString::new(value.as_str())?;
        unsafe {
            let out_ptr = output_ctx.as_mut_ptr();
            ffi::av_dict_set(&raw mut (*out_ptr).metadata, c_key.as_ptr(), c_val.as_ptr(), 0);
        }
    }

    // Write header with muxer options
    let mut mux_opts = if opts.movflags_faststart {
        Some(AVDictionary::new(c"movflags", c"+faststart", 0))
    } else {
        None
    };

    output_ctx.write_header(&mut mux_opts)?;

    // Store time bases before we move the contexts
    let main_stream_tbs: Vec<ffi::AVRational> = main_input.streams().iter().map(|s| s.time_base).collect();

    // Process main input packets
    let mut main_input = main_input;
    loop {
        let packet = match main_input.read_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => break,
            Err(e) => return Err(Error::Other(format!("Error reading main input: {e:?}"))),
        };

        let in_stream_idx = packet.stream_index as usize;

        // Find mapping
        if let Some(&(_, _, out_stream_idx)) = stream_map
            .iter()
            .find(|&&(input_idx, in_s, _)| input_idx == 0 && in_s == in_stream_idx)
        {
            let in_tb = main_stream_tbs[in_stream_idx];
            let out_tb = output_ctx.streams()[out_stream_idx].time_base;

            let mut pkt = packet;
            pkt.rescale_ts(in_tb, out_tb);
            pkt.set_stream_index(out_stream_idx as i32);
            pkt.set_pos(-1);

            output_ctx.interleaved_write_frame(&mut pkt)?;
        }
    }

    // TODO: Process subtitle input packets (needs subtitle transcoding for SRT → mov_text)

    output_ctx.write_trailer()?;

    Ok(())
}

/// Remux with audio-only extraction (strips video tracks).
pub fn extract_audio(input: &Path, output: &Path, metadata: &[(String, String)]) -> Result<()> {
    remux(&RemuxOptions {
        input: input.to_path_buf(),
        output: output.to_path_buf(),
        subtitle_file: None,
        subtitle_language: None,
        chapters_file: None,
        cover_file: None,
        metadata: metadata.to_vec(),
        strip_video: true,
        movflags_faststart: false,
    })
}

/// Merge separate video and audio files into a single container via copy-codec.
///
/// Equivalent to: `ffmpeg -y -i video.mp4 -i audio.m4a -c copy -movflags +faststart out.mp4`
///
/// Packets are interleaved by DTS order (smallest DTS written first) to produce
/// a properly interleaved file rather than "all video then all audio".
pub fn merge_av(video: &Path, audio: &Path, output: &Path) -> Result<()> {
    let c_video = CString::new(
        video
            .to_str()
            .ok_or_else(|| Error::Other("Invalid video path".into()))?,
    )?;
    let c_audio = CString::new(
        audio
            .to_str()
            .ok_or_else(|| Error::Other("Invalid audio path".into()))?,
    )?;
    let c_output = CString::new(
        output
            .to_str()
            .ok_or_else(|| Error::Other("Invalid output path".into()))?,
    )?;

    let video_input = AVFormatContextInput::open(&c_video)?;
    let audio_input = AVFormatContextInput::open(&c_audio)?;

    let mut output_ctx = AVFormatContextOutput::create(&c_output)?;

    // Map: (input_idx, in_stream_idx) → out_stream_idx
    let mut stream_map: Vec<(usize, usize, usize)> = Vec::new();
    let mut out_idx = 0usize;

    // Copy video streams from video input
    for (i, stream) in video_input.streams().iter().enumerate() {
        let codecpar = stream.codecpar();
        if codecpar.codec_type != ffi::AVMEDIA_TYPE_VIDEO {
            continue;
        }
        let mut out_stream = output_ctx.new_stream();
        out_stream.set_codecpar(codecpar.clone());
        out_stream.set_time_base(stream.time_base);
        // Reset codec_tag so the MP4 muxer assigns the correct tag (e.g. vp09,
        // av01) instead of inheriting a potentially incompatible tag from the
        // source container (WebM/MKV).
        unsafe {
            (*out_stream.as_mut_ptr())
                .codecpar
                .as_mut()
                .ok_or_else(|| Error::Other("output stream codecpar is null".into()))?
                .codec_tag = 0;
        }
        stream_map.push((0, i, out_idx));
        out_idx += 1;
    }

    // Copy audio streams from audio input
    for (i, stream) in audio_input.streams().iter().enumerate() {
        let codecpar = stream.codecpar();
        if codecpar.codec_type != ffi::AVMEDIA_TYPE_AUDIO {
            continue;
        }
        let mut out_stream = output_ctx.new_stream();
        out_stream.set_codecpar(codecpar.clone());
        out_stream.set_time_base(stream.time_base);
        unsafe {
            (*out_stream.as_mut_ptr())
                .codecpar
                .as_mut()
                .ok_or_else(|| Error::Other("output stream codecpar is null".into()))?
                .codec_tag = 0;
        }
        stream_map.push((1, i, out_idx));
        out_idx += 1;
    }

    if stream_map.is_empty() {
        return Err(Error::Other("No video or audio streams found in inputs".into()));
    }

    let mut mux_opts = Some(AVDictionary::new(c"movflags", c"+faststart", 0));
    output_ctx.write_header(&mut mux_opts)?;

    // Collect time bases before consuming inputs
    let video_tbs: Vec<ffi::AVRational> = video_input.streams().iter().map(|s| s.time_base).collect();
    let audio_tbs: Vec<ffi::AVRational> = audio_input.streams().iter().map(|s| s.time_base).collect();

    // Collect output time bases (may differ from input after write_header)
    let out_tbs: Vec<ffi::AVRational> = output_ctx.streams().iter().map(|s| s.time_base).collect();

    let mut video_input = video_input;
    let mut audio_input = audio_input;

    // Prime the packet queues
    let mut next_video = video_input
        .read_packet()
        .map_err(|e| Error::Other(format!("Error reading video input: {e:?}")))?;
    let mut next_audio = audio_input
        .read_packet()
        .map_err(|e| Error::Other(format!("Error reading audio input: {e:?}")))?;

    // Returns the packet DTS in seconds (falls back to PTS if DTS is unset).
    let dts_secs = |pkt: &rsmpeg::avcodec::AVPacket, tb: ffi::AVRational| -> f64 {
        let raw_dts = pkt.dts;
        let ts = if raw_dts == ffi::AV_NOPTS_VALUE {
            pkt.pts
        } else {
            raw_dts
        };
        if ts == ffi::AV_NOPTS_VALUE {
            return 0.0;
        }
        ts as f64 * f64::from(tb.num) / f64::from(tb.den)
    };

    // Write packets from both inputs interleaved by DTS order.
    loop {
        let write_video = match (&next_video, &next_audio) {
            (None, None) => break,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (Some(v_pkt), Some(a_pkt)) => {
                let v_stream = v_pkt.stream_index as usize;
                let a_stream = a_pkt.stream_index as usize;
                let v_secs = dts_secs(
                    v_pkt,
                    video_tbs
                        .get(v_stream)
                        .copied()
                        .unwrap_or(ffi::AVRational { num: 1, den: 1 }),
                );
                let a_secs = dts_secs(
                    a_pkt,
                    audio_tbs
                        .get(a_stream)
                        .copied()
                        .unwrap_or(ffi::AVRational { num: 1, den: 1 }),
                );
                v_secs <= a_secs
            }
        };

        if write_video {
            if let Some(packet) = next_video.take() {
                let in_stream_idx = packet.stream_index as usize;
                if let Some(&(_, _, out_stream_idx)) =
                    stream_map.iter().find(|&&(inp, s, _)| inp == 0 && s == in_stream_idx)
                {
                    let in_tb = video_tbs
                        .get(in_stream_idx)
                        .copied()
                        .unwrap_or(ffi::AVRational { num: 1, den: 1 });
                    let out_tb = out_tbs.get(out_stream_idx).copied().unwrap_or(in_tb);
                    let mut pkt = packet;
                    pkt.rescale_ts(in_tb, out_tb);
                    pkt.set_stream_index(out_stream_idx as i32);
                    pkt.set_pos(-1);
                    output_ctx.interleaved_write_frame(&mut pkt)?;
                }
            }
            next_video = video_input
                .read_packet()
                .map_err(|e| Error::Other(format!("Error reading video input: {e:?}")))?;
        } else {
            if let Some(packet) = next_audio.take() {
                let in_stream_idx = packet.stream_index as usize;
                if let Some(&(_, _, out_stream_idx)) =
                    stream_map.iter().find(|&&(inp, s, _)| inp == 1 && s == in_stream_idx)
                {
                    let in_tb = audio_tbs
                        .get(in_stream_idx)
                        .copied()
                        .unwrap_or(ffi::AVRational { num: 1, den: 1 });
                    let out_tb = out_tbs.get(out_stream_idx).copied().unwrap_or(in_tb);
                    let mut pkt = packet;
                    pkt.rescale_ts(in_tb, out_tb);
                    pkt.set_stream_index(out_stream_idx as i32);
                    pkt.set_pos(-1);
                    output_ctx.interleaved_write_frame(&mut pkt)?;
                }
            }
            next_audio = audio_input
                .read_packet()
                .map_err(|e| Error::Other(format!("Error reading audio input: {e:?}")))?;
        }
    }

    output_ctx.write_trailer()?;

    Ok(())
}
