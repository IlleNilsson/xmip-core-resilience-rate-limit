#![forbid(unsafe_code)]

//! The rate limit guard — a technology of `xmip-core-resilience` (ADR-0048).
//!
//! A token bucket: one permit is added every interval, up to a burst, and an
//! attempt takes one. When none is left the attempt waits for the next
//! permit rather than being refused — a rate limit shapes the flow, it never
//! turns anything away. The wait spends the permit that will exist when it
//! ends, so a queue of waiting attempts is spaced one interval apart. The
//! bucket lives behind a lock, so one limit may stand in front of many
//! callers, and is refilled by the monotonic reading of the Clock it is
//! handed (`xcore::Clock::monotonic_nanos`; ADR-0068, amendment
//! 2026-11-15), so a test steps its clock to the next permit.

use std::sync::{Arc, Mutex};
use std::time::Duration;
use xcore::Clock;
use xcore::clock::{between, nanos};
use xcore::lock::Locked;

use resilience::{Attempt, Decision, Guard};

#[derive(Debug)]
struct Bucket {
    tokens: u32,
    /// When the bucket was last brought up to date, on the clock's monotonic
    /// reading; the permits earned since are not yet in `tokens`.
    refilled_at: i128,
}

/// The rate limit guard.
pub struct RateLimit {
    interval: Duration,
    burst: u32,
    clock: Arc<dyn Clock>,
    bucket: Mutex<Bucket>,
}

impl RateLimit {
    /// One permit every `permits_per` counted on `clock`, up to `burst` held
    /// at once. A burst of zero is taken as one; an interval of zero never
    /// waits.
    #[must_use]
    pub fn new(permits_per: Duration, burst: u32, clock: Arc<dyn Clock>) -> Self {
        let burst = burst.max(1);
        let refilled_at = clock.monotonic_nanos();
        Self {
            interval: permits_per,
            burst,
            clock,
            bucket: Mutex::new(Bucket {
                tokens: burst,
                refilled_at,
            }),
        }
    }

    /// The permits held right now, after the ones earned since the last
    /// attempt are counted in.
    #[must_use]
    pub fn available(&self) -> u32 {
        let mut bucket = self.lock();
        let now = self.clock.monotonic_nanos();
        self.refill(&mut bucket, now);
        bucket.tokens
    }

    fn refill(&self, bucket: &mut Bucket, now: i128) {
        if self.interval.is_zero() {
            bucket.tokens = self.burst;
            bucket.refilled_at = now;
            return;
        }
        let earned = between(bucket.refilled_at, now).as_nanos() / self.interval.as_nanos();
        if earned == 0 {
            return;
        }
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        bucket.tokens = bucket.tokens.saturating_add(earned).min(self.burst);
        bucket.refilled_at = if bucket.tokens == self.burst {
            now
        } else {
            let refilled = nanos(self.interval.saturating_mul(earned));
            bucket.refilled_at.saturating_add(refilled)
        };
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.bucket.locked()
    }
}

impl Guard for RateLimit {
    fn technology(&self) -> &'static str {
        "rate-limit"
    }

    fn before(&self, _: u32) -> Decision {
        let mut bucket = self.lock();
        let now = self.clock.monotonic_nanos();
        self.refill(&mut bucket, now);
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            return Decision::Proceed;
        }
        // The next permit lands one interval after the last refill; this
        // attempt takes it, so the refill clock moves on by one interval.
        let ready_at = bucket.refilled_at.saturating_add(nanos(self.interval));
        bucket.refilled_at = ready_at;
        Decision::Wait(between(now, ready_at))
    }

    fn after(&self, _: &Attempt) -> Decision {
        Decision::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resilience::{Guarded, execute};
    use std::assert_matches;
    use std::cell::Cell;
    use std::time::Instant;
    use xcore::clock::Stepped;
    use xcore::{Failure, SystemClock};

    /// The span a permit is added in, for the limits here on a stepped clock.
    const SECOND: Duration = Duration::from_secs(1);

    /// A limit of one permit a [`SECOND`] up to `burst`, refilled by the
    /// clock beside it.
    fn stepped(burst: u32) -> (RateLimit, Arc<Stepped>) {
        let clock = Arc::new(Stepped::default());
        let counted = Arc::clone(&clock) as Arc<dyn Clock>;
        (RateLimit::new(SECOND, burst, counted), clock)
    }

    /// A limit of one permit a millisecond, up to one, on the operating
    /// system's clock: what `execute` waits for, it waits out.
    fn waited() -> RateLimit {
        RateLimit::new(Duration::from_millis(1), 1, Arc::new(SystemClock))
    }

    #[test]
    fn a_burst_goes_at_once_and_the_next_attempt_waits_for_its_permit() {
        let (limit, clock) = stepped(2);
        assert_eq!(limit.technology(), "rate-limit");
        assert_eq!(limit.available(), 2);
        assert_eq!(limit.before(1), Decision::Proceed);
        assert_eq!(limit.before(2), Decision::Proceed);
        assert_eq!(limit.available(), 0);
        clock.pass(Duration::from_millis(100));
        assert_eq!(limit.before(3), Decision::Wait(Duration::from_millis(900)));
        assert_eq!(
            limit.before(4),
            Decision::Wait(Duration::from_millis(1_900)),
            "the permit after the one the third waits for"
        );
    }

    #[test]
    fn the_guard_never_refuses_and_the_outcome_always_stands() {
        let (limit, _) = stepped(1);
        for number in 1..=5 {
            assert!(!matches!(limit.before(number), Decision::Refuse(_)));
        }
        let failed = Attempt {
            number: 1,
            elapsed: Duration::ZERO,
            failure: Some(Failure::permanent("broken")),
        };
        assert_eq!(limit.after(&failed), Decision::Proceed);
        let unlimited = RateLimit::new(Duration::ZERO, 0, Arc::new(Stepped::default()));
        assert_eq!(unlimited.before(1), Decision::Proceed);
    }

    #[test]
    fn a_permit_comes_back_each_interval_and_never_past_the_burst() {
        let (limit, clock) = stepped(3);
        assert_eq!(limit.before(1), Decision::Proceed);
        assert_eq!(limit.before(2), Decision::Proceed);
        assert_eq!(limit.before(3), Decision::Proceed);
        assert_eq!(limit.available(), 0);
        clock.pass(SECOND.saturating_sub(Duration::from_nanos(1)));
        assert_eq!(limit.available(), 0, "a nanosecond short");
        clock.pass(Duration::from_nanos(1));
        assert_eq!(limit.available(), 1, "one interval, one permit");
        clock.pass(SECOND * 10);
        assert_eq!(limit.available(), 3, "refilled, and held at the burst");
        assert_eq!(limit.before(4), Decision::Proceed);
    }

    #[test]
    fn under_execute_every_attempt_runs_once_its_permit_is_there() {
        let limit = waited();
        let guards: [&dyn Guard; 1] = [&limit];
        let calls = Cell::new(0);
        let started = Instant::now();
        for _ in 0..3 {
            let outcome = execute(&guards, || {
                calls.set(calls.get() + 1);
                Ok(calls.get())
            });
            assert_matches!(outcome, Ok(Guarded::Done(_)), "{outcome:?}");
        }
        assert_eq!(calls.get(), 3);
        assert!(
            started.elapsed() >= Duration::from_millis(1),
            "the second waited"
        );
    }

    /// Tries again on a retryable failure, as the retry technology does.
    struct Again(u32);

    impl Guard for Again {
        fn technology(&self) -> &'static str {
            "retry"
        }

        fn before(&self, _: u32) -> Decision {
            Decision::Proceed
        }

        fn after(&self, attempt: &Attempt) -> Decision {
            match &attempt.failure {
                Some(failure) if failure.retryable && attempt.number < self.0 => {
                    Decision::Wait(Duration::ZERO)
                }
                _ => Decision::Proceed,
            }
        }
    }

    #[test]
    fn a_rate_limit_ahead_of_retry_spends_a_permit_on_every_attempt() {
        let limit = waited();
        let guards: [&dyn Guard; 2] = [&limit, &Again(3)];
        let calls = Cell::new(0);
        let outcome = execute(&guards, || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(Failure::retryable("again"))
            } else {
                Ok("done")
            }
        });
        assert_eq!(outcome, Ok(Guarded::Done("done")));
        assert_eq!(calls.get(), 3);
        assert!(limit.available() <= 1, "no attempt went without a permit");
    }
}
