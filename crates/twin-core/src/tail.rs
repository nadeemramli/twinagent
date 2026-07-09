//! Tail-follow reader for growing JSONL files.
//!
//! Agent CLIs append one JSON object per line and may flush mid-line; files
//! are occasionally truncated or replaced (rotation). The reader only ever
//! consumes *complete* lines and keeps its offset at a line boundary, so a
//! persisted offset is always a safe resume point.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Follows one JSONL file, yielding newly completed lines on each poll.
#[derive(Debug)]
pub struct TailReader {
    path: PathBuf,
    offset: u64,
}

impl TailReader {
    /// Follow `path` from the beginning.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self::from_offset(path, 0)
    }

    /// Resume following `path` from a previously persisted byte offset.
    /// The offset must lie on a line boundary — [`TailReader::offset`] always
    /// does.
    pub fn from_offset(path: impl Into<PathBuf>, offset: u64) -> Self {
        Self {
            path: path.into(),
            offset,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Byte offset just past the last complete line consumed. Persist this to
    /// resume after a restart.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read any lines completed since the last poll.
    ///
    /// A trailing partial line is left in the file and re-read on the next
    /// poll once the writer finishes it. If the file shrank (truncation or
    /// rotation by replacement) the reader restarts from the beginning. A
    /// missing file yields no lines rather than an error, since agent data
    /// files appear and disappear as sessions come and go.
    pub fn poll(&mut self) -> io::Result<Vec<String>> {
        // Reopen on every poll so rotation-by-rename picks up the new file.
        let mut file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };

        let len = file.metadata()?.len();
        if len < self.offset {
            // File was truncated or replaced by a shorter one: start over.
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(Vec::new());
        }

        file.seek(SeekFrom::Start(self.offset))?;
        let mut buf = Vec::with_capacity((len - self.offset) as usize);
        file.read_to_end(&mut buf)?;

        // Consume only up to the last newline; the remainder is a partial
        // line still being written.
        let Some(last_newline) = buf.iter().rposition(|&b| b == b'\n') else {
            return Ok(Vec::new());
        };
        let complete = &buf[..=last_newline];
        self.offset += complete.len() as u64;

        let lines = complete
            .split(|&b| b == b'\n')
            .filter(|l| !l.is_empty())
            .map(|l| {
                let l = l.strip_suffix(b"\r").unwrap_or(l);
                String::from_utf8_lossy(l).into_owned()
            })
            .collect();
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;

    fn append(path: &Path, bytes: &[u8]) {
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        f.write_all(bytes).unwrap();
    }

    #[test]
    fn reads_complete_lines_and_holds_partial_until_finished() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut reader = TailReader::new(&path);

        append(&path, b"{\"a\":1}\n{\"b\":");
        assert_eq!(reader.poll().unwrap(), vec!["{\"a\":1}".to_string()]);
        // Partial line is not consumed and does not advance the offset.
        assert_eq!(reader.offset(), 8);
        assert!(reader.poll().unwrap().is_empty());

        append(&path, b"2}\n");
        assert_eq!(reader.poll().unwrap(), vec!["{\"b\":2}".to_string()]);
        assert_eq!(reader.offset(), 16);
    }

    #[test]
    fn missing_file_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut reader = TailReader::new(dir.path().join("absent.jsonl"));
        assert!(reader.poll().unwrap().is_empty());
    }

    #[test]
    fn restarts_after_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut reader = TailReader::new(&path);

        append(&path, b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(reader.poll().unwrap().len(), 2);

        std::fs::write(&path, b"{\"c\":3}\n").unwrap();
        assert_eq!(reader.poll().unwrap(), vec!["{\"c\":3}".to_string()]);
        assert_eq!(reader.offset(), 8);
    }

    #[test]
    fn restarts_after_rotation_by_rename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let mut reader = TailReader::new(&path);

        append(&path, b"{\"a\":1}\n{\"b\":2}\n");
        assert_eq!(reader.poll().unwrap().len(), 2);

        std::fs::rename(&path, dir.path().join("s.jsonl.old")).unwrap();
        std::fs::write(&path, b"{\"c\":3}\n").unwrap();
        assert_eq!(reader.poll().unwrap(), vec!["{\"c\":3}".to_string()]);
    }

    #[test]
    fn resumes_from_persisted_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        append(&path, b"{\"a\":1}\n{\"b\":2}\n");

        let mut first = TailReader::new(&path);
        assert_eq!(first.poll().unwrap().len(), 2);
        let saved = first.offset();

        append(&path, b"{\"c\":3}\n");
        let mut resumed = TailReader::from_offset(&path, saved);
        assert_eq!(resumed.poll().unwrap(), vec!["{\"c\":3}".to_string()]);
    }

    #[test]
    fn handles_crlf_and_blank_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        append(&path, b"{\"a\":1}\r\n\n{\"b\":2}\n");

        let mut reader = TailReader::new(&path);
        assert_eq!(
            reader.poll().unwrap(),
            vec!["{\"a\":1}".to_string(), "{\"b\":2}".to_string()]
        );
    }
}
