//! Client-side throttle gates.
//!
//! The Cloudflare zone is at its rule cap and the origin applies its own flood
//! checks, so the client enforces spacing locally and surfaces the wait in the
//! UI instead of hammering and 429ing. All timing uses `tokio::time::Instant`
//! so tests can run under `tokio::time::pause()`.

use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;

struct State {
    next_free: Instant,
    /// External cool-down floor (Retry-After / server flood hints); a wait may
    /// not start before this instant even if the regular slot is free.
    penalty_until: Option<Instant>,
}

/// A spacing gate: every `wait()` reserves the next slot `min` after the
/// previous one. Not a queue — concurrent callers serialise onto the slots.
pub struct Gate {
    min: Duration,
    state: Mutex<State>,
}

impl Gate {
    pub fn new(min_ms: u64) -> Self {
        Gate {
            min: Duration::from_millis(min_ms),
            state: Mutex::new(State {
                next_free: Instant::now(),
                penalty_until: None,
            }),
        }
    }

    /// Reserve the next slot and sleep until it starts.
    pub async fn wait(&self) {
        let target = {
            let mut s = self.state.lock().unwrap();
            let now = Instant::now();
            let mut target = s.next_free.max(now);
            if let Some(penalty) = s.penalty_until {
                target = target.max(penalty);
            }
            s.next_free = target + self.min;
            target
        };
        let now = Instant::now();
        if target > now {
            tokio::time::sleep_until(target).await;
        }
    }

    /// Impose a cool-down floor starting now (Retry-After, post-write flood
    /// mirror). Repeated calls extend to the max.
    ///
    /// `Instant + Duration` panics on overflow (issue #554: a hostile or
    /// broken origin sending a 19-20 digit `Retry-After` reached this before
    /// `error_from_response` clamped it). `checked_add` makes that
    /// impossible here too, defense in depth — a `dur` too large for the
    /// clock to represent falls back to a generous but safe 24h penalty
    /// rather than ever panicking.
    pub fn penalize(&self, dur: Duration) {
        let mut s = self.state.lock().unwrap();
        let now = Instant::now();
        const FALLBACK: Duration = Duration::from_secs(24 * 3600);
        let until = now
            .checked_add(dur)
            .or_else(|| now.checked_add(FALLBACK))
            .unwrap_or(now);
        s.penalty_until = Some(match s.penalty_until {
            Some(existing) => existing.max(until),
            None => until,
        });
    }

    /// Test/diagnostic hook: current enforced wait for a caller starting now.
    pub fn pending_wait(&self) -> Duration {
        let s = self.state.lock().unwrap();
        let now = Instant::now();
        let mut target = s.next_free.max(now);
        if let Some(penalty) = s.penalty_until {
            target = target.max(penalty);
        }
        if target > now {
            target - now
        } else {
            Duration::ZERO
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn waits_are_sequential_and_spaced() {
        let gate = Gate::new(250);
        tokio::join!(gate.wait(), gate.wait());
        // Two waits consume two slots: the second starts >= min after the first.
        assert!(gate.pending_wait() <= Duration::from_millis(250));
    }

    #[tokio::test(start_paused = true)]
    async fn penalty_extends_the_floor() {
        let gate = Gate::new(1);
        gate.penalize(Duration::from_secs(30));
        assert!(gate.pending_wait() >= Duration::from_secs(29));
        let t0 = Instant::now();
        gate.wait().await;
        assert!(Instant::now() - t0 >= Duration::from_secs(29));
    }

    #[tokio::test(start_paused = true)]
    async fn penalty_takes_max_not_sum() {
        let gate = Gate::new(1);
        gate.penalize(Duration::from_secs(30));
        gate.penalize(Duration::from_secs(10));
        let wait = gate.pending_wait();
        assert!(wait <= Duration::from_secs(31) && wait >= Duration::from_secs(28));
    }

    /// Issue #554: a hostile or broken origin's `Retry-After` could reach
    /// `penalize` unclamped (before `error_from_response` grew its own
    /// clamp) and `Instant::now() + Duration::from_secs(u64::MAX)` panics
    /// ("overflow when adding duration to instant"). `penalize` must survive
    /// the largest possible `Duration` without panicking, on top of
    /// `error_from_response`'s clamp — defense in depth, not either/or.
    #[tokio::test(start_paused = true)]
    async fn penalize_with_a_duration_too_large_for_the_clock_does_not_panic() {
        let gate = Gate::new(1);
        gate.penalize(Duration::from_secs(u64::MAX));
        // Must land on *some* future wait, not panic and not silently do
        // nothing.
        assert!(gate.pending_wait() > Duration::ZERO);
    }
}
