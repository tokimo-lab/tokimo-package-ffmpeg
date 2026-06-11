//! DTS-based mux scheduler — prevents A/V desync by throttling fast streams.
//!
//! Mirrors `FFmpeg` CLI's `ffmpeg_sched.c` approach: tracks the last-written DTS
//! of every output stream and blocks any stream whose DTS exceeds the slowest
//! ("trailing") stream by more than [`SCHEDULE_TOLERANCE_US`].
//!
//! # Zero-copy / zero-alloc hot path
//!
//! [`report_dts`] performs a single `AtomicI64::store` + `Condvar::notify_all`.
//! [`throttle`] performs atomic loads and only falls into the condvar slow-path
//! when actual backpressure is needed.

use rsmpeg::ffi;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// Maximum allowed DTS gap between the fastest and slowest output streams.
/// Matches `FFmpeg` CLI's `SCHEDULE_TOLERANCE` (100 ms in `AV_TIME_BASE` µs).
const SCHEDULE_TOLERANCE_US: i64 = 100_000;

/// Condvar poll ceiling — avoids permanent deadlock if a stream silently dies.
const WAIT_TIMEOUT: Duration = Duration::from_millis(10);

/// Cross-thread DTS tracker with condvar-based backpressure.
///
/// Shared via `Arc<MuxScheduler>` between the main demux/mux loop and any
/// threaded encode pipelines (audio, GPU video).
pub struct MuxScheduler {
    /// Last DTS written to (or produced for) each output stream, in µs.
    stream_dts: Box<[AtomicI64]>,
    /// Set `true` when a stream has no more packets to produce.
    stream_finished: Box<[AtomicBool]>,
    /// Wakes up threads blocked in [`throttle`].
    notify: Condvar,
    /// Paired with `notify`; the bool inside is unused — we rely on atomics.
    lock: Mutex<()>,
}

impl MuxScheduler {
    pub fn new(num_output_streams: usize) -> Self {
        Self {
            stream_dts: (0..num_output_streams)
                .map(|_| AtomicI64::new(ffi::AV_NOPTS_VALUE))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            stream_finished: (0..num_output_streams)
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            notify: Condvar::new(),
            lock: Mutex::new(()),
        }
    }

    /// Record the latest output DTS (in µs) for a stream and wake blocked writers.
    #[inline]
    pub fn report_dts(&self, out_stream_idx: usize, dts_us: i64) {
        if dts_us == ffi::AV_NOPTS_VALUE {
            return;
        }
        self.stream_dts[out_stream_idx].store(dts_us, Ordering::Release);
        self.notify.notify_all();
    }

    /// Mark a stream as finished (excluded from trailing-DTS calculation).
    pub fn mark_finished(&self, out_stream_idx: usize) {
        self.stream_finished[out_stream_idx].store(true, Ordering::Release);
        self.notify.notify_all();
    }

    /// Block until writing a packet with `dts_us` on `out_stream_idx` would not
    /// put this stream more than [`SCHEDULE_TOLERANCE_US`] ahead of the slowest
    /// active stream.
    ///
    /// Returns immediately when:
    /// - `dts_us` is `AV_NOPTS_VALUE`, or
    /// - not all other streams have reported their first DTS yet, or
    /// - the gap is within tolerance, or
    /// - this is the only active stream.
    #[inline]
    pub fn throttle(&self, out_stream_idx: usize, dts_us: i64) {
        if dts_us == ffi::AV_NOPTS_VALUE {
            return;
        }
        if self.gap_ok(out_stream_idx, dts_us) {
            return;
        }
        self.throttle_slow(out_stream_idx, dts_us);
    }

    #[cold]
    fn throttle_slow(&self, out_stream_idx: usize, dts_us: i64) {
        let mut guard = self.lock.lock().unwrap_or_else(|e| {
            tracing::warn!("scheduler mutex poisoned, recovering: {e}");
            e.into_inner()
        });
        loop {
            if self.gap_ok(out_stream_idx, dts_us) {
                return;
            }
            (guard, _) = self.notify.wait_timeout(guard, WAIT_TIMEOUT).unwrap_or_else(|e| {
                tracing::error!("scheduler condvar wait_timeout failed: {e}");
                e.into_inner()
            });
        }
    }

    /// `true` when it is safe to write a packet with `dts_us`.
    #[inline]
    fn gap_ok(&self, out_stream_idx: usize, dts_us: i64) -> bool {
        let trailing = self.trailing_dts(out_stream_idx);
        trailing == ffi::AV_NOPTS_VALUE || dts_us - trailing < SCHEDULE_TOLERANCE_US
    }

    /// Minimum DTS across all active output streams, **excluding** `skip_idx`.
    ///
    /// Returns [`ffi::AV_NOPTS_VALUE`] when:
    /// - any non-finished, non-skipped stream has not yet reported a DTS, or
    /// - no other active streams exist.
    fn trailing_dts(&self, skip_idx: usize) -> i64 {
        let mut min = i64::MAX;
        for (i, dts_atom) in self.stream_dts.iter().enumerate() {
            if i == skip_idx {
                continue;
            }
            if self.stream_finished[i].load(Ordering::Acquire) {
                continue;
            }
            let dts = dts_atom.load(Ordering::Acquire);
            if dts == ffi::AV_NOPTS_VALUE {
                return ffi::AV_NOPTS_VALUE;
            }
            min = min.min(dts);
        }
        if min == i64::MAX { ffi::AV_NOPTS_VALUE } else { min }
    }
}

/// Convert a DTS value from a stream timebase to microseconds (`AV_TIME_BASE`).
///
/// Uses `i128` intermediate to avoid overflow for large DTS values.
#[inline]
pub fn dts_to_us(dts: i64, tb: ffi::AVRational) -> i64 {
    if dts == ffi::AV_NOPTS_VALUE || tb.den == 0 {
        return ffi::AV_NOPTS_VALUE;
    }
    ((i128::from(dts) * i128::from(tb.num) * 1_000_000) / i128::from(tb.den)) as i64
}
