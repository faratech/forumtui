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
    ///
    /// Cancellation-safe (#623): the reservation is made under the lock and
    /// the sleep happens outside it, so a waiter aborted mid-sleep (the
    /// app's `abort_writes` on logout / session recovery) used to leave its
    /// slot burned — `next_free` pushed 30 s out for a request that never
    /// went. The slot is held open by a `Reservation` guard instead: dropped
    /// mid-wait it rolls `next_free` back, but only while it is still the
    /// newest reservation (a later waiter's slot legitimately builds on it).
    pub async fn wait(&self) {
        let reservation = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            let mut target = s.next_free.max(now);
            if let Some(penalty) = s.penalty_until {
                target = target.max(penalty);
            }
            s.next_free = target + self.min;
            Reservation {
                gate: self,
                target,
                armed: true,
            }
        };
        let now = Instant::now();
        if reservation.target > now {
            tokio::time::sleep_until(reservation.target).await;
        }
        reservation.consume();
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
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
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
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
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

/// One waiter's live claim on a slot (#623). Dropping it without
/// `consume()` — i.e. the future was aborted before the wait finished —
/// rolls the slot back when it is still the gate's newest reservation.
struct Reservation<'a> {
    gate: &'a Gate,
    target: Instant,
    armed: bool,
}

impl Reservation<'_> {
    fn consume(mut self) {
        self.armed = false;
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut s) = self.gate.state.lock() {
            let mine = match self.target.checked_add(self.gate.min) {
                Some(end) => end,
                None => return, // cannot have been recorded as next_free
            };
            if s.next_free == mine {
                s.next_free = self.target;
            }
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

    /// A panic while some caller holds the lock poisons it; the gate must
    /// keep serving later waiters from the intact state instead of
    /// cascading the panic into every request that follows (#646).
    #[test]
    fn waiters_survive_a_poisoned_mutex() {
        let gate = Gate::new(250);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = gate.state.lock().unwrap();
            panic!("poison the gate lock");
        }));
        assert!(
            gate.pending_wait() <= Duration::from_millis(250),
            "pending_wait must answer through the poisoned lock"
        );
        gate.penalize(Duration::from_secs(1));
        assert!(gate.pending_wait() >= Duration::from_millis(500));
    }

    /// #623: a waiter aborted mid-sleep frees its slot — an aborted write
    /// used to leave `next_free` pushed a full write cool-down out, so the
    /// next send after recovery waited for a request that never went.
    #[tokio::test(start_paused = true)]
    async fn an_aborted_wait_frees_its_slot() {
        let gate = std::sync::Arc::new(Gate::new(30_000));
        // Consume slot 0 (instant under the paused clock) so the spawned
        // waiter parks sleeping on the T+30s slot; its reservation pushes
        // the gate's next_free to T+60s.
        gate.wait().await;
        let g2 = gate.clone();
        let waiter = tokio::spawn(async move {
            g2.wait().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(gate.pending_wait(), Duration::from_secs(60), "test setup: the slot is held");

        waiter.abort();
        tokio::task::yield_now().await;
        assert_eq!(
            gate.pending_wait(),
            Duration::from_secs(30),
            "the aborted waiter must release its slot"
        );

        // A completed wait keeps its spacing (the rollback must not
        // over-fire): this wait completes at T+30s (the clock advances to
        // it) and re-anchors next_free to T+60s — 30s of spacing from now.
        gate.wait().await;
        assert_eq!(gate.pending_wait(), Duration::from_secs(30));
    }

    /// #623: a queued waiter's slot is built on its predecessor's — an
    /// aborted EARLIER waiter must not steal the live one's spacing.
    #[tokio::test(start_paused = true)]
    async fn an_aborted_wait_does_not_rollback_under_a_later_reservation() {
        let gate = std::sync::Arc::new(Gate::new(1_000));
        // Slot 0, gone; `first` parks at T+1s, then the inline wait at T+2s.
        gate.wait().await;
        let g2 = gate.clone();
        let first = tokio::spawn(async move {
            g2.wait().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(gate.pending_wait(), Duration::from_millis(2000), "test setup");
        // The inline wait parks at T+2s (the clock advances to it) and
        // re-anchors next_free to T+3s — 1s of spacing from now.
        gate.wait().await;
        assert_eq!(gate.pending_wait(), Duration::from_millis(1000));

        first.abort();
        tokio::task::yield_now().await;
        assert_eq!(
            gate.pending_wait(),
            Duration::from_millis(1000),
            "the live inline reservation must keep its spacing"
        );
    }
}
