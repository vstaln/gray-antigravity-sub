use std::io::Write;
use std::path::{Path, PathBuf};

/// Write `bytes` to `path` atomically (temp file + rename), mode 0600,
/// creating the parent directory (0700). Concurrent writers never expose
/// a torn file: readers see the old content or the new, whole.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    private_dir(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        let r = f.write_all(bytes).and_then(|_| f.flush());
        drop(f);
        r?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub(crate) fn lock_file(path: &Path) -> std::io::Result<std::fs::File> {
    private_dir(path.parent().unwrap_or(Path::new(".")))?;
    let mut lock = path.as_os_str().to_os_string();
    lock.push(".lock");
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let file = opts.open(PathBuf::from(lock))?;
    file.lock()?;
    Ok(file)
}

fn private_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}
