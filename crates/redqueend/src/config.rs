//! Daemon configuration read from `/etc/red-queen/`.

use std::io::{self, Read};
use std::path::Path;

use rq_core::{SafetyConfig, SafetyFile};

/// Where the safety limits live.
pub const SAFETY_FILE: &str = "/etc/red-queen/safety.toml";

/// Largest config file accepted.
const MAX_BYTES: u64 = 16 * 1024;

fn read_capped(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file is too large",
        ));
    }
    Ok(text)
}

/// Loads the safety limits. Never fails: a missing file means defaults, and
/// an unreadable or invalid one means defaults plus a warning. Values are
/// clamped into safe ranges, so a bad file can't loosen a limit beyond what
/// the code allows.
pub fn load_safety(path: &Path) -> (SafetyConfig, Vec<String>) {
    let text = match read_capped(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (SafetyConfig::default(), vec![]),
        Err(e) => {
            return (
                SafetyConfig::default(),
                vec![format!(
                    "cannot read {}: {e}; using the defaults",
                    path.display()
                )],
            );
        }
    };
    match toml::from_str::<SafetyFile>(&text) {
        Ok(file) => file.into_config(),
        Err(e) => (
            SafetyConfig::default(),
            vec![format!(
                "invalid {}: {e}; using the defaults",
                path.display().to_string()
            )],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rq_core::HARD_MIN_PERCENT;

    fn load(text: &str) -> (SafetyConfig, Vec<String>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("safety.toml");
        std::fs::write(&path, text).expect("write");
        load_safety(&path)
    }

    #[test]
    fn missing_file_means_defaults_silently() {
        let (c, w) = load_safety(Path::new("/nonexistent/safety.toml"));
        assert_eq!(c, SafetyConfig::default());
        assert!(w.is_empty());
    }

    #[test]
    fn partial_file_overrides_only_what_it_names() {
        let (c, w) = load("min_fan_percent = 45\ncritical_cpu_celsius = 85\n");
        assert_eq!(c.min_percent.get(), 45);
        assert_eq!(c.critical_cpu.celsius() as u32, 85);
        assert_eq!(c.critical_gpu, SafetyConfig::default().critical_gpu);
        assert!(w.is_empty());
    }

    #[test]
    fn out_of_range_values_are_clamped_with_warnings() {
        let (c, w) = load("min_fan_percent = 0\ncritical_cpu_celsius = 150\n");
        assert_eq!(c.min_percent.get(), HARD_MIN_PERCENT);
        assert_eq!(c.critical_cpu.celsius() as u32, 100);
        assert_eq!(w.len(), 2, "{w:?}");
    }

    #[test]
    fn typos_and_garbage_fall_back_to_defaults_with_a_warning() {
        for bad in [
            "min_fan_pecent = 10\n",
            "this is not toml",
            "min_fan_percent = \"high\"\n",
        ] {
            let (c, w) = load(bad);
            assert_eq!(c, SafetyConfig::default(), "{bad:?}");
            assert_eq!(w.len(), 1, "{bad:?}: {w:?}");
        }
    }

    #[test]
    fn oversized_files_are_refused() {
        let (c, w) = load(&"# pad\n".repeat(10_000));
        assert_eq!(c, SafetyConfig::default());
        assert_eq!(w.len(), 1);
    }
}
