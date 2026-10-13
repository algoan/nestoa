//! Warnings reported to the user, recorded so tests can assert on them.

use std::cell::RefCell;
use std::fmt::Display;

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub fn warn(message: impl Display) {
    let message = message.to_string();
    eprintln!("warning: {message}");
    WARNINGS.with_borrow_mut(|w| w.push(message));
}

/// The warnings recorded so far on this thread, clearing them.
#[cfg(test)]
pub fn take_warnings() -> Vec<String> {
    WARNINGS.take()
}
