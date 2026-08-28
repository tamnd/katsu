//! Where what a program prints actually goes.
//!
//! `console.log` has to write somewhere, and the somewhere cannot be `println!`. A test wants to
//! read back what a program printed, an embedder wants it in its own log, and the command line
//! wants it on standard output. All three are the same runtime with a different sink in it, so the
//! sink is a value the isolate owns rather than a decision baked into the builtin.
//!
//! # Why there are two streams and not two sinks
//!
//! `console.log` goes to standard output and `console.error` goes to standard error, and a program
//! that redirects one and not the other depends on that. Passing the stream to a single sink rather
//! than holding two of them means a recorder sees both in the order they were written, which is what
//! somebody reading test output wants, while the standard sink still puts each one where it belongs.
//!
//! # What is not here yet
//!
//! A write that fails is dropped. Node turns a closed pipe into an `EPIPE` error and exits, which
//! needs an exit code and a process object to hang it on, and neither exists in M0. Dropping it is
//! written down here rather than discovered by somebody piping katsu into `head`.

use std::fmt;
use std::io::Write;
use std::sync::{Arc, Mutex};

/// Which of the two standard streams a write belongs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output, which is where `console.log` goes.
    Out,
    /// Standard error, which is where `console.error` and `console.warn` go.
    Err,
}

/// Somewhere a program's output can go.
///
/// `Send` because an isolate is `Send`, so everything it owns has to be. Not `Sync`, because an
/// isolate is deliberately not, and requiring it would rule out the obvious implementations.
pub trait Output: fmt::Debug + Send {
    /// Write `text` exactly as given, with no newline added and none removed.
    fn write(&mut self, stream: Stream, text: &str);
}

/// The process's own standard output and standard error.
#[derive(Clone, Copy, Debug, Default)]
pub struct Standard;

impl Output for Standard {
    fn write(&mut self, stream: Stream, text: &str) {
        // The lock is taken and dropped per write rather than held, because a native can call
        // anything and holding a lock across it is how a runtime deadlocks against itself. Rust
        // buffers stdout by line, which is the same buffering a terminal expects.
        let _ = match stream {
            Stream::Out => std::io::stdout().write_all(text.as_bytes()),
            Stream::Err => std::io::stderr().write_all(text.as_bytes()),
        };
    }
}

/// A sink that keeps everything written to it, for tests and for an embedder that wants the text.
///
/// Cloning gives another handle on the same buffer, which is how the caller keeps a way to read what
/// a program printed after handing the sink to the runtime.
#[derive(Clone, Debug, Default)]
pub struct Recorder(Arc<Mutex<String>>);

impl Recorder {
    /// A recorder with nothing in it.
    #[must_use]
    pub fn new() -> Recorder {
        Recorder::default()
    }

    /// Everything written so far, leaving it in place.
    ///
    /// A poisoned lock reads as empty rather than panicking. The only way to poison it is a panic
    /// inside a write, and turning that into a second panic while somebody is trying to read the
    /// output that would explain the first one helps nobody.
    #[must_use]
    pub fn text(&self) -> String {
        self.0.lock().map(|text| text.clone()).unwrap_or_default()
    }

    /// Everything written so far, emptying the buffer.
    #[must_use]
    pub fn take(&self) -> String {
        self.0
            .lock()
            .map(|mut text| std::mem::take(&mut *text))
            .unwrap_or_default()
    }
}

impl Output for Recorder {
    fn write(&mut self, _stream: Stream, text: &str) {
        if let Ok(mut buffer) = self.0.lock() {
            buffer.push_str(text);
        }
    }
}

/// A sink that throws everything away.
///
/// For an embedder that runs untrusted code and does not want its `console.log` in the host's logs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Discard;

impl Output for Discard {
    fn write(&mut self, _stream: Stream, _text: &str) {}
}

#[cfg(test)]
mod tests {
    use super::{Discard, Output, Recorder, Stream};

    #[test]
    fn a_recorder_keeps_what_was_written_in_the_order_it_was_written() {
        let recorder = Recorder::new();
        let mut sink = recorder.clone();
        sink.write(Stream::Out, "first\n");
        sink.write(Stream::Err, "second\n");
        sink.write(Stream::Out, "third\n");
        assert_eq!(recorder.text(), "first\nsecond\nthird\n");
    }

    #[test]
    fn taking_the_text_empties_the_buffer() {
        let recorder = Recorder::new();
        recorder.clone().write(Stream::Out, "once\n");
        assert_eq!(recorder.take(), "once\n");
        assert_eq!(recorder.text(), "");
    }

    #[test]
    fn a_clone_writes_into_the_same_buffer() {
        // The property the whole type exists for: the caller keeps a handle, the runtime gets one,
        // and both see the same text.
        let held = Recorder::new();
        let mut given_away = held.clone();
        given_away.write(Stream::Out, "hello\n");
        assert_eq!(held.text(), "hello\n");
    }

    #[test]
    fn discarding_output_keeps_nothing_and_does_not_fail() {
        let mut sink = Discard;
        sink.write(Stream::Out, "gone");
        sink.write(Stream::Err, "also gone");
    }
}
