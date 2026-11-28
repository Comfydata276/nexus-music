//! The points between the durable steps of every write the backend makes, for experiment
//! X2's tests: a test sets a hook, and at each point the hook may stop the program there
//! (as a kill would) or have that step fail as a full disk would. **Nothing but a test sets
//! a hook**; without one each point does nothing.
//!
//! Every write of the data and of the store kept apart passes through these, so a test can
//! find them all by counting rather than by being told where they are.

use std::io;
use std::sync::OnceLock;

/// Called at each point with its name; `true` makes the step there fail with "no space".
pub type Hook = Box<dyn Fn(&'static str) -> bool + Send + Sync>;

static HOOK: OnceLock<Hook> = OnceLock::new();

/// Sets the hook, once for the program. Whether it was set by this call.
pub fn set(hook: Hook) -> bool {
    HOOK.set(hook).is_ok()
}

/// A point named `name`.
pub fn point(name: &'static str) -> io::Result<()> {
    match HOOK.get() {
        Some(hook) if hook(name) => Err(io::Error::new(io::ErrorKind::StorageFull, "no space left (a test's)")),
        _ => Ok(()),
    }
}
