//! Crash-safe file replacement.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

/// Replace the contents of `path` atomically.
///
/// The data goes to a temporary file in the same directory, which is flushed to
/// disk and renamed over the original. Readers see either the old or the new
/// notebook, never an empty or half-written one, even if nbripout is killed or the
/// disk fills up. Symlinks are followed so the link itself stays intact, and the
/// original permission bits are preserved.
pub fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path)?;
    let dir = target.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "path has no parent directory")
    })?;
    let permissions = fs::metadata(&target)?.permissions();

    let mut tmp = tempfile::Builder::new()
        .prefix(".nbripout-")
        .suffix(".tmp")
        .tempfile_in(dir)?;
    tmp.write_all(data)?;
    tmp.as_file().set_permissions(permissions)?;
    sync(tmp.as_file())?;
    tmp.persist(&target).map_err(|e| e.error)?;
    Ok(())
}

/// Flush the file to the storage device before it replaces the original.
///
/// On Apple platforms `File::sync_all` issues `F_FULLFSYNC`, which drains the
/// whole drive cache and can cost milliseconds per file right after other writes;
/// a plain `fsync`, as used by most macOS software, is enough for ordering the
/// data before the rename.
#[cfg(unix)]
fn sync(file: &fs::File) -> io::Result<()> {
    rustix::fs::fsync(file).map_err(io::Error::from)
}

#[cfg(not(unix))]
fn sync(file: &fs::File) -> io::Result<()> {
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_content_and_keeps_symlinks_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.ipynb");
        fs::write(&real, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&real, fs::Permissions::from_mode(0o640)).unwrap();
            let link = dir.path().join("link.ipynb");
            std::os::unix::fs::symlink(&real, &link).unwrap();
            write_atomic(&link, b"new").unwrap();
            assert!(fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink());
            let mode = fs::metadata(&real).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o640);
        }
        #[cfg(not(unix))]
        write_atomic(&real, b"new").unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"new");
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            if cfg!(unix) { 2 } else { 1 }
        );
    }
}
