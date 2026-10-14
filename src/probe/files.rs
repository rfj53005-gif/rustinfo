use std::fs;
use std::path::{Path, PathBuf};
pub(super) fn read_trimmed(p: &Path) -> Option<String> {
    fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

pub(super) fn read_num<T: std::str::FromStr>(p: &Path) -> Option<T> {
    read_trimmed(p)?.parse().ok()
}

/// Paths are injectable so collectors can be exercised against captured sysfs/procfs.
#[derive(Clone)]
pub(super) struct Paths {
    pub proc_root: PathBuf,
    pub sys_root: PathBuf,
    pub dev_root: PathBuf,
}

impl Default for Paths {
    fn default() -> Self {
        Self {
            proc_root: "/proc".into(),
            sys_root: "/sys".into(),
            dev_root: "/dev".into(),
        }
    }
}

impl Paths {
    pub fn proc(&self, relative: &str) -> PathBuf {
        self.proc_root.join(relative)
    }
    pub fn sys(&self, relative: &str) -> PathBuf {
        self.sys_root.join(relative)
    }
    pub fn dev(&self, relative: &str) -> PathBuf {
        self.dev_root.join(relative)
    }
}
