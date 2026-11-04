//! The log file, beside the binary.
//!
//! A terminal app hides its own failures: the message is one line on a status row and it is
//! gone the moment the next key arrives. A file beside the binary keeps the reason a
//! download stopped, which is the question this file exists to answer.
//!
//! The policy is size rotation, not the daily rotation the daemon uses. A front end someone
//! runs for a minute at a time writes a few kilobytes a day, and a directory of dated files
//! would be noise; one file that never grows past a few megabytes is what this program
//! needs.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::EnvFilter;

/// The file's name, beside the executable.
pub const NAME: &str = "arioso-speaks.log";

/// How large the file may grow before it is replaced.
const LIMIT: u64 = 4 * 1024 * 1024;

/// Where the log goes: beside the binary, or in the data directory if that is not writable.
#[must_use]
pub fn path() -> PathBuf {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && dir.is_dir()
    {
        return dir.join(NAME);
    }
    arioso_models::data_dir().join(NAME)
}

/// Start logging, and say where the records go.
///
/// `level` is the config's level, and `ARIOSO_LOG` overrides it, so a person debugging one
/// run does not have to edit a file. A log that cannot be opened is reported on stderr and
/// the program continues: losing the log is not a reason to lose the program.
pub fn init(level: &str) -> Option<PathBuf> {
    let filter = EnvFilter::try_from_env("ARIOSO_LOG")
        .or_else(|_| EnvFilter::try_new(level))
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let path = path();
    match Rotating::open(&path, LIMIT) {
        Ok(writer) => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                // A file has no terminal, so escape codes would be noise in it.
                .with_ansi(false)
                .with_writer(writer)
                .try_init();
            Some(path)
        }
        Err(error) => {
            eprintln!(
                "cannot write a log to {} ({error}); logging to stderr instead",
                path.display()
            );
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_writer(std::io::stderr)
                .try_init();
            None
        }
    }
}

/// A writer that replaces the file with itself once it passes a size.
#[derive(Debug)]
struct Rotating {
    state: Mutex<Open>,
}

#[derive(Debug)]
struct Open {
    path: PathBuf,
    limit: u64,
    file: File,
    written: u64,
}

impl Rotating {
    fn open(path: &Path, limit: u64) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            state: Mutex::new(Open {
                path: path.to_path_buf(),
                limit,
                file,
                written,
            }),
        })
    }
}

impl Write for &Rotating {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut open = match self.state.lock() {
            Ok(guard) => guard,
            // A panic elsewhere must not cost the records that follow it.
            Err(poisoned) => poisoned.into_inner(),
        };
        open.roll(buf.len() as u64);
        let written = open.file.write(buf)?;
        open.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.state.lock() {
            Ok(mut open) => open.file.flush(),
            Err(poisoned) => poisoned.into_inner().file.flush(),
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Rotating {
    type Writer = &'a Rotating;

    fn make_writer(&'a self) -> Self::Writer {
        self
    }
}

impl Open {
    /// Start a new file when this record would pass the limit.
    fn roll(&mut self, incoming: u64) {
        if self.written + incoming <= self.limit {
            return;
        }
        let _ = self.file.flush();
        // One previous file is kept: the failure usually happened just before the rotation.
        let previous = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&previous);
        if std::fs::rename(&self.path, &previous).is_err() {
            // A rename that fails is not a reason to stop writing.
            self.written = 0;
            return;
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(file) => {
                self.file = file;
                self.written = 0;
            }
            Err(error) => {
                // Keep the old handle rather than lose every later record.
                eprintln!(
                    "log file {} could not be reopened ({error})",
                    self.path.display()
                );
                self.written = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_small_file_is_not_rolled() {
        let dir = std::env::temp_dir().join(format!("speaks-log-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.log");
        let _ = std::fs::remove_file(&path);
        let rotating = Rotating::open(&path, 64).expect("open");
        (&rotating).write_all(b"one\n").expect("write");
        (&rotating).write_all(b"two\n").expect("write");
        assert!(path.exists());
        assert!(!path.with_extension("log.1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn passing_the_limit_moves_the_old_file_aside() {
        let dir = std::env::temp_dir().join(format!("speaks-roll-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("t.log");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(path.with_extension("log.1"));
        let rotating = Rotating::open(&path, 8).expect("open");
        (&rotating).write_all(b"12345678\n").expect("write");
        (&rotating).write_all(b"more\n").expect("write");
        let previous = path.with_extension("log.1");
        assert!(previous.exists(), "the old file should have been kept");
        let kept = std::fs::read_to_string(&previous).expect("read");
        assert!(kept.contains("12345678"), "{kept:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
