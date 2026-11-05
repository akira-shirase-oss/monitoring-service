//! A bounded log file: writes rotate to `<name>.1` once `max_bytes` would be exceeded,
//! keeping exactly two generations. This is deliberately not `tracing-appender`'s time-
//! based rotation — the contract is a *byte ceiling* (disk guardrail, M4.2), not a
//! calendar. Stdout stays the default; `SMON_LOG_FILE` opts into this.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Shareable handle: `tracing_subscriber` clones it per write batch, every clone locks
/// the same inner file so rotation stays single-filed.
#[derive(Debug, Clone)]
pub struct BoundedLog(Arc<Mutex<RotatingFile>>);

impl BoundedLog {
    /// Open `path` for appending, rotating immediately if a previous run already left it
    /// over the ceiling. `max_bytes` is per live file; total footprint is at most
    /// `2 * max_bytes` (live + `.1`).
    pub fn open(path: impl AsRef<Path>, max_bytes: u64) -> io::Result<Self> {
        let mut inner = RotatingFile::open(path.as_ref(), max_bytes.max(1))?;
        if inner.size >= inner.max_bytes {
            inner.rotate()?;
        }
        Ok(Self(Arc::new(Mutex::new(inner))))
    }
}

impl Write for BoundedLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?
            .write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("log lock poisoned"))?
            .flush()
    }
}

impl<'a> tracing_subscriber::fmt::writer::MakeWriter<'a> for BoundedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[derive(Debug)]
struct RotatingFile {
    path: PathBuf,
    max_bytes: u64,
    file: File,
    size: u64,
}

impl RotatingFile {
    fn open(path: &Path, max_bytes: u64) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            max_bytes,
            file,
            size,
        })
    }

    fn rotated_path(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push(".1");
        PathBuf::from(name)
    }

    /// Close the live file, move it to `.1` (clobbering the previous generation), and
    /// reopen fresh. A line that would straddle the ceiling lands in the new file — the
    /// bound is checked before each write, so the file can never grow past the ceiling
    /// by more than the tail call raced past it, and that is at most one write batch.
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        std::fs::rename(&self.path, self.rotated_path())?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

impl Write for RotatingFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.size > 0 && self.size + buf.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn writes_under_the_ceiling_stay_in_one_file() {
        let tmp = dir();
        let path = tmp.path().join("monitor.log");
        let mut log = BoundedLog::open(&path, 4096).expect("open");
        log.write_all(b"line one\n").expect("write");
        log.flush().expect("flush");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "line one\n");
        assert!(!path.with_file_name("monitor.log.1").exists());
    }

    #[test]
    fn crossing_the_ceiling_rotates_and_keeps_two_generations() {
        let tmp = dir();
        let path = tmp.path().join("monitor.log");
        let mut log = BoundedLog::open(&path, 128).expect("open");
        let chunk = [b'x'; 100];
        for _ in 0..4 {
            log.write_all(&chunk).expect("write");
        }
        log.flush().expect("flush");
        let live = std::fs::metadata(&path).expect("live").len();
        let rotated = std::fs::metadata(tmp.path().join("monitor.log.1"))
            .expect("rotated generation exists")
            .len();
        assert!(live <= 128, "live file bounded: {live}");
        assert!(rotated > 0 && rotated <= 200, "one prior generation kept");
        assert!(!tmp.path().join("monitor.log.2").exists());
    }

    #[test]
    fn a_second_rotation_discards_the_oldest_generation() {
        let tmp = dir();
        let path = tmp.path().join("monitor.log");
        let mut log = BoundedLog::open(&path, 64).expect("open");
        for i in 0..8u8 {
            log.write_all(format!("line-{i:02}-{}\n", "y".repeat(50)).as_bytes())
                .expect("write");
        }
        log.flush().expect("flush");
        let oldest =
            std::fs::read_to_string(tmp.path().join("monitor.log.1")).expect("rotated generation");
        assert!(oldest.contains("line-"), "rotated keeps one generation");
        assert!(!tmp.path().join("monitor.log.2").exists());
    }
}
