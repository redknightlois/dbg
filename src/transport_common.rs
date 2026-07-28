//! Helpers shared by the DAP and Inspector transports. Both have a
//! near-identical "fire a resume/step, wait for the next stop or
//! end-of-session" ritual — extracted here so the two implementations
//! can't drift (e.g. "DAP checks terminated, Inspector doesn't" — a
//! drift bug caught by audit).

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

/// Minimal trait each transport's `State` struct exposes so the shared
/// wait loop can tell whether a new stop arrived, whether the session
/// is still alive, and whether the adapter terminated. DAP has a real
/// `terminated` event; Inspector collapses that into `alive`.
pub trait StopState {
    fn clear_pending(&mut self);
    fn has_pending_hit(&self) -> bool;
    fn alive(&self) -> bool;
    fn terminated(&self) -> bool {
        false
    }
    fn stop_generation(&self) -> u64 {
        0
    }
}

/// Clear the pending-hit flag, fire `action`, then block the current
/// thread until the state's condvar reports a new pending hit, the
/// session dies, the adapter terminates, or `timeout` elapses.
///
/// Returns an empty string on success — transports historically kept
/// the DAP/Inspector "exec returned a string" signature even though
/// the payload is never meaningful; the caller shows a `[via <tool>]`
/// header plus any subsequent stack/locals output.
pub fn wait_for_stop<S, F>(
    state: &Arc<(Mutex<S>, Condvar)>,
    action: F,
    timeout: Duration,
) -> Result<String>
where
    S: StopState,
    F: FnOnce() -> Result<()>,
{
    let baseline = {
        let (lock, _) = &**state;
        let mut guard = lock.lock().unwrap();
        // A stop which arrived after a previous timeout is still useful
        // to the next action. Consume it before issuing another resume.
        if guard.has_pending_hit() {
            return Ok(String::new());
        }
        guard.clear_pending();
        guard.stop_generation()
    };
    action()?;
    let deadline = Instant::now() + timeout;
    let (lock, cvar) = &**state;
    let mut guard = lock.lock().unwrap();
    while guard.alive()
        && (!guard.has_pending_hit() || guard.stop_generation() <= baseline)
        && !guard.terminated()
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timeout waiting for stopped event");
        }
        let r = cvar.wait_timeout(guard, remaining).unwrap();
        guard = r.0;
    }
    Ok(String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;

    struct TestState {
        pending: bool,
        alive: bool,
        generation: u64,
    }

    impl StopState for TestState {
        fn clear_pending(&mut self) {
            self.pending = false;
        }
        fn has_pending_hit(&self) -> bool {
            self.pending
        }
        fn alive(&self) -> bool {
            self.alive
        }
        fn stop_generation(&self) -> u64 {
            self.generation
        }
    }

    #[test]
    fn a_stop_that_arrives_after_timeout_is_kept_for_the_next_action() {
        let state = Arc::new((
            Mutex::new(TestState {
                pending: false,
                alive: true,
                generation: 0,
            }),
            Condvar::new(),
        ));
        let late_state = state.clone();
        let late = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            let (lock, cvar) = &*late_state;
            let mut guard = lock.lock().unwrap();
            guard.pending = true;
            guard.generation = 1;
            cvar.notify_all();
        });

        assert!(wait_for_stop(&state, || Ok(()), Duration::from_millis(5)).is_err());
        late.join().unwrap();

        let action_called = AtomicBool::new(false);
        wait_for_stop(
            &state,
            || {
                action_called.store(true, Ordering::Relaxed);
                Ok(())
            },
            Duration::from_millis(5),
        )
        .unwrap();
        assert!(!action_called.load(Ordering::Relaxed));
    }
}
