//! Decides when the UI's window has failed, so the core can start it again
//! (owner decisions, 21 October and 16 November 2026).
//!
//! WebView2 sometimes fails on its own: its processes start and exit within
//! seconds and leave the UI process running with a blank window. Two signs
//! are watched:
//!
//! * the page never sends a request within [`READY_TIMEOUT`] of the UI
//!   starting; and
//! * after the web view's processes appeared in the UI's Job, the UI is left
//!   alone in it for [`ALONE_GRACE`]. The grace covers a normal close, when
//!   the web view ends a moment before the UI does.
//!
//! A failed window is started again once without asking ([`Retries`]). A
//! failure within [`AUTO_RETRY_INTERVAL`] of that retry asks the user, so a
//! window that keeps failing cannot restart in a loop.
//!
//! The decisions are pure, so they are tested without a window.

use std::time::{Duration, Instant};

/// How long the page has to send its first request.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the UI may run without any web view process before that counts
/// as a failure.
pub const ALONE_GRACE: Duration = Duration::from_secs(3);
/// How long after an automatic retry another failure is asked about instead
/// of retried without asking.
pub const AUTO_RETRY_INTERVAL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The page sent no request in time.
    NotReady,
    /// The web view's processes exited while the UI kept running.
    WebViewGone,
}

pub struct Watch {
    started: Instant,
    web_view_seen: bool,
    alone_since: Option<Instant>,
}

impl Watch {
    pub fn new(started: Instant) -> Self {
        Self {
            started,
            web_view_seen: false,
            alone_since: None,
        }
    }

    /// Takes one observation: whether the page has reached the core, and how
    /// many processes the UI's Job holds (`None` if it could not be read;
    /// then only readiness is judged).
    pub fn observe(
        &mut self,
        now: Instant,
        ready: bool,
        processes: Option<u32>,
    ) -> Option<Failure> {
        match processes {
            Some(count) if count > 1 => {
                self.web_view_seen = true;
                self.alone_since = None;
            }
            Some(_) if self.web_view_seen => {
                let since = *self.alone_since.get_or_insert(now);
                if now.saturating_duration_since(since) >= ALONE_GRACE {
                    return Some(Failure::WebViewGone);
                }
            }
            _ => {}
        }
        if !ready && now.saturating_duration_since(self.started) >= READY_TIMEOUT {
            return Some(Failure::NotReady);
        }
        None
    }
}

/// Decides, across the UI runs of one core, whether a failed window is
/// started again without asking.
#[derive(Debug, Default)]
pub struct Retries {
    last_automatic: Option<Instant>,
}

impl Retries {
    /// Takes one failure at `now`. Returns `true`, and counts it as an
    /// automatic retry, when no automatic retry was made within
    /// [`AUTO_RETRY_INTERVAL`]; otherwise the user must be asked.
    pub fn automatic(&mut self, now: Instant) -> bool {
        if self
            .last_automatic
            .is_some_and(|last| now.saturating_duration_since(last) < AUTO_RETRY_INTERVAL)
        {
            return false;
        }
        self.last_automatic = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, seconds: f64) -> Instant {
        start + Duration::from_secs_f64(seconds)
    }

    /// GIVEN a page that never sends a request WHEN the timeout passes THEN
    /// the window counts as failed, and not before.
    #[test]
    fn given_no_request_then_the_window_fails_only_after_the_timeout() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert_eq!(watch.observe(at(start, 29.5), false, Some(5)), None);
        assert_eq!(
            watch.observe(at(start, 30.0), false, Some(5)),
            Some(Failure::NotReady)
        );
    }

    /// GIVEN a page that has reached the core WHEN time passes with the web
    /// view running THEN nothing fails.
    #[test]
    fn given_a_ready_page_with_its_web_view_then_nothing_fails() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        for second in 0..600 {
            assert_eq!(watch.observe(at(start, second as f64), true, Some(6)), None);
        }
    }

    /// GIVEN the web view's processes appeared WHEN the UI is left alone for
    /// the grace period THEN the web view counts as gone, even after the page
    /// was ready.
    #[test]
    fn given_the_web_view_exits_and_the_ui_stays_then_it_fails_after_the_grace() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert_eq!(watch.observe(at(start, 1.0), true, Some(6)), None);
        assert_eq!(watch.observe(at(start, 10.0), true, Some(1)), None);
        assert_eq!(watch.observe(at(start, 12.5), true, Some(1)), None);
        assert_eq!(
            watch.observe(at(start, 13.0), true, Some(1)),
            Some(Failure::WebViewGone)
        );
    }

    /// GIVEN the UI alone only briefly (a web view restarting a process, or a
    /// normal close) THEN nothing fails, and the grace starts again.
    #[test]
    fn given_the_ui_alone_briefly_then_nothing_fails() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert_eq!(watch.observe(at(start, 1.0), true, Some(6)), None);
        assert_eq!(watch.observe(at(start, 2.0), true, Some(1)), None);
        assert_eq!(watch.observe(at(start, 4.0), true, Some(4)), None);
        assert_eq!(watch.observe(at(start, 5.0), true, Some(1)), None);
        assert_eq!(watch.observe(at(start, 7.5), true, Some(1)), None);
    }

    /// GIVEN the UI alone before any web view process appeared (still
    /// starting) THEN only the readiness timeout applies.
    #[test]
    fn given_the_ui_alone_while_starting_then_only_the_timeout_applies() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert_eq!(watch.observe(at(start, 20.0), false, Some(1)), None);
        assert_eq!(
            watch.observe(at(start, 30.0), false, Some(1)),
            Some(Failure::NotReady)
        );
    }

    /// GIVEN the Job cannot be read THEN the process rule is skipped and the
    /// readiness timeout still applies.
    #[test]
    fn given_an_unreadable_job_then_only_readiness_is_judged() {
        let start = Instant::now();
        let mut watch = Watch::new(start);
        assert_eq!(watch.observe(at(start, 1.0), true, Some(6)), None);
        assert_eq!(watch.observe(at(start, 60.0), true, None), None);
        let mut waiting = Watch::new(start);
        assert_eq!(
            waiting.observe(at(start, 31.0), false, None),
            Some(Failure::NotReady)
        );
    }

    /// GIVEN no earlier automatic retry WHEN a window fails THEN it is
    /// started again without asking.
    #[test]
    fn given_a_first_failure_then_the_window_is_retried_without_asking() {
        let start = Instant::now();
        assert!(Retries::default().automatic(at(start, 5.0)));
    }

    /// GIVEN an automatic retry WHEN its replacement fails within the
    /// interval THEN the user is asked, every time, until the interval has
    /// passed since that retry; then one more failure is retried without
    /// asking.
    #[test]
    fn given_a_recent_automatic_retry_then_the_next_failures_ask_until_the_interval_passes() {
        let start = Instant::now();
        let interval = AUTO_RETRY_INTERVAL.as_secs_f64();
        let mut retries = Retries::default();
        assert!(retries.automatic(start));
        assert!(!retries.automatic(at(start, 15.0)));
        assert!(!retries.automatic(at(start, interval - 0.5)));
        assert!(retries.automatic(at(start, interval)));
        assert!(!retries.automatic(at(start, interval + 1.0)));
    }
}
