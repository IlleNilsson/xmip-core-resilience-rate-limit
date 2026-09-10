#![forbid(unsafe_code)]

//! The rate limit guard — a technology of `xmip-core-resilience` (ADR-0048).
//!
//! A token bucket: one permit is added every interval, up to a burst, and an
//! attempt takes one. When none is left the attempt waits for the next
//! permit rather than being refused — a rate limit shapes the flow, it never
//! turns anything away. The wait spends the permit that will exist when it
//! ends, so a queue of waiting attempts is spaced one interval apart. The
//! bucket lives behind a lock, so one limit may stand in front of many
//! callers.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use resilience::{Attempt, Decision, Guard};

#[derive(Debug)]
struct Bucket {
    tokens: u32,
    /// When the bucket was last brought up to date; the permits earned since
    /// are not yet in `tokens`.
    refilled_at: Instant,
}

/// The rate limit guard.
#[derive(Debug)]
pub struct RateLimit {
    interval: Duration,
    burst: u32,
    bucket: Mutex<Bucket>,
}

impl RateLimit {
    /// One permit every `permits_per`, up to `burst` held at once. A burst of
    /// zero is taken as one; an interval of zero never waits.
    #[must_use]
    pub fn new(permits_per: Duration, burst: u32) -> Self {
        let burst = burst.max(1);
        Self {
            interval: permits_per,
            burst,
            bucket: Mutex::new(Bucket {
                tokens: burst,
                refilled_at: Instant::now(),
            }),
        }
    }

    /// The permits held right now, after the ones earned since the last
    /// attempt are counted in.
    #[must_use]
    pub fn available(&self) -> u32 {
        let mut bucket = self.lock();
        self.refill(&mut bucket);
        bucket.tokens
    }

    fn refill(&self, bucket: &mut Bucket) {
        if self.interval.is_zero() {
            bucket.tokens = self.burst;
            bucket.refilled_at = Instant::now();
            return;
        }
        let earned = bucket.refilled_at.elapsed().as_nanos() / self.interval.as_nanos();
        if earned == 0 {
            return;
        }
        let earned = u32::try_from(earned).unwrap_or(u32::MAX);
        bucket.tokens = bucket.tokens.saturating_add(earned).min(self.burst);
        bucket.refilled_at = if bucket.tokens == self.burst {
            Instant::now()
        } else {
            bucket.refilled_at + self.interval.saturating_mul(earned)
        };
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.bucket
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Guard for RateLimit {
    fn technology(&self) -> &'static str {
        "rate-limit"
    }

    fn before(&self, _: u32) -> Decision {
        let mut bucket = self.lock();
        self.refill(&mut bucket);
        if bucket.tokens > 0 {
            bucket.tokens -= 1;
            return Decision::Proceed;
        }
        // The next permit lands one interval after the last refill; this
        // attempt takes it, so the refill clock moves on by one interval.
        let ready_at = bucket.refilled_at + self.interval;
        bucket.refilled_at = ready_at;
        Decision::Wait(ready_at.saturating_duration_since(Instant::now()))
    }

    fn after(&self, _: &Attempt) -> Decision {
        Decision::Proceed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use resilience::{Failure, Guarded, execute};
    use std::cell::Cell;

    #[test]
    fn a_burst_goes_at_once_and_the_next_attempt_waits_for_its_permit() {
        let limit = RateLimit::new(Duration::from_secs(1), 2);
        assert_eq!(limit.technology(), "rate-limit");
        assert_eq!(limit.available(), 2);
        assert_eq!(limit.before(1), Decision::Proceed);
        assert_eq!(limit.before(2), Decision::Proceed);
        assert_eq!(limit.available(), 0);
        match limit.before(3) {
            Decision::Wait(wait) => {
                assert!(wait > Duration::from_millis(900), "{wait:?}");
                assert!(wait <= Duration::from_secs(1), "{wait:?}");
            }
            other => panic!("expected a wait, got {other:?}"),
        }
        match limit.before(4) {
            Decision::Wait(wait) => assert!(wait > Duration::from_secs(1), "{wait:?}"),
            other => panic!("expected a longer wait, got {other:?}"),
        }
    }

    #[test]
    fn the_guard_never_refuses_and_the_outcome_always_stands() {
        let limit = RateLimit::new(Duration::from_secs(1), 1);
        for number in 1..=5 {
            assert!(!matches!(limit.before(number), Decision::Refuse(_)));
        }
        let failed = Attempt {
            number: 1,
            elapsed: Duration::ZERO,
            failure: Some(Failure::permanent("broken")),
        };
        assert_eq!(limit.after(&failed), Decision::Proceed);
        assert_eq!(
            RateLimit::new(Duration::ZERO, 0).before(1),
            Decision::Proceed
        );
    }

    #[test]
    fn permits_come_back_with_time_and_never_past_the_burst() {
        let limit = RateLimit::new(Duration::from_millis(1), 3);
        assert_eq!(limit.before(1), Decision::Proceed);
        assert_eq!(limit.before(2), Decision::Proceed);
        assert_eq!(limit.before(3), Decision::Proceed);
        assert_eq!(limit.available(), 0);
        std::thread::sleep(Duration::from_millis(10));
        assert_eq!(limit.available(), 3, "refilled, and held at the burst");
        assert_eq!(limit.before(4), Decision::Proceed);
    }

    #[test]
    fn under_execute_every_attempt_runs_once_its_permit_is_there() {
        let limit = RateLimit::new(Duration::from_millis(1), 1);
        let guards: [&dyn Guard; 1] = [&limit];
        let calls = Cell::new(0);
        let started = Instant::now();
        for _ in 0..3 {
            let outcome = execute(&guards, || {
                calls.set(calls.get() + 1);
                Ok(calls.get())
            });
            assert!(matches!(outcome, Ok(Guarded::Done(_))), "{outcome:?}");
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
                Some(failure) if failure.is_retryable() && attempt.number < self.0 => {
                    Decision::Wait(Duration::ZERO)
                }
                _ => Decision::Proceed,
            }
        }
    }

    #[test]
    fn a_rate_limit_ahead_of_retry_spends_a_permit_on_every_attempt() {
        let limit = RateLimit::new(Duration::from_millis(1), 1);
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
