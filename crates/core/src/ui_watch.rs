//! Decides when the UI's window has failed, so the core can offer to start
//! it again (owner decision, 21 October 2026).
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
//! The decision is pure, so it is tested without a window.

use std::time::{Duration, Instant};

/// How long the page has to send its first request.
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the UI may run without any web view process before that counts
/// as a failure.
pub const ALONE_GRACE: Duration = Duration::from_secs(3);

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
}
