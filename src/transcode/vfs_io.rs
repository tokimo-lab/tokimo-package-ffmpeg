//! Custom AVIO context that reads from a `DirectInput` callback instead of HTTP.
//!
//! This eliminates the HTTP→VFS→SMB round-trip overhead during seek operations.
//! With HTTP input, each `avformat_seek_file` triggers 3 HTTP range requests
//! (~500ms over SMB). With this custom AVIO, seeks are instant (just a position
//! counter update) and reads go directly through the callback with a read-ahead
//! buffer to amortize per-call overhead.

use std::ffi::CString;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use rsmpeg::avformat::{AVFormatContextInput, AVIOContextContainer, AVIOContextCustom};
use rsmpeg::avutil::AVMem;
use rsmpeg::ffi;

use super::DirectInput;

type ReadFn = Box<dyn FnMut(&mut Vec<u8>, &mut [u8]) -> i32 + Send + 'static>;
type SeekFn = Box<dyn FnMut(&mut Vec<u8>, i64, i32) -> i64 + Send + 'static>;

/// Read-ahead buffer size. Each VFS read fetches this much data at once,
/// then serves subsequent `FFmpeg` reads from the in-process buffer.
///
/// Larger values = more concurrent SMB sub-reads per VFS call → higher throughput.
/// Benchmark (2.5 Gbps LAN, SMB 3.1.1):
///   4 MB  → ~57 MB/s  (4 concurrent sub-reads)
///   32 MB → ~172 MB/s (32 concurrent sub-reads)
/// Memory cost: one buffer per HLS session (32 MB × N sessions).
const READAHEAD_BYTES_HLS: u64 = 32 * 1024 * 1024; // 32 MB — HLS / transcode
const READAHEAD_BYTES_DEFAULT: u64 = 4 * 1024 * 1024; // 4 MB — conservative default

/// Recommended `readahead_bytes` for HLS / transcode sessions.
pub const READAHEAD_HLS: u64 = READAHEAD_BYTES_HLS;

/// AVIO buffer size passed to `avio_alloc_context`.
const AVIO_BUF_SIZE: usize = 256 * 1024; // 256 KB

struct IoState {
    input: Arc<DirectInput>,
    position: u64,
    /// Cached read-ahead data.
    buf: Vec<u8>,
    /// File offset where `buf[0]` corresponds to.
    buf_start: u64,
    /// Effective read-ahead fetch size (from DirectInput or default).
    readahead_bytes: u64,
    /// Stats: total VFS fetch calls (lifetime).
    fetch_count: u64,
    /// Stats: total bytes fetched from VFS (lifetime).
    fetch_bytes: u64,
    /// Stats: total time spent in VFS reads (lifetime, µs).
    fetch_time_us: u64,
    /// Stats: VFS fetch calls since last seek.
    pass_fetch_count: u64,
    /// Stats: bytes fetched since last seek.
    pass_fetch_bytes: u64,
    /// Stats: VFS read time since last seek (µs).
    pass_fetch_time_us: u64,
}

impl IoState {
    fn new(input: Arc<DirectInput>) -> Self {
        let readahead_bytes = input.readahead_bytes.unwrap_or(READAHEAD_BYTES_DEFAULT);
        Self {
            readahead_bytes,
            input,
            position: 0,
            buf: Vec::new(),
            buf_start: 0,
            fetch_count: 0,
            fetch_bytes: 0,
            fetch_time_us: 0,
            pass_fetch_count: 0,
            pass_fetch_bytes: 0,
            pass_fetch_time_us: 0,
        }
    }

    fn read(&mut self, out: &mut [u8]) -> i32 {
        let file_size = self.input.size;
        if self.position >= file_size {
            return ffi::AVERROR_EOF;
        }

        let buf_end = self.buf_start + self.buf.len() as u64;

        // Serve from read-ahead buffer if possible
        if self.position >= self.buf_start && self.position < buf_end {
            let offset_in_buf = (self.position - self.buf_start) as usize;
            let available = self.buf.len() - offset_in_buf;
            let n = available.min(out.len());
            out[..n].copy_from_slice(&self.buf[offset_in_buf..offset_in_buf + n]);
            self.position += n as u64;
            return n as i32;
        }

        // Buffer miss — fetch from VFS with read-ahead.
        // read_at returns an owned Vec<u8>, so we assign it directly to self.buf
        // without a pre-allocation or an extra copy.
        let remaining = file_size - self.position;
        let fetch_size = self.readahead_bytes.min(remaining) as usize;

        let t0 = Instant::now();
        match (self.input.read_at)(self.position, fetch_size) {
            Ok(bytes) if bytes.is_empty() => ffi::AVERROR_EOF,
            Ok(bytes) => {
                let n = bytes.len();
                let elapsed_us = t0.elapsed().as_micros() as u64;
                self.fetch_count += 1;
                self.fetch_bytes += n as u64;
                self.fetch_time_us += elapsed_us;
                self.pass_fetch_count += 1;
                self.pass_fetch_bytes += n as u64;
                self.pass_fetch_time_us += elapsed_us;

                // Log first 10 fetches after each seek, then every 50th
                if self.pass_fetch_count <= 10 || self.fetch_count.is_multiple_of(50) {
                    // tracing::debug!(
                    //     "[vfs-avio] fetch #{} (pass #{}): offset={}MB size={}KB took={:.1}ms (pass: {}MB, {:.0}ms)",
                    //     self.fetch_count,
                    //     self.pass_fetch_count,
                    //     self.position / 1024 / 1024,
                    //     n / 1024,
                    //     elapsed_us as f64 / 1000.0,
                    //     self.pass_fetch_bytes / 1024 / 1024,
                    //     self.pass_fetch_time_us as f64 / 1000.0,
                    // );
                }

                // Copy to AVIO output buffer (unavoidable — AVIO owns this pointer).
                let copy_n = n.min(out.len());
                out[..copy_n].copy_from_slice(&bytes[..copy_n]);

                // Cache the rest for future reads (Vec moved in, no extra alloc).
                self.buf_start = self.position;
                self.buf = bytes;
                self.position += copy_n as u64;

                copy_n as i32
            }
            Err(e) => {
                tracing::info!("[vfs-avio] read error at offset {}: {}", self.position, e);
                ffi::AVERROR_EOF
            }
        }
    }

    fn seek(&mut self, offset: i64, whence: i32) -> i64 {
        const AVSEEK_SIZE: i32 = 0x10000;
        const SEEK_SET: i32 = 0;
        const SEEK_CUR: i32 = 1;
        const SEEK_END: i32 = 2;

        // Handle AVSEEK_SIZE first — it's a query, must not modify position.
        // AVSEEK_SIZE (0x10000) would be masked to 0 by 0xFFFF, colliding
        // with SEEK_SET, so check it before masking.
        if whence & AVSEEK_SIZE != 0 {
            return self.input.size as i64;
        }

        // Strip AVSEEK_FORCE (0x20000) — it's a hint, not a seek mode
        let whence_base = whence & 0xFF;

        // Reset per-pass stats on SEEK_SET (new seek pass starting)
        if whence_base == SEEK_SET {
            self.pass_fetch_count = 0;
            self.pass_fetch_bytes = 0;
            self.pass_fetch_time_us = 0;
        }

        match whence_base {
            SEEK_SET => {
                self.position = offset.max(0) as u64;
                self.position as i64
            }
            SEEK_CUR => {
                let new_pos = self.position as i64 + offset;
                self.position = new_pos.max(0) as u64;
                self.position as i64
            }
            SEEK_END => {
                let new_pos = self.input.size as i64 + offset;
                self.position = new_pos.max(0) as u64;
                self.position as i64
            }
            _ => -1,
        }
    }
}

/// Create an `AVFormatContextInput` backed by a `DirectInput` instead of HTTP.
///
/// The custom AVIO eliminates HTTP overhead during seek (each seek is just a
/// position counter update). A 4 MB read-ahead buffer amortizes VFS call
/// overhead for sequential reads.
pub(crate) fn open_direct_input(
    input: Arc<DirectInput>,
    probe_opts: &mut Option<rsmpeg::avutil::AVDictionary>,
) -> crate::error::Result<AVFormatContextInput> {
    let state = Arc::new(Mutex::new(IoState::new(input.clone())));
    let _readahead_kb = state
        .lock()
        .unwrap_or_else(|e| {
            tracing::warn!("mutex poisoned, recovering: {e}");
            e.into_inner()
        })
        .readahead_bytes
        / 1024;

    let state_r = state.clone();
    let state_s = state.clone();

    let read_fn: ReadFn = Box::new(move |_data, buf| {
        state_r
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("read mutex poisoned, recovering: {e}");
                e.into_inner()
            })
            .read(buf)
    });

    let seek_fn: SeekFn = Box::new(move |_data, offset, whence| {
        state_s
            .lock()
            .unwrap_or_else(|e| {
                tracing::warn!("seek mutex poisoned, recovering: {e}");
                e.into_inner()
            })
            .seek(offset, whence)
    });

    let buffer = AVMem::new(AVIO_BUF_SIZE);
    let custom_io = AVIOContextCustom::alloc_context(
        buffer,
        false,  // read mode
        vec![], // unused opaque data (we capture state in closures)
        Some(read_fn),
        None, // no write
        Some(seek_fn),
    );

    // Pass a filename hint so the MKV demuxer is auto-detected.
    // The URL is not used for I/O when a custom AVIO is provided.
    let filename_hint = input.filename_hint.as_deref().unwrap_or("input.mkv");
    let c_hint =
        CString::new(filename_hint).unwrap_or_else(|_| CString::new("input.mkv").expect("constant has no NUL"));

    // tracing::debug!(
    //     "[vfs-avio] Opening: size={}MB, hint={}, readahead={}KB",
    //     input.size / 1024 / 1024,
    //     filename_hint,
    //     readahead_kb,
    // );

    let ctx = AVFormatContextInput::builder()
        .url(&c_hint)
        .io_context(AVIOContextContainer::Custom(custom_io))
        .options(probe_opts)
        .open()
        .map_err(|e| crate::error::Error::Other(format!("Failed to open direct input: {e}")))?;

    Ok(ctx)
}
