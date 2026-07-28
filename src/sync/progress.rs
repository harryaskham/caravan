//! Bounded, human-facing sync progress reporting.
//!
//! Sync is a long, mostly-network operation. Without progress the CLI looks
//! hung for a minute or more while it discovers, rewrites, converges, and
//! revalidates. This channel is deliberately observational: it carries one
//! bounded line per stage boundary, never influences policy, and is dropped
//! entirely unless a caller installs an observer, so JSON, MCP, and hook
//! surfaces keep their exact byte-for-byte contracts.

use std::cell::RefCell;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One bounded stage boundary inside a sync tick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncProgressEvent {
    /// Stable stage identity, aligned with operation-lock checkpoint phases.
    pub phase: String,
    /// Bounded human detail; never provider output, secrets, or raw logs.
    pub detail: String,
}

const MAX_DETAIL_BYTES: usize = 300;

type Observer = Box<dyn Fn(&SyncProgressEvent)>;

thread_local! {
    static OBSERVER: RefCell<Option<Observer>> = const { RefCell::new(None) };
}

/// Run `operation` with `observer` receiving bounded stage events.
///
/// The observer is thread-local and restored afterwards, so nested calls and
/// concurrent operations on other threads are unaffected.
pub fn observing<T>(
    observer: impl Fn(&SyncProgressEvent) + 'static,
    operation: impl FnOnce() -> T,
) -> T {
    let previous = OBSERVER.with(|slot| slot.replace(Some(Box::new(observer))));
    let result = operation();
    OBSERVER.with(|slot| {
        *slot.borrow_mut() = previous;
    });
    result
}

/// Emit one bounded stage event when an observer is installed.
pub(crate) fn emit(phase: &str, detail: impl Into<String>) {
    OBSERVER.with(|slot| {
        let borrowed = slot.borrow();
        let Some(observer) = borrowed.as_ref() else {
            return;
        };
        let mut detail = detail.into();
        if detail.len() > MAX_DETAIL_BYTES {
            let mut end = MAX_DETAIL_BYTES;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
            detail.push('…');
        }
        observer(&SyncProgressEvent {
            phase: phase.to_owned(),
            detail,
        });
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn events_are_dropped_without_an_observer() {
        // No panic, no allocation contract, and no output for JSON/MCP callers.
        emit("provider_convergence", "ignored");
    }

    #[test]
    fn observer_sees_ordered_bounded_events_and_is_restored() {
        let outer = Rc::new(RefCell::new(Vec::new()));
        let inner = Rc::new(RefCell::new(Vec::new()));
        let outer_sink = Rc::clone(&outer);
        let inner_sink = Rc::clone(&inner);

        observing(
            move |event| outer_sink.borrow_mut().push(event.clone()),
            || {
                emit("initial_discovery", "reading provider graph");
                observing(
                    move |event| inner_sink.borrow_mut().push(event.clone()),
                    || emit("nested", "inner only"),
                );
                emit("provider_convergence", "x".repeat(1_000));
            },
        );

        let outer = outer.borrow();
        assert_eq!(outer.len(), 2, "nested observers never leak upward");
        assert_eq!(outer[0].phase, "initial_discovery");
        assert_eq!(outer[0].detail, "reading provider graph");
        assert_eq!(outer[1].phase, "provider_convergence");
        assert!(
            outer[1].detail.len() <= MAX_DETAIL_BYTES + 4,
            "detail stays bounded for a terminal"
        );
        assert!(outer[1].detail.ends_with('…'));
        assert_eq!(inner.borrow().len(), 1);

        // The observer is unregistered again after the scope ends.
        emit("after", "dropped");
        assert_eq!(outer.len(), 2);
    }
}
