//! Named race windows: places in the runtime where two things hand off, and a way for a test to
//! hold one of them open.
//!
//! A race between "the answer is final" and "a monitor has not yet reported" is hard to prove by
//! running it many times: it fails one run in two on a loaded host and never on the developer's
//! machine. A *window* is a call to [`window`] (or [`window_blocking`]) placed at the handoff. In
//! a normal build it compiles to nothing. In a test build, or a build with the `race-windows`
//! feature, a test can *arm* the point and the code stops there, so the losing order of events
//! happens every time instead of sometimes. A fix is proven by a test that arms the window it
//! closes: it fails without the fix and passes with it.
//!
//! Two ways to arm a point:
//!
//!   * [`arm_delay`] — the code sleeps at the point for a fixed time. Enough when the window only
//!     needs to be wide.
//!   * [`hold`] — the code blocks at the point until the test calls [`Hold::release`], and the
//!     test can wait with [`Hold::reached`] until the code is actually there. Use this when the
//!     test has to do something *while* the code is at the point.
//!
//! A `race-windows` build of the real binary can also be armed from outside the process with
//! `MURMUR_RACE_WINDOWS=point=millis[,point=millis...]`, which is how an end-to-end run (a real
//! `mur run`) forces a window. That variable is read only by builds that carry the feature.
//!
//! Points are plain `&'static str` names, listed in [`point`]. Arming a point takes a lock for
//! that point, so two tests that arm the same one run one after the other, never together.

/// Every named window, so a test and the code it forces agree on the name.
#[allow(dead_code)]
pub(crate) mod point {
    /// In the agent loop's end: the attempt returned, and the runtime is about to decide the
    /// session's final status. A breach latched after this point must still be seen.
    pub(crate) const ATTEMPT_RETURNED: &str = "attempt.returned";
    /// In a workdir guard's polling thread, before it walks the directories: held there, the poll
    /// never comes round, so the session's status has to be decided by the final measurement.
    pub(crate) const WORKDIR_POLL: &str = "workdir.poll";
    /// In the termination routine, after it cancelled every live task and before it raised
    /// `terminating`: a task that finishes in the gap must still end the session with the
    /// termination's own ending (a deadline is `deadline_reached`, never `ok`).
    pub(crate) const TERMINATION_AFTER_CANCEL: &str = "termination.after_cancel";
    /// In a session process's output pump, after it read its last chunk and before it marks the
    /// stream ended: the exit must not be reported ahead of the output.
    pub(crate) const PUMP_BEFORE_END: &str = "pump.before_end";
}

#[cfg(not(any(test, feature = "race-windows")))]
mod imp {
    /// A named handoff. Nothing in this build.
    #[inline(always)]
    pub(crate) async fn window(_point: &'static str) {}

    /// A named handoff for code that is not async. Nothing in this build.
    #[inline(always)]
    pub(crate) fn window_blocking(_point: &'static str) {}

    /// [`window_blocking`] that only a test armed for `scope` stops. Nothing in this build.
    #[inline(always)]
    pub(crate) fn window_blocking_scoped(_point: &'static str, _scope: &str) {}
}

#[cfg(any(test, feature = "race-windows"))]
mod imp {
    use std::collections::HashMap;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
    use std::time::Duration;

    #[cfg_attr(not(test), allow(dead_code))]
    enum How {
        Delay(Duration),
        Hold(Arc<Gate>),
    }

    #[derive(Default)]
    struct GateState {
        reached: bool,
        released: bool,
    }

    #[derive(Default)]
    struct Gate {
        state: Mutex<GateState>,
        changed: Condvar,
    }

    struct Registry {
        armed: Mutex<HashMap<String, How>>,
        /// One lock per point, held for as long as a test keeps the point armed.
        locks: Mutex<HashMap<String, &'static Mutex<()>>>,
    }

    fn registry() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            #[cfg_attr(not(feature = "race-windows"), allow(unused_mut))]
            let mut armed = HashMap::new();
            // Only the feature build reads the environment: a unit test arms in code.
            #[cfg(feature = "race-windows")]
            if let Ok(spec) = std::env::var("MURMUR_RACE_WINDOWS") {
                for pair in spec.split(',') {
                    if let Some((name, millis)) = pair.split_once('=') {
                        if let (Some(name), Ok(millis)) = (
                            super::KNOWN_POINTS
                                .iter()
                                .find(|known| **known == name.trim()),
                            millis.trim().parse::<u64>(),
                        ) {
                            armed.insert(
                                (*name).to_string(),
                                How::Delay(Duration::from_millis(millis)),
                            );
                        }
                    }
                }
            }
            Registry {
                armed: Mutex::new(armed),
                locks: Mutex::new(HashMap::new()),
            }
        })
    }

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// What the code at a point must do right now: nothing, sleep, or block on a gate.
    enum Action {
        Pass,
        Sleep(Duration),
        Block(Arc<Gate>),
    }

    /// A point armed for `scope` first, then the point armed for any scope.
    fn action_at(point: &'static str, scope: &str) -> Action {
        let armed = lock(&registry().armed);
        let found = armed.get(&key(point, scope)).or_else(|| armed.get(point));
        match found {
            None => Action::Pass,
            Some(How::Delay(delay)) => Action::Sleep(*delay),
            Some(How::Hold(gate)) => {
                let gate = Arc::clone(gate);
                drop(armed);
                let mut state = lock(&gate.state);
                state.reached = true;
                gate.changed.notify_all();
                drop(state);
                Action::Block(gate)
            }
        }
    }

    fn key(point: &str, scope: &str) -> String {
        if scope.is_empty() {
            point.to_string()
        } else {
            format!("{point}@{scope}")
        }
    }

    fn released(gate: &Gate) -> bool {
        lock(&gate.state).released
    }

    pub(crate) async fn window(point: &'static str) {
        window_scoped(point, "").await;
    }

    async fn window_scoped(point: &'static str, scope: &str) {
        match action_at(point, scope) {
            Action::Pass => {}
            Action::Sleep(delay) => tokio::time::sleep(delay).await,
            Action::Block(gate) => {
                while !released(&gate) {
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            }
        }
    }

    pub(crate) fn window_blocking(point: &'static str) {
        window_blocking_scoped(point, "");
    }

    pub(crate) fn window_blocking_scoped(point: &'static str, scope: &str) {
        match action_at(point, scope) {
            Action::Pass => {}
            Action::Sleep(delay) => std::thread::sleep(delay),
            Action::Block(gate) => {
                let mut state = lock(&gate.state);
                while !state.released {
                    state = gate
                        .changed
                        .wait(state)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            }
        }
    }

    /// A point armed by a test. Disarms, and frees anything held at it, on drop.
    pub(crate) struct Armed {
        key: String,
        gate: Option<Arc<Gate>>,
        _exclusive: MutexGuard<'static, ()>,
    }

    impl Drop for Armed {
        fn drop(&mut self) {
            lock(&registry().armed).remove(&self.key);
            if let Some(gate) = &self.gate {
                lock(&gate.state).released = true;
                gate.changed.notify_all();
            }
        }
    }

    fn arm(key: String, how: impl FnOnce() -> (How, Option<Arc<Gate>>)) -> Armed {
        #[allow(clippy::explicit_auto_deref)]
        let exclusive: &'static Mutex<()> = *lock(&registry().locks)
            .entry(key.clone())
            .or_insert_with(|| Box::leak(Box::new(Mutex::new(()))));
        let exclusive = lock(exclusive);
        let (armed, gate) = how();
        lock(&registry().armed).insert(key.clone(), armed);
        Armed {
            key,
            gate,
            _exclusive: exclusive,
        }
    }

    /// Makes the code at `point` sleep for `delay` every time it gets there, until the returned
    /// guard drops.
    #[allow(dead_code)]
    pub(crate) fn arm_delay(point: &'static str, delay: Duration) -> Armed {
        arm(key(point, ""), || (How::Delay(delay), None))
    }

    /// A point the test holds shut.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) struct Hold {
        armed: Armed,
    }

    #[cfg_attr(not(test), allow(dead_code))]
    impl Hold {
        fn gate(&self) -> &Gate {
            self.armed.gate.as_deref().expect("a hold has a gate")
        }

        /// Blocks until some code is at the point, or `timeout` passes. `true` when it is there.
        /// A test that gets `false` has proved nothing, and must fail rather than go on.
        pub(crate) fn reached(&self, timeout: Duration) -> bool {
            let gate = self.gate();
            let deadline = std::time::Instant::now() + timeout;
            let mut state = lock(&gate.state);
            while !state.reached {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if left.is_zero() {
                    return false;
                }
                state = gate
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0;
            }
            true
        }

        /// Lets the code at the point go on.
        pub(crate) fn release(&self) {
            let gate = self.gate();
            lock(&gate.state).released = true;
            gate.changed.notify_all();
        }
    }

    /// Makes the code at `point` stop there until [`Hold::release`] (or the drop of the hold).
    #[allow(dead_code)]
    pub(crate) fn hold(point: &'static str) -> Hold {
        hold_scoped(point, "")
    }

    /// [`hold`] for the code that passes `scope` to [`window_scoped`] or [`window_blocking_scoped`]
    /// only: other code at the same point, in a test running beside this one, goes through.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn hold_scoped(point: &'static str, scope: &str) -> Hold {
        let gate = Arc::new(Gate::default());
        Hold {
            armed: arm(key(point, scope), || {
                (How::Hold(Arc::clone(&gate)), Some(gate))
            }),
        }
    }
}

#[allow(unused_imports)]
#[cfg(any(test, feature = "race-windows"))]
pub(crate) use imp::{arm_delay, hold, hold_scoped};
pub(crate) use imp::{window, window_blocking, window_blocking_scoped};

/// The names the environment variable may arm.
#[cfg(feature = "race-windows")]
const KNOWN_POINTS: &[&str] = &[
    point::ATTEMPT_RETURNED,
    point::WORKDIR_POLL,
    point::PUMP_BEFORE_END,
    point::TERMINATION_AFTER_CANCEL,
];
