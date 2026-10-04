//! The "fans may be under manual control" marker.
//!
//! It is written (durably) *before* any fan leaves firmware control and
//! removed only after automatic control is confirmed. If the daemon dies in
//! between, the next start (or the service's stop hook) sees the marker and
//! hands the fans back to the firmware.

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Where the marker lives in production.
pub const DEFAULT_FLAG_PATH: &str = "/var/lib/red-queen/manual-fan-control";

/// A durable boolean.
pub trait ManualFlag: Send + Sync + std::fmt::Debug {
    /// Records that manual control may be active. Must be durable when it
    /// returns.
    fn set(&self) -> io::Result<()>;
    /// Records that automatic control is confirmed.
    fn clear(&self) -> io::Result<()>;
    /// Whether the marker is present.
    fn is_set(&self) -> bool;
}

/// A marker file.
#[derive(Debug)]
pub struct FileFlag {
    path: PathBuf,
}

impl FileFlag {
    /// A marker at `path`.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn sync_parent(&self) -> io::Result<()> {
        match self.path.parent() {
            Some(dir) => fs::File::open(dir)?.sync_all(),
            None => Ok(()),
        }
    }
}

impl ManualFlag for FileFlag {
    fn set(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o750)
                .create(dir)?;
        }
        let tmp = self.path.with_extension("tmp");
        {
            let mut f = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o640)
                .open(&tmp)?;
            f.write_all(b"fans may be under manual control\n")?;
            f.sync_all()?;
        }
        fs::rename(&tmp, &self.path)?;
        self.sync_parent()
    }

    fn clear(&self) -> io::Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => self.sync_parent(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn is_set(&self) -> bool {
        self.path.exists()
    }
}

/// An in-memory marker, for tests.
#[derive(Debug, Default)]
pub struct MemoryFlag {
    set: AtomicBool,
    fail_set: AtomicBool,
}

impl MemoryFlag {
    /// A cleared marker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Makes later [`ManualFlag::set`] calls fail, like a full or
    /// read-only disk.
    pub fn fail_on_set(&self, fail: bool) {
        self.fail_set.store(fail, Ordering::SeqCst);
    }
}

impl ManualFlag for MemoryFlag {
    fn set(&self) -> io::Result<()> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(io::Error::other("simulated disk failure"));
        }
        self.set.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn clear(&self) -> io::Result<()> {
        self.set.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn is_set(&self) -> bool {
        self.set.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_flag_lifecycle() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let flag = FileFlag::new(dir.path().join("state/manual-fan-control"));
        assert!(!flag.is_set());
        flag.clear()?; // clearing an unset flag is fine
        flag.set()?;
        assert!(flag.is_set());
        flag.set()?; // idempotent
        flag.clear()?;
        assert!(!flag.is_set());
        Ok(())
    }

    #[test]
    fn file_flag_is_private_and_leaves_no_temp_file() -> io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state/manual-fan-control");
        let flag = FileFlag::new(&path);
        flag.set()?;
        let mode = fs::metadata(&path)?.permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
        let dir_mode = fs::metadata(path.parent().expect("parent"))?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o750);
        let names: Vec<_> = fs::read_dir(path.parent().expect("parent"))?
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        Ok(())
    }

    #[test]
    fn unwritable_location_is_an_error_not_a_silent_success() {
        let flag = FileFlag::new("/proc/definitely/not/writable/flag");
        assert!(flag.set().is_err());
    }

    #[test]
    fn memory_flag_can_simulate_failure() {
        let f = MemoryFlag::new();
        f.fail_on_set(true);
        assert!(f.set().is_err());
        assert!(!f.is_set());
        f.fail_on_set(false);
        assert!(f.set().is_ok() && f.is_set());
    }
}
