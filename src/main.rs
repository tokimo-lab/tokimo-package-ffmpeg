#![allow(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_in_result)]
use tokimo_package_ffmpeg::{media::probe, transcode};

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "ffmpeg-tool", version, about = "Media probing and transcoding tool")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Probe a media file or URL and display stream information (like ffprobe)
    Probe {
        /// File path or URL (http://, https://, rtmp://, etc.)
        file: String,

        /// Output as JSON (like ffprobe -`print_format` json)
        #[arg(short, long)]
        json: bool,
    },

    /// Transcode a media file with optional hardware acceleration
    Transcode {
        /// Input file path
        input: PathBuf,

        /// Output file path
        output: PathBuf,

        /// Video codec (`h264_nvenc`, `hevc_nvenc`, `av1_nvenc`, `h264_vaapi`, `hevc_vaapi`, `h264_qsv`, `h264_amf`, libx264, libx265, libsvtav1, copy)
        #[arg(long, default_value = "libx264")]
        video_codec: String,

        /// Audio codec (`libfdk_aac`, aac, libopus, copy)
        #[arg(long, default_value = "libfdk_aac")]
        audio_codec: String,

        /// Decode backend (cuda, vaapi, qsv, amf, videotoolbox, rkmpp); auto-inferred from video-codec if omitted
        #[arg(long)]
        decode: Option<String>,

        /// Filter backend (native, opencl, vulkan, software); defaults to native (same device as decode)
        #[arg(long)]
        filter_backend: Option<String>,

        /// Encoder preset (fast, medium, slow)
        #[arg(long, default_value = "medium")]
        preset: String,

        /// Constant rate factor (quality-based encoding)
        #[arg(long)]
        crf: Option<u32>,

        /// Target video bitrate (e.g., 5000k, 10M)
        #[arg(long)]
        bitrate: Option<String>,

        /// Output resolution (e.g., 1920x1080, 1280x720)
        #[arg(long)]
        resolution: Option<String>,

        /// Limit transcode duration in seconds (e.g., 30)
        #[arg(long)]
        duration: Option<f64>,

        /// Show progress during transcoding
        #[arg(long)]
        progress: bool,

        /// Seek position in seconds (equivalent to -ss)
        #[arg(long)]
        seek: Option<f64>,

        /// Custom video filter chain for GPU pipeline (e.g. "`setparams=...,tonemap_cuda`=...")
        #[arg(long)]
        video_filter: Option<String>,

        /// Video encoder profile (e.g. "high", "main")
        #[arg(long)]
        video_profile: Option<String>,

        /// Maximum bitrate (e.g. "8000k")
        #[arg(long)]
        maxrate: Option<String>,

        /// VBV buffer size (e.g. "16000k")
        #[arg(long)]
        bufsize: Option<String>,

        /// GOP size / keyframe interval
        #[arg(long)]
        gop: Option<i32>,

        /// Minimum keyframe interval
        #[arg(long)]
        keyint_min: Option<i32>,

        /// Audio bitrate (e.g. "640k")
        #[arg(long)]
        audio_bitrate: Option<String>,

        /// Number of audio channels
        #[arg(long)]
        audio_channels: Option<i32>,
    },

    /// Benchmark multi-seek scenario (simulates persistent process with CUDA reuse)
    BenchSeek {
        /// Input file path
        input: PathBuf,

        /// Comma-separated seek positions in seconds (e.g. "0,1800,3600,5400,7200")
        #[arg(long)]
        seeks: String,

        /// All other transcode options follow
        #[arg(long, default_value = "h264_nvenc")]
        video_codec: String,
        #[arg(long, default_value = "libfdk_aac")]
        audio_codec: String,
        #[arg(long)]
        decode: Option<String>,
        #[arg(long)]
        filter_backend: Option<String>,
        #[arg(long, default_value = "medium")]
        preset: String,
        #[arg(long)]
        bitrate: Option<String>,
        #[arg(long, default_value = "6")]
        duration: String,
        #[arg(long)]
        video_filter: Option<String>,
        #[arg(long)]
        video_profile: Option<String>,
        #[arg(long)]
        maxrate: Option<String>,
        #[arg(long)]
        bufsize: Option<String>,
        #[arg(long)]
        gop: Option<i32>,
        #[arg(long)]
        keyint_min: Option<i32>,
        #[arg(long)]
        audio_bitrate: Option<String>,
        #[arg(long)]
        audio_channels: Option<i32>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Probe { file, json } => {
            let info = probe::probe_file(&file)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                probe::print_info(&info);
            }
        }
        Commands::Transcode {
            input,
            output,
            video_codec,
            audio_codec,
            decode,
            filter_backend,
            preset,
            crf,
            bitrate,
            resolution,
            duration,
            progress,
            seek,
            video_filter,
            video_profile,
            maxrate,
            bufsize,
            gop,
            keyint_min,
            audio_bitrate,
            audio_channels,
        } => {
            let opts = transcode::TranscodeOptions {
                input,
                output,
                video_codec,
                audio_codec,
                audio_only: false,
                audio_stream_index: None,
                decode,
                filter_backend,
                preset,
                crf,
                bitrate,
                resolution,
                duration,
                progress,
                seek,
                video_filter,
                video_profile,
                maxrate,
                bufsize,
                gop,
                keyint_min,
                audio_bitrate,
                audio_channels,
                cancel: None,
                pause: None,
                hls: None,
                force_key_frames_interval: None,
                audio_sample_rate: None,
                accurate_seek: false,
                cached_device_ctx: None,
                direct_input: None,
            };
            transcode::transcode(&opts)?;
        }
        Commands::BenchSeek {
            input,
            seeks,
            video_codec,
            audio_codec,
            decode,
            filter_backend,
            preset,
            bitrate,
            duration,
            video_filter,
            video_profile,
            maxrate,
            bufsize,
            gop,
            keyint_min,
            audio_bitrate,
            audio_channels,
        } => {
            use std::time::Instant;

            let seek_positions: Vec<f64> = seeks
                .split(',')
                .map(|s| s.trim().parse::<f64>().expect("Invalid seek position"))
                .collect();
            let dur: f64 = duration.parse().expect("Invalid duration");

            eprintln!("=== Bench-Seek: {} positions, {}s each ===", seek_positions.len(), dur);

            let total_start = Instant::now();
            let mut times = Vec::new();

            for (i, &seek_secs) in seek_positions.iter().enumerate() {
                let output = PathBuf::from(format!("/tmp/bench_seek_{i}.mp4"));
                let opts = transcode::TranscodeOptions {
                    input: input.clone(),
                    output,
                    video_codec: video_codec.clone(),
                    audio_codec: audio_codec.clone(),
                    audio_only: false,
                    audio_stream_index: None,
                    decode: decode.clone(),
                    filter_backend: filter_backend.clone(),
                    preset: preset.clone(),
                    crf: None,
                    bitrate: bitrate.clone(),
                    resolution: None,
                    duration: Some(dur),
                    progress: false,
                    seek: Some(seek_secs),
                    video_filter: video_filter.clone(),
                    video_profile: video_profile.clone(),
                    maxrate: maxrate.clone(),
                    bufsize: bufsize.clone(),
                    gop,
                    keyint_min,
                    audio_bitrate: audio_bitrate.clone(),
                    audio_channels,
                    cancel: None,
                    pause: None,
                    hls: None,
                    force_key_frames_interval: None,
                    audio_sample_rate: None,
                    accurate_seek: false,
                    cached_device_ctx: None,
                    direct_input: None,
                };
                let t = Instant::now();
                transcode::transcode(&opts)?;
                let elapsed = t.elapsed().as_secs_f64() * 1000.0;
                times.push((seek_secs, elapsed));
            }

            let total = total_start.elapsed().as_secs_f64() * 1000.0;
            eprintln!("\n=== Bench-Seek Results ===");
            for (seek, ms) in &times {
                eprintln!("  seek={seek:>5.0}s → {ms:>7.0}ms");
            }
            let avg: f64 = times.iter().map(|(_, ms)| ms).sum::<f64>() / times.len() as f64;
            eprintln!("  average:    {avg:>7.0}ms");
            eprintln!("  total:      {:>7.0}ms ({} seeks)", total, times.len());

            // Cleanup temp files
            for i in 0..seek_positions.len() {
                let _ = std::fs::remove_file(format!("/tmp/bench_seek_{i}.mp4"));
            }
        }
    }

    Ok(())
}
