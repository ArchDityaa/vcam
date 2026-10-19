//! Health monitoring for the output pipeline.
//!
//! Two failure modes were observed in practice: the pipeline stalls when the
//! device stops draining (which used to wedge the device), and memory grows
//! without bound when the output is reconfigured while another application
//! holds the device. This watchdog evaluates a few cheap signals and asks the
//! controller to rebuild the output pipeline, or to exit so systemd restarts
//! the daemon cleanly.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use vcam_core::WatchdogConfig;

/// What the controller should do after evaluating the health.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchdogVerdict {
    /// Everything looks healthy.
    Healthy,
    /// A problem was detected but the backoff has not elapsed yet.
    Waiting(String),
    /// Rebuild the output pipeline now.
    Recover(String),
    /// Give up and let the service manager restart the daemon.
    Restart(String),
}

const MIN_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Growth over this window that exceeds a quarter of `max_rss_mb` is treated
/// as a leak even when the absolute cap has not been reached yet.
const RSS_WINDOW: Duration = Duration::from_secs(45);
/// A fully reset watchdog forgets past recoveries after this much clean time.
const CLEAN_PERIOD: Duration = Duration::from_secs(60);

struct RssSample {
    at: Instant,
    bytes: u64,
}

pub struct Watchdog {
    enabled: bool,
    max_rss_bytes: u64,
    stall_timeout: Duration,
    max_push_failures: u32,
    max_recoveries: u32,

    last_success: Instant,
    last_recovery: Option<Instant>,
    consecutive_failures: u32,
    stall_since: Option<Instant>,
    recoveries: u32,
    backoff: Duration,
    next_attempt: Option<Instant>,
    samples: VecDeque<RssSample>,
    last_sample: Option<Instant>,
}

impl Watchdog {
    pub fn new(config: &WatchdogConfig, now: Instant) -> Self {
        Self {
            enabled: config.enabled,
            max_rss_bytes: config.max_rss_mb.saturating_mul(1024 * 1024),
            stall_timeout: Duration::from_millis(config.stall_timeout_ms),
            max_push_failures: config.max_push_failures,
            max_recoveries: config.max_recoveries,
            last_success: now,
            last_recovery: None,
            consecutive_failures: 0,
            stall_since: None,
            recoveries: 0,
            backoff: MIN_BACKOFF,
            next_attempt: None,
            samples: VecDeque::new(),
            last_sample: None,
        }
    }

    pub fn record_success(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        self.stall_since = None;
        self.last_success = now;
        if let Some(last) = self.last_recovery {
            if now.duration_since(last) >= CLEAN_PERIOD {
                self.recoveries = 0;
                self.backoff = MIN_BACKOFF;
                self.next_attempt = None;
                self.last_recovery = None;
            }
        }
    }

    pub fn record_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
    }

    /// Called after a recovery attempt rebuilt the pipeline successfully.
    pub fn note_recovery_ok(&mut self, now: Instant) {
        self.consecutive_failures = 0;
        self.stall_since = None;
        self.last_success = now;
        self.last_recovery = Some(now);
    }

    /// Called when a recovery attempt failed to rebuild the pipeline.
    pub fn note_recovery_failed(&mut self) {
        // Leave the counters untouched so the next check retries with backoff.
    }

    /// Evaluate the health and decide what to do.
    ///
    /// `level` is the bytes currently queued in `appsrc` and `level_max` its
    /// configured maximum; a level pinned at the maximum means the device
    /// stopped draining.
    pub fn check(
        &mut self,
        now: Instant,
        rss_bytes: u64,
        playing: bool,
        level: u64,
        level_max: u64,
    ) -> WatchdogVerdict {
        if !self.enabled {
            return WatchdogVerdict::Healthy;
        }

        self.sample_rss(now, rss_bytes);

        // A pin at the high watermark means frames are piling up.
        if level_max > 0 && level >= level_max / 2 {
            match self.stall_since {
                Some(since) if now.duration_since(since) >= self.stall_timeout => {
                    return self.trigger(now, "the virtual camera stopped draining frames");
                }
                Some(_) => {}
                None => self.stall_since = Some(now),
            }
        } else {
            self.stall_since = None;
        }

        if self.consecutive_failures >= self.max_push_failures {
            return self.trigger(
                now,
                &format!(
                    "{} consecutive frame pushes failed",
                    self.consecutive_failures
                ),
            );
        }

        if playing && now.duration_since(self.last_success) >= self.stall_timeout {
            return self.trigger(now, "no frame reached the virtual camera in time");
        }

        if let Some(reason) = self.rss_problem() {
            return self.trigger(now, &reason);
        }

        WatchdogVerdict::Healthy
    }

    fn trigger(&mut self, now: Instant, reason: &str) -> WatchdogVerdict {
        if self.recoveries >= self.max_recoveries {
            return WatchdogVerdict::Restart(format!(
                "{reason}; exhausted {0} recovery attempts",
                self.max_recoveries
            ));
        }
        match self.next_attempt {
            Some(at) if now < at => WatchdogVerdict::Waiting(reason.to_string()),
            _ => {
                self.recoveries = self.recoveries.saturating_add(1);
                self.next_attempt = Some(now + self.backoff);
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                WatchdogVerdict::Recover(reason.to_string())
            }
        }
    }

    fn sample_rss(&mut self, now: Instant, rss_bytes: u64) {
        if let Some(last) = self.last_sample {
            if now.duration_since(last) < SAMPLE_INTERVAL {
                return;
            }
        }
        self.last_sample = Some(now);
        self.samples.push_back(RssSample {
            at: now,
            bytes: rss_bytes,
        });
        while let Some(front) = self.samples.front() {
            if now.duration_since(front.at) > RSS_WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    fn rss_problem(&self) -> Option<String> {
        let newest = self.samples.back()?;
        if newest.bytes > self.max_rss_bytes {
            return Some(format!(
                "resident memory {} MiB exceeds the {} MiB limit",
                newest.bytes / (1024 * 1024),
                self.max_rss_bytes / (1024 * 1024)
            ));
        }
        let oldest = self.samples.front()?;
        let span = newest.at.duration_since(oldest.at);
        if span >= RSS_MIN_WINDOW {
            let growth = newest.bytes.saturating_sub(oldest.bytes);
            if growth > self.max_rss_bytes / 4 {
                return Some(format!(
                    "resident memory grew by {} MiB in {}s",
                    growth / (1024 * 1024),
                    span.as_secs()
                ));
            }
        }
        None
    }
}

/// The RSS growth heuristic needs at least this much history.
const RSS_MIN_WINDOW: Duration = Duration::from_secs(30);

/// Resident set size of this process, in bytes.
pub fn current_rss_bytes() -> u64 {
    let Ok(text) = std::fs::read_to_string("/proc/self/statm") else {
        return 0;
    };
    let Some(resident) = text.split_whitespace().nth(1) else {
        return 0;
    };
    let Ok(pages) = resident.parse::<u64>() else {
        return 0;
    };
    // SAFETY: sysconf has no preconditions.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    let page_size = if page_size > 0 {
        page_size as u64
    } else {
        4096
    };
    pages.saturating_mul(page_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> WatchdogConfig {
        WatchdogConfig {
            enabled: true,
            max_rss_mb: 256,
            stall_timeout_ms: 1000,
            max_push_failures: 3,
            max_recoveries: 2,
        }
    }

    #[test]
    fn healthy_by_default() {
        let now = Instant::now();
        let mut watchdog = Watchdog::new(&config(), now);
        let verdict = watchdog.check(now, 100 * 1024 * 1024, true, 0, 1024);
        assert_eq!(verdict, WatchdogVerdict::Healthy);
    }

    #[test]
    fn consecutive_failures_trigger_recovery() {
        let now = Instant::now();
        let mut watchdog = Watchdog::new(&config(), now);
        watchdog.record_failure();
        watchdog.record_failure();
        watchdog.record_failure();
        let verdict = watchdog.check(now, 100 * 1024 * 1024, true, 0, 1024);
        assert!(matches!(verdict, WatchdogVerdict::Recover(_)));
    }

    #[test]
    fn backoff_then_restart_when_exhausted() {
        let now = Instant::now();
        let mut watchdog = Watchdog::new(&config(), now);
        for _ in 0..3 {
            watchdog.record_failure();
        }
        // First recovery.
        assert!(matches!(
            watchdog.check(now, 10, true, 0, 1024),
            WatchdogVerdict::Recover(_)
        ));
        // Inside the backoff window: waiting.
        let verdict = watchdog.check(now, 10, true, 0, 1024);
        assert!(matches!(verdict, WatchdogVerdict::Waiting(_)));
        // After the backoff, the second (final) recovery.
        let later = now + Duration::from_secs(1);
        assert!(matches!(
            watchdog.check(later, 10, true, 0, 1024),
            WatchdogVerdict::Recover(_)
        ));
        // Budget exhausted: give up.
        let later = later + Duration::from_secs(5);
        assert!(matches!(
            watchdog.check(later, 10, true, 0, 1024),
            WatchdogVerdict::Restart(_)
        ));
    }

    #[test]
    fn rss_cap_triggers_recovery() {
        let now = Instant::now();
        let mut watchdog = Watchdog::new(&config(), now);
        let verdict = watchdog.check(now, 512 * 1024 * 1024, true, 0, 1024);
        assert!(matches!(verdict, WatchdogVerdict::Recover(_)));
    }

    #[test]
    fn stall_is_detected_after_timeout() {
        let now = Instant::now();
        let mut watchdog = Watchdog::new(&config(), now);
        // Level pinned at the watermark, but not long enough yet.
        assert_eq!(
            watchdog.check(now, 10, true, 1024, 1024),
            WatchdogVerdict::Healthy
        );
        let later = now + Duration::from_secs(2);
        assert!(matches!(
            watchdog.check(later, 10, true, 1024, 1024),
            WatchdogVerdict::Recover(_)
        ));
    }
}
