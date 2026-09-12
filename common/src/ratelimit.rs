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

/// A spacing gate: successful dispatches are at least `min` apart.
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

    /// Wait until dispatch is permitted. Waiting does not consume a future
    /// slot, so cancellation cannot leave a hole in the schedule.
    pub async fn wait(&self) {
        Self::wait_all(&[self]).await;
    }

    /// Acquire all applicable lanes together, immediately before dispatch.
    /// Locks have a stable order; no lock or future reservation spans a sleep.
    /// Every wake rechecks server penalties as well as competing dispatches.
    pub async fn wait_all(gates: &[&Gate]) {
        loop {
            Self::wait_until_ready(gates).await;
            if Self::try_acquire_all(gates) {
                return;
            }
        }
    }

    /// Wait without consuming capacity. Callers can then acquire an async
    /// prerequisite (such as the token lock) before trying to dispatch.
    pub async fn wait_until_ready(gates: &[&Gate]) {
        while let Some(target) = Self::schedule(gates, false) {
            tokio::time::sleep_until(target).await;
        }
    }

    /// Atomically acquire every lane if all are ready now. Never blocks on an
    /// async prerequisite or consumes a slot for a request that is not ready.
    pub fn try_acquire_all(gates: &[&Gate]) -> bool {
        Self::schedule(gates, true).is_none()
    }

    fn schedule(gates: &[&Gate], consume: bool) -> Option<Instant> {
        let mut gates = gates.to_vec();
        gates.sort_unstable_by_key(|gate| *gate as *const Gate as usize);
        gates.dedup_by_key(|gate| *gate as *const Gate as usize);
        let mut states: Vec<_> = gates
            .iter()
            .map(|gate| gate.state.lock().unwrap_or_else(|e| e.into_inner()))
            .collect();
        let now = Instant::now();
        let target = states.iter().fold(now, |target, state| {
            target
                .max(state.next_free)
                .max(state.penalty_until.unwrap_or(now))
        });
        if target > now {
            return Some(target);
        }
        if consume {
            for (gate, state) in gates.iter().zip(states.iter_mut()) {
                state.next_free = now + gate.min;
            }
        }
        None
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
        assert_eq!(
            gate.pending_wait(),
            Duration::from_secs(30),
            "waiting consumes no future slot"
        );

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
        assert_eq!(
            gate.pending_wait(),
            Duration::from_millis(1000),
            "waiting consumes no future slot"
        );
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
    #[tokio::test(start_paused = true)]
    async fn queued_waiters_observe_new_penalties_and_keep_spacing() {
        let gate = std::sync::Arc::new(Gate::new(100));
        gate.wait().await;
        let start = Instant::now();
        let mut tasks = Vec::new();
        for _ in 0..3 {
            let gate = gate.clone();
            tasks.push(tokio::spawn(async move {
                gate.wait().await;
                Instant::now()
            }));
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        gate.penalize(Duration::from_millis(500));
        let mut times = Vec::new();
        for task in tasks {
            times.push(task.await.unwrap());
        }
        times.sort();
        assert!(times[0] - start >= Duration::from_millis(520));
        assert!(
            times
                .windows(2)
                .all(|t| t[1] - t[0] >= Duration::from_millis(100))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn combined_lanes_do_not_bunch_after_a_global_penalty() {
        let api = Gate::new(250);
        let search = Gate::new(3000);
        api.penalize(Duration::from_secs(10));
        let start = Instant::now();
        let (a, b) = tokio::join!(
            async {
                Gate::wait_all(&[&api, &search]).await;
                Instant::now()
            },
            async {
                Gate::wait_all(&[&search, &api]).await;
                Instant::now()
            },
        );
        assert!(a.min(b) - start >= Duration::from_secs(10));
        assert!(a.max(b) - a.min(b) >= Duration::from_secs(3));
    }
    #[tokio::test(start_paused = true)]
    async fn waiting_without_dispatch_does_not_burn_a_write_slot() {
        let gate = Gate::new(30_000);
        gate.penalize(Duration::from_secs(10));
        Gate::wait_until_ready(&[&gate]).await;
        assert_eq!(gate.pending_wait(), Duration::ZERO);
        let start = Instant::now();
        gate.wait().await;
        assert_eq!(Instant::now(), start, "refreshing the prerequisite costs no extra cooldown");
    }

}
