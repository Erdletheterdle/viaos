//! Turning the engine's log framing into whole lines (spec section 46).
//!
//! Docker hands out however many bytes arrived together, which may be half a
//! line, or three of them, or a line split across two chunks because the
//! application flushed mid-write. A viewer wants lines, and every consumer
//! downstream -- the QUIC frame, the SSE event, the terminal -- is line
//! oriented, so the reassembly happens once, here, next to the engine that
//! caused it.
//!
//! Nothing in this module holds a buffer that grows without a reader: the
//! stream is polled by whoever is watching, and when nobody is, it is not
//! polled at all.

use std::collections::VecDeque;

use futures::{Stream, StreamExt};

use crate::Error;
use crate::types::{LogLine, LogSource};

/// Largest line that will be assembled before it is emitted as-is.
///
/// A process that writes a gigabyte without a newline must not become a
/// gigabyte held in the agent's memory. The bytes are not lost -- they are
/// emitted as a line that happens to end where the limit did.
const MAX_LINE: usize = 64 * 1024;

/// Reassembles chunks into lines, one per container.
///
/// stdout and stderr are buffered separately: they are independent byte
/// streams, and interleaving their partial lines would splice one process's
/// half-written error into the middle of its own output.
pub(crate) struct Assembler {
    service: String,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    ready: VecDeque<LogLine>,
}

impl Assembler {
    pub(crate) fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            stdout: Vec::new(),
            stderr: Vec::new(),
            ready: VecDeque::new(),
        }
    }

    /// Feed one chunk from the engine.
    pub(crate) fn push(&mut self, source: LogSource, bytes: &[u8]) {
        let buffer = match source {
            LogSource::Stdout => &mut self.stdout,
            LogSource::Stderr => &mut self.stderr,
        };
        buffer.extend_from_slice(bytes);

        while let Some(at) = buffer.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buffer.drain(..=at).collect();
            self.ready.push_back(build(&self.service, source, &line[..line.len() - 1]));
        }
        // `while`, because one chunk is however many bytes arrived together:
        // a container attached to a tty hands over raw output with no framing
        // at all, so a single chunk can be several times the limit. Cutting
        // once per chunk would leave the remainder buffered and let the
        // buffer grow by the excess on every chunk -- which is the unbounded
        // memory this limit exists to prevent.
        while buffer.len() > MAX_LINE {
            let line: Vec<u8> = buffer.drain(..MAX_LINE).collect();
            self.ready.push_back(build(&self.service, source, &line));
        }
    }

    /// Emit whatever is still buffered.
    ///
    /// A container killed mid-line still wrote those bytes, and they are
    /// usually the most interesting ones in the file.
    pub(crate) fn flush(&mut self) {
        for source in [LogSource::Stdout, LogSource::Stderr] {
            let buffer = match source {
                LogSource::Stdout => std::mem::take(&mut self.stdout),
                LogSource::Stderr => std::mem::take(&mut self.stderr),
            };
            if !buffer.is_empty() {
                self.ready.push_back(build(&self.service, source, &buffer));
            }
        }
    }

    pub(crate) fn next_line(&mut self) -> Option<LogLine> {
        self.ready.pop_front()
    }
}

/// Split the engine's timestamp off the front of a line.
///
/// Docker prefixes each line with an RFC 3339 timestamp when asked to. Parsed
/// off rather than left in the text so a viewer can choose whether to show it,
/// and so two services' lines can be read as one stream.
fn build(service: &str, source: LogSource, bytes: &[u8]) -> LogLine {
    // Lossy on purpose: a log line is whatever the application printed, and
    // refusing to show a line because one byte was not UTF-8 helps nobody.
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_suffix('\r').unwrap_or(&text);

    let (at, line) = match text.split_once(' ') {
        Some((prefix, rest)) if looks_like_a_timestamp(prefix) => {
            (Some(prefix.to_string()), rest.to_string())
        }
        _ => (None, text.to_string()),
    };
    LogLine { service: service.to_string(), source, at, line }
}

/// Whether a token is the engine's timestamp rather than the first word the
/// application printed.
///
/// Checked rather than assumed: `timestamps` is a request, and a line that
/// arrives without one must not lose its first word to this.
fn looks_like_a_timestamp(token: &str) -> bool {
    token.len() >= 20
        && token.starts_with(|c: char| c.is_ascii_digit())
        && token.contains('T')
        && token.ends_with('Z')
}

/// Adapt a stream of engine chunks into a stream of lines.
///
/// Written as a stream rather than a task writing into a channel so that
/// nothing runs when nobody polls: dropping the returned stream drops the
/// engine request with it, which is the whole "nothing is forwarded when
/// nobody is watching" property (spec section 41).
pub(crate) fn lines<S>(service: String, chunks: S) -> impl Stream<Item = Result<LogLine, Error>>
where
    S: Stream<Item = Result<(LogSource, Vec<u8>), Error>> + Send + 'static,
{
    struct State<S> {
        chunks: S,
        assembler: Assembler,
        /// Reported after the buffered lines, not instead of them: the bytes
        /// already received are real, and an engine connection dropping is
        /// often exactly when the last line matters.
        failure: Option<Error>,
        drained: bool,
    }

    let state = State {
        chunks: Box::pin(chunks),
        assembler: Assembler::new(service),
        failure: None,
        drained: false,
    };

    futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(line) = state.assembler.next_line() {
                return Some((Ok(line), state));
            }
            if let Some(e) = state.failure.take() {
                return Some((Err(e), state));
            }
            if state.drained {
                return None;
            }
            match state.chunks.next().await {
                Some(Ok((source, bytes))) => state.assembler.push(source, &bytes),
                Some(Err(e)) => {
                    state.assembler.flush();
                    state.failure = Some(e);
                    state.drained = true;
                }
                None => {
                    state.assembler.flush();
                    state.drained = true;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(assembler: &mut Assembler) -> Vec<String> {
        let mut out = Vec::new();
        while let Some(line) = assembler.next_line() {
            out.push(line.line);
        }
        out
    }

    #[test]
    fn a_chunk_carrying_several_lines_becomes_several_lines() {
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"one\ntwo\nthree\n");
        assert_eq!(drain(&mut a), vec!["one", "two", "three"]);
    }

    #[test]
    fn a_line_split_across_chunks_is_rejoined() {
        // The common case, and the one that makes naive forwarding look
        // almost right: the application flushed mid-line.
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"start of ");
        assert!(drain(&mut a).is_empty(), "half a line is not a line yet");
        a.push(LogSource::Stdout, b"a line\n");
        assert_eq!(drain(&mut a), vec!["start of a line"]);
    }

    #[test]
    fn stdout_and_stderr_do_not_splice_into_each_other() {
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"out partial");
        a.push(LogSource::Stderr, b"err complete\n");
        a.push(LogSource::Stdout, b" finished\n");
        let lines = drain(&mut a);
        assert_eq!(
            lines,
            vec!["err complete", "out partial finished"],
            "each stream is its own byte pipe and must be buffered as one"
        );
    }

    #[test]
    fn a_trailing_partial_line_survives_the_end_of_the_stream() {
        // A container killed mid-line still wrote those bytes, and they are
        // usually the interesting ones.
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"panicked at ");
        a.flush();
        assert_eq!(drain(&mut a), vec!["panicked at "]);
    }

    #[test]
    fn the_engine_timestamp_is_parsed_off_rather_than_left_in_the_text() {
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"2026-08-23T10:00:00.123456789Z v44 tick 10:00:00\n");
        let line = a.next_line().unwrap();
        assert_eq!(line.at.as_deref(), Some("2026-08-23T10:00:00.123456789Z"));
        assert_eq!(line.line, "v44 tick 10:00:00");
    }

    #[test]
    fn a_line_with_no_timestamp_keeps_its_first_word() {
        // Timestamps are a request, not a guarantee. Assuming one would eat
        // the first word of every line from an engine that did not add them.
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"database reachable; entering main loop\n");
        let line = a.next_line().unwrap();
        assert_eq!(line.at, None);
        assert_eq!(line.line, "database reachable; entering main loop");
    }

    #[test]
    fn a_line_that_never_ends_is_cut_rather_than_buffered_forever() {
        // A process writing without a newline must not become unbounded
        // memory on a device with 512 MB of it.
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, &vec![b'x'; MAX_LINE + 10]);
        let lines = drain(&mut a);
        assert_eq!(lines.len(), 1, "the line is emitted at the limit");
        assert_eq!(lines[0].len(), MAX_LINE);
        assert_eq!(a.stdout.len(), 10, "and only the remainder is still held");
    }

    #[test]
    fn a_chunk_many_times_the_limit_is_cut_as_many_times_as_it_takes() {
        // The invariant is about the buffer, not about the chunk: nothing
        // promises the engine hands over 64 KB at a time, and a container
        // attached to a tty hands over whatever the process wrote. Feeding
        // one chunk per push and cutting once per push would grow this buffer
        // by the excess every time, which is the gigabyte the module doc says
        // must never be held.
        let mut a = Assembler::new("detector");
        for _ in 0..5 {
            a.push(LogSource::Stdout, &vec![b'x'; MAX_LINE * 3]);
            assert!(
                a.stdout.len() <= MAX_LINE,
                "after a push the buffer holds at most one unterminated line, not {}",
                a.stdout.len()
            );
        }
        let lines = drain(&mut a);
        let emitted: usize = lines.iter().map(String::len).sum();
        assert_eq!(
            emitted + a.stdout.len(),
            MAX_LINE * 15,
            "nothing is lost by cutting: every byte fed is either emitted or still buffered"
        );
        assert!(lines.iter().all(|l| l.len() == MAX_LINE), "each cut is exactly at the limit");
    }

    #[test]
    fn invalid_utf8_is_shown_rather_than_dropped() {
        let mut a = Assembler::new("detector");
        a.push(LogSource::Stdout, b"before \xff\xfe after\n");
        assert_eq!(drain(&mut a).len(), 1, "a bad byte must not cost the line");
    }

    #[tokio::test]
    async fn the_stream_ends_when_the_engine_stream_does() {
        let chunks = futures::stream::iter(vec![
            Ok((LogSource::Stdout, b"one\ntwo".to_vec())),
            Ok((LogSource::Stdout, b" and a half".to_vec())),
        ]);
        let collected: Vec<_> = lines("detector".into(), chunks)
            .map(|r| r.unwrap().line)
            .collect()
            .await;
        assert_eq!(collected, vec!["one", "two and a half"]);
    }
}
