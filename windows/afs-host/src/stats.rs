//! Per-callback counters and timings.
//!
//! Exists because guessing at which operation is slow does not work. The only
//! instrumentation before this was on directory enumeration, so enumeration was
//! the only thing that ever showed up in the log — which says nothing about
//! whether it is actually the bottleneck.
//!
//! Everything here is lock-free and off the critical path: an `Instant::now`
//! pair and two relaxed atomic adds per callback.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Anything at least this slow gets its own line, because it is long enough for
/// a person to notice as a stall.
pub const SLOW_OP: Duration = Duration::from_millis(500);

#[derive(Default)]
pub struct OpStat {
    pub count: AtomicU64,
    pub nanos: AtomicU64,
    pub slowest_nanos: AtomicU64,
}

impl OpStat {
    fn record(&self, elapsed: Duration) {
        let nanos = elapsed.as_nanos() as u64;
        self.count.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(nanos, Ordering::Relaxed);
        self.slowest_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    fn take(&self) -> (u64, Duration, Duration) {
        let count = self.count.swap(0, Ordering::Relaxed);
        let nanos = self.nanos.swap(0, Ordering::Relaxed);
        let slowest = self.slowest_nanos.swap(0, Ordering::Relaxed);
        (
            count,
            Duration::from_nanos(nanos),
            Duration::from_nanos(slowest),
        )
    }
}

#[derive(Default)]
pub struct Stats {
    pub security: OpStat,
    pub open: OpStat,
    pub create: OpStat,
    pub close: OpStat,
    pub cleanup: OpStat,
    pub read: OpStat,
    pub write: OpStat,
    pub get_info: OpStat,
    pub set_info: OpStat,
    pub readdir: OpStat,

    pub read_bytes: AtomicU64,
    pub write_bytes: AtomicU64,

    /// Directory listings served without touching USB.
    pub dir_hit: AtomicU64,
    /// Listings served stale because a refresh was already in flight.
    pub dir_stale: AtomicU64,
    /// Listings actually fetched from the device.
    pub dir_fetch: AtomicU64,
}

impl Stats {
    /// Formats and clears the interval's counters. `None` when nothing happened,
    /// so an idle mount stays quiet.
    pub fn take_report(&self) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();

        for (name, stat) in [
            ("sec", &self.security),
            ("open", &self.open),
            ("create", &self.create),
            ("close", &self.close),
            ("cleanup", &self.cleanup),
            ("read", &self.read),
            ("write", &self.write),
            ("getinfo", &self.get_info),
            ("setinfo", &self.set_info),
            ("readdir", &self.readdir),
        ] {
            let (count, total, slowest) = stat.take();
            if count == 0 {
                continue;
            }
            // Mean is what shows a systemic cost; max is what shows a stall.
            let mean = total / count.min(u32::MAX as u64) as u32;
            parts.push(format!(
                "{name} {count}x tot={total:.0?} avg={mean:.1?} max={slowest:.0?}"
            ));
        }

        let read_bytes = self.read_bytes.swap(0, Ordering::Relaxed);
        let write_bytes = self.write_bytes.swap(0, Ordering::Relaxed);
        let hit = self.dir_hit.swap(0, Ordering::Relaxed);
        let stale = self.dir_stale.swap(0, Ordering::Relaxed);
        let fetch = self.dir_fetch.swap(0, Ordering::Relaxed);

        if parts.is_empty() && read_bytes == 0 && write_bytes == 0 {
            return None;
        }

        if read_bytes > 0 {
            parts.push(format!("read={:.1}MiB", read_bytes as f64 / (1 << 20) as f64));
        }
        if write_bytes > 0 {
            parts.push(format!(
                "wrote={:.1}MiB",
                write_bytes as f64 / (1 << 20) as f64
            ));
        }
        if hit + stale + fetch > 0 {
            parts.push(format!("dir(hit={hit} stale={stale} fetch={fetch})"));
        }

        Some(parts.join("  "))
    }
}

/// Times one callback and records it on drop.
///
/// Drop-based so it works regardless of which `?` the callback returns through,
/// and so it does not have to wrap the body in a closure — that fights the borrow
/// checker over the `&mut` out-parameters WinFsp callbacks take.
pub struct OpTimer<'a> {
    stat: &'a OpStat,
    op: &'static str,
    path: &'a str,
    started: Instant,
}

impl<'a> OpTimer<'a> {
    pub fn new(stat: &'a OpStat, op: &'static str, path: &'a str) -> Self {
        OpTimer {
            stat,
            op,
            path,
            started: Instant::now(),
        }
    }
}

impl Drop for OpTimer<'_> {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed();
        self.stat.record(elapsed);
        if elapsed >= SLOW_OP {
            let path = if self.path.is_empty() {
                "\\"
            } else {
                self.path
            };
            log::warn!("slow {}: '{}' took {:.1?}", self.op, path, elapsed);
        }
    }
}
