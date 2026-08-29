//! When this process started, and how long ago that was.
//!
//! One origin per process, stamped once, read by everything that needs to say how much time has
//! passed. `performance.now()` is the caller that made it necessary and `Date.now()` and the event
//! loop's timers will be the next two, so it lives here rather than inside the builtin that wanted
//! it first.
//!
//! # Two clocks, because one clock cannot answer both questions
//!
//! An origin is an [`Instant`] and a [`SystemTime`] taken next to each other, and the pair is not
//! redundant. Elapsed time has to come from the monotonic clock, because the wall clock moves when
//! NTP corrects it or when somebody changes the timezone, and a benchmark that reports a negative
//! duration because a time server nudged the machine mid-run is worse than no benchmark. The wall
//! clock has to be there too, because `performance.timeOrigin` is defined as a real date, the number
//! of milliseconds since the Unix epoch at the moment the runtime began, and a monotonic clock has
//! no epoch to be a number of milliseconds since. Its zero is whenever the operating system felt
//! like starting to count, which on Linux is boot and on macOS is not documented to be anything.
//!
//! So the monotonic clock answers "how long ago" and the wall clock answers "when", they are read
//! within nanoseconds of each other so that `timeOrigin + now()` lands on the current date, and
//! neither is asked the other's question.
//!
//! # Why it is stamped from `main` rather than lazily
//!
//! The origin is process start, and process start is a moment nobody can go back and measure. The
//! closest anybody can get is to read the clock as the first thing the program does, which is what
//! [`start`] is for and why the command line binary calls it before it parses its arguments.
//!
//! It is also fine if nothing calls it. The first read stamps the origin if no one else has, so an
//! embedder who never heard of this function gets an origin at the moment their first piece of
//! JavaScript asked what time it was, rather than a panic or a zero. That is the right answer for an
//! embedder anyway: in a host process that has been up for six hours before it decides to run a
//! script, the host's process start is not the runtime's beginning and reporting it would be
//! misleading in a way that a lazy stamp is not.
//!
//! # What this does not attempt
//!
//! The real process start, from `/proc/self/stat` on Linux or `KERN_PROCARGS` on macOS. Those are
//! genuinely earlier, because they are before the dynamic linker and before Rust's runtime setup,
//! and they are per platform code answering a question nobody has yet asked us. The gap between them
//! and the first line of `main` is microseconds of a number that is otherwise measured against
//! itself, and the moment somebody needs it the shape here does not change, only where the origin
//! comes from.
//!
//! Coarsening. Browsers round `performance.now()` to five microseconds or worse, because a
//! high resolution timer in a page shared with an attacker is half of a Spectre gadget. Node does
//! not, and gives the full resolution of the platform clock, which was measured at about four
//! hundred nanoseconds between consecutive calls on the machine this was written on. We match Node,
//! because a server runtime's threat model is not a browser tab's and because a benchmark harness
//! that cannot see a microsecond is not much of a benchmark harness.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The moment this process's runtime began, on both clocks.
struct Origin {
    /// The monotonic reading, which is what elapsed time is measured against.
    monotonic: Instant,
    /// Milliseconds since the Unix epoch at the same moment, which is what `timeOrigin` reports.
    epoch_ms: f64,
}

/// Stamped by whoever gets here first, and never again.
static ORIGIN: OnceLock<Origin> = OnceLock::new();

/// Read both clocks now and use that as the origin, if nothing has set one yet.
///
/// Idempotent, and deliberately so. Calling it twice is not an error and the second call does
/// nothing, because the alternative is an origin that moves and a `performance.now()` that goes
/// backwards for anybody who was already holding an earlier reading.
///
/// Call it as early as possible. The command line binary calls it as the first statement in `main`,
/// before argument parsing and before the logger is built, because everything after that point is
/// time this process spent starting up and a runtime that excludes its own startup from its own
/// clock is reporting a flattering number rather than a true one.
pub fn start() {
    let _ = origin();
}

/// Milliseconds since the origin, fractional, monotonically non decreasing.
///
/// This is `performance.now()`. Two calls can return the same number, on a platform whose clock is
/// coarser than the time between them, and the second can never return less than the first.
#[must_use]
pub fn now_ms() -> f64 {
    origin().monotonic.elapsed().as_secs_f64() * 1000.0
}

/// Milliseconds since the Unix epoch at the moment the origin was stamped, fractional.
///
/// This is `performance.timeOrigin`, and it is a constant for the life of the process. Adding
/// [`now_ms`] to it gives the current date, which is the relationship the two are specified to have
/// and the one a program combining them relies on.
#[must_use]
pub fn origin_ms() -> f64 {
    origin().epoch_ms
}

/// The origin, stamping it from both clocks if this is the first anyone has asked.
fn origin() -> &'static Origin {
    ORIGIN.get_or_init(|| {
        let monotonic = Instant::now();
        // Before the epoch means the system clock is set to some time in the 1960s, which is a
        // machine with a dead battery rather than a case worth propagating an error for. Zero is
        // what `Date.now()` would report on such a machine anyway, and the monotonic reading next to
        // it is unaffected, so elapsed time stays correct even there.
        let epoch_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |since| since.as_secs_f64() * 1000.0);
        Origin {
            monotonic,
            epoch_ms,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::{now_ms, origin_ms, start};

    #[test]
    fn time_moves_forward_and_never_backward() {
        let readings: Vec<f64> = (0..64).map(|_| now_ms()).collect();
        for pair in readings.windows(2) {
            assert!(
                pair[1] >= pair[0],
                "the clock went backwards, from {} to {}",
                pair[0],
                pair[1]
            );
        }
        // Sixty four reads of a clock take longer than nothing on any machine that can run this, so
        // a run where the last equals the first means the clock is not being read at all.
        assert!(
            readings[readings.len() - 1] > readings[0],
            "sixty four reads produced no elapsed time at all, so the clock is stuck at {}",
            readings[0]
        );
    }

    #[test]
    fn elapsed_time_is_reported_in_milliseconds() {
        // The unit is the entire interface, and getting it wrong by a factor of a thousand is the
        // kind of mistake that survives review because every number still looks like a number.
        let before = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let slept = now_ms() - before;
        assert!(
            (20.0..500.0).contains(&slept),
            "a 20 ms sleep measured as {slept} ms, which is the wrong unit or the wrong clock"
        );
    }

    #[test]
    fn the_origin_is_a_real_date_and_adding_now_to_it_gives_the_current_one() {
        let now_from_the_wall_clock = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the system clock should be after 1970")
            .as_secs_f64()
            * 1000.0;
        let now_from_the_origin = origin_ms() + now_ms();
        // A second of slack, which is far more than the two readings can drift apart and far less
        // than any plausible unit or epoch mistake. Getting the epoch wrong puts these decades
        // apart, and getting the unit wrong puts them a factor of a thousand apart.
        assert!(
            (now_from_the_origin - now_from_the_wall_clock).abs() < 1000.0,
            "timeOrigin plus now is {now_from_the_origin}, and the wall clock says \
             {now_from_the_wall_clock}"
        );
    }

    #[test]
    fn stamping_the_origin_twice_does_not_move_it() {
        // The property that makes `start` safe to call from anywhere. If a second call reset the
        // origin, every reading taken before it would suddenly be in the future.
        start();
        let first = origin_ms();
        let elapsed = now_ms();
        start();
        assert!(
            (origin_ms() - first).abs() < f64::EPSILON,
            "the origin moved from {first} to {}",
            origin_ms()
        );
        assert!(
            now_ms() >= elapsed,
            "the clock went backwards across a second call to start"
        );
    }
}
