//! Single-frame image decode, scale, and encode via FFI.
//!
//! Replaces subprocess calls like:
//!   ffmpeg -i input.heic -vframes 1 -vf scale=W:-1 -f image2pipe -vcodec mjpeg pipe:1
//!
//! Handles HEIC tile grid assembly (stream groups) for `FFmpeg` 7.0+.

use crate::error::{Error, Result, ResultExt};
use rsmpeg::{
    avcodec::{AVCodec, AVCodecContext},
    avfilter::{AVFilter, AVFilterGraph, AVFilterInOut},
    avformat::AVFormatContextInput,
    avutil::AVFrame,
    error::RsmpegError,
    ffi,
};
use std::ffi::CString;
use std::path::Path;

/// Output image format.
#[derive(Debug, Clone, Copy)]
pub enum ImageFormat {
    Jpeg,
    WebP,
    Png,
}

impl ImageFormat {
    fn encoder_name(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "mjpeg",
            ImageFormat::WebP => "libwebp",
            ImageFormat::Png => "png",
        }
    }

    pub fn mime_type(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::WebP => "image/webp",
            ImageFormat::Png => "image/png",
        }
    }

    pub(crate) fn pix_fmt(self) -> ffi::AVPixelFormat {
        match self {
            ImageFormat::Jpeg => ffi::AV_PIX_FMT_YUVJ420P,
            ImageFormat::WebP => ffi::AV_PIX_FMT_YUV420P,
            ImageFormat::Png => ffi::AV_PIX_FMT_RGB24,
        }
    }
}

/// Options for image decoding.
pub struct ImageDecodeOptions {
    /// Target width (height auto-calculated to preserve aspect ratio).
    /// None = keep original size.
    pub width: Option<u32>,
    /// Output format.
    pub format: ImageFormat,
    /// Encode quality (1-31 for JPEG where 2 is high quality, 0-100 for WebP).
    pub quality: u8,
}

impl Default for ImageDecodeOptions {
    fn default() -> Self {
        Self {
            width: None,
            format: ImageFormat::Jpeg,
            quality: 2,
        }
    }
}

/// Decode an image file to raw encoded bytes (JPEG/WebP/PNG).
///
/// Handles:
/// - Standard images (AVIF, PNG, JPEG, TIFF, BMP, etc.)
/// - HEIC/HEIF with tile grid stream groups (auto-assembly)
/// - RAW camera formats (via `FFmpeg`'s rawvideo/image2 decoders)
pub fn decode_image(input_path: &Path, opts: &ImageDecodeOptions) -> Result<Vec<u8>> {
    let path_str = input_path
        .to_str()
        .ok_or_else(|| Error::Other("Input path contains invalid UTF-8".into()))?;
    let c_path = CString::new(path_str)?;

    let lower = path_str.to_lowercase();
    let is_heic = std::path::Path::new(&lower)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("heic"))
        || std::path::Path::new(&lower)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("heif"));

    let input_ctx = AVFormatContextInput::open(&c_path)?;

    // For HEIC with tile grid: decode all tiles and composite
    if is_heic
        && let Some(tg) = read_tile_grid_info(&input_ctx)
        && tg.nb_tiles > 1
    {
        let frame = decode_tile_grid(input_ctx, &tg)?;
        let target_pix_fmt = opts.format.pix_fmt();
        let filtered = apply_scale_filter(&frame, target_pix_fmt, opts.width, None)?;
        return encode_image_frame(&filtered, opts);
    }

    // Standard single-stream path
    let (stream_idx, mut dec_ctx) = {
        let (idx, codec_id) = find_best_video_stream(&input_ctx, is_heic)?;
        let decoder = AVCodec::find_decoder(codec_id)
            .ok_or_else(|| Error::Other(format!("No decoder for codec {codec_id:?}")))?;
        let stream = &input_ctx.streams()[idx];
        let codecpar = stream.codecpar();

        let mut dec = AVCodecContext::new(&decoder);
        dec.apply_codecpar(&codecpar)?;
        dec.set_pkt_timebase(stream.time_base);
        dec.open(None)?;
        (idx, dec)
    };

    let frame = decode_first_frame(&mut dec_ctx, input_ctx, stream_idx)?;

    let target_pix_fmt = opts.format.pix_fmt();
    let filtered = apply_scale_filter(&frame, target_pix_fmt, opts.width, None)?;

    encode_image_frame(&filtered, opts)
}

/// Decode image from in-memory bytes.
pub fn decode_image_from_bytes(data: &[u8], filename_hint: &str, opts: &ImageDecodeOptions) -> Result<Vec<u8>> {
    // Write to temp file, decode, then clean up.
    // Using temp file because FFmpeg's custom AVIO for in-memory read
    // requires complex setup; temp file is simpler and sufficient here.
    let ext = filename_hint.rsplit('.').next().unwrap_or("bin");
    let tmp_path = std::env::temp_dir().join(format!("tokimo_img_{}.{}", uuid_v4_simple(), ext));

    std::fs::write(&tmp_path, data)
        .map_err(|_e| Error::Other(format!("Failed to write temp file: {}", tmp_path.display())))?;

    let result = decode_image(&tmp_path, opts);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

// ── Internal helpers ────────────────────────────────────────────────────────

fn find_best_video_stream(input_ctx: &AVFormatContextInput, is_heic: bool) -> Result<(usize, ffi::AVCodecID)> {
    if is_heic {
        // For HEIC: try to find the stream associated with a tile grid group.
        unsafe {
            let ctx = input_ctx.as_ptr();
            let nb_groups = (*ctx).nb_stream_groups;

            if nb_groups > 0 {
                for g in 0..nb_groups as usize {
                    let group = *(*ctx).stream_groups.add(g);
                    if (*group).type_ == ffi::AV_STREAM_GROUP_PARAMS_TILE_GRID && (*group).nb_streams > 0 {
                        let stream = *(*group).streams.add(0);
                        let idx = (*stream).index as usize;
                        let cp = (*stream).codecpar;
                        return Ok((idx, (*cp).codec_id));
                    }
                }
            }
        }
    }

    // Standard path: find best video stream
    for (i, stream) in input_ctx.streams().iter().enumerate() {
        let codecpar = stream.codecpar();
        if codecpar.codec_type == ffi::AVMEDIA_TYPE_VIDEO {
            return Ok((i, codecpar.codec_id));
        }
    }
    Err(Error::Other("No video stream found in image file".into()))
}

/// Tile grid metadata extracted from `AVStreamGroupTileGrid`.
struct TileGridInfo {
    nb_tiles: usize,
    coded_width: i32,
    coded_height: i32,
    width: i32,
    height: i32,
    horizontal_offset: i32,
    vertical_offset: i32,
    /// (`stream_group_local_idx`, `x_offset`, `y_offset`) per tile
    tiles: Vec<TileOffset>,
    /// Global stream indices for each group-local stream
    stream_indices: Vec<usize>,
    /// Codec ID (all tiles share the same codec)
    codec_id: ffi::AVCodecID,
}

struct TileOffset {
    /// Index into the group's stream list (group-local)
    idx: usize,
    horizontal: i32,
    vertical: i32,
}

/// Read tile grid info from the first `TILE_GRID` stream group.
fn read_tile_grid_info(input_ctx: &AVFormatContextInput) -> Option<TileGridInfo> {
    unsafe {
        let ctx = input_ctx.as_ptr();
        let nb_groups = (*ctx).nb_stream_groups;
        if nb_groups == 0 {
            return None;
        }

        for g in 0..nb_groups as usize {
            let group = *(*ctx).stream_groups.add(g);
            if (*group).type_ != ffi::AV_STREAM_GROUP_PARAMS_TILE_GRID {
                continue;
            }
            let tg = (*group).params.tile_grid;
            if tg.is_null() {
                continue;
            }
            let nb_tiles = (*tg).nb_tiles as usize;
            if nb_tiles <= 1 {
                continue;
            }

            let nb_streams = (*group).nb_streams as usize;
            let mut stream_indices = Vec::with_capacity(nb_streams);
            for s in 0..nb_streams {
                let st = *(*group).streams.add(s);
                stream_indices.push((*st).index as usize);
            }

            let mut tiles = Vec::with_capacity(nb_tiles);
            for i in 0..nb_tiles {
                let off = &*(*tg).offsets.add(i);
                tiles.push(TileOffset {
                    idx: off.idx as usize,
                    horizontal: off.horizontal,
                    vertical: off.vertical,
                });
            }

            // Get codec from first stream
            let first_st = *(*group).streams.add(0);
            let codec_id = (*(*first_st).codecpar).codec_id;

            return Some(TileGridInfo {
                nb_tiles,
                coded_width: (*tg).coded_width,
                coded_height: (*tg).coded_height,
                width: (*tg).width,
                height: (*tg).height,
                horizontal_offset: (*tg).horizontal_offset,
                vertical_offset: (*tg).vertical_offset,
                tiles,
                stream_indices,
                codec_id,
            });
        }
        None
    }
}

/// Decode all tiles and composite into a single frame.
fn decode_tile_grid(mut input_ctx: AVFormatContextInput, tg: &TileGridInfo) -> Result<AVFrame> {
    // Build a mapping: global_stream_index → group_local_index
    let mut stream_to_local: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (local_idx, &global_idx) in tg.stream_indices.iter().enumerate() {
        stream_to_local.insert(global_idx, local_idx);
    }

    // Set up one decoder per unique stream (tiles may reference the same stream)
    let decoder = AVCodec::find_decoder(tg.codec_id).ok_or_else(|| Error::Other("No decoder for tile codec".into()))?;

    let mut decoders: Vec<AVCodecContext> = Vec::with_capacity(tg.stream_indices.len());
    for &global_idx in &tg.stream_indices {
        let stream = &input_ctx.streams()[global_idx];
        let codecpar = stream.codecpar();
        let mut dec = AVCodecContext::new(&decoder);
        dec.apply_codecpar(&codecpar)?;
        dec.set_pkt_timebase(stream.time_base);
        dec.open(None)?;
        decoders.push(dec);
    }

    // Decode all tile frames
    let mut decoded_frames: Vec<Option<AVFrame>> = vec![None; tg.stream_indices.len()];
    let mut decoded_count = 0;
    let total = tg.stream_indices.len();

    loop {
        if decoded_count >= total {
            break;
        }
        let packet = match input_ctx.read_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                // EOF: flush remaining decoders
                for (local_idx, dec) in decoders.iter_mut().enumerate() {
                    if decoded_frames[local_idx].is_some() {
                        continue;
                    }
                    dec.send_packet(None).ok();
                    if let Ok(frame) = dec.receive_frame() {
                        decoded_frames[local_idx] = Some(frame);
                    }
                }
                break;
            }
            Err(e) => return Err(Error::Other(format!("Error reading packet: {e:?}"))),
        };

        let stream_idx = packet.stream_index as usize;
        let Some(&local_idx) = stream_to_local.get(&stream_idx) else {
            continue;
        };

        if decoded_frames[local_idx].is_some() {
            continue;
        }

        decoders[local_idx].send_packet(Some(&packet))?;

        match decoders[local_idx].receive_frame() {
            Ok(frame) => {
                decoded_frames[local_idx] = Some(frame);
                decoded_count += 1;
            }
            Err(RsmpegError::DecoderDrainError) => {}
            Err(e) => return Err(Error::Other(format!("Tile decode error: {e:?}"))),
        }
    }

    // Verify we got all tiles
    for (i, f) in decoded_frames.iter().enumerate() {
        if f.is_none() {
            return Err(Error::Other(format!("Failed to decode tile stream {i}")));
        }
    }

    // All tiles are decoded. Now composite onto canvas.
    // First, determine the pixel format from the first tile.
    let first_tile = decoded_frames[0].as_ref().context("first tile frame is missing")?;
    let pix_fmt = first_tile.format;

    // Create canvas frame
    let mut canvas = create_canvas(tg.coded_width, tg.coded_height, pix_fmt)?;

    // Blit each tile onto canvas
    blit_tiles(&mut canvas, &decoded_frames, tg, pix_fmt)?;

    // Crop to presentation dimensions if needed
    if tg.width != tg.coded_width
        || tg.height != tg.coded_height
        || tg.horizontal_offset != 0
        || tg.vertical_offset != 0
    {
        crop_frame(
            &canvas,
            tg.horizontal_offset,
            tg.vertical_offset,
            tg.width,
            tg.height,
            pix_fmt,
        )
    } else {
        Ok(canvas)
    }
}

/// Create a blank canvas `AVFrame`.
fn create_canvas(width: i32, height: i32, pix_fmt: i32) -> Result<AVFrame> {
    let mut frame = AVFrame::new();
    frame.set_width(width);
    frame.set_height(height);
    frame.set_format(pix_fmt);
    frame.get_buffer(0)?;
    frame.make_writable()?;

    // Zero-fill: Y=0 (black), UV=128 (neutral chroma)
    unsafe {
        let f = frame.as_mut_ptr();
        for p in 0..ffi::AV_NUM_DATA_POINTERS as usize {
            if (*f).data[p].is_null() || (*f).linesize[p] == 0 {
                break;
            }
            let plane_h = if p == 0 { height as usize } else { (height as usize) / 2 };
            let size = (*f).linesize[p] as usize * plane_h;
            let fill_val: u8 = if p == 0 { 0 } else { 128 };
            std::ptr::write_bytes((*f).data[p], fill_val, size);
        }
    }
    Ok(frame)
}

/// Blit decoded tile frames onto the canvas.
fn blit_tiles(canvas: &mut AVFrame, tiles: &[Option<AVFrame>], tg: &TileGridInfo, pix_fmt: i32) -> Result<()> {
    canvas.make_writable()?;

    let is_yuv420 = pix_fmt == ffi::AV_PIX_FMT_YUV420P || pix_fmt == ffi::AV_PIX_FMT_YUVJ420P;

    unsafe {
        let canvas_ptr = canvas.as_mut_ptr();

        for tile_info in &tg.tiles {
            let tile_frame = tiles[tile_info.idx]
                .as_ref()
                .ok_or_else(|| Error::Other("Missing tile frame".into()))?;

            let tile_w = tile_frame.width as usize;
            let tile_h = tile_frame.height as usize;
            let off_x = tile_info.horizontal as usize;
            let off_y = tile_info.vertical as usize;

            // Blit Y plane (plane 0)
            blit_plane(canvas_ptr, tile_frame.as_ptr(), 0, off_x, off_y, tile_w, tile_h);

            if is_yuv420 {
                // Blit U and V planes (half resolution)
                blit_plane(
                    canvas_ptr,
                    tile_frame.as_ptr(),
                    1,
                    off_x / 2,
                    off_y / 2,
                    tile_w / 2,
                    tile_h / 2,
                );
                blit_plane(
                    canvas_ptr,
                    tile_frame.as_ptr(),
                    2,
                    off_x / 2,
                    off_y / 2,
                    tile_w / 2,
                    tile_h / 2,
                );
            }
        }
    }
    Ok(())
}

/// Copy one plane of a tile onto the canvas at (`off_x`, `off_y`).
unsafe fn blit_plane(
    canvas: *mut ffi::AVFrame,
    tile: *const ffi::AVFrame,
    plane: usize,
    off_x: usize,
    off_y: usize,
    tile_w: usize,
    tile_h: usize,
) {
    unsafe {
        let canvas_linesize = (*canvas).linesize[plane] as usize;
        let tile_linesize = (*tile).linesize[plane] as usize;

        let dst_base = (*canvas).data[plane];
        let src_base = (*tile).data[plane];

        for row in 0..tile_h {
            let dst = dst_base.add((off_y + row) * canvas_linesize + off_x);
            let src = src_base.add(row * tile_linesize);
            std::ptr::copy_nonoverlapping(src, dst, tile_w);
        }
    }
}

/// Crop a frame using a filter graph.
fn crop_frame(frame: &AVFrame, x: i32, y: i32, w: i32, h: i32, _pix_fmt: i32) -> Result<AVFrame> {
    let filter_graph = AVFilterGraph::new();

    let buffersrc = AVFilter::get_by_name(c"buffer").ok_or_else(|| Error::Other("buffer filter not found".into()))?;
    let buffersink =
        AVFilter::get_by_name(c"buffersink").ok_or_else(|| Error::Other("buffersink filter not found".into()))?;

    let src_args = format!(
        "video_size={}x{}:pix_fmt={}:time_base=1/1:pixel_aspect=1/1",
        frame.width, frame.height, frame.format
    );
    let src_args_c = CString::new(src_args)?;

    let mut buffersrc_ctx = filter_graph.create_filter_context(&buffersrc, c"in", Some(&src_args_c))?;

    let mut buffersink_ctx = filter_graph
        .alloc_filter_context(&buffersink, c"out")
        .ok_or_else(|| Error::Other("Cannot create buffer sink".into()))?;
    buffersink_ctx.init_dict(&mut None)?;

    let crop_spec = CString::new(format!("crop={w}:{h}:{x}:{y}"))?;

    let outputs = AVFilterInOut::new(c"in", &mut buffersrc_ctx, 0);
    let inputs = AVFilterInOut::new(c"out", &mut buffersink_ctx, 0);
    let (_inputs, _outputs) = filter_graph.parse_ptr(&crop_spec, Some(inputs), Some(outputs))?;
    filter_graph.config()?;

    buffersrc_ctx.buffersrc_add_frame(Some(frame.clone()), None)?;
    buffersrc_ctx.buffersrc_add_frame(None::<AVFrame>, None).ok();

    let cropped = buffersink_ctx.buffersink_get_frame(None)?;

    Ok(cropped)
}

fn decode_first_frame(
    dec_ctx: &mut AVCodecContext,
    mut input_ctx: AVFormatContextInput,
    stream_idx: usize,
) -> Result<AVFrame> {
    // Read packets until we decode a frame
    loop {
        let packet = match input_ctx.read_packet() {
            Ok(Some(pkt)) => pkt,
            Ok(None) => {
                // EOF — flush decoder
                dec_ctx.send_packet(None).ok();
                return dec_ctx
                    .receive_frame()
                    .map_err(|e| Error::Other(format!("No frame decoded: {e:?}")));
            }
            Err(e) => return Err(Error::Other(format!("Error reading packet: {e:?}"))),
        };

        if packet.stream_index as usize != stream_idx {
            continue;
        }

        dec_ctx.send_packet(Some(&packet))?;

        match dec_ctx.receive_frame() {
            Ok(frame) => return Ok(frame),
            Err(RsmpegError::DecoderDrainError) => {}
            Err(e) => return Err(Error::Other(format!("Decode error: {e:?}"))),
        }
    }
}

pub(crate) fn apply_scale_filter(
    frame: &AVFrame,
    target_pix_fmt: ffi::AVPixelFormat,
    target_width: Option<u32>,
    target_height: Option<u32>,
) -> Result<AVFrame> {
    let filter_graph = AVFilterGraph::new();

    let buffersrc = AVFilter::get_by_name(c"buffer").ok_or_else(|| Error::Other("buffer filter not found".into()))?;
    let buffersink =
        AVFilter::get_by_name(c"buffersink").ok_or_else(|| Error::Other("buffersink filter not found".into()))?;

    let src_args = format!(
        "video_size={}x{}:pix_fmt={}:time_base=1/1:pixel_aspect=1/1",
        frame.width, frame.height, frame.format
    );
    let src_args_c = CString::new(src_args)?;

    let mut buffersrc_ctx = filter_graph.create_filter_context(&buffersrc, c"in", Some(&src_args_c))?;

    let mut buffersink_ctx = filter_graph
        .alloc_filter_context(&buffersink, c"out")
        .ok_or_else(|| Error::Other("Cannot create buffer sink".into()))?;
    buffersink_ctx.opt_set_bin(c"pix_fmts", &target_pix_fmt)?;
    buffersink_ctx.init_dict(&mut None)?;

    let filter_spec = match (target_width, target_height) {
        (Some(w), Some(h)) => CString::new(format!("scale={w}:{h}"))?,
        (Some(w), None) => CString::new(format!("scale={w}:-1"))?,
        (None, Some(h)) => CString::new(format!("scale=-1:{h}"))?,
        (None, None) => CString::new("null")?,
    };

    let outputs = AVFilterInOut::new(c"in", &mut buffersrc_ctx, 0);
    let inputs = AVFilterInOut::new(c"out", &mut buffersink_ctx, 0);
    let (_inputs, _outputs) = filter_graph.parse_ptr(&filter_spec, Some(inputs), Some(outputs))?;
    filter_graph.config()?;

    buffersrc_ctx.buffersrc_add_frame(Some(frame.clone()), None)?;
    buffersrc_ctx.buffersrc_add_frame(None::<AVFrame>, None).ok();

    let filtered = buffersink_ctx.buffersink_get_frame(None)?;

    Ok(filtered)
}

pub(crate) fn encode_image_frame(frame: &AVFrame, opts: &ImageDecodeOptions) -> Result<Vec<u8>> {
    let encoder_name = opts.format.encoder_name();
    let c_name = CString::new(encoder_name)?;
    let encoder =
        AVCodec::find_encoder_by_name(&c_name).ok_or_else(|| Error::Other("Image encoder not found".into()))?;

    let mut enc_ctx = AVCodecContext::new(&encoder);
    enc_ctx.set_width(frame.width);
    enc_ctx.set_height(frame.height);
    enc_ctx.set_pix_fmt(frame.format);
    enc_ctx.set_time_base(ffi::AVRational { num: 1, den: 1 });

    // Set quality
    match opts.format {
        ImageFormat::Jpeg => {
            // mjpeg quality: qmin/qmax (lower = better, 2 = high quality)
            unsafe {
                (*enc_ctx.as_mut_ptr()).qmin = i32::from(opts.quality);
                (*enc_ctx.as_mut_ptr()).qmax = i32::from(opts.quality);
            }
        }
        ImageFormat::WebP => {
            // libwebp uses "quality" option
            let q_str = CString::new(opts.quality.to_string())?;
            let enc_opts = Some(rsmpeg::avutil::AVDictionary::new(c"quality", &q_str, 0));
            enc_ctx.open(enc_opts)?;
            // Skip the open below
            return encode_and_collect(enc_ctx, frame);
        }
        ImageFormat::Png => {}
    }

    enc_ctx.open(None)?;

    encode_and_collect(enc_ctx, frame)
}

fn encode_and_collect(mut enc_ctx: AVCodecContext, frame: &AVFrame) -> Result<Vec<u8>> {
    let mut output = Vec::new();

    enc_ctx.send_frame(Some(frame))?;

    loop {
        match enc_ctx.receive_packet() {
            Ok(pkt) => {
                let data = unsafe { std::slice::from_raw_parts(pkt.data, pkt.size as usize) };
                output.extend_from_slice(data);
            }
            Err(RsmpegError::EncoderDrainError | RsmpegError::EncoderFlushedError) => {
                break;
            }
            Err(e) => return Err(Error::Other(format!("Encode error: {e:?}"))),
        }
    }

    // Flush encoder
    enc_ctx.send_frame(None).ok();
    while let Ok(pkt) = enc_ctx.receive_packet() {
        let data = unsafe { std::slice::from_raw_parts(pkt.data, pkt.size as usize) };
        output.extend_from_slice(data);
    }

    if output.is_empty() {
        return Err(Error::Other("Encoder produced no output".into()));
    }

    Ok(output)
}

fn uuid_v4_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{t:x}")
}
