//! The event loop and I/O.
//!
//! Node's observable phase ordering is the contract we implement. libuv is not. We run on
//! tokio, thread per core, with epoll and kqueue by default and io_uring behind a feature
//! flag, because Docker 25 blocks io_uring syscalls in its default seccomp profile and
//! software that requires it simply fails to start in a normal container.
//! See `spec/12-concurrency-and-io.md`.

/// One turn of the loop, in the order Node observably runs them.
///
/// A program can tell these apart with `setTimeout`, `setImmediate` and `process.nextTick`,
/// so the ordering is part of the compatibility surface rather than an implementation
/// detail we are free to change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Expired `setTimeout` and `setInterval` callbacks.
    Timers,
    /// Deferred callbacks from the previous iteration.
    PendingCallbacks,
    /// Internal loop bookkeeping. Not observable, listed so the ordering is complete.
    Idle,
    /// Poll for completed I/O. Where the loop blocks when there is nothing else to do.
    Poll,
    /// `setImmediate` callbacks.
    Check,
    /// `close` events.
    Close,
}

impl Phase {
    /// The phases in the order one turn runs them.
    #[must_use]
    pub const fn order() -> [Phase; 6] {
        [
            Phase::Timers,
            Phase::PendingCallbacks,
            Phase::Idle,
            Phase::Poll,
            Phase::Check,
            Phase::Close,
        ]
    }
}

/// Whether this build has the io_uring backend compiled in.
///
/// Even when it is compiled in it is not the default. It is an accelerator that the
/// operator opts into on a host they control, per `spec/12-concurrency-and-io.md`.
#[must_use]
pub const fn uring_compiled_in() -> bool {
    cfg!(feature = "uring")
}

#[cfg(test)]
mod tests {
    use super::Phase;

    #[test]
    fn the_phase_order_is_the_one_node_programs_can_observe() {
        let order = Phase::order();
        assert_eq!(order[0], Phase::Timers);
        assert_eq!(order[3], Phase::Poll);
        assert_eq!(order[4], Phase::Check);
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "phases must be totally ordered"
        );
    }
}
