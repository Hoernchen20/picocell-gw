//! USB console building blocks free of I/O, so they can be tested on the host: the log
//! history (a ring of whole lines) and the line editor for typed commands.

#![no_std]

use core::fmt;

use heapless::Vec;

/// The last log lines, oldest dropped first. Each line is stored as a tag byte (the
/// level), the text and `\n`. Positions are sequence numbers (bytes ever written), so
/// a reader can tell when the lines it has not read yet were overwritten.
pub struct History<const N: usize> {
    buf: [u8; N],
    /// Start of the oldest line.
    tail: u64,
    /// End of the newest line.
    head: u64,
}

/// One step of reading the history.
#[derive(Debug, PartialEq)]
pub enum Read {
    /// `len` bytes of text (cut to the buffer) in `out`; the next line starts at `next`.
    Line { tag: u8, len: usize, next: u64 },
    /// The position was overwritten; the oldest line starts at `next`.
    Lost { next: u64 },
    /// Nothing newer.
    End,
}

impl<const N: usize> Default for History<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> History<N> {
    /// Longest stored text; longer lines are cut.
    pub const MAX_TEXT: usize = N / 4;

    pub const fn new() -> Self {
        Self { buf: [0; N], tail: 0, head: 0 }
    }

    /// Position after the newest line.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// Position of the oldest line.
    pub fn oldest(&self) -> u64 {
        self.tail
    }

    fn at(&self, seq: u64) -> u8 {
        self.buf[(seq % N as u64) as usize]
    }

    fn put(&mut self, b: u8) {
        self.buf[(self.head % N as u64) as usize] = b;
        self.head += 1;
    }

    fn drop_line(&mut self) {
        while self.tail < self.head {
            let b = self.at(self.tail);
            self.tail += 1;
            if b == b'\n' {
                break;
            }
        }
    }

    /// Appends a line; line breaks in `text` become blanks.
    pub fn push(&mut self, tag: u8, text: &[u8]) {
        let text = &text[..text.len().min(Self::MAX_TEXT)];
        let len = text.len() as u64 + 2;
        while self.head + len - self.tail > N as u64 {
            self.drop_line();
        }
        self.put(tag);
        for &b in text {
            self.put(if b == b'\n' { b' ' } else { b });
        }
        self.put(b'\n');
    }

    /// Reads the line starting at `seq` into `out`.
    pub fn read(&self, seq: u64, out: &mut [u8]) -> Read {
        if seq < self.tail {
            return Read::Lost { next: self.tail };
        }
        if seq >= self.head {
            return Read::End;
        }
        let tag = self.at(seq);
        let mut s = seq + 1;
        let mut len = 0;
        loop {
            let b = self.at(s);
            s += 1;
            if b == b'\n' {
                break;
            }
            if len < out.len() {
                out[len] = b;
                len += 1;
            }
        }
        Read::Line { tag, len, next: s }
    }

    /// Position of the `n`-th line from the end (the oldest one if there are fewer).
    pub fn last_lines(&self, n: usize) -> u64 {
        let mut s = self.head;
        for _ in 0..n {
            if s <= self.tail {
                break;
            }
            // s - 1 is the previous line's `\n`.
            s -= 1;
            while s > self.tail && self.at(s - 1) != b'\n' {
                s -= 1;
            }
        }
        s
    }
}

/// `fmt::Write` into a byte buffer that keeps what fits and drops the rest.
pub struct Trunc<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> Trunc<'a> {
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, len: 0 }
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl fmt::Write for Trunc<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// What the terminal should show after a key.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    /// Print the character.
    Echo(u8),
    /// Remove the last character.
    Erase,
    /// Line full.
    Bell,
    /// The line is complete; take it with `take()`.
    Enter,
    /// Ctrl-C: line dropped.
    Cancel,
    /// Ctrl-U: line cleared.
    Clear,
}

#[derive(Clone, Copy)]
enum Esc {
    No,
    Start,
    Csi,
}

/// Input line: printable ASCII, backspace, Ctrl-C, Ctrl-U; escape sequences (cursor
/// keys) are skipped. CR, LF and CR LF each end a line.
pub struct Editor<const N: usize> {
    line: Vec<u8, N>,
    esc: Esc,
    last_cr: bool,
}

impl<const N: usize> Default for Editor<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Editor<N> {
    pub const fn new() -> Self {
        Self { line: Vec::new(), esc: Esc::No, last_cr: false }
    }

    pub fn feed(&mut self, b: u8) -> Action {
        let after_cr = core::mem::replace(&mut self.last_cr, b == b'\r');
        match self.esc {
            Esc::Start => {
                self.esc = if b == b'[' { Esc::Csi } else { Esc::No };
                return Action::None;
            }
            Esc::Csi => {
                if (0x40..=0x7e).contains(&b) {
                    self.esc = Esc::No;
                }
                return Action::None;
            }
            Esc::No => {}
        }
        match b {
            b'\r' => Action::Enter,
            b'\n' if after_cr => Action::None,
            b'\n' => Action::Enter,
            0x1b => {
                self.esc = Esc::Start;
                Action::None
            }
            0x08 | 0x7f => match self.line.pop() {
                Some(_) => Action::Erase,
                None => Action::None,
            },
            0x03 => {
                self.line.clear();
                Action::Cancel
            }
            0x15 => {
                self.line.clear();
                Action::Clear
            }
            0x20..=0x7e => match self.line.push(b) {
                Ok(()) => Action::Echo(b),
                Err(_) => Action::Bell,
            },
            _ => Action::None,
        }
    }

    /// The line typed so far (printable ASCII only).
    pub fn line(&self) -> &str {
        core::str::from_utf8(&self.line).unwrap_or("")
    }

    /// Takes the line and starts a new one.
    pub fn take(&mut self) -> Vec<u8, N> {
        core::mem::take(&mut self.line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines<const N: usize>(h: &History<N>, mut seq: u64) -> ([(u8, [u8; 16], usize); 8], usize) {
        let mut v = [(0, [0; 16], 0); 8];
        let mut n = 0;
        loop {
            let mut out = [0; 16];
            match h.read(seq, &mut out) {
                Read::Line { tag, len, next } => {
                    v[n] = (tag, out, len);
                    n += 1;
                    seq = next;
                }
                Read::Lost { next } => seq = next,
                Read::End => return (v, n),
            }
        }
    }

    fn text(l: &(u8, [u8; 16], usize)) -> &[u8] {
        &l.1[..l.2]
    }

    #[test]
    fn history_drops_oldest_lines() {
        // 32 bytes: each "lineN" takes 7.
        let mut h: History<32> = History::new();
        for i in 0..6u8 {
            h.push(b'I', &[b'l', b'i', b'n', b'e', b'0' + i]);
        }
        let (v, n) = lines(&h, h.oldest());
        // 4 lines fit (28 bytes): line2..line5.
        assert_eq!(n, 4);
        assert_eq!(text(&v[0]), b"line2");
        assert_eq!(text(&v[3]), b"line5");
        assert_eq!(v[0].0, b'I');
        assert_eq!(h.head() - h.oldest(), 28);
    }

    #[test]
    fn reader_notices_lost_lines() {
        let mut h: History<32> = History::new();
        h.push(b'I', b"first");
        let cursor = h.oldest();
        for _ in 0..5 {
            h.push(b'W', b"later");
        }
        let mut out = [0; 16];
        assert_eq!(h.read(cursor, &mut out), Read::Lost { next: h.oldest() });
        assert_eq!(h.read(h.head(), &mut out), Read::End);
    }

    #[test]
    fn long_lines_and_newlines() {
        let mut h: History<32> = History::new();
        h.push(b'E', b"a\nb 0123456789abcdef");
        let mut out = [0; 16];
        // Cut to N / 4 = 8 bytes when stored.
        assert_eq!(h.read(0, &mut out), Read::Line { tag: b'E', len: 8, next: 10 });
        assert_eq!(&out[..8], b"a b 0123");
        // Cut to the reader's buffer as well.
        let mut small = [0; 3];
        assert_eq!(h.read(0, &mut small), Read::Line { tag: b'E', len: 3, next: 10 });
    }

    #[test]
    fn last_lines() {
        let mut h: History<64> = History::new();
        assert_eq!(h.last_lines(3), 0);
        for t in [&b"one"[..], b"two", b"three"] {
            h.push(b'I', t);
        }
        let (v, n) = lines(&h, h.last_lines(2));
        assert_eq!(n, 2);
        assert_eq!(text(&v[0]), b"two");
        assert_eq!(h.last_lines(0), h.head());
        assert_eq!(h.last_lines(10), h.oldest());
        // After wrapping around the buffer.
        for _ in 0..10 {
            h.push(b'I', b"wrap");
        }
        h.push(b'I', b"end");
        let (v, n) = lines(&h, h.last_lines(1));
        assert_eq!((n, text(&v[0])), (1, &b"end"[..]));
    }

    #[test]
    fn trunc() {
        use core::fmt::Write;
        let mut buf = [0; 8];
        let mut t = Trunc::new(&mut buf);
        write!(t, "{} {}", 12345, "abcdef").unwrap();
        assert_eq!(t.as_bytes(), b"12345 ab");
    }

    #[test]
    fn editor() {
        let mut e: Editor<4> = Editor::new();
        assert_eq!(e.feed(b'a'), Action::Echo(b'a'));
        // Cursor key: ESC [ A.
        for b in *b"\x1b[A" {
            assert_eq!(e.feed(b), Action::None);
        }
        assert_eq!(e.feed(b'b'), Action::Echo(b'b'));
        assert_eq!(e.feed(0x7f), Action::Erase);
        for b in *b"xyz" {
            e.feed(b);
        }
        assert_eq!(e.feed(b'!'), Action::Bell);
        assert_eq!(e.line(), "axyz");
        assert_eq!(e.feed(b'\r'), Action::Enter);
        assert_eq!(&e.take()[..], b"axyz");
        // LF right after CR belongs to it.
        assert_eq!(e.feed(b'\n'), Action::None);
        assert_eq!(e.feed(b'\n'), Action::Enter);
        e.feed(b'q');
        assert_eq!(e.feed(0x03), Action::Cancel);
        assert_eq!(e.line(), "");
        assert_eq!(e.feed(0x08), Action::None);
        assert_eq!(e.feed(0xc3), Action::None);
    }
}
