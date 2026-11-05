//! Reads lines from any [`BufRead`] without ever keeping more of a line than a cap allows.

use std::io::{self, BufRead};

/// The result of one [`read_bounded_line`] call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Line {
    /// A complete line without its `\n` or `\r\n` terminator.
    Text(String),
    /// The line has more than `cap` content bytes. It was discarded through its
    /// `\n` (or the end of the input), so the next call starts at the following
    /// line.
    TooLong,
    /// No bytes remain.
    Eof,
}

#[cfg(test)]
thread_local! {
    /// Largest capacity the in-progress line has had on this thread.
    static PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How often the in-progress line has had to grow its buffer on this thread.
    static GROWTHS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Largest capacity the buffer had on this thread while a line already known
    /// to be too long was being discarded; `None` if no line was.
    static DISCARDING: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// The bytes of the line being read. They, and the capacity that holds them,
/// never exceed `limit`, and there are none once the line is known to be too long.
struct Pending {
    bytes: Vec<u8>,
    limit: usize,
    overflowed: bool,
}

impl Pending {
    fn push(&mut self, chunk: &[u8]) {
        if self.overflowed {
            #[cfg(test)]
            DISCARDING.with(|held| held.set(held.get().max(Some(self.bytes.capacity()))));
            return;
        }
        let used = self.bytes.len();
        if chunk.len() > self.limit - used {
            self.overflowed = true;
            self.bytes = Vec::new();
            return;
        }
        if self.bytes.capacity() - used < chunk.len() {
            // Grow geometrically, but never past the limit.
            let wanted = (used + chunk.len()).max(self.bytes.capacity().saturating_mul(2));
            #[cfg(test)]
            GROWTHS.with(|growths| growths.set(growths.get() + 1));
            self.bytes.reserve_exact(wanted.min(self.limit) - used);
        }
        self.bytes.extend_from_slice(chunk);
        #[cfg(test)]
        {
            assert!(
                !self.overflowed,
                "kept bytes of a line that is already too long"
            );
            PEAK.with(|peak| peak.set(peak.get().max(self.bytes.capacity())));
        }
    }
}

/// Reads the next line of `reader`, keeping at most `cap + 1` bytes of it.
///
/// `cap` limits the content of a line. Its `\n` is never counted, and a `\r`
/// directly before the `\n` is stripped and not counted, as [`BufRead::lines`]
/// does; a final line without a `\n` keeps a trailing `\r` as counted content.
/// A longer line is discarded as it arrives, through its `\n`, and reported as
/// [`Line::TooLong`]; the next call then returns the following line. A line
/// that fits and is not UTF-8 is an `InvalidData` error, again as with `lines`;
/// the discarded part of a longer line is never checked.
pub(crate) fn read_bounded_line(reader: &mut impl BufRead, cap: usize) -> io::Result<Line> {
    // A line that fits keeps `cap` content bytes and one `\r`, nothing more.
    let mut line = Pending {
        bytes: Vec::new(),
        limit: cap.saturating_add(1),
        overflowed: false,
    };
    let mut started = false;
    let mut terminated = false;
    loop {
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if chunk.is_empty() {
            break;
        }
        started = true;
        let newline = chunk.iter().position(|&byte| byte == b'\n');
        line.push(&chunk[..newline.unwrap_or(chunk.len())]);
        let used = newline.map_or(chunk.len(), |index| index + 1);
        reader.consume(used);
        if newline.is_some() {
            terminated = true;
            break;
        }
    }
    if !started {
        return Ok(Line::Eof);
    }
    if line.overflowed {
        return Ok(Line::TooLong);
    }
    let mut bytes = line.bytes;
    if terminated && bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    if bytes.len() > cap {
        return Ok(Line::TooLong);
    }
    String::from_utf8(bytes).map(Line::Text).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::Cell,
        io::{BufReader, Cursor, Read},
    };

    /// Chunk sizes that put a chunk boundary at every position of a line.
    const CHUNKS: [usize; 4] = [1, 2, 3, 8192];

    /// What one call returned; an `io::Error` cannot be compared.
    #[derive(Debug, PartialEq)]
    enum Got {
        Text(String),
        TooLong,
        InvalidUtf8,
    }

    fn text(line: &str) -> Got {
        Got::Text(line.into())
    }

    fn reader(input: &[u8], chunk: usize) -> BufReader<Cursor<Vec<u8>>> {
        BufReader::with_capacity(chunk, Cursor::new(input.to_vec()))
    }

    /// One outcome per line until the end of `input`.
    fn lines(input: &[u8], chunk: usize, cap: usize) -> Vec<Got> {
        let mut reader = reader(input, chunk);
        let mut out = Vec::new();
        // Every call before the end consumes a byte, so this many calls suffice.
        for _ in 0..=input.len() {
            match read_bounded_line(&mut reader, cap) {
                Ok(Line::Text(line)) => out.push(Got::Text(line)),
                Ok(Line::TooLong) => out.push(Got::TooLong),
                Ok(Line::Eof) => return out,
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                    assert_eq!(error.to_string(), "stream did not contain valid UTF-8");
                    out.push(Got::InvalidUtf8);
                }
            }
        }
        panic!("no end of input for {input:?} (chunk {chunk}, cap {cap})");
    }

    #[test]
    fn cap_minus_one_cap_and_cap_plus_one_bytes_with_and_without_a_newline() {
        for chunk in CHUNKS {
            for cap in [1usize, 2, 5, 17] {
                for terminator in ["", "\n"] {
                    for length in [cap - 1, cap, cap + 1] {
                        let line = "a".repeat(length);
                        let expected = if length > cap {
                            vec![Got::TooLong]
                        } else if line.is_empty() && terminator.is_empty() {
                            vec![]
                        } else {
                            vec![Got::Text(line.clone())]
                        };
                        assert_eq!(
                            lines(format!("{line}{terminator}").as_bytes(), chunk, cap),
                            expected,
                            "chunk {chunk}, cap {cap}, {length} bytes, terminator {terminator:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn a_carriage_return_before_the_newline_is_stripped_and_not_counted() {
        for chunk in CHUNKS {
            for cap in [0usize, 1, 4, 9] {
                let fits = "a".repeat(cap);
                assert_eq!(
                    lines(format!("{fits}\r\n").as_bytes(), chunk, cap),
                    [Got::Text(fits.clone())],
                    "chunk {chunk}, cap {cap}"
                );
                let over = "a".repeat(cap + 1);
                assert_eq!(
                    lines(format!("{over}\r\n").as_bytes(), chunk, cap),
                    [Got::TooLong],
                    "chunk {chunk}, cap {cap}"
                );
            }
            // Only the one `\r` that touches the `\n` is a terminator.
            assert_eq!(lines(b"a\r\r\n", chunk, 2), [text("a\r")]);
            assert_eq!(lines(b"a\r\r\n", chunk, 1), [Got::TooLong]);
            assert_eq!(lines(b"a\rb\r\n", chunk, 3), [text("a\rb")]);
        }
    }

    #[test]
    fn a_final_line_keeps_a_trailing_carriage_return_as_counted_content() {
        for chunk in CHUNKS {
            for cap in [1usize, 3, 8] {
                let fits = format!("{}\r", "a".repeat(cap - 1));
                assert_eq!(
                    lines(fits.as_bytes(), chunk, cap),
                    [Got::Text(fits.clone())],
                    "chunk {chunk}, cap {cap}"
                );
                let over = format!("{}\r", "a".repeat(cap));
                assert_eq!(
                    lines(over.as_bytes(), chunk, cap),
                    [Got::TooLong],
                    "chunk {chunk}, cap {cap}"
                );
            }
            assert_eq!(lines(b"\r", chunk, 1), [text("\r")]);
            assert_eq!(lines(b"\r", chunk, 0), [Got::TooLong]);
            assert_eq!(lines(b"a\r\nb\r", chunk, 8), [text("a"), text("b\r")]);
        }
    }

    #[test]
    fn the_largest_cap_accepts_every_line_without_overflowing() {
        for chunk in CHUNKS {
            assert_eq!(
                lines(b"abc\r\nd", chunk, usize::MAX),
                [text("abc"), text("d")]
            );
        }
    }

    #[test]
    fn a_final_line_without_a_newline_is_returned() {
        for chunk in CHUNKS {
            assert_eq!(lines(b"a", chunk, 4), [text("a")]);
            assert_eq!(lines(b"a\nbc", chunk, 4), [text("a"), text("bc")]);
            assert_eq!(lines(b"a\nbcdef", chunk, 4), [text("a"), Got::TooLong]);
            assert_eq!(lines(b"abcd", chunk, 4), [text("abcd")]);
        }
    }

    #[test]
    fn blank_lines_are_empty_text_and_an_empty_tail_is_the_end() {
        for chunk in CHUNKS {
            assert!(lines(b"", chunk, 4).is_empty());
            assert_eq!(lines(b"\n", chunk, 4), [text("")]);
            assert_eq!(lines(b"\r\n", chunk, 4), [text("")]);
            assert_eq!(lines(b"\n\n\n", chunk, 4), [text(""), text(""), text("")]);
            assert_eq!(
                lines(b"a\n\r\nb\n", chunk, 4),
                [text("a"), text(""), text("b")]
            );
            assert_eq!(lines(b"a\n", chunk, 4), [text("a")]);
            assert_eq!(lines(b"\n\r\n", chunk, 0), [text(""), text("")]);
        }
    }

    #[test]
    fn an_overlong_line_is_discarded_and_the_next_line_is_read() {
        let cap = 4;
        for chunk in CHUNKS {
            for size in [cap + 1, cap + 2, 40, 8193, 20000] {
                let long = "a".repeat(size);
                for terminator in ["\n", "\r\n"] {
                    assert_eq!(
                        lines(
                            format!("{long}{terminator}ok{terminator}").as_bytes(),
                            chunk,
                            cap
                        ),
                        [Got::TooLong, text("ok")],
                        "chunk {chunk}, {size} bytes, terminator {terminator:?}"
                    );
                }
                assert_eq!(
                    lines(
                        format!("x\n{long}\ny\n{long}\n{long}\nz").as_bytes(),
                        chunk,
                        cap
                    ),
                    [
                        text("x"),
                        Got::TooLong,
                        text("y"),
                        Got::TooLong,
                        Got::TooLong,
                        text("z")
                    ],
                    "chunk {chunk}, {size} bytes"
                );
            }
        }
    }

    #[test]
    fn an_overlong_stream_without_a_newline_is_reported_once_then_ends() {
        for chunk in CHUNKS {
            for size in [5usize, 6, 100, 9000] {
                let mut reader = reader("a".repeat(size).as_bytes(), chunk);
                assert_eq!(read_bounded_line(&mut reader, 4).unwrap(), Line::TooLong);
                assert_eq!(read_bounded_line(&mut reader, 4).unwrap(), Line::Eof);
                assert_eq!(read_bounded_line(&mut reader, 4).unwrap(), Line::Eof);
            }
        }
    }

    #[test]
    fn multibyte_characters_at_the_cap_and_at_chunk_boundaries() {
        for chunk in CHUNKS {
            for glyph in ["é", "€", "😀"] {
                for before in 0..=4 {
                    for after in 0..=3 {
                        let line = format!("{}{glyph}{}", "x".repeat(before), "y".repeat(after));
                        for cap in line.len().saturating_sub(2)..=line.len() + 1 {
                            for terminator in ["", "\n", "\r\n"] {
                                let expected = if line.len() <= cap {
                                    vec![text(&line)]
                                } else {
                                    vec![Got::TooLong]
                                };
                                assert_eq!(
                                    lines(format!("{line}{terminator}").as_bytes(), chunk, cap),
                                    expected,
                                    "{line:?} ({} bytes), cap {cap}, chunk {chunk}, \
                                     terminator {terminator:?}",
                                    line.len()
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn invalid_utf8_in_an_accepted_line_is_the_error_std_reports() {
        let std_error = Cursor::new(b"ab\xFFcd\n".to_vec())
            .lines()
            .next()
            .unwrap()
            .unwrap_err();
        for chunk in CHUNKS {
            let mut reader = reader(b"ab\xFFcd\nnext\n", chunk);
            let error = read_bounded_line(&mut reader, 16).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error.kind(), std_error.kind());
            assert_eq!(error.to_string(), "stream did not contain valid UTF-8");
            assert_eq!(error.to_string(), std_error.to_string());
            // The bad line is consumed, as std consumes it, so reading goes on.
            assert_eq!(
                read_bounded_line(&mut reader, 16).unwrap(),
                Line::Text("next".into())
            );
            for input in [
                &b"\xC3"[..],
                b"\xE2\x82",
                b"\xF0\x9F\x98\n",
                b"\xC0\x80\n",
                b"\xED\xA0\x80\r\n",
            ] {
                assert_eq!(lines(input, chunk, 16), [Got::InvalidUtf8], "{input:?}");
            }
        }
    }

    #[test]
    fn invalid_utf8_beyond_the_cap_is_never_checked() {
        for chunk in CHUNKS {
            assert_eq!(
                lines(b"\xFF\xFE\xFD\xFC\xFB\xFA\n", chunk, 3),
                [Got::TooLong]
            );
            // Four bytes are still held for a cap of three; they are not checked either.
            assert_eq!(lines(b"\xFF\xFF\xFF\xFF\n", chunk, 3), [Got::TooLong]);
            assert_eq!(
                lines(b"abcd\xFF\xFFefg\xC3\nok\n", chunk, 3),
                [Got::TooLong, text("ok")]
            );
            assert_eq!(
                lines(b"abcd\xFF\xFE\r\nok", chunk, 3),
                [Got::TooLong, text("ok")]
            );
            assert_eq!(lines(b"abcdefgh\xFF", chunk, 3), [Got::TooLong]);
        }
    }

    /// The largest capacity the in-progress line reached while reading `input`.
    fn peak(input: &[u8], chunk: usize, cap: usize) -> usize {
        PEAK.with(|peak| peak.set(0));
        lines(input, chunk, cap);
        PEAK.with(Cell::get)
    }

    #[test]
    fn a_line_never_holds_more_than_the_cap_plus_a_carriage_return() {
        for chunk in CHUNKS {
            for cap in [0usize, 1, 7, 64, 1000] {
                // Lines of many times the cap: they must be discarded as they arrive.
                let huge = "a".repeat(cap * 100 + 5000);
                let input = format!("{huge}\n{huge}\r\n{huge}");
                assert!(
                    peak(input.as_bytes(), chunk, cap) <= cap + 1,
                    "chunk {chunk}, cap {cap}"
                );
                // A line that fits grows to its own size and no further, however
                // the growth steps fall, so the probe is also known to see a line.
                for tail in ["\n", "\r\n", ""] {
                    let fits = format!("{}{tail}", "a".repeat(cap));
                    let held = peak(fits.as_bytes(), chunk, cap);
                    assert!(
                        held >= cap && held <= cap + 1,
                        "chunk {chunk}, cap {cap}, tail {tail:?}: {held}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_long_line_read_a_byte_at_a_time_grows_its_buffer_geometrically() {
        // The cap is far above the line, so the limit never cuts a growth short.
        let length = (1usize << 20) + 12_345;
        let mut input = vec![b'a'; length];
        input.push(b'\n');
        GROWTHS.with(|growths| growths.set(0));
        assert_eq!(
            lines(&input, 1, usize::MAX / 2),
            [Got::Text("a".repeat(length))]
        );
        let growths = GROWTHS.with(Cell::get);
        // Doubling takes about log2(length) growths; adding the bytes as they come
        // would take one per byte.
        let bound = 2 * length.ilog2() as usize + 2;
        assert!(
            (1..=bound).contains(&growths),
            "{growths} growths for {length} bytes, at most {bound} expected"
        );
    }

    #[test]
    fn an_overlong_line_holds_no_buffer_while_it_is_discarded() {
        let long = "a".repeat(20_000);
        let input = format!("x\n{long}\nok\n");
        for chunk in CHUNKS {
            DISCARDING.with(|held| held.set(None));
            assert_eq!(
                lines(input.as_bytes(), chunk, 16),
                [text("x"), Got::TooLong, text("ok")],
                "chunk {chunk}"
            );
            // Whatever was kept before the line was known to be too long has been
            // freed, not just emptied.
            assert_eq!(DISCARDING.with(Cell::get), Some(0), "chunk {chunk}");
        }
    }

    /// Hands out its chunks one per `fill_buf`, as a pipe does, and counts how
    /// often more data had to be fetched.
    struct Script {
        chunks: Vec<&'static [u8]>,
        current: &'static [u8],
        fetches: usize,
        interrupt: bool,
        interrupted: bool,
        fail_at_end: bool,
    }

    impl Script {
        fn new(chunks: &[&'static [u8]]) -> Self {
            Self {
                chunks: chunks.iter().rev().copied().collect(),
                current: b"",
                fetches: 0,
                interrupt: false,
                interrupted: false,
                fail_at_end: false,
            }
        }
    }

    impl Read for Script {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let count = available.len().min(out.len());
            out[..count].copy_from_slice(&available[..count]);
            self.consume(count);
            Ok(count)
        }
    }

    impl BufRead for Script {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.current.is_empty() {
                if self.interrupt && !self.interrupted {
                    self.interrupted = true;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.interrupted = false;
                self.fetches += 1;
                match self.chunks.pop() {
                    Some(chunk) => self.current = chunk,
                    None if self.fail_at_end => return Err(io::Error::other("disk on fire")),
                    None => {}
                }
            }
            Ok(self.current)
        }

        fn consume(&mut self, amount: usize) {
            self.current = &self.current[amount..];
        }
    }

    #[test]
    fn a_line_is_returned_as_soon_as_its_newline_arrives() {
        let mut script = Script::new(&[b"ab\n", b"cdefg", b"hi\n", b"jk\r\n"]);
        assert_eq!(read_bounded_line(&mut script, 4).unwrap(), text_line("ab"));
        assert_eq!(script.fetches, 1, "read past the newline");
        // Overlong: discarded through its newline, and no further.
        assert_eq!(read_bounded_line(&mut script, 4).unwrap(), Line::TooLong);
        assert_eq!(script.fetches, 3, "read past the newline");
        assert_eq!(read_bounded_line(&mut script, 4).unwrap(), text_line("jk"));
        assert_eq!(script.fetches, 4, "read past the newline");
        assert_eq!(read_bounded_line(&mut script, 4).unwrap(), Line::Eof);
        assert_eq!(script.fetches, 5);
    }

    fn text_line(line: &str) -> Line {
        Line::Text(line.into())
    }

    #[test]
    fn interrupted_reads_are_retried() {
        let mut script = Script::new(&[b"a", b"b\r", b"\ncd", b"e"]);
        script.interrupt = true;
        assert_eq!(read_bounded_line(&mut script, 8).unwrap(), text_line("ab"));
        assert_eq!(read_bounded_line(&mut script, 8).unwrap(), text_line("cde"));
        assert_eq!(read_bounded_line(&mut script, 8).unwrap(), Line::Eof);
        let mut script = Script::new(&[b"abcdef", b"gh\n", b"ok\n"]);
        script.interrupt = true;
        assert_eq!(read_bounded_line(&mut script, 3).unwrap(), Line::TooLong);
        assert_eq!(read_bounded_line(&mut script, 3).unwrap(), text_line("ok"));
    }

    #[test]
    fn read_errors_are_returned() {
        let mut script = Script::new(&[b"abc", b"de"]);
        script.fail_at_end = true;
        let error = read_bounded_line(&mut script, 16).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "disk on fire");
        let mut script = Script::new(&[b"abcdefghij", b"kl"]);
        script.fail_at_end = true;
        let error = read_bounded_line(&mut script, 3).unwrap_err();
        assert_eq!(error.to_string(), "disk on fire");
    }

    /// Every sequence of up to `depth` of the symbols.
    fn sequences(symbols: &[&[u8]], depth: usize) -> Vec<Vec<u8>> {
        let mut all = vec![Vec::new()];
        let mut level = vec![Vec::new()];
        for _ in 0..depth {
            level = level
                .iter()
                .flat_map(|prefix| {
                    symbols.iter().map(move |symbol| {
                        let mut next = prefix.clone();
                        next.extend_from_slice(symbol);
                        next
                    })
                })
                .collect();
            all.extend(level.iter().cloned());
        }
        all
    }

    // std's `lines()` is the oracle for what a line is; the cap is the rule that
    // a line is accepted exactly when its content has at most `cap` bytes.
    #[test]
    fn every_short_input_splits_as_std_lines_with_the_cap_applied() {
        let symbols: [&[u8]; 5] = [b"a", "é".as_bytes(), "😀".as_bytes(), b"\r", b"\n"];
        for input in sequences(&symbols, 5) {
            let oracle: Vec<String> = Cursor::new(input.clone())
                .lines()
                .map(Result::unwrap)
                .collect();
            for cap in 0..=6 {
                let expected: Vec<Got> = oracle
                    .iter()
                    .map(|line| {
                        if line.len() <= cap {
                            Got::Text(line.clone())
                        } else {
                            Got::TooLong
                        }
                    })
                    .collect();
                for chunk in CHUNKS {
                    assert_eq!(
                        lines(&input, chunk, cap),
                        expected,
                        "{input:?}, cap {cap}, chunk {chunk}"
                    );
                }
            }
        }
    }
}
