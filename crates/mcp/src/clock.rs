//! Time source for polling loops, so tests can run them without sleeping.

use std::time::{Duration, Instant, SystemTime};

pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn wall(&self) -> SystemTime;
    fn sleep(&self, duration: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn wall(&self) -> SystemTime {
        SystemTime::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// Poll `ready` every `interval` until it returns `Some` or `timeout` passes.
pub fn poll<T>(
    clock: &dyn Clock,
    timeout: Duration,
    interval: Duration,
    mut ready: impl FnMut() -> crate::error::Result<Option<T>>,
) -> crate::error::Result<Option<T>> {
    let deadline = clock.now() + timeout;
    loop {
        if let Some(value) = ready()? {
            return Ok(Some(value));
        }
        let now = clock.now();
        if now >= deadline {
            return Ok(None);
        }
        clock.sleep(interval.min(deadline - now));
    }
}

#[cfg(test)]
pub mod fake {
    use super::Clock;
    use std::{
        sync::Mutex,
        time::{Duration, Instant, SystemTime},
    };

    /// A clock whose `sleep` only advances its own time.
    pub struct FakeClock {
        start: Instant,
        elapsed: Mutex<Duration>,
    }

    impl FakeClock {
        pub fn new() -> Self {
            Self {
                start: Instant::now(),
                elapsed: Mutex::new(Duration::ZERO),
            }
        }

        pub fn elapsed(&self) -> Duration {
            *self.elapsed.lock().unwrap()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            self.start + self.elapsed()
        }

        fn wall(&self) -> SystemTime {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000) + self.elapsed()
        }

        fn sleep(&self, duration: Duration) {
            *self.elapsed.lock().unwrap() += duration;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fake::FakeClock, *};

    #[test]
    fn poll_gives_up_at_the_deadline_without_real_sleeps() {
        let clock = FakeClock::new();
        let mut calls = 0;
        let found: Option<()> = poll(
            &clock,
            Duration::from_secs(1),
            Duration::from_millis(300),
            || {
                calls += 1;
                Ok(None)
            },
        )
        .unwrap();
        assert!(found.is_none());
        assert_eq!(clock.elapsed(), Duration::from_secs(1));
        assert_eq!(calls, 5);
    }

    #[test]
    fn poll_returns_the_first_ready_value() {
        let clock = FakeClock::new();
        let mut calls = 0;
        let found = poll(
            &clock,
            Duration::from_secs(10),
            Duration::from_millis(100),
            || {
                calls += 1;
                Ok((calls == 3).then_some(calls))
            },
        )
        .unwrap();
        assert_eq!(found, Some(3));
        assert_eq!(clock.elapsed(), Duration::from_millis(200));
    }
}
