//! Stopping a case that is never going to finish.
//!
//! A conformance suite contains programs that loop forever on purpose and programs that loop
//! forever because the engine running them got something wrong, and a runner cannot tell them apart
//! in advance. Without a way out, one bad case takes the whole fifty thousand case run with it, and
//! the answer arrives as a hung terminal rather than as a number.
//!
//! The mechanism is the interrupt flag from spec 5.6, which the interpreter checks on every loop
//! back edge. That is the whole guarantee and it is worth being precise about what it does not
//! cover: a program stuck in straight line code has no back edge to check at, so it is not
//! stoppable. Straight line code is finite by construction, so this is a bound on how long a case
//! runs rather than a hole, but the bound is the program's length rather than the timeout.
//!
//! One thread watches all of them rather than one thread per case. Fifty thousand cases means fifty
//! thousand threads, and the operating system charges for each one whether it ever fires or not.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use katsu_runtime::Interrupt;

/// How often the watcher looks. Coarse on purpose, since the timeout it enforces is in seconds and
/// a thread that wakes a hundred times a second to find nothing to do is a thread competing with
/// the workers for a core.
const TICK: Duration = Duration::from_millis(100);

/// The registry of what is running, shared with the watching thread.
type Running = Arc<Mutex<HashMap<u64, (Interrupt, Instant)>>>;

/// A thread that interrupts anything that has been running too long.
#[derive(Debug)]
pub(crate) struct Watchdog {
    running: Running,
    next: AtomicU64,
    stopping: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    limit: Duration,
}

/// One case's entry in the registry, which removes itself when it goes out of scope.
///
/// A guard rather than a pair of calls, because the case it is watching can return through an error
/// or a caught panic as easily as through the bottom of the function, and an entry left behind
/// would have the watchdog interrupting whatever runs next on that thread.
#[derive(Debug)]
pub(crate) struct Ticket<'a> {
    running: &'a Running,
    id: u64,
}

impl Watchdog {
    /// Start watching, with the limit every case gets.
    #[must_use]
    pub(crate) fn new(limit: Duration) -> Watchdog {
        let running: Running = Arc::new(Mutex::new(HashMap::new()));
        let stopping = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let running = Arc::clone(&running);
            let stopping = Arc::clone(&stopping);
            move || {
                while !stopping.load(Ordering::Acquire) {
                    std::thread::sleep(TICK);
                    let now = Instant::now();
                    // The lock is held for the length of a walk over at most one entry per worker
                    // thread, which is single digits, and it is never held while anything runs.
                    let Ok(entries) = running.lock() else {
                        // A worker panicked while holding the lock, which cannot happen because
                        // nothing runs under it, but a poisoned lock is not worth taking the
                        // watchdog down over. Stop watching and let the timeouts stop being
                        // enforced rather than abort a run that is otherwise fine.
                        return;
                    };
                    for (interrupt, deadline) in entries.values() {
                        if now >= *deadline {
                            // Idempotent, so asking twice on consecutive ticks costs nothing.
                            interrupt.request();
                        }
                    }
                }
            }
        });
        Watchdog {
            running,
            next: AtomicU64::new(0),
            stopping,
            thread: Some(thread),
            limit,
        }
    }

    /// Watch one interpreter until the returned ticket is dropped.
    #[must_use]
    pub(crate) fn watch(&self, interrupt: Interrupt) -> Ticket<'_> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + self.limit;
        if let Ok(mut entries) = self.running.lock() {
            entries.insert(id, (interrupt, deadline));
        }
        Ticket {
            running: &self.running,
            id,
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            // Joining rather than detaching, so that a run cannot finish printing its report while
            // a thread is still holding a handle to an interpreter.
            drop(thread.join());
        }
    }
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.running.lock() {
            entries.remove(&self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use katsu_runtime::Interrupt;

    use super::Watchdog;

    #[test]
    fn something_that_runs_too_long_is_asked_to_stop() {
        let watchdog = Watchdog::new(Duration::from_millis(50));
        let interrupt = Interrupt::default();
        let ticket = watchdog.watch(interrupt.clone());
        assert!(!interrupt.requested(), "asked to stop before the deadline");
        std::thread::sleep(Duration::from_millis(400));
        assert!(interrupt.requested(), "never asked to stop");
        drop(ticket);
    }

    #[test]
    fn a_case_that_finishes_in_time_is_left_alone_and_so_is_the_next_one() {
        // The second half is the one worth having. An entry left behind after a case returns would
        // interrupt whatever ran next on that thread, which shows up as a random scattering of
        // timeouts that move between runs.
        let watchdog = Watchdog::new(Duration::from_millis(200));
        let first = Interrupt::default();
        drop(watchdog.watch(first.clone()));
        std::thread::sleep(Duration::from_millis(400));
        assert!(!first.requested(), "an entry outlived its case");
    }
}
