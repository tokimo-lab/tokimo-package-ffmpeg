# Build Guide

## Prerequisites

- **OS:** Ubuntu 24.04+ (or any Debian-based Linux)
- **GPU:** NVIDIA with CUDA 12.x drivers (for hardware acceleration)
- **Rust:** 1.81.0+ (`rustup install stable`)
- **Docker:** 20.10+ with BuildKit (for Docker build method)

## Quick Start (Docker — Recommended)

The Docker build is fully reproducible and handles all dependencies automatically.

```bash
# Clone the repo
git clone https://github.com/tokimo-lab/tokimo-ffmpeg.git
cd tokimo-ffmpeg

# Build FFmpeg inside Docker (with BuildKit caching)
make docker

# Extract runtime dependencies
make docker-deps

# Build the Rust tool
make rust-build

# Test it
make probe FILE="your-video.mkv"
```

**Build times:**
- First Docker build: ~15-25 minutes (compiling FFmpeg from source)
- Subsequent builds: ~1-3 minutes (ccache + BuildKit layer caching)
- Rust build: ~10-20 seconds

## Local Build (Ubuntu 24.04)

If you prefer building natively without Docker:

```bash
# 1. Install system dependencies (requires sudo)
make deps

# 2. Build patched FFmpeg from source
make build

# 3. Build Rust tool
make rust-build

# 4. Verify
make info
```

## Build System Details

### Makefile Targets

| Target | Description |
|--------|-------------|
| `make help` | Show all available commands |
| `make docker` | Build FFmpeg in Docker (recommended) |
| `make docker-deps` | Extract runtime .so dependencies from Docker |
| `make deps` | Install build dependencies (Ubuntu, requires sudo) |
| `make build` | Build FFmpeg locally (clone → patch → configure → compile) |
| `make rust-build` | Compile Rust ffmpeg-tool binary |
| `make rust-test` | Run Rust tests |
| `make info` | Show FFmpeg version and capabilities |
| `make probe` | Probe a media file |
| `make transcode` | Run a transcoding job |
| `make clean` | Remove build/ and install/ directories |
| `make clean-all` | Full cleanup (including ffmpeg-src/, patches/) |

### Environment Variables

The Makefile automatically sets these for Rust compilation:

```bash
FFMPEG_PKG_CONFIG_PATH="./install/lib/pkgconfig"   # rsmpeg finds FFmpeg headers
FFMPEG_INCLUDE_DIR="./install/include"               # FFI header location
LD_LIBRARY_PATH="./install/lib:./install/deps"       # Runtime library search
```

If running the binary directly (outside `make`), set `LD_LIBRARY_PATH`:

```bash
LD_LIBRARY_PATH=install/lib:install/deps ./target/release/ffmpeg-tool probe video.mkv
```

### What the Docker Build Does

```
Stage 1 (nvidia/cuda:12.8.1-devel-ubuntu24.04):
  ├── Install 80+ packages (build tools, codecs, GPU libs)
  ├── Enable ccache for fast rebuilds
  ├── Clone patched FFmpeg (7.1.3, 94 patches)
  ├── Apply all 94 patches (CUDA, VAAPI, QSV, AMF, VT, Vulkan)
  ├── Build third-party: nv-codec-headers, Vulkan-Headers, AMF headers
  ├── Configure FFmpeg with 90+ flags (--enable-gpl, --enable-cuda, etc.)
  └── make -j$(nproc) && make install → /workspace/install/

Stage 2 (scratch):
  └── COPY install/ (binaries + libraries + headers + pkgconfig)
```

**Cache mounts** (persisted across builds):
- `/root/.cache/ccache` — C compiler cache
- `/workspace/ffmpeg-src` — avoid re-cloning 1GB FFmpeg source
- `/workspace/third-party` — nv-codec-headers, Vulkan headers

### FFmpeg Configuration

The build enables all major features:

**Video Codecs:** x264, x265, VP8/VP9, AV1 (dav1d, svtav1, aom), WebP, JPEG-XL, OpenJPEG
**Audio Codecs:** AAC (fdk-aac), Opus, Vorbis, MP3 (LAME), Theora
**Hardware Accel:** CUDA/NVENC/NVDEC, VAAPI, QSV (libvpl/libmfx), Vulkan, OpenCL, AMF
**Filters:** tonemap_cuda, scale_cuda, bwdif_cuda, OpenCL tonemap, tonemapx (SIMD)
**Protocols:** SRT, file, pipe
**Subtitles:** libass, freetype, fontconfig, fribidi, harfbuzz

### install/ Directory Structure

After building:

```
install/
├── bin/
│   ├── ffmpeg              # Patched FFmpeg binary (for reference/comparison)
│   └── ffprobe             # Patched ffprobe binary
├── lib/
│   ├── libavcodec.so.61    # Shared libraries (linked by Rust tool)
│   ├── libavformat.so.61
│   ├── libavfilter.so.10
│   ├── libavutil.so.59
│   ├── libswscale.so.8
│   ├── libswresample.so.5
│   ├── libpostproc.so.58
│   ├── libavdevice.so.61
│   └── pkgconfig/          # .pc files for rsmpeg/pkg-config
├── include/                # C headers (for FFI binding generation)
│   ├── libavcodec/
│   ├── libavformat/
│   ├── libavfilter/
│   └── ...
├── deps/                   # Runtime dependencies (from `make docker-deps`)
│   ├── libx264.so.164
│   ├── libx265.so.199
│   ├── libopus.so.0
│   └── ... (60+ .so files)
└── share/ffmpeg/           # Presets and examples
```

### Cargo Configuration

`.cargo/config.toml` is **not committed** to the repo (gitignored — contains absolute local paths).

After building FFmpeg, run once to generate it:
```bash
make setup-cargo
```

This writes the correct `FFMPEG_PKG_CONFIG_PATH` / `FFMPEG_INCLUDE_DIR` for your machine,
enabling bare `cargo build --release` to work. Alternatively, use `make rust-build` directly
(it always passes the right env vars without needing the config file).

### build.rs

The Rust build script:
1. Detects `./install/lib` directory
2. Adds `-L native=./install/lib` (link search path)
3. Adds `-Wl,-rpath,./install/lib` (embed library path in binary)
4. Sets `rerun-if-changed=install` (re-link when FFmpeg is rebuilt)

## Troubleshooting

### "libavcodec.so not found" at runtime

Set `LD_LIBRARY_PATH`:
```bash
export LD_LIBRARY_PATH=$PWD/install/lib:$PWD/install/deps
```

### Docker build fails with "no space left on device"

BuildKit cache can grow large. Prune it:
```bash
docker builder prune
```

### "CUDA not available" when running

Ensure NVIDIA drivers are installed and `nvidia-smi` works. In WSL2:
```bash
nvidia-smi  # Should show your GPU
```

### Rust build can't find FFmpeg headers

Ensure `.cargo/config.toml` points to the correct install path, or set manually:
```bash
FFMPEG_PKG_CONFIG_PATH=$PWD/install/lib/pkgconfig cargo build --release
```
