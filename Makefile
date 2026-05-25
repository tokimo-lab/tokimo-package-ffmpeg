.PHONY: help build deps clone patches clean rsmpeg info docker docker-deps rust-build rust-test probe transcode setup-cargo

SHELL := /bin/bash

# ─── Configuration ───────────────────────────────────────────────
FFMPEG_GIT_URL   ?= https://github.com/jellyfin/jellyfin-ffmpeg.git
FFMPEG_REF       ?= jellyfin
FFMPEG_JOBS      ?= $(shell nproc 2>/dev/null || echo 4)
ENABLE_NVIDIA    ?= 1
ENABLE_AMF       ?= 1

ROOT_DIR     := $(shell pwd)
SRC_DIR      := $(ROOT_DIR)/ffmpeg-src
BUILD_DIR    := $(ROOT_DIR)/build
INSTALL_DIR  := $(ROOT_DIR)/install
PATCHES_DIR  := $(ROOT_DIR)/patches
RSMPEG_DIR   := $(ROOT_DIR)/rsmpeg

# Colors
CYAN  := \033[36m
GREEN := \033[32m
YELLOW := \033[33m
NC    := \033[0m

# ─── Help ────────────────────────────────────────────────────────
help: ## Show this help
	@printf "$(CYAN)FFmpeg Test Project$(NC)\n\n"
	@printf "$(GREEN)Commands:$(NC)\n"
	@printf "  $(YELLOW)make build$(NC)        - 一键编译: clone → patch → configure → build\n"
	@printf "  $(YELLOW)make docker$(NC)       - Docker 编译 FFmpeg (带缓存)\n"
	@printf "  $(YELLOW)make rust-build$(NC)   - 编译 Rust ffmpeg-tool\n"
	@printf "  $(YELLOW)make rust-test$(NC)    - 运行 Rust 测试\n"
	@printf "  $(YELLOW)make probe$(NC)        - 用 ffmpeg-tool 探测测试文件\n"
	@printf "  $(YELLOW)make deps$(NC)         - 安装编译依赖 (需要 sudo)\n"
	@printf "  $(YELLOW)make clone$(NC)        - 仅 clone jellyfin-ffmpeg 源码\n"
	@printf "  $(YELLOW)make patches$(NC)      - 提取 patches 到 patches/ 目录供查看\n"
	@printf "  $(YELLOW)make rsmpeg$(NC)       - clone rsmpeg 到 rsmpeg/ 目录\n"
	@printf "  $(YELLOW)make info$(NC)         - 查看已编译 ffmpeg 的版本和能力\n"
	@printf "  $(YELLOW)make clean$(NC)        - 清理 build 和 install 目录\n"
	@printf "  $(YELLOW)make clean-all$(NC)    - 清理所有 (含源码)\n"

# ─── One-command build ───────────────────────────────────────────
build: ## Clone → Apply Patches → Configure → Build FFmpeg
	@./scripts/build-ffmpeg.sh \
		--src "$(SRC_DIR)" \
		--build "$(BUILD_DIR)" \
		--install "$(INSTALL_DIR)" \
		--patches "$(PATCHES_DIR)" \
		--ref "$(FFMPEG_REF)" \
		--jobs "$(FFMPEG_JOBS)" \
		$(if $(filter 0,$(ENABLE_NVIDIA)),--no-nvidia) \
		$(if $(filter 0,$(ENABLE_AMF)),--no-amf)

# ─── Install dependencies ───────────────────────────────────────
deps: ## 安装 FFmpeg 编译依赖 (Ubuntu/Debian)
	@./scripts/install-deps.sh

# ─── Clone source ────────────────────────────────────────────────
clone: ## Clone jellyfin-ffmpeg 源码
	@if [ -d "$(SRC_DIR)/.git" ]; then \
		echo "[clone] Updating existing source..."; \
		git -C "$(SRC_DIR)" fetch --tags --prune origin; \
		git -C "$(SRC_DIR)" checkout --force -B "$(FFMPEG_REF)" "origin/$(FFMPEG_REF)"; \
	else \
		echo "[clone] Cloning jellyfin-ffmpeg..."; \
		git clone "$(FFMPEG_GIT_URL)" "$(SRC_DIR)"; \
		git -C "$(SRC_DIR)" checkout --force -B "$(FFMPEG_REF)" "origin/$(FFMPEG_REF)"; \
	fi

# ─── Extract patches for viewing ────────────────────────────────
patches: clone ## 提取 debian patches 到 patches/ 目录
	@./scripts/extract-patches.sh "$(SRC_DIR)" "$(PATCHES_DIR)"

# ─── Clone rsmpeg ────────────────────────────────────────────────
rsmpeg: ## Clone rsmpeg (Rust FFmpeg 绑定)
	@if [ -d "$(RSMPEG_DIR)/.git" ]; then \
		echo "[rsmpeg] Updating..."; \
		git -C "$(RSMPEG_DIR)" pull --rebase; \
	else \
		echo "[rsmpeg] Cloning rsmpeg..."; \
		git clone https://github.com/larksuite/rsmpeg.git "$(RSMPEG_DIR)"; \
	fi
	@echo "[rsmpeg] Done. Source at $(RSMPEG_DIR)/"

# ─── Info ────────────────────────────────────────────────────────
info: ## 查看已编译 ffmpeg 的版本和硬件加速能力
	@BIN="$(INSTALL_DIR)/bin/ffmpeg"; \
	if [ ! -x "$$BIN" ]; then \
		echo "FFmpeg not built yet. Run: make build"; exit 1; \
	fi; \
	LIB="$(INSTALL_DIR)/lib"; \
	export LD_LIBRARY_PATH="$$LIB$${LD_LIBRARY_PATH:+:$$LD_LIBRARY_PATH}"; \
	echo "=== Version ==="; \
	"$$BIN" -hide_banner -version | head -3; \
	echo ""; \
	echo "=== Hardware Accelerations ==="; \
	"$$BIN" -hide_banner -hwaccels 2>/dev/null || true; \
	echo ""; \
	echo "=== Key Encoders ==="; \
	"$$BIN" -hide_banner -encoders 2>/dev/null | grep -E 'nvenc|vaapi|amf|qsv|aac|opus|x264|x265|libsvtav1' || true

# ─── Clean ───────────────────────────────────────────────────────
clean: ## 清理 build 和 install 目录
	rm -rf "$(BUILD_DIR)" "$(INSTALL_DIR)"

clean-all: clean ## 清理所有 (含源码、patches、rsmpeg)
	rm -rf "$(SRC_DIR)" "$(PATCHES_DIR)"/*.patch "$(RSMPEG_DIR)"

# ─── Rust env vars for custom FFmpeg ─────────────────────────────
# rusty_ffmpeg reads these in its build.rs
FFMPEG_ENV = \
	FFMPEG_PKG_CONFIG_PATH="$(INSTALL_DIR)/lib/pkgconfig" \
	FFMPEG_INCLUDE_DIR="$(INSTALL_DIR)/include" \
	LD_LIBRARY_PATH="$(INSTALL_DIR)/lib:$(INSTALL_DIR)/deps$${LD_LIBRARY_PATH:+:$$LD_LIBRARY_PATH}"

# ─── Docker build ────────────────────────────────────────────────
docker: ## Docker 编译 patched FFmpeg (带缓存)
	@./scripts/docker-build.sh

docker-deps: docker ## 从 Docker 提取运行时依赖库到 install/deps/
	@echo "[docker-deps] Extracting runtime dependencies..."
	@mkdir -p "$(INSTALL_DIR)/deps"
	@docker run --rm -v "$(INSTALL_DIR)/deps:/output" nvidia/cuda:12.8.1-devel-ubuntu24.04 bash -c '\
		DEBIAN_FRONTEND=noninteractive apt-get update -qq && \
		apt-get install -y -qq --no-install-recommends \
			libfdk-aac2 libmp3lame0 libopus0 libvorbis0a libvorbisenc2 libsoxr0 \
			libtheora0 libopenmpt0 libbluray2 libdav1d7 libaom3 libsvtav1enc1d1 \
			libx264-164 libx265-199 libvpx9 libwebp7 libopenjp2-7 libjxl0.7 \
			libzimg2 libchromaprint1 libsrt1.5-gnutls libgnutls30t64 libzvbi0 \
			libass9 libdrm2 libva2 libvulkan1 libplacebo338 libshaderc1 \
			ocl-icd-libopencl1 libvdpau1 libnuma1 libfreetype6 libfontconfig1 \
			libharfbuzz0b libfribidi0 libogg0 libmpg123-0t64 >/dev/null 2>&1 && \
		for lib in libfdk-aac libmp3lame libopus libvorbis libvorbisenc libsoxr \
			libtheora libtheoradec libtheoraenc libopenmpt libbluray libdav1d \
			libaom libSvtAv1Enc libx264 libx265 libvpx libwebp libwebpmux \
			libwebpdemux libsharpyuv libopenjp2 libjxl libjxl_threads libhwy \
			libzimg libchromaprint libsrt-gnutls libzvbi libass libdrm libva \
			libva-drm libva-x11 libvulkan libplacebo libshaderc_shared libOpenCL \
			libvdpau libnuma libfreetype libfontconfig libharfbuzz libfribidi \
			libgnutls libogg libmpg123 libX11 libXext libXau libXdmcp; do \
			find /usr/lib/x86_64-linux-gnu -maxdepth 1 -name "$${lib}.so*" \
				-exec cp -a {} /output/ \; 2>/dev/null; \
		done && \
		echo "[docker-deps] Copied $$(ls /output/*.so* 2>/dev/null | wc -l) runtime libraries"'

# ─── Rust build & test ───────────────────────────────────────────
rust-build: ## 编译 Rust ffmpeg-tool
	@test -d "$(INSTALL_DIR)/lib" || { echo "FFmpeg not built. Run: make docker"; exit 1; }
	$(FFMPEG_ENV) cargo build --release
	@echo ""
	@echo "✅ Binary at: target/release/ffmpeg-tool"

setup-cargo: ## 生成 .cargo/config.toml（供 cargo build 直接使用）
	@test -d "$(INSTALL_DIR)/lib" || { echo "FFmpeg not built. Run: make docker"; exit 1; }
	@mkdir -p .cargo
	@printf '[env]\nFFMPEG_PKG_CONFIG_PATH = "%s/lib/pkgconfig"\nFFMPEG_INCLUDE_DIR = "%s/include"\n' \
		"$(abspath $(INSTALL_DIR))" "$(abspath $(INSTALL_DIR))" > .cargo/config.toml
	@echo "✅ .cargo/config.toml written (gitignored, local only)"

rust-test: ## 运行 Rust 测试
	@test -d "$(INSTALL_DIR)/lib" || { echo "FFmpeg not built. Run: make docker"; exit 1; }
	$(FFMPEG_ENV) cargo test

TEST_FILE ?= $(HOME)/media/movie/Eternity and a Day (1998)/Eternity and a Day (1998) Bluray-1080p.mkv
OUTPUT_FILE ?= /tmp/ffmpeg-test-output.mp4
TRANSCODE_OPTS ?=

probe: rust-build ## 用 ffmpeg-tool 探测测试文件
	$(FFMPEG_ENV) ./target/release/ffmpeg-tool probe "$(TEST_FILE)"

transcode: rust-build ## 转码测试文件 (可用 TRANSCODE_OPTS 自定义参数)
	$(FFMPEG_ENV) ./target/release/ffmpeg-tool transcode $(TRANSCODE_OPTS) "$(TEST_FILE)" "$(OUTPUT_FILE)"
