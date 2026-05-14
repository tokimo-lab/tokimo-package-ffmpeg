# tokimo-package-ffmpeg

⚠️ This crate no longer builds the FFmpeg native binaries itself. Native builds (Linux/macOS/Windows) live in [tokimo-lab/tokimo-lib](https://github.com/tokimo-lab/tokimo-lib). This repo only contains the safe Rust FFI bindings consumed via libloading.

High-performance media transcoding library and CLI tool, built on Rust FFI bindings to a patched FFmpeg. Implements a composable 3-stage hardware pipeline — decode, filter, encode — each stage independently configurable with automatic cross-device interop.

[中文文档](README.zh-CN.md)

## Why

| Problem with CLI subprocess | Solution with FFI |
|---|---|
| Process spawn per seek (~30ms fork+exec) | Zero overhead, same process |
| CUDA driver reinit per seek (~200ms) | Persistent GPU context (~60ms warm) |
| No backpressure (SIGSTOP/SIGCONT hack) | Bounded channel frame-level control |
| Parse stderr for errors | Structured AVERROR codes |
| File system polling for progress | Frame-level PTS callbacks |
| Monolithic configuration | Composable 3-stage pipeline API |

**Result:** 29% faster seek latency vs CLI FFmpeg in 10-seek playback simulation.

## Library Usage

```rust
use ffmpeg_tool::{
    transcode, TranscodeOptions,
    build_pipeline, resolve_pipeline, HwType, FilterBackend, FallbackLevel,
    probe_file, MediaInfo,
};

// Probe a file
let info = probe_file(Path::new("input.mkv"))?;
println!("Duration: {}s, streams: {}", info.format.duration, info.format.nb_streams);

// Build a hardware pipeline programmatically
let pipeline = build_pipeline(
    Some(HwType::Cuda),       // decode: NVIDIA NVDEC
    FilterBackend::Native,     // filter: scale_cuda / tonemap_cuda
    Some(HwType::Cuda),       // encode: NVENC
    "hevc_nvenc",             // encoder name
);
assert_eq!(pipeline.fallback, FallbackLevel::FullHw);

// Or resolve from codec name (auto-infers backend)
let pipeline = resolve_pipeline("h264_nvenc", None, None);
// → Cuda → Native → Cuda(h264_nvenc) [full-hw]

// Cross-device pipeline (VAAPI decode → OpenCL filter → QSV encode)
let pipeline = build_pipeline(
    Some(HwType::Vaapi),
    FilterBackend::OpenCL,
    Some(HwType::Qsv),
    "hevc_qsv",
);
assert_eq!(pipeline.fallback, FallbackLevel::CrossDevice);

// Transcode
let opts = TranscodeOptions {
    input: "input.mkv".into(),
    output: "output.mp4".into(),
    video_codec: "hevc_nvenc".into(),
    audio_codec: "aac".into(),
    decode: None,           // auto-infer from video_codec
    filter_backend: None,   // default: Native
    preset: "p1".into(),
    bitrate: Some("8000k".into()),
    duration: Some(30.0),
    seek: Some(3600.0),
    ..Default::default()
};
transcode(&opts)?;
```

## CLI (Testing Tool)

```bash
export LD_LIBRARY_PATH=$PWD/install/lib:$PWD/install/deps

# GPU transcode — backend auto-inferred from codec name
ffmpeg-tool transcode input.mkv output.mp4 --video-codec hevc_nvenc --audio-codec aac

# HDR → SDR tone mapping (Jellyfin-equivalent pipeline)
ffmpeg-tool transcode input.mkv output.mp4 \
  --video-codec h264_nvenc --audio-codec aac --preset p1 --bitrate 8000k \
  --video-filter "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc,tonemap_cuda=format=yuv420p:p=bt709:t=bt709:m=bt709:tonemap=bt2390:peak=100:desat=0"

# Cross-device (needs matching hardware)
ffmpeg-tool transcode input.mkv output.mp4 \
  --video-codec hevc_qsv --decode vaapi --filter-backend opencl

# Software encoding (no GPU needed)
ffmpeg-tool transcode input.mkv output.mp4 --video-codec libx264 --crf 23

# 10-seek benchmark
ffmpeg-tool bench-seek input.mkv --seeks "0,720,1440,2160,2880,3600,4320,5040,5760,6480" \
  --video-codec h264_nvenc --duration 3 --bitrate 8000k
```

## Composable 3-Stage Pipeline

Matching Jellyfin's architecture, each stage is independently configurable:

```
┌──────────┐     ┌──────────┐     ┌──────────┐
│  DECODE  │ ──→ │  FILTER  │ ──→ │  ENCODE  │
│  --decode│     │--filter- │     │--video-  │
│  (cuda)  │     │  backend │     │  codec   │
│          │     │(opencl)  │     │(hevc_qsv)│
└──────────┘     └──────────┘     └──────────┘
```

Cross-device interop via FFmpeg `hwmap`:
- **OpenCL bridge**: `vaapi → hwmap=derive_device=opencl → [filter] → hwmap → vaapi`
- **Vulkan bridge**: `vaapi → hwmap=derive_device=vulkan → [libplacebo] → hwmap → vaapi`

### 5-Level Fallback Chain

```
1. Full HW       decode + native filter + encode on same device (zero-copy)
   ↓ fail
2. Cross-device   decode(VAAPI) → hwmap(OpenCL) → encode(QSV)
   ↓ fail
3. Mixed A        SW decode → hwupload → HW filter + HW encode
   ↓ fail
4. Mixed B        HW decode → hwdownload → SW filter + HW encode
   ↓ fail
5. Software       libx264 / libx265 / libsvtav1
```

## Benchmarks

**Test:** 速度与激情6 — 18GB 4K HEVC HDR (BT.2020/PQ) → H.264 SDR (tonemap_cuda + h264_nvenc)  
**GPU:** NVIDIA RTX 4080 16GB, WSL2

### 10-Seek Playback Simulation (3s per seek, HDR tonemap)

| Seek Position | CLI FFmpeg | Rust FFI | Δ |
|---|---|---|---|
| 0s | 854 ms | 798 ms | −7% |
| 720s (12min) | 852 ms | 559 ms | −34% |
| 1440s (24min) | 991 ms | 683 ms | −31% |
| 2160s (36min) | 912 ms | 589 ms | −35% |
| 2880s (48min) | 996 ms | 698 ms | −30% |
| 3600s (1hr) | 904 ms | 573 ms | −37% |
| 4320s (1hr12) | 958 ms | 632 ms | −34% |
| 5040s (1hr24) | 1249 ms | 991 ms | −21% |
| 5760s (1hr36) | 1203 ms | 893 ms | −26% |
| 6480s (1hr48) | 905 ms | 561 ms | −38% |
| **Average** | **982 ms** | **698 ms** | **−29%** |
| **Total** | **9824 ms** | **6976 ms** | **−29%** |

### Direct Transcode (10s, no tonemap)

| Codec | CLI FFmpeg | Rust FFI | Δ |
|---|---|---|---|
| HEVC NVENC | 1894 ms | 1728 ms | −8.8% |
| H264 NVENC | 1874 ms | 1723 ms | −8.1% |
| AV1 NVENC | 2493 ms | 2379 ms | −4.6% |

See [docs/benchmarks.md](docs/benchmarks.md) for full breakdown.

## Hardware Support

### 7 HW Backends

| Backend | Type | Platform | Decode | Filter | Encode |
|---|---|---|---|---|---|
| **NVIDIA CUDA** | `HwType::Cuda` | Linux/Windows | ✅ NVDEC | ✅ scale/tonemap/overlay/transpose_cuda | ✅ NVENC |
| **AMD AMF** | `HwType::Amf` | Windows | ✅ D3D11VA | ✅ OpenCL bridge | ✅ AMF |
| **Intel QSV** | `HwType::Qsv` | Linux/Windows | ✅ QSV | ✅ VPP + OpenCL bridge | ✅ QSV |
| **VAAPI** | `HwType::Vaapi` | Linux | ✅ VAAPI | ✅ native + OpenCL/Vulkan bridge | ✅ VAAPI |
| **VideoToolbox** | `HwType::Videotoolbox` | macOS | ✅ VT | ✅ scale/tonemap/overlay/transpose_vt | ✅ VT |
| **RKMPP** | `HwType::Rkmpp` | Linux ARM | ✅ RKMPP | ✅ RKRGA + OpenCL bridge | ✅ RKMPP |
| **V4L2M2M** | `HwType::V4l2m2m` | Linux | ✅ V4L2 | SW only | ✅ V4L2 |

### Encoder Matrix

| Codec | NVIDIA | AMD AMF | Intel QSV | VAAPI | VideoToolbox | RKMPP | V4L2M2M |
|---|---|---|---|---|---|---|---|
| H.264 | h264_nvenc | h264_amf | h264_qsv | h264_vaapi | h264_videotoolbox | h264_rkmpp | h264_v4l2m2m |
| HEVC | hevc_nvenc | hevc_amf | hevc_qsv | hevc_vaapi | hevc_videotoolbox | hevc_rkmpp | — |
| AV1 | av1_nvenc | av1_amf | av1_qsv | av1_vaapi | — | — | — |

### Filter Matrix

| Filter | CUDA | OpenCL | Vulkan | VAAPI | QSV VPP | VideoToolbox | RKRGA |
|---|---|---|---|---|---|---|---|
| **Scale** | scale_cuda | scale_opencl | scale_vulkan | scale_vaapi | scale_qsv | scale_vt | scale_rkrga |
| **Tonemap** | tonemap_cuda | tonemap_opencl | libplacebo | tonemap_vaapi | vpp_qsv | tonemap_videotoolbox | — |
| **Deinterlace** | yadif_cuda | yadif_opencl | — | deinterlace_vaapi | deinterlace_qsv | yadif_videotoolbox | — |
| **Overlay** | overlay_cuda | overlay_opencl | overlay_vulkan | overlay_vaapi | overlay_qsv | overlay_videotoolbox | overlay_rkrga |
| **Transpose** | transpose_cuda | transpose_opencl | transpose_vulkan | transpose_vaapi | — | transpose_vt | — |

### Cross-Device Interop (5 types)

| Interop | Direction | Use Case |
|---|---|---|
| OpenCL ↔ D3D11VA | Bidirectional | AMD AMF (Win), Intel QSV (Win) |
| OpenCL ↔ VAAPI | Bidirectional | Intel iHD (Linux), AMD (Linux) |
| Vulkan ↔ VAAPI | Bidirectional | AMD radeonsi + libplacebo |
| OpenCL ↔ QSV | Bidirectional | Intel cross-device tonemap |
| OpenCL ↔ RKMPP | Bidirectional | Rockchip ARM tonemap |

## Architecture

```
src/
├── lib.rs                   # Library entry: re-exports transcode, probe, hw types
├── main.rs                  # CLI testing tool (clap)
├── probe.rs        (615L)   # Media probing (JSON output)
└── transcode/
    ├── mod.rs      (989L)   # Orchestrator: open → pipeline → mux
    ├── hw.rs       (499L)   # 3-stage pipeline: HwType, FilterBackend, HwPipeline
    ├── filter.rs   (363L)   # Filter graphs: unified + cross-device hwmap
    ├── encode.rs   (167L)   # Encoder setup, options, flush
    └── pipeline.rs (276L)   # Threaded pipeline (decode / filter+encode / audio)
```

### Pipeline Threads

```
Main thread:    demux ──────────────────────────── mux
                  │                                  ▲
                  ▼                                  │
Decode:         HW decode → send frame               │
                               │                     │
                               ▼                     │
Filter+Enc:    [hwmap] → filter → [hwmap] → encode ──┘
Audio:          decode → aformat → aac encode ───────┘
```

## Build

```bash
# Docker build (recommended — handles all 80+ dependencies)
make docker && make docker-deps && make rust-build

# Set runtime library path
export LD_LIBRARY_PATH=$PWD/install/lib:$PWD/install/deps
```

See [docs/build-guide.md](docs/build-guide.md) for full details.

## Documentation

| Document | Description |
|---|---|
| [docs/architecture.md](docs/architecture.md) | Module structure, pipeline design, memory model |
| [docs/benchmarks.md](docs/benchmarks.md) | 10-seek benchmark, codec comparison, probe perf |
| [docs/build-guide.md](docs/build-guide.md) | Prerequisites, Docker/local build, troubleshooting |
| [docs/cli-usage.md](docs/cli-usage.md) | All subcommands, options, examples |
| [docs/test-scripts.md](docs/test-scripts.md) | Validation tests, benchmark scripts, CLI comparison |
| [docs/jellyfin-hw-pipelines.md](docs/jellyfin-hw-pipelines.md) | Hardware pipeline analysis: all 25+ pipeline paths |
| [docs/tokimo-hw-support.md](docs/tokimo-hw-support.md) | Hardware backend coverage tables |

## License

Uses GPL-licensed FFmpeg with non-free codecs (fdk-aac). The compiled FFmpeg libraries are GPL v3.
