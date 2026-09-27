//! A process-held advisory lock; the inode is never removed or replaced.
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub(crate) struct DirectoryOwner {
    _file: File,
}
impl DirectoryOwner {
    pub(crate) fn acquire(directory: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("writer.lock"))?;
        fs2::FileExt::try_lock_exclusive(&file).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                std::io::Error::new(e.kind(), "the data directory already has a writer")
            } else {
                e
            }
        })?;
        Ok(Self { _file: file })
    }
}
