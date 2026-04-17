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
    {
        let (lock, _) = &**state;
        lock.lock().unwrap().clear_pending();
    }
    action()?;
    let deadline = Instant::now() + timeout;
    let (lock, cvar) = &**state;
    let mut guard = lock.lock().unwrap();
    while guard.alive() && !guard.has_pending_hit() && !guard.terminated() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("timeout waiting for stopped event");
        }
        let r = cvar.wait_timeout(guard, remaining).unwrap();
        guard = r.0;
    }
    Ok(String::new())
}
