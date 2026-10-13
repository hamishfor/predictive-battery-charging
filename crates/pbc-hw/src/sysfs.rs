use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Root of a sysfs-like tree. `SysRoot::system()` is the real `/sys`.
#[derive(Debug, Clone)]
pub struct SysRoot {
    root: PathBuf,
}

impl SysRoot {
    pub fn system() -> Self {
        Self::new("/sys")
    }

    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn path(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.root.join(rel)
    }

    pub fn read(&self, rel: impl AsRef<Path>) -> io::Result<String> {
        fs::read_to_string(self.path(rel)).map(|s| s.trim().to_string())
    }

    pub fn read_opt(&self, rel: impl AsRef<Path>) -> Option<String> {
        self.read(rel).ok()
    }

    pub fn read_u64(&self, rel: impl AsRef<Path>) -> Option<u64> {
        self.read_opt(rel)?.parse().ok()
    }

    pub fn write(&self, rel: impl AsRef<Path>, value: &str) -> io::Result<()> {
        fs::write(self.path(rel), value)
    }

    pub fn exists(&self, rel: impl AsRef<Path>) -> bool {
        self.path(rel).exists()
    }

    pub fn list(&self, rel: impl AsRef<Path>) -> io::Result<Vec<String>> {
        let mut out: Vec<String> = fs::read_dir(self.path(rel))?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        Ok(out)
    }
}
