//! Build and activation output is captured to files, not database blobs. The
//! generation record keeps only a reference — the path, the total size, and the
//! last few lines — so history stays cheap to query while the full stream stays
//! available on disk.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::generations::LogRef;

/// Number of trailing lines kept for the at-a-glance tail.
const TAIL_LINES: usize = 40;

/// A log being written line by line as output streams in. `finish` seals it and
/// returns the reference stored on the generation.
pub struct LogFile {
    writer: BufWriter<File>,
    path: PathBuf,
    size: u64,
    tail: VecDeque<String>,
}

impl LogFile {
    pub fn create(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let file = File::create(&path).map_err(|e| Error::io(&path, e))?;
        Ok(Self {
            writer: BufWriter::new(file),
            path,
            size: 0,
            tail: VecDeque::with_capacity(TAIL_LINES + 1),
        })
    }

    /// Append one line (a trailing newline is added).
    pub fn write_line(&mut self, line: &str) -> Result<()> {
        self.writer
            .write_all(line.as_bytes())
            .and_then(|_| self.writer.write_all(b"\n"))
            .map_err(|e| Error::io(&self.path, e))?;
        self.size += line.len() as u64 + 1;
        self.tail.push_back(line.to_string());
        if self.tail.len() > TAIL_LINES {
            self.tail.pop_front();
        }
        Ok(())
    }

    /// Flush and return the reference to store on the generation.
    pub fn finish(mut self) -> Result<LogRef> {
        self.writer.flush().map_err(|e| Error::io(&self.path, e))?;
        Ok(LogRef {
            path: self.path.to_string_lossy().into_owned(),
            size: self.size,
            tail: self.tail.into_iter().collect::<Vec<_>>().join("\n"),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_size_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut log = LogFile::create(dir.path().join("1-build.log")).unwrap();
        for i in 0..100 {
            log.write_line(&format!("line {i}")).unwrap();
        }
        let r = log.finish().unwrap();

        let on_disk = std::fs::read_to_string(&r.path).unwrap();
        assert_eq!(on_disk.lines().count(), 100);
        assert_eq!(r.size as usize, on_disk.len());
        // Tail holds only the last TAIL_LINES lines, ending at the last one.
        assert_eq!(r.tail.lines().count(), TAIL_LINES);
        assert!(r.tail.ends_with("line 99"));
        assert!(!r.tail.contains("line 0\n"));
    }
}
