//! Access to the system's pseudo-filesystems through a configurable root.

use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use tracing::debug;

/// Largest file we read from sysfs/procfs/etc. Real attributes are tiny;
/// the cap protects against unexpected large files.
const MAX_READ: u64 = 64 * 1024;

/// The root that `/sys`, `/proc` and `/etc` paths are resolved against:
/// `/` on a real system, a temporary directory in tests.
#[derive(Debug, Clone)]
pub struct SystemRoot {
    root: PathBuf,
}

impl SystemRoot {
    /// The real system.
    pub fn host() -> Self {
        Self::at("/")
    }

    /// A tree rooted at `root`.
    pub fn at(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = fs::canonicalize(&root).unwrap_or(root);
        Self { root }
    }

    /// Resolves an absolute system path inside this root.
    pub fn path(&self, abs: impl AsRef<Path>) -> PathBuf {
        let abs = abs.as_ref();
        self.root.join(abs.strip_prefix("/").unwrap_or(abs))
    }

    /// Reads a small text file, without the trailing newline.
    /// Missing, unreadable or non-UTF-8 files give `None`.
    pub fn read_string(&self, abs: impl AsRef<Path>) -> Option<String> {
        let path = self.path(abs);
        match read_capped(&path) {
            Ok(s) => Some(s.trim_end_matches(['\n', '\r']).to_owned()),
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    debug!(path = %path.display(), error = %e, "unreadable");
                }
                None
            }
        }
    }

    /// Reads and parses a file; `None` if missing or malformed.
    pub fn read_parse<T: FromStr>(&self, abs: impl AsRef<Path>) -> Option<T> {
        self.read_string(abs)?.trim().parse().ok()
    }

    /// Writes `value` to an **existing** attribute file in a single write.
    ///
    /// Never creates files. Callers must only pass paths that discovery
    /// found and that an operation's allow-list names; this function does
    /// no policy checks of its own.
    pub fn write_attr(&self, abs: impl AsRef<Path>, value: &str) -> io::Result<()> {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(self.path(abs))?;
        file.write_all(value.as_bytes())
    }

    /// Whether the path exists (following symlinks).
    pub fn exists(&self, abs: impl AsRef<Path>) -> bool {
        self.path(abs).exists()
    }

    /// Whether the path exists, without following a final symlink.
    pub fn exists_no_follow(&self, abs: impl AsRef<Path>) -> bool {
        fs::symlink_metadata(self.path(abs)).is_ok()
    }

    /// Whether the current process may read the file.
    pub fn is_readable(&self, abs: impl AsRef<Path>) -> bool {
        fs::File::open(self.path(abs)).is_ok()
    }

    /// Unix permission bits, if the file exists.
    pub fn mode(&self, abs: impl AsRef<Path>) -> Option<u32> {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(self.path(abs))
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
    }

    /// Sorted entry names of a directory; empty if missing.
    pub fn list_dir(&self, abs: impl AsRef<Path>) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path(abs))
            .map(|it| {
                it.filter_map(Result::ok)
                    .filter_map(|e| e.file_name().into_string().ok())
                    .collect()
            })
            .unwrap_or_default();
        names.sort_unstable_by(|a, b| natural_cmp(a, b));
        names
    }

    /// The last component of a symlink's target (for example the driver
    /// name behind `.../driver`).
    pub fn link_name(&self, abs: impl AsRef<Path>) -> Option<String> {
        let target = fs::read_link(self.path(abs)).ok()?;
        target.file_name()?.to_str().map(str::to_owned)
    }

    /// The canonical system path (as seen from inside the root).
    pub fn canonical(&self, abs: impl AsRef<Path>) -> Option<PathBuf> {
        let real = fs::canonicalize(self.path(abs)).ok()?;
        let rel = real.strip_prefix(&self.root).ok()?;
        Some(Path::new("/").join(rel))
    }
}

fn read_capped(path: &Path) -> io::Result<String> {
    let mut s = String::new();
    fs::File::open(path)?
        .take(MAX_READ)
        .read_to_string(&mut s)?;
    Ok(s)
}

/// Orders names so that `hwmon2` sorts before `hwmon10`.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let split = |s: &str| {
        let digits = s.len() - s.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let (head, tail) = s.split_at(s.len() - digits);
        (head.to_owned(), tail.parse::<u64>().ok())
    };
    let (ha, na) = split(a);
    let (hb, nb) = split(b);
    ha.cmp(&hb).then(na.cmp(&nb)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn natural_order() {
        let mut v = vec!["hwmon10", "hwmon2", "hwmon1", "acpitz"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["acpitz", "hwmon1", "hwmon2", "hwmon10"]);
    }

    #[test]
    fn path_resolution() {
        let r = SystemRoot {
            root: PathBuf::from("/tmp/x"),
        };
        assert_eq!(r.path("/sys/class"), PathBuf::from("/tmp/x/sys/class"));
    }
}
