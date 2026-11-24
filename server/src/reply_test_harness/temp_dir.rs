use std::path::{Path, PathBuf};

pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("reply-test-harness-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).expect("create a temp dir for the reply test harness");
        Self(path)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
