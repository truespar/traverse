//! Thread-local session metadata for the executor.
//!
//! ISO/IEC 39075 section 20.3 defines `SESSION_USER`, the identity of the user who
//! opened the session. The executor has no notion of a user; the HTTP surface
//! (or another front end) supplies it.
//!
//! Rather than thread a session argument through every call site, the front end
//! sets a thread-local before running the executor and clears it after. Outside
//! that scope `current_user()` is `None` and `SESSION_USER` evaluates to `NULL`
//! (not an error), so unit tests and driverless integrations work without a
//! session.

use std::cell::RefCell;

use crate::ast::PathMode;

thread_local! {
    static SESSION_USER: RefCell<Option<String>> = const { RefCell::new(None) };
    static PATH_MODE: RefCell<PathMode> = const { RefCell::new(PathMode::Trail) };
}

/// Sets this thread's ISO section 14.5 path-matching mode; the guard restores the previous one on drop.
/// The matcher reads it via [`current_path_mode`] for variable-length paths and applies the
/// matching edge / node uniqueness rules (see `find_variable_length_paths`).
pub fn set_path_mode(mode: PathMode) -> PathModeGuard {
    let previous = PATH_MODE.with(|cell| cell.replace(mode));
    PathModeGuard { previous }
}

/// Current path-matching mode; [`PathMode::Trail`] (the openCypher / ISO default) when no guard is
/// active.
pub fn current_path_mode() -> PathMode {
    PATH_MODE.with(|cell| *cell.borrow())
}

/// Guard from [`set_path_mode`]; restores the prior mode on drop, including during unwinding.
#[must_use = "the path mode is only set for the lifetime of the guard"]
pub struct PathModeGuard {
    previous: PathMode,
}

impl Drop for PathModeGuard {
    fn drop(&mut self) {
        let prev = self.previous;
        PATH_MODE.with(|cell| *cell.borrow_mut() = prev);
    }
}

/// Sets this thread's current user; the guard restores the previous value on drop.
pub fn set_session_user(user: impl Into<String>) -> SessionUserGuard {
    let previous = SESSION_USER.with(|cell| cell.replace(Some(user.into())));
    SessionUserGuard { previous }
}

/// Current user, for `SESSION_USER`.
pub fn current_user() -> Option<String> {
    SESSION_USER.with(|cell| cell.borrow().clone())
}

/// Guard from [`set_session_user`]; restores the prior user on drop, including during unwinding.
#[must_use = "the session user is only set for the lifetime of the guard"]
pub struct SessionUserGuard {
    previous: Option<String>,
}

impl Drop for SessionUserGuard {
    fn drop(&mut self) {
        let prev = self.previous.take();
        SESSION_USER.with(|cell| *cell.borrow_mut() = prev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_starts_empty() {
        assert!(current_user().is_none());
    }

    #[test]
    fn guard_sets_and_restores() {
        assert!(current_user().is_none());
        {
            let _g = set_session_user("alice");
            assert_eq!(current_user().as_deref(), Some("alice"));
        }
        assert!(current_user().is_none());
    }

    #[test]
    fn nested_guards_restore_in_order() {
        let _outer = set_session_user("alice");
        {
            let _inner = set_session_user("bob");
            assert_eq!(current_user().as_deref(), Some("bob"));
        }
        assert_eq!(current_user().as_deref(), Some("alice"));
    }
}
