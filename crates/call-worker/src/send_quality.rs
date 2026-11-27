//! The Auto controller of the own camera's send tier (ADR-0070, proposed).
//! Pure: no I/O, no clock reads. The caller gives it the time and one sample
//! of the link about every 2 seconds, and it says when the tier changes.
//! One controller serves one camera run, so the lowest round-trip time that
//! it keeps is the lowest of that run.
//!
//! Rules:
//! - Up one tier after [`UP_SAMPLES`] good samples in a row, when `hold` has
//!   passed since the last change. A good sample has an available bandwidth
//!   of at least the current tier's bitrate and a round-trip time that is
//!   low ([`RTT_GOOD_MAX`]) and near the lowest of the run.
//! - Down one tier after [`DOWN_SAMPLES`] bad samples in a row, at any time.
//!   A bad sample has an available bandwidth under 0.8 of the current tier's
//!   bitrate, or a round-trip time above [`RTT_BAD_MIN`].
//! - `hold` starts at [`HOLD_START`]. A step down within [`FAILED_WITHIN`] of
//!   a step up doubles it (up to [`HOLD_MAX`]). A tier that a step up gave
//!   and that holds [`HOLD_RESET_AFTER`] sets it back. A tier that a step
//!   down gave does not: the wait after a failure must last.
//! - A sample with a missing value is neither good nor bad: it changes no
//!   count. A sample that comes sooner than [`MIN_GAP`] after the last one
//!   that counted is skipped, so a statistics view that reads every second
//!   does not make the controller twice as fast.

use std::time::{Duration, Instant};

use crate::send_tier::SendTier;

/// Good samples in a row to step up (about 6 seconds).
pub const UP_SAMPLES: u32 = 3;
/// Bad samples in a row to step down (about 4 seconds).
pub const DOWN_SAMPLES: u32 = 2;
/// The least time between two samples that count.
pub const MIN_GAP: Duration = Duration::from_millis(1500);
/// A good round-trip time is at most this ...
pub const RTT_GOOD_MAX: Duration = Duration::from_millis(250);
/// ... and at most 1.5 times the lowest of the run, plus [`RTT_SLACK`].
const RTT_SLACK: Duration = Duration::from_millis(50);
/// A round-trip time above this is bad.
pub const RTT_BAD_MIN: Duration = Duration::from_millis(400);
/// A bandwidth under 4/5 of the tier's bitrate is bad.
const LOW_PERCENT: f64 = 0.8;
/// The first time to wait after a change before a step up.
pub const HOLD_START: Duration = Duration::from_secs(10);
/// The longest wait after failed steps up.
pub const HOLD_MAX: Duration = Duration::from_secs(120);
/// A step down this soon after a step up is a failed step up.
pub const FAILED_WITHIN: Duration = Duration::from_secs(15);
/// A tier that a step up gave sets the wait back after it holds this long.
pub const HOLD_RESET_AFTER: Duration = Duration::from_secs(60);
/// The tier that Auto starts at, when the top tier allows it.
pub const START_TIER: SendTier = SendTier::P480;

/// One reading of the link. A value that WebRTC did not give is `None`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sample {
    /// The estimate of the bandwidth for sending, in bit/s.
    pub bits_per_second: Option<f64>,
    pub rtt_seconds: Option<f64>,
}

/// What one sample says about the link at the current tier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    Good,
    Bad,
    /// In between, or a value is missing.
    Neutral,
}

#[derive(Debug)]
pub struct AutoQuality {
    top: SendTier,
    tier: SendTier,
    /// The lowest round-trip time of this camera run.
    lowest_rtt: Option<Duration>,
    good_in_a_row: u32,
    bad_in_a_row: u32,
    hold: Duration,
    /// When the tier last changed, or the run began.
    changed_at: Instant,
    /// Whether the last change was a step up.
    stepped_up: bool,
    last_counted: Option<Instant>,
}

impl AutoQuality {
    /// A controller for a camera that starts at `now`. It starts at
    /// [`START_TIER`], or at `top` when `top` is smaller.
    pub fn new(top: SendTier, now: Instant) -> Self {
        Self {
            top,
            tier: START_TIER.min(top),
            lowest_rtt: None,
            good_in_a_row: 0,
            bad_in_a_row: 0,
            hold: HOLD_START,
            changed_at: now,
            stepped_up: false,
            last_counted: None,
        }
    }

    /// The tier now.
    pub fn tier(&self) -> SendTier {
        self.tier
    }

    /// Lowers the top tier to `cap` at `now`: the camera's first picture
    /// came after the open's wait, and it is smaller than the top
    /// (ADR-0070). A tier above the new top drops to it at once. Returns the
    /// new tier when it changes.
    pub fn cap_top(&mut self, now: Instant, cap: SendTier) -> Option<SendTier> {
        self.top = self.top.min(cap);
        if self.tier <= self.top {
            return None;
        }
        self.moved(now, self.top, false);
        Some(self.top)
    }

    /// Takes one sample taken at `now`. Returns the new tier when it
    /// changes.
    pub fn sample(&mut self, now: Instant, sample: Sample) -> Option<SendTier> {
        if self
            .last_counted
            .is_some_and(|last| now.saturating_duration_since(last) < MIN_GAP)
        {
            return None;
        }
        self.last_counted = Some(now);
        let rtt = sample
            .rtt_seconds
            .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
        if let Some(rtt) = rtt {
            self.lowest_rtt = Some(self.lowest_rtt.map_or(rtt, |lowest| lowest.min(rtt)));
        }
        if self.stepped_up && now.saturating_duration_since(self.changed_at) >= HOLD_RESET_AFTER {
            self.hold = HOLD_START;
        }
        let bits = sample
            .bits_per_second
            .filter(|bits| bits.is_finite() && *bits >= 0.0);
        match self.judge(bits, rtt) {
            Verdict::Good => {
                self.good_in_a_row = self.good_in_a_row.saturating_add(1);
                self.bad_in_a_row = 0;
            }
            Verdict::Bad => {
                self.bad_in_a_row = self.bad_in_a_row.saturating_add(1);
                self.good_in_a_row = 0;
            }
            Verdict::Neutral => {
                // A missing value changes no count; a value in the middle
                // ends both runs.
                if bits.is_some() && rtt.is_some() {
                    self.good_in_a_row = 0;
                    self.bad_in_a_row = 0;
                }
            }
        }
        // A sample with a missing value changes no count and no tier. Counts
        // from earlier samples must not step up on it.
        if bits.is_none() || rtt.is_none() {
            return None;
        }
        if self.bad_in_a_row >= DOWN_SAMPLES {
            return self.step_down(now);
        }
        if self.good_in_a_row >= UP_SAMPLES
            && now.saturating_duration_since(self.changed_at) >= self.hold
        {
            return self.step_up(now);
        }
        None
    }

    fn judge(&self, bits: Option<f64>, rtt: Option<Duration>) -> Verdict {
        let (Some(bits), Some(rtt)) = (bits, rtt) else {
            return Verdict::Neutral;
        };
        let bitrate = self.tier.bitrate() as f64;
        if bits < bitrate * LOW_PERCENT || rtt > RTT_BAD_MIN {
            return Verdict::Bad;
        }
        let lowest = self.lowest_rtt.unwrap_or(rtt);
        let near_lowest = lowest * 3 / 2 + RTT_SLACK;
        if bits >= bitrate && rtt <= RTT_GOOD_MAX && rtt <= near_lowest {
            Verdict::Good
        } else {
            Verdict::Neutral
        }
    }

    fn step_up(&mut self, now: Instant) -> Option<SendTier> {
        let next = self.tier.up().filter(|next| *next <= self.top)?;
        self.moved(now, next, true);
        Some(next)
    }

    fn step_down(&mut self, now: Instant) -> Option<SendTier> {
        let next = self.tier.down()?;
        if self.stepped_up && now.saturating_duration_since(self.changed_at) <= FAILED_WITHIN {
            self.hold = (self.hold * 2).min(HOLD_MAX);
        }
        self.moved(now, next, false);
        Some(next)
    }

    fn moved(&mut self, now: Instant, tier: SendTier, up: bool) {
        self.tier = tier;
        self.changed_at = now;
        self.stepped_up = up;
        self.good_in_a_row = 0;
        self.bad_in_a_row = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The time between samples that the caller aims for.
    const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
    const GOOD_RTT: f64 = 0.04;

    fn sample(bits: f64, rtt: f64) -> Sample {
        Sample {
            bits_per_second: Some(bits),
            rtt_seconds: Some(rtt),
        }
    }

    /// A link with plenty of bandwidth.
    fn rich() -> Sample {
        sample(50_000_000.0, GOOD_RTT)
    }

    /// A link that is too poor for P720 and above.
    fn poor() -> Sample {
        sample(300_000.0, GOOD_RTT)
    }

    /// Feeds `count` samples 2 seconds apart from `*now`, which moves on.
    /// Returns the tiers that came out.
    fn feed(
        quality: &mut AutoQuality,
        now: &mut Instant,
        count: u32,
        what: Sample,
    ) -> Vec<SendTier> {
        let mut out = Vec::new();
        for _ in 0..count {
            *now += SAMPLE_INTERVAL;
            out.extend(quality.sample(*now, what));
        }
        out
    }

    /// Feeds good samples until the tier steps up, then 2 poor ones, which
    /// fail the step at once.
    fn fail_a_step_up(quality: &mut AutoQuality, now: &mut Instant) {
        while feed(quality, now, 1, rich()).is_empty() {}
        assert_eq!(feed(quality, now, 2, poor()).len(), 1);
    }

    /// GIVEN a good link WHEN samples keep coming THEN the tier climbs one
    /// step at a time to the top and stays there.
    #[test]
    fn given_a_good_link_then_the_tier_climbs_to_the_top() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        assert_eq!(quality.tier(), SendTier::P480);
        let steps = feed(&mut quality, &mut now, 100, rich());
        assert_eq!(steps, [SendTier::P720, SendTier::P1080, SendTier::P1440]);
        assert_eq!(quality.tier(), SendTier::P1440);
    }

    /// GIVEN a good link WHEN the first samples come THEN the first step up
    /// waits for the hold of 10 seconds, though 3 good samples came sooner.
    #[test]
    fn given_a_new_run_then_the_first_step_up_waits_for_the_hold() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        // Samples at 2, 4, 6 and 8 seconds.
        assert!(feed(&mut quality, &mut now, 4, rich()).is_empty());
        // The 5th sample is at 10 seconds.
        assert_eq!(feed(&mut quality, &mut now, 1, rich()), [SendTier::P720]);
    }

    /// GIVEN a top tier below the start WHEN the controller starts THEN it
    /// starts at the top and never goes above it.
    #[test]
    fn given_a_low_top_then_the_tier_never_passes_it() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P360, start);
        assert_eq!(quality.tier(), SendTier::P360);
        assert!(feed(&mut quality, &mut now, 50, rich()).is_empty());
        let mut quality = AutoQuality::new(SendTier::P720, start);
        feed(&mut quality, &mut now, 100, rich());
        assert_eq!(quality.tier(), SendTier::P720);
    }

    /// GIVEN a run whose tier climbed to 1080p WHEN the top is capped at
    /// 720p (a late first picture) THEN the tier drops to 720p at once and
    /// never climbs past it. GIVEN a tier below the cap THEN nothing changes
    /// at the cap, and the tier climbs only to it.
    #[test]
    fn given_a_cap_on_the_top_then_the_tier_follows_it() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        for _ in 0..100 {
            if quality.tier() == SendTier::P1080 {
                break;
            }
            feed(&mut quality, &mut now, 1, rich());
        }
        assert_eq!(quality.tier(), SendTier::P1080);
        assert_eq!(quality.cap_top(now, SendTier::P720), Some(SendTier::P720));
        assert_eq!(quality.tier(), SendTier::P720);
        assert!(feed(&mut quality, &mut now, 100, rich()).is_empty());
        assert_eq!(quality.tier(), SendTier::P720);

        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        assert_eq!(quality.cap_top(start, SendTier::P720), None);
        assert_eq!(quality.tier(), SendTier::P480);
        assert_eq!(feed(&mut quality, &mut now, 100, rich()), [SendTier::P720]);
    }

    /// GIVEN a link with too little bandwidth WHEN 2 samples come THEN the
    /// tier drops one step at once, and it never goes below the floor.
    #[test]
    fn given_low_bandwidth_then_the_tier_drops_to_the_floor() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        assert!(feed(&mut quality, &mut now, 1, poor()).is_empty());
        assert_eq!(feed(&mut quality, &mut now, 1, poor()), [SendTier::P360]);
        assert!(feed(&mut quality, &mut now, 20, poor()).is_empty());
        assert_eq!(quality.tier(), SendTier::P360);
    }

    /// GIVEN a link with a high round-trip time WHEN 2 samples come THEN the
    /// tier drops, even with plenty of bandwidth.
    #[test]
    fn given_a_high_round_trip_time_then_the_tier_drops() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        let slow = sample(50_000_000.0, 0.45);
        assert!(feed(&mut quality, &mut now, 1, slow).is_empty());
        assert_eq!(feed(&mut quality, &mut now, 1, slow), [SendTier::P360]);
    }

    /// GIVEN a round-trip time far over the lowest of the run WHEN samples
    /// come THEN the tier does not climb, and does not drop while the time
    /// stays under 400 ms.
    #[test]
    fn given_a_round_trip_time_far_over_the_lowest_then_the_tier_holds() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        assert!(feed(&mut quality, &mut now, 1, sample(50_000_000.0, 0.02)).is_empty());
        // 120 ms is over 1.5 * 20 ms + 50 ms = 80 ms.
        assert!(feed(&mut quality, &mut now, 50, sample(50_000_000.0, 0.12)).is_empty());
        assert_eq!(quality.tier(), SendTier::P480);
    }

    /// GIVEN steps up that fail WHEN they repeat THEN the hold doubles each
    /// time up to 120 seconds, and a tier that a step up gave and that holds
    /// 60 seconds sets it back to 10 seconds.
    #[test]
    fn given_failed_steps_up_then_the_hold_doubles_and_later_resets() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        let mut holds = Vec::new();
        for _ in 0..6 {
            fail_a_step_up(&mut quality, &mut now);
            assert_eq!(quality.tier(), SendTier::P480);
            holds.push(quality.hold.as_secs());
        }
        assert_eq!(holds, [20, 40, 80, 120, 120, 120]);
        // The wait is kept for as long as it lasts: no step up before it.
        let down_at = now;
        while feed(&mut quality, &mut now, 1, rich()).is_empty() {}
        assert!(now - down_at >= Duration::from_secs(120));
        // Hold the new tier for 60 seconds: the wait is 10 seconds again.
        feed(&mut quality, &mut now, 31, sample(1_400_000.0, GOOD_RTT));
        assert_eq!(quality.hold, HOLD_START);
    }

    /// GIVEN a step down that comes long after a step up WHEN it happens
    /// THEN it is no failure and the hold stays.
    #[test]
    fn given_a_late_step_down_then_the_hold_stays() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        feed(&mut quality, &mut now, 5, rich());
        assert_eq!(quality.tier(), SendTier::P720);
        // 20 seconds at P720 on a link that is enough, but not good.
        feed(&mut quality, &mut now, 10, sample(1_300_000.0, GOOD_RTT));
        assert_eq!(feed(&mut quality, &mut now, 2, poor()), [SendTier::P480]);
        assert_eq!(quality.hold, HOLD_START);
    }

    /// GIVEN samples with a missing value WHEN they come THEN no count
    /// changes: they make no step and break no run of good samples.
    #[test]
    fn given_missing_values_then_nothing_changes() {
        let start = Instant::now();
        let none = Sample {
            bits_per_second: None,
            rtt_seconds: None,
        };
        let no_rtt = Sample {
            bits_per_second: Some(1.0),
            rtt_seconds: None,
        };
        let no_bits = Sample {
            bits_per_second: None,
            rtt_seconds: Some(5.0),
        };
        // Numbers that are not numbers count as missing.
        let not_numbers = Sample {
            bits_per_second: Some(f64::NAN),
            rtt_seconds: Some(f64::INFINITY),
        };
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        for missing in [none, no_rtt, no_bits, not_numbers] {
            assert!(feed(&mut quality, &mut now, 30, missing).is_empty());
        }
        assert_eq!(quality.tier(), SendTier::P480);
        // Missing values between good ones do not end the run.
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        feed(&mut quality, &mut now, 2, rich());
        feed(&mut quality, &mut now, 3, none);
        assert_eq!(feed(&mut quality, &mut now, 1, rich()), [SendTier::P720]);
    }

    /// GIVEN 3 good samples before the hold ends WHEN incomplete samples come
    /// after the hold ended THEN the tier does not step up, and the first
    /// complete good sample after the hold steps up.
    #[test]
    fn given_incomplete_samples_after_the_hold_then_no_step_up_until_a_complete_one() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        let none = Sample {
            bits_per_second: None,
            rtt_seconds: None,
        };
        let no_rtt = Sample {
            bits_per_second: Some(50_000_000.0),
            rtt_seconds: None,
        };
        // Good samples at 2, 4 and 6 seconds: the run is long enough, the
        // hold of 10 seconds is not over.
        assert!(feed(&mut quality, &mut now, 3, rich()).is_empty());
        // Incomplete samples at 8 and 10 seconds: the hold ends at 10.
        assert!(feed(&mut quality, &mut now, 1, none).is_empty());
        assert!(feed(&mut quality, &mut now, 1, no_rtt).is_empty());
        assert_eq!(quality.tier(), SendTier::P480);
        // A complete good sample at 12 seconds steps up.
        assert_eq!(feed(&mut quality, &mut now, 1, rich()), [SendTier::P720]);
    }

    /// GIVEN a sample in the middle (not good, not bad) WHEN it comes in a
    /// run of good ones THEN the run starts again.
    #[test]
    fn given_a_middle_sample_then_the_run_starts_again() {
        let start = Instant::now();
        let mut now = start;
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        feed(&mut quality, &mut now, 5, rich());
        assert_eq!(quality.tier(), SendTier::P720);
        // Over 0.8 and under 1.0 of the P720 bitrate.
        let middle = sample(1_300_000.0, GOOD_RTT);
        // The hold of 10 seconds passes during these samples.
        feed(&mut quality, &mut now, 2, rich());
        feed(&mut quality, &mut now, 3, middle);
        assert!(feed(&mut quality, &mut now, 2, rich()).is_empty());
        assert_eq!(feed(&mut quality, &mut now, 1, rich()), [SendTier::P1080]);
    }

    /// GIVEN samples that come faster than every 1.5 seconds WHEN they come
    /// THEN the extra ones are skipped.
    #[test]
    fn given_fast_samples_then_the_extra_ones_are_skipped() {
        let start = Instant::now();
        let mut quality = AutoQuality::new(SendTier::P1440, start);
        let mut out = Vec::new();
        // One a second for 12 seconds: they count at 1, 3, 5, 7, 9 and 11
        // seconds. The hold of 10 seconds passes at the sample of 11 seconds.
        for second in 1..=12 {
            out.extend(quality.sample(start + Duration::from_secs(second), rich()));
        }
        assert_eq!(out, [SendTier::P720]);
    }

    /// GIVEN a sample with a time before the start WHEN it comes THEN
    /// nothing breaks.
    #[test]
    fn given_an_earlier_time_then_nothing_breaks() {
        let start = Instant::now();
        let later = start + Duration::from_secs(100);
        let mut quality = AutoQuality::new(SendTier::P1440, later);
        assert_eq!(quality.sample(start, rich()), None);
    }
}
