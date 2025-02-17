//! This crate provides a simple implementation of a ratelimiter that can be
//! shared between threads.
//!
//! ```
//! use ratelimit::Ratelimiter;
//! use std::time::Duration;
//!
//! // Constructs a ratelimiter that generates 1 tokens/s with no burst. This
//! // can be used to produce a steady rate of requests. The ratelimiter starts
//! // with no tokens available, which means across application restarts, we
//! // cannot exceed the configured ratelimit.
//! let ratelimiter = Ratelimiter::builder(1, Duration::from_secs(1))
//!     .build()
//!     .unwrap();
//!
//! // Another use case might be admission control, where we start with some
//! // initial budget and replenish it periodically. In this example, our
//! // ratelimiter allows 1000 tokens/hour. For every hour long sliding window,
//! // no more than 1000 tokens can be acquired. But all tokens can be used in
//! // a single burst. Additional calls to `try_wait()` will return an error
//! // until the next token addition.
//! //
//! // This is popular approach with public API ratelimits.
//! let ratelimiter = Ratelimiter::builder(1000, Duration::from_secs(3600))
//!     .max_tokens(1000)
//!     .initial_available(1000)
//!     .build()
//!     .unwrap();
//!
//! // For very high rates, we should avoid using too short of an interval due
//! // to limits of system clock resolution. Instead, it's better to allow some
//! // burst and add multiple tokens per interval. The resulting ratelimiter
//! // here generates 50 million tokens/s and allows no more than 50 tokens to
//! // be acquired in any 1 microsecond long window.
//! let ratelimiter = Ratelimiter::builder(50, Duration::from_micros(1))
//!     .max_tokens(50)
//!     .build()
//!     .unwrap();
//!
//! // constructs a ratelimiter that generates 100 tokens/s with no burst
//! let ratelimiter = Ratelimiter::builder(1, Duration::from_millis(10))
//!     .build()
//!     .unwrap();
//!
//! for _ in 0..10 {
//!     // a simple sleep-wait
//!     if let Err(sleep) = ratelimiter.try_wait() {
//!            std::thread::sleep(sleep);
//!            continue;
//!     }
//!     
//!     // do some ratelimited action here    
//! }
//! ```

use clocksource::precise::{AtomicInstant, Duration, Instant};
use core::sync::atomic::{AtomicU64, Ordering};
use parking_lot::RwLock;
use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum Error {
    #[error("available tokens cannot be set higher than max tokens")]
    AvailableTokensTooHigh,
    #[error("max tokens cannot be less than the refill amount")]
    MaxTokensTooLow,
    #[error("refill amount cannot exceed the max tokens")]
    RefillAmountTooHigh,
    #[error("refill interval in nanoseconds exceeds maximum u64")]
    RefillIntervalTooLong,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct Parameters {
    capacity: u64,
    refill_amount: u64,
    refill_interval: Duration,
}

pub struct Ratelimiter {
    available: AtomicU64,
    dropped: AtomicU64,
    parameters: RwLock<Parameters>,
    refill_at: AtomicInstant,
}

impl Ratelimiter {
    /// Initialize a builder that will construct a `Ratelimiter` that adds the
    /// specified `amount` of tokens to the token bucket after each `interval`
    /// has elapsed.
    ///
    /// Note: In practice, the system clock resolution imposes a lower bound on
    /// the `interval`. To be safe, it is recommended to set the interval to be
    /// no less than 1 microsecond. This also means that the number of tokens
    /// per interval should be > 1 to achieve rates beyond 1 million tokens/s.
    pub fn builder(amount: u64, interval: core::time::Duration) -> Builder {
        Builder::new(amount, interval)
    }

    /// Return the current effective rate of the Ratelimiter in tokens/second
    pub fn rate(&self) -> f64 {
        let parameters = self.parameters.read();

        parameters.refill_amount as f64 * 1_000_000_000.0
            / parameters.refill_interval.as_nanos() as f64
    }

    /// Return the current interval between refills.
    pub fn refill_interval(&self) -> core::time::Duration {
        let parameters = self.parameters.read();

        core::time::Duration::from_nanos(parameters.refill_interval.as_nanos())
    }

    /// Allows for changing the interval between refills at runtime.
    pub fn set_refill_interval(&self, duration: core::time::Duration) -> Result<(), Error> {
        if duration.as_nanos() > u64::MAX as u128 {
            return Err(Error::RefillIntervalTooLong);
        }

        let mut parameters = self.parameters.write();

        parameters.refill_interval = Duration::from_nanos(duration.as_nanos() as u64);
        Ok(())
    }

    /// Return the current number of tokens to be added on each refill.
    pub fn refill_amount(&self) -> u64 {
        let parameters = self.parameters.read();

        parameters.refill_amount
    }

    /// Allows for changing the number of tokens to be added on each refill.
    pub fn set_refill_amount(&self, amount: u64) -> Result<(), Error> {
        let mut parameters = self.parameters.write();

        if amount > parameters.capacity {
            Err(Error::RefillAmountTooHigh)
        } else {
            parameters.refill_amount = amount;
            Ok(())
        }
    }

    /// Returns the maximum number of tokens that can
    pub fn max_tokens(&self) -> u64 {
        let parameters = self.parameters.read();

        parameters.capacity
    }

    /// Allows for changing the maximum number of tokens that can be held by the
    /// ratelimiter for immediate use. This effectively sets the burst size. The
    /// configured value must be greater than or equal to the refill amount.
    pub fn set_max_tokens(&self, amount: u64) -> Result<(), Error> {
        let mut parameters = self.parameters.write();

        if amount < parameters.refill_amount {
            Err(Error::MaxTokensTooLow)
        } else {
            parameters.capacity = amount;
            loop {
                let available = self.available();
                if amount > available {
                    if self
                        .available
                        .compare_exchange(available, amount, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                    {
                        break;
                    }
                } else {
                    break;
                }
            }
            Ok(())
        }
    }

    /// Returns the number of tokens currently available.
    pub fn available(&self) -> u64 {
        self.available.load(Ordering::Relaxed)
    }

    /// Returns the time of the next refill.
    pub fn next_refill(&self) -> Instant {
        self.refill_at.load(Ordering::Relaxed)
    }

    /// Sets the number of tokens available to some amount. Returns an error if
    /// the amount exceeds the bucket capacity.
    pub fn set_available(&self, amount: u64) -> Result<(), Error> {
        let parameters = self.parameters.read();
        if amount > parameters.capacity {
            Err(Error::AvailableTokensTooHigh)
        } else {
            self.available.store(amount, Ordering::Release);
            Ok(())
        }
    }

    /// Returns the number of tokens that have been dropped due to bucket
    /// overflowing.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Internal function to refill the token bucket. Called as part of
    /// `try_wait()`
    fn refill(&self, time: Instant) -> Result<(), core::time::Duration> {
        // will hold the number of elapsed refill intervals
        let mut intervals;
        // will hold a read lock for the refill parameters
        let mut parameters;

        loop {
            // determine when next refill should occur
            let refill_at = self.refill_at.load(Ordering::Relaxed);

            // if this time is before the next refill is due, return
            if time < refill_at {
                return Err(core::time::Duration::from_nanos(
                    (refill_at - time).as_nanos(),
                ));
            }

            // acquire read lock for refill parameters
            parameters = self.parameters.read();

            intervals = (time - refill_at).as_nanos() / parameters.refill_interval.as_nanos() + 1;

            // calculate when the following refill would be
            let next_refill =
                refill_at + Duration::from_nanos(intervals * parameters.refill_interval.as_nanos());

            // compare/exchange, if race, loop and check if we still need to
            // refill before trying again
            if self
                .refill_at
                .compare_exchange(refill_at, next_refill, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
        }

        // figure out how many tokens we might add
        let amount = intervals * parameters.refill_amount;

        let available = self.available.load(Ordering::Acquire);

        if available + amount >= parameters.capacity {
            // we will fill the bucket up to the capacity
            let to_add = parameters.capacity - available;
            self.available.fetch_add(to_add, Ordering::Release);

            // and increment the number of tokens dropped
            self.dropped.fetch_add(amount - to_add, Ordering::Relaxed);
        } else {
            self.available.fetch_add(amount, Ordering::Release);
        }

        Ok(())
    }

    pub fn return_n(&self, n: u64) {
        self.available
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |a| {
                Some(std::cmp::min(a + n, self.max_tokens()))
            })
            .unwrap();
    }

    /// Non-blocking function to "wait" for a single token. On success, a single
    /// token has been acquired. On failure, a `Duration` hinting at when the
    /// next refill would occur is returned.
    pub fn try_wait_n(&self, n: u64) -> Result<(), core::time::Duration> {
        // We have an outer loop that drives the refilling of the token bucket.
        // This will only be repeated if we refill successfully, but somebody
        // else takes the newly available token(s) before we can attempt to
        // acquire one.
        loop {
            // Attempt to refill the bucket. This makes sure we are moving the
            // time forward, issuing new tokens, hitting our max capacity, etc.
            let refill_result = self.refill(Instant::now());

            // Note: right now it doesn't matter if refill succeeded or failed.
            // We might already have tokens available. Even if refill failed we
            // check if there are tokens and attempt to acquire one.

            // Our inner loop deals with acquiring a token. It will only repeat
            // if there is a race on the available tokens. This can occur
            // between:
            // - the refill in the outer loop and the load in the inner loop
            // - the load and the compare exchange, both in the inner loop
            //
            // Both these cases mean that somebody has taken a token we had
            // hoped to acquire. However, the handling of these cases differs.
            loop {
                // load the count of available tokens
                let available = self.available.load(Ordering::Acquire);

                // Two cases if there are no available tokens, we have:
                // - Failed to refill and the bucket was empty. This means we
                //   should early return with an error that provides the caller
                //   with the duration until next refill.
                // - Succeeded to refill but there are now no tokens. This is
                //   only hit if somebody else took the token between refill and
                //   load. In this case, we break the inner loop and repeat from
                //   the top of the outer loop.
                //
                // Note: this is when it matters if the refill was successful.
                // We use the success or failure to determine if there was a
                // race.
                if available == 0 {
                    match refill_result {
                        Ok(_) => {
                            // This means we raced. Refill succeeded but another
                            // caller has taken the token. We break the inner
                            // loop and try to refill again.
                            break;
                        }
                        Err(e) => {
                            // Refill failed and there were no tokens already
                            // available. We return the error which contains a
                            // duration until the next refill.
                            return Err(e * (n/self.refill_amount()) as u32);
                        }
                    }
                }

                // If we made it here, available is > 0 and so we can attempt to
                // acquire a token by doing a simple compare exchange on
                // available with the new value.
                match available.overflowing_sub(n) {
                    (new, false) => {
                        if self
                            .available
                            .compare_exchange(available, new, Ordering::AcqRel, Ordering::Acquire)
                            .is_ok()
                        {
                            // We have acquired a token and can return successfully
                            return Ok(());
                        }
                    }
                    (new, true) => {
                        let short = u64::MAX - new;
                        return Err(self.refill_interval() * (short/self.refill_amount()) as u32);
                    }
                }


                // If we raced on the compare exchange, we need to repeat the
                // token acquisition. Either there will be another token we can
                // try to acquire, or we will break and attempt a refill again.
            }
        }
    }

    pub fn try_wait(&self) -> Result<(), core::time::Duration> {
        self.try_wait_n(1)
    }
}

pub struct Builder {
    initial_available: u64,
    max_tokens: u64,
    refill_amount: u64,
    refill_interval: core::time::Duration,
}

impl Builder {
    /// Initialize a new builder that will add `amount` tokens after each
    /// `interval` has elapsed.
    fn new(amount: u64, interval: core::time::Duration) -> Self {
        Self {
            // default of zero tokens initially
            initial_available: 0,
            // default of one to prohibit bursts
            max_tokens: 1,
            refill_amount: amount,
            refill_interval: interval,
        }
    }

    /// Set the max tokens that can be held in the the `Ratelimiter` at any
    /// time. This limits the size of any bursts by placing an upper bound on
    /// the number of tokens available for immediate use.
    ///
    /// By default, the max_tokens will be set to one unless the refill amount
    /// requires a higher value.
    ///
    /// The selected value cannot be lower than the refill amount.
    pub fn max_tokens(mut self, tokens: u64) -> Self {
        self.max_tokens = tokens;
        self
    }

    /// Set the number of tokens that are initially available. For admission
    /// control scenarios, you may wish for there to be some tokens initially
    /// available to avoid delays or discards until the ratelimit is hit. When
    /// using it to enforce a ratelimit on your own process, for example when
    /// generating outbound requests, you may want there to be zero tokens
    /// availble initially to make your application more well-behaved in event
    /// of process restarts.
    ///
    /// The default is that no tokens are initially available.
    pub fn initial_available(mut self, tokens: u64) -> Self {
        self.initial_available = tokens;
        self
    }

    /// Consumes this `Builder` and attempts to construct a `Ratelimiter`.
    pub fn build(self) -> Result<Ratelimiter, Error> {
        if self.max_tokens < self.refill_amount {
            return Err(Error::MaxTokensTooLow);
        }

        if self.refill_interval.as_nanos() > u64::MAX as u128 {
            return Err(Error::RefillIntervalTooLong);
        }

        let available = AtomicU64::new(self.initial_available);

        let parameters = Parameters {
            capacity: self.max_tokens,
            refill_amount: self.refill_amount,
            refill_interval: Duration::from_nanos(self.refill_interval.as_nanos() as u64),
        };

        let refill_at = AtomicInstant::new(Instant::now() + self.refill_interval);

        Ok(Ratelimiter {
            available,
            dropped: AtomicU64::new(0),
            parameters: parameters.into(),
            refill_at,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::*;
    use std::time::{Duration, Instant};

    macro_rules! approx_eq {
        ($value:expr, $target:expr) => {
            let value: f64 = $value;
            let target: f64 = $target;
            assert!(value >= target * 0.999, "{value} >= {}", target * 0.999);
            assert!(value <= target * 1.001, "{value} <= {}", target * 1.001);
        };
    }

    // test that the configured rate and calculated effective rate are close
    #[test]
    pub fn rate() {
        // amount + interval
        let rl = Ratelimiter::builder(4, Duration::from_nanos(333))
            .max_tokens(4)
            .build()
            .unwrap();

        approx_eq!(rl.rate(), 12012012.0);
    }

    // quick test that a ratelimiter yields tokens at the desired rate
    #[test]
    pub fn wait() {
        let rl = Ratelimiter::builder(1, Duration::from_micros(10))
            .build()
            .unwrap();

        let mut count = 0;

        let now = Instant::now();
        let end = now + Duration::from_millis(10);
        while Instant::now() < end {
            if rl.try_wait().is_ok() {
                count += 1;
            }
        }

        assert!(count >= 600);
        assert!(count <= 1400);
    }

    // quick test that a ratelimiter yields n tokens at the desired rate
    #[test]
    pub fn wait_n() {
        let rl = Ratelimiter::builder(1, Duration::from_micros(10))
            .max_tokens(3)
            .build()
            .unwrap();

        let mut count = 0;
        assert!((Duration::from_micros(10)..Duration::from_micros(30))
            .contains(&rl.try_wait_n(3).unwrap_err()));

        let now = Instant::now();
        let end = now + Duration::from_millis(10);
        while Instant::now() < end {
            if rl.try_wait_n(3).is_ok() {
                count += 1;
            }
        }

        assert!(count >= 200);
        assert!(count <= 460);
    }

    // quick test that a ratelimiter accepts n returned tokens
    #[test]
    pub fn return_n() {
        let rl = Ratelimiter::builder(1, Duration::from_micros(10))
            .max_tokens(3)
            .build()
            .unwrap();

        assert!((Duration::from_micros(10)..Duration::from_micros(30))
            .contains(&rl.try_wait_n(3).unwrap_err()));
        rl.return_n(3);
        assert!(&rl.try_wait_n(3).is_ok());
    }

    // quick test that an idle ratelimiter doesn't build up excess capacity
    #[test]
    pub fn idle() {
        let rl = Ratelimiter::builder(1, Duration::from_millis(1))
            .initial_available(1)
            .build()
            .unwrap();

        std::thread::sleep(Duration::from_millis(10));
        assert!(rl.next_refill() < clocksource::precise::Instant::now());

        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_err());
        assert!(rl.dropped() >= 8);
        assert!(rl.next_refill() >= clocksource::precise::Instant::now());

        std::thread::sleep(Duration::from_millis(5));
        assert!(rl.next_refill() < clocksource::precise::Instant::now());
    }

    // quick test that capacity acts as expected
    #[test]
    pub fn capacity() {
        let rl = Ratelimiter::builder(1, Duration::from_millis(10))
            .max_tokens(10)
            .initial_available(0)
            .build()
            .unwrap();

        std::thread::sleep(Duration::from_millis(100));
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_ok());
        assert!(rl.try_wait().is_err());
    }
}
