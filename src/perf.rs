use std::{
    mem::MaybeUninit,
    time::{Duration, Instant},
};

/// Return the process peak RSS in bytes, if the platform exposes it.
pub fn peak_rss_bytes() -> Option<u64> {
    let mut usage = MaybeUninit::<nix::libc::rusage>::zeroed();
    // SAFETY: `usage` points to valid writable memory for `getrusage`.
    let rc = unsafe { nix::libc::getrusage(nix::libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }

    // SAFETY: `getrusage` succeeded, so `usage` was initialized by libc.
    let usage = unsafe { usage.assume_init() };

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        Some(usage.ru_maxrss as u64)
    }

    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    {
        Some((usage.ru_maxrss as u64).saturating_mul(1024))
    }
}

fn fmt_duration_ms(elapsed: Duration) -> String {
    format!("{:.2}ms", elapsed.as_secs_f64() * 1_000.0)
}

fn fmt_peak_rss() -> String {
    peak_rss_bytes()
        .map(|bytes| format!("{:.1}MiB", bytes as f64 / (1024.0 * 1024.0)))
        .unwrap_or_else(|| "n/a".to_owned())
}

pub fn log_page_timing(stage: &str, page_num: usize, elapsed: Duration) {
    log::debug!(
        target: "perf",
        "{stage} page={} elapsed={} peak_rss={}",
        page_num,
        fmt_duration_ms(elapsed),
        fmt_peak_rss(),
    );
}

pub fn log_cache_state(stage: &str, pages: usize, bytes: u64, evicted: usize) {
    log::debug!(
        target: "perf",
        "{stage} resident_pages={} resident_bytes={} ({:.1}MiB) evicted={} peak_rss={}",
        pages,
        bytes,
        bytes as f64 / (1024.0 * 1024.0),
        evicted,
        fmt_peak_rss(),
    );
}

pub fn log_first_page_ready(started_at: Instant, page_num: usize) {
    log::info!(
        target: "perf",
        "first_page_ready page={} elapsed={} peak_rss={}",
        page_num,
        fmt_duration_ms(started_at.elapsed()),
        fmt_peak_rss(),
    );
}
