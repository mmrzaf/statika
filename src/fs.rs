use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub struct RootDir {
    fd: OwnedFd,
}

impl RootDir {
    pub fn open(path: &Path) -> io::Result<Self> {
        let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "document root contains NUL")
        })?;
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn open_file(&self, components: &[Vec<u8>]) -> io::Result<File> {
        if components.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "empty path"));
        }

        let mut current = duplicate_fd(self.fd.as_raw_fd())?;
        for (index, component) in components.iter().enumerate() {
            let component = CString::new(component.as_slice())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
            let is_last = index + 1 == components.len();
            let flags = if is_last {
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW
            };
            let fd = unsafe { libc::openat(current.as_raw_fd(), component.as_ptr(), flags) };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            current = unsafe { OwnedFd::from_raw_fd(fd) };
        }

        let file = File::from(current);
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "not a regular file",
            ));
        }
        Ok(file)
    }
}

pub fn gzip_path(components: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut gzip = components.to_vec();
    if let Some(last) = gzip.last_mut() {
        last.extend_from_slice(b".gz");
    }
    gzip
}

pub fn is_not_found(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied | io::ErrorKind::NotADirectory
    ) || matches!(error.raw_os_error(), Some(libc::ELOOP))
}

fn duplicate_fd(fd: libc::c_int) -> io::Result<OwnedFd> {
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duplicated < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

#[cfg(test)]
mod tests {
    use super::RootDir;
    use std::ffi::CString;
    use std::fs;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::symlink;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn open_file_rejects_symlinks() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("statika-fs-{}-{suffix}", std::process::id()));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(base.join("outside.txt"), b"secret").unwrap();
        let outside_dir = base.join("outside");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("secret.txt"), b"secret").unwrap();
        symlink(base.join("outside.txt"), root.join("link.txt")).unwrap();
        symlink(&outside_dir, root.join("linked-dir")).unwrap();

        let root_dir = RootDir::open(&root).unwrap();
        assert!(root_dir.open_file(&[b"link.txt".to_vec()]).is_err());
        assert!(root_dir
            .open_file(&[b"linked-dir".to_vec(), b"secret.txt".to_vec()])
            .is_err());

        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn open_file_rejects_fifo_without_blocking() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("statika-fifo-{}-{suffix}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        let fifo = base.join("pipe");
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);

        let root_dir = RootDir::open(&base).unwrap();
        let start = Instant::now();
        assert!(root_dir.open_file(&[b"pipe".to_vec()]).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));

        fs::remove_dir_all(base).unwrap();
    }
}
