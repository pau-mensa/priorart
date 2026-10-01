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
        create_private_dir(directory)?;
        let file = private_options()
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

/// Creates `directory` and any missing parents readable by the owner only.
pub(crate) fn create_private_dir(directory: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(directory)
}

/// Creates `path` if missing, readable by the owner only. SQLite gives its WAL
/// and shared-memory files the database file's permissions.
pub(crate) fn create_private_file(path: &Path) -> std::io::Result<()> {
    private_options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map(drop)
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
}
