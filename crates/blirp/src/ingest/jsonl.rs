//! Incremental line reader for append-only transcript files, plain or
//! zstd-compressed.
//!
//! The position is a byte offset plus line count, guarded by the file's
//! identity (creation time or inode) and a hash of its first bytes. A
//! different identity, a shorter file or a changed head means the file was
//! truncated, rotated or rewritten: reading restarts at 0 and the caller
//! resets its state (events dedupe on `(session_id, seq)`). Only complete
//! lines are consumed, so a line being written is picked up next time; a
//! trailing line without newline is consumed once the file has been quiet
//! for [`STALE_TAIL_MS`].

use super::Result;
use super::text::fnv64;
use serde::{Deserialize, Serialize};
use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Bytes hashed to detect a rewritten file.
const HEAD_LEN: u64 = 256;
/// A final line without newline counts as complete after this much quiet.
pub const STALE_TAIL_MS: i64 = 60_000;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FilePos {
    /// Bytes consumed (decompressed bytes for zstd files).
    #[serde(default)]
    pub offset: u64,
    /// Lines consumed; the index of the next line.
    #[serde(default)]
    pub line: u64,
    #[serde(default)]
    pub ident: String,
    #[serde(default)]
    pub head_len: u64,
    #[serde(default)]
    pub head: u64,
}

fn identity(md: &Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", md.dev(), md.ino())
    }
    #[cfg(not(unix))]
    {
        md.created()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos().to_string())
            .unwrap_or_default()
    }
}

fn mtime_ms(md: &Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The first `len` bytes, or `None` when the file is shorter than that.
fn read_head(r: &mut (impl Read + ?Sized), len: u64) -> Option<Vec<u8>> {
    let mut buf = Vec::with_capacity(usize::try_from(len).unwrap_or(0));
    r.take(len).read_to_end(&mut buf).ok()?;
    (buf.len() as u64 == len).then_some(buf)
}

pub struct Lines {
    reader: Box<dyn BufRead + Send>,
    pos: FilePos,
    /// True when the stored position was discarded and reading restarts at 0.
    pub reset: bool,
    /// True when an incomplete final line was left for later.
    pub partial: bool,
    pub mtime_ms: i64,
    head_buf: Vec<u8>,
}

fn open_decoder(path: &Path) -> Result<Box<dyn BufRead + Send>> {
    let f = File::open(path)?;
    let dec = zstd::stream::read::Decoder::new(f)?;
    Ok(Box::new(BufReader::with_capacity(256 * 1024, dec)))
}

impl Lines {
    /// Open `path` positioned after `pos`.
    pub fn open(path: &Path, pos: &FilePos, compressed: bool) -> Result<Lines> {
        let file = File::open(path)?;
        let md = file.metadata()?;
        let ident = identity(&md);
        let mtime_ms = mtime_ms(&md);
        let mut pos = pos.clone();
        let mut reset = !pos.ident.is_empty() && pos.ident != ident;
        let mut head_buf = Vec::new();
        let mut check_head = |r: &mut dyn Read, pos: &FilePos| -> bool {
            match read_head(r, pos.head_len) {
                Some(h) if fnv64(&h) == pos.head => {
                    head_buf = h;
                    true
                }
                _ => false,
            }
        };
        let reader: Box<dyn BufRead + Send> = if compressed {
            drop(file);
            let mut r = open_decoder(path)?;
            if !reset && pos.head_len > 0 && !check_head(&mut r, &pos) {
                reset = true;
            }
            if !reset {
                let skip = pos.offset.saturating_sub(pos.head_len);
                let skipped =
                    std::io::copy(&mut (&mut r).take(skip), &mut std::io::sink()).unwrap_or(0);
                if skipped < skip {
                    reset = true;
                }
            }
            if reset { open_decoder(path)? } else { r }
        } else {
            let mut file = file;
            if !reset && md.len() < pos.offset {
                reset = true;
            }
            if !reset && pos.head_len > 0 && !check_head(&mut file, &pos) {
                reset = true;
            }
            file.seek(SeekFrom::Start(if reset { 0 } else { pos.offset }))?;
            Box::new(BufReader::with_capacity(256 * 1024, file))
        };
        if reset || pos.ident.is_empty() {
            pos = FilePos {
                ident,
                ..FilePos::default()
            };
            head_buf.clear();
        }
        Ok(Lines {
            reader,
            pos,
            reset,
            partial: false,
            mtime_ms,
            head_buf,
        })
    }

    pub fn pos(&self) -> &FilePos {
        &self.pos
    }

    /// The position as if reading had stopped before line `line`, which
    /// starts at byte `offset` (both previously passed to the callback).
    pub fn rewound(&self, line: u64, offset: u64) -> FilePos {
        FilePos {
            offset,
            line,
            ..self.pos.clone()
        }
    }

    /// Call `f(line_index, line)` for every complete line (without the line
    /// terminator; blank lines are counted but not passed). A decode error
    /// in a compressed file (frame still being written) ends the read.
    pub fn for_each(&mut self, mut f: impl FnMut(u64, &[u8]) -> Result<()>) -> Result<()> {
        self.for_each_at(|ix, _, line| f(ix, line))
    }

    /// [`Lines::for_each`] that also passes the byte offset where the line
    /// starts, so callers can rewind to it (see [`Lines::rewound`]).
    pub fn for_each_at(&mut self, mut f: impl FnMut(u64, u64, &[u8]) -> Result<()>) -> Result<()> {
        let mut buf = Vec::with_capacity(64 * 1024);
        let now = blirp_core::now_ms();
        loop {
            buf.clear();
            let n = match self.reader.read_until(b'\n', &mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    tracing::debug!(error = %e, "transcript read stopped");
                    self.partial = !buf.is_empty();
                    break;
                }
            };
            if n == 0 {
                break;
            }
            let complete = buf.ends_with(b"\n");
            if !complete && now - self.mtime_ms < STALE_TAIL_MS {
                self.partial = true;
                break;
            }
            self.track_head(&buf);
            let line_ix = self.pos.line;
            let start = self.pos.offset;
            self.pos.offset += n as u64;
            self.pos.line += 1;
            let mut line: &[u8] = &buf;
            while let [rest @ .., b'\n' | b'\r'] = line {
                line = rest;
            }
            if !line.iter().all(u8::is_ascii_whitespace) {
                f(line_ix, start, line)?;
            }
            if !complete {
                break;
            }
        }
        Ok(())
    }

    /// Hash the first HEAD_LEN bytes as they are consumed.
    fn track_head(&mut self, chunk: &[u8]) {
        if self.pos.head_len >= HEAD_LEN {
            return;
        }
        if self.head_buf.len() as u64 != self.pos.offset {
            // Head bytes not contiguous with this chunk; keep what is recorded.
            return;
        }
        let want = usize::try_from(HEAD_LEN).unwrap_or(256) - self.head_buf.len();
        self.head_buf
            .extend_from_slice(&chunk[..chunk.len().min(want)]);
        self.pos.head_len = self.head_buf.len() as u64;
        self.pos.head = fnv64(&self.head_buf);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::io::Write;

    fn read_all(path: &Path, pos: &FilePos, z: bool) -> (Vec<(u64, String)>, Lines) {
        let mut l = Lines::open(path, pos, z).unwrap();
        let mut out = Vec::new();
        l.for_each(|i, b| {
            out.push((i, String::from_utf8_lossy(b).into_owned()));
            Ok(())
        })
        .unwrap();
        (out, l)
    }

    #[test]
    fn resumes_skips_partial_and_detects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "a\r\n\nb\n{\"partial").unwrap();
        let (got, l) = read_all(&p, &FilePos::default(), false);
        assert_eq!(got, [(0, "a".into()), (2, "b".into())]);
        assert!(l.partial);
        let pos = l.pos().clone();
        assert_eq!(pos.line, 3);

        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(b"\":1}\nc\n").unwrap();
        drop(f);
        let (got, l) = read_all(&p, &pos, false);
        assert_eq!(got, [(3, "{\"partial\":1}".into()), (4, "c".into())]);
        assert!(!l.reset);
        let pos = l.pos().clone();

        // Rewritten shorter: restart from 0.
        std::fs::write(&p, "z\n").unwrap();
        let (got, l) = read_all(&p, &pos, false);
        assert!(l.reset);
        assert_eq!(got, [(0, "z".into())]);

        // Rewritten with a different head but longer: also a restart.
        let pos = l.pos().clone();
        std::fs::write(&p, "y\nmore lines here\n").unwrap();
        let (got, l) = read_all(&p, &pos, false);
        assert!(l.reset);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn zstd_frames_append() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl.zst");
        let frame = |s: &str| zstd::encode_all(s.as_bytes(), 3).unwrap();
        std::fs::write(&p, frame("one\ntwo\n")).unwrap();
        let (got, l) = read_all(&p, &FilePos::default(), true);
        assert_eq!(got.len(), 2);
        let pos = l.pos().clone();
        let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
        f.write_all(&frame("three\n")).unwrap();
        // A torn frame at the end is ignored until complete.
        f.write_all(&frame("four\n")[..5]).unwrap();
        drop(f);
        let (got, l) = read_all(&p, &pos, true);
        assert_eq!(got, [(2, "three".into())]);
        assert!(!l.reset);
    }
}
