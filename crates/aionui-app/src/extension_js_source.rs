//! Independent, opt-in source-byte reader; no execution or authorization.
//!
//! The host must acquire a trustworthy already-open root directory. Success
//! proves neither curation, user ownership nor immutable content (including
//! hardlinks/in-place writes). The byte bound is NOT an I/O wall-clock bound.
use std::{fs::File, path::Path};

/// Fixed, non-sensitive categories; no paths or OS error text escape this API.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceError {
    Unavailable,
    InvalidPath,
    WrongRootKind,
    Resolve,
    NotRegular,
    ProcUnavailable,
    IdentityChanged,
    Io,
    TooLarge,
    InvalidUtf8,
}

/// Read at most 65536 UTF-8 source bytes relative to a host-provided root.
///
/// Linux requires openat2 and a trusted host /proc mount exposing this process's
/// fd view, with no malicious mount substitution. fstatfs establishes procfs
/// type only, not ownership of that view; O_NOFOLLOW covers the final component
/// only. A privileged mount substitution could cause the read-open to activate
/// a wrong target before the subsequent identity check. This is outside the
/// ordinary handler-directory writer threat model. Detected failures are closed,
/// with no original-path/canonicalization fallback. Symlinks and mount crossings are
/// rejected. URL-like colon-containing paths are not accepted. Nothing is run.
/// Disabled/non-Linux builds return immediately without inspecting root/path.
pub fn read_bounded_source(root: &File, handler: &Path) -> Result<String, SourceError> {
    #[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
    {
        enabled::read(root, handler)
    }
    #[cfg(not(all(feature = "experimental-extension-js", target_os = "linux")))]
    {
        let _ = (root, handler);
        Err(SourceError::Unavailable)
    }
}

#[cfg(all(feature = "experimental-extension-js", target_os = "linux"))]
mod enabled {
    use super::*;
    use std::{
        ffi::CString,
        io::{self, Read},
        mem::MaybeUninit,
        os::{
            fd::{AsRawFd, FromRawFd, RawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };

    const LIMIT: u64 = 64 * 1024;
    // Linux openat2 UAPI: three u64 fields, irrespective of pointer width.
    // repr(C) and the exact struct size are part of this syscall ABI.
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    const RESOLVE_NO_XDEV: u64 = 0x01;
    const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
    const RESOLVE_NO_SYMLINKS: u64 = 0x04;
    const RESOLVE_BENEATH: u64 = 0x08;

    fn stat(file: &File) -> Result<libc::stat, SourceError> {
        let mut value = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: the live borrowed fd and writable aligned stat storage remain
        // valid throughout fstat. Only success initializes the entire C value.
        if unsafe { libc::fstat(file.as_raw_fd(), value.as_mut_ptr()) } != 0 {
            return Err(SourceError::Io);
        }
        // SAFETY: successful fstat initialized value; no fd ownership transfers.
        Ok(unsafe { value.assume_init() })
    }

    fn lexical(handler: &Path) -> Result<CString, SourceError> {
        let bytes = handler.as_os_str().as_bytes();
        if bytes.is_empty()
            || bytes.contains(&b':')
            || handler
                .components()
                .any(|c| matches!(c, Component::RootDir | Component::Prefix(_) | Component::ParentDir))
        {
            return Err(SourceError::InvalidPath);
        }
        CString::new(bytes).map_err(|_| SourceError::InvalidPath)
    }

    fn resolve_error(error: io::Error) -> SourceError {
        match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EPERM | libc::EINVAL | libc::E2BIG) => SourceError::Unavailable,
            _ => SourceError::Resolve,
        }
    }

    fn pin(root: &File, handler: &Path) -> Result<(File, libc::stat), SourceError> {
        let path = lexical(handler)?;
        if stat(root)?.st_mode & libc::S_IFMT != libc::S_IFDIR {
            return Err(SourceError::WrongRootKind);
        }
        let how = OpenHow {
            flags: (libc::O_PATH | libc::O_CLOEXEC) as u64,
            mode: 0,
            // NO_XDEV deliberately also rejects bind/submount crossings even
            // when their paths are beneath the held root.
            resolve: RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_XDEV,
        };
        // SAFETY: syscall's variadic ABI receives an int dirfd, a NUL-terminated
        // path, a pointer to the repr(C) UAPI struct, and its size_t byte size.
        // All referenced storage and the borrowed root fd outlive this call.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                root.as_raw_fd(),
                path.as_ptr(),
                &how as *const OpenHow,
                size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(resolve_error(io::Error::last_os_error()));
        }
        // SAFETY: successful openat2 returns one new owned fd; File alone closes
        // it, including on every error path. O_PATH does not open device drivers.
        let pinned = unsafe { File::from_raw_fd(fd as RawFd) };
        let metadata = stat(&pinned)?;
        if metadata.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(SourceError::NotRegular);
        }
        Ok((pinned, metadata))
    }

    fn proc_directory() -> Result<File, SourceError> {
        // Host precondition: /proc and its mount namespace are trusted, with no
        // malicious mount substitution. fstatfs below checks procfs type only,
        // NOT whether this directory is our current process's fd view. Another
        // real procfs fd directory could reopen a wrong target (and activate a
        // device) before the later identity check; privileged mount control is
        // outside the ordinary handler-directory writer threat model.
        // SAFETY: fixed NUL-terminated path; no mode arg is required without
        // O_CREAT. O_NOFOLLOW protects only the final path component.
        let fd = unsafe {
            libc::open(
                c"/proc/self/fd".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if fd < 0 {
            return Err(SourceError::ProcUnavailable);
        }
        // SAFETY: open returned a new owned fd; transfer ownership exactly once.
        let directory = unsafe { File::from_raw_fd(fd) };
        let mut fs = MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: directory stays alive and fs is writable aligned storage.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
            return Err(SourceError::ProcUnavailable);
        }
        // SAFETY: only a successful fstatfs reaches this initialized read.
        if unsafe { fs.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
            return Err(SourceError::ProcUnavailable);
        }
        Ok(directory)
    }

    fn read_pinned(pinned: &File, metadata: &libc::stat) -> Result<String, SourceError> {
        let directory = proc_directory()?;
        // Decimal digits only, derived from OUR live pinned fd, never a caller
        // proc path. Keeping pinned alive prevents descriptor reuse here.
        let number = CString::new(pinned.as_raw_fd().to_string()).map_err(|_| SourceError::ProcUnavailable)?;
        // SAFETY: valid directory fd and CString. Intentionally follow ONLY
        // this kernel fd magiclink, after requiring the O_PATH inode be REG,
        // relying on the trusted host /proc view precondition documented above.
        // O_NONBLOCK alone before that check would still open device drivers.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                number.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOCTTY,
            )
        };
        if fd < 0 {
            return Err(SourceError::ProcUnavailable);
        }
        // SAFETY: successful openat returns a new fd owned only by reader.
        let reader = unsafe { File::from_raw_fd(fd) };
        let current = stat(&reader)?;
        if current.st_mode & libc::S_IFMT != libc::S_IFREG
            || current.st_dev != metadata.st_dev
            || current.st_ino != metadata.st_ino
        {
            return Err(SourceError::IdentityChanged);
        }
        let mut bytes = Vec::new();
        // Read from this SAME verified fd. Metadata size alone cannot bound
        // concurrent growth; the extra byte detects overflow without unbounded IO.
        reader
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SourceError::Io)?;
        if bytes.len() > LIMIT as usize {
            return Err(SourceError::TooLarge);
        }
        String::from_utf8(bytes).map_err(|_| SourceError::InvalidUtf8)
    }

    pub(super) fn read(root: &File, handler: &Path) -> Result<String, SourceError> {
        let (pinned, metadata) = pin(root, handler)?;
        read_pinned(&pinned, &metadata)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::{fs, os::unix::fs::symlink};
        use tempfile::TempDir;

        fn fixture() -> (TempDir, File) {
            let temp = tempfile::tempdir().unwrap();
            let root = File::open(temp.path()).unwrap();
            (temp, root)
        }

        #[test]
        fn relative_and_nested_regular() {
            let (temp, root) = fixture();
            fs::create_dir(temp.path().join("nested")).unwrap();
            for path in ["handler.js", "nested/handler.js"] {
                fs::write(temp.path().join(path), "export const x = '你好';").unwrap();
                assert_eq!(
                    read_bounded_source(&root, Path::new(path)).unwrap(),
                    "export const x = '你好';"
                );
            }
        }

        #[test]
        fn exact_limit_overflow_and_invalid_utf8() {
            let (temp, root) = fixture();
            for (len, expected) in [(65536, Ok("a".repeat(65536))), (65537, Err(SourceError::TooLarge))] {
                fs::write(temp.path().join("source"), vec![b'a'; len]).unwrap();
                assert_eq!(read_bounded_source(&root, Path::new("source")), expected);
            }
            fs::write(temp.path().join("source"), [0xff]).unwrap();
            assert_eq!(
                read_bounded_source(&root, Path::new("source")),
                Err(SourceError::InvalidUtf8)
            );
            fs::write(temp.path().join("source"), []).unwrap();
            assert_eq!(read_bounded_source(&root, Path::new("source")), Ok(String::new()));
        }

        #[test]
        fn lexical_rejections() {
            let (_temp, root) = fixture();
            for path in [
                "",
                "/etc/passwd",
                "../source",
                "nested/../source",
                "x\0y",
                "https://example/source",
                "file:source",
                "node:source",
            ] {
                assert_eq!(
                    read_bounded_source(&root, Path::new(path)),
                    Err(SourceError::InvalidPath)
                );
            }
        }

        #[test]
        fn wrong_root_and_nonregular_without_read_open() {
            let (temp, root) = fixture();
            fs::write(temp.path().join("file"), "ok").unwrap();
            let wrong = File::open(temp.path().join("file")).unwrap();
            assert_eq!(
                read_bounded_source(&wrong, Path::new("file")),
                Err(SourceError::WrongRootKind)
            );
            fs::create_dir(temp.path().join("dir")).unwrap();
            let fifo = CString::new(temp.path().join("fifo").as_os_str().as_bytes()).unwrap();
            // SAFETY: fixture-owned path and valid mode; creates but never opens a FIFO.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            for path in [".", "dir", "fifo"] {
                assert_eq!(
                    read_bounded_source(&root, Path::new(path)),
                    Err(SourceError::NotRegular)
                );
            }
        }

        #[test]
        fn all_symlink_positions_rejected() {
            let (temp, root) = fixture();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("file"), "outside").unwrap();
            fs::create_dir(temp.path().join("inside")).unwrap();
            fs::write(temp.path().join("inside/file"), "inside").unwrap();
            symlink("inside/file", temp.path().join("final")).unwrap();
            symlink("inside", temp.path().join("intermediate")).unwrap();
            symlink(outside.path(), temp.path().join("outside")).unwrap();
            symlink(outside.path().join("file"), temp.path().join("outside-final")).unwrap();
            for path in ["final", "intermediate/file", "outside/file", "outside-final"] {
                assert_eq!(read_bounded_source(&root, Path::new(path)), Err(SourceError::Resolve));
            }
        }

        #[test]
        fn atomic_replacement_keeps_pinned_inode_and_held_root() {
            let (temp, root) = fixture();
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("file"), "outside").unwrap();
            fs::create_dir(temp.path().join("root")).unwrap();
            fs::write(temp.path().join("root/file"), "inside").unwrap();
            let held = File::open(temp.path().join("root")).unwrap();
            let (pinned, metadata) = pin(&held, Path::new("file")).unwrap();
            // Deterministically replace the handler after pinning; no timing gamble.
            symlink(outside.path().join("file"), temp.path().join("replacement")).unwrap();
            fs::rename(temp.path().join("replacement"), temp.path().join("root/file")).unwrap();
            assert_eq!(read_pinned(&pinned, &metadata).unwrap(), "inside");
            assert_eq!(read_bounded_source(&held, Path::new("file")), Err(SourceError::Resolve));
            fs::remove_file(temp.path().join("root/file")).unwrap();
            fs::write(temp.path().join("root/file"), "held").unwrap();
            fs::rename(temp.path().join("root"), temp.path().join("moved")).unwrap();
            symlink(outside.path(), temp.path().join("root")).unwrap();
            assert_eq!(read_bounded_source(&held, Path::new("file")).unwrap(), "held");
            // This is path/inode pinning, NOT a promise against hardlinks or
            // in-place content mutation by another writer.
            drop(root);
        }

        #[test]
        fn unavailable_errno_mapping_only_not_fault_injection() {
            for errno in [libc::ENOSYS, libc::EPERM, libc::EINVAL, libc::E2BIG] {
                assert_eq!(
                    resolve_error(io::Error::from_raw_os_error(errno)),
                    SourceError::Unavailable
                );
            }
            for errno in [libc::ENOENT, libc::ELOOP, libc::EXDEV, libc::EAGAIN] {
                assert_eq!(resolve_error(io::Error::from_raw_os_error(errno)), SourceError::Resolve);
            }
        }
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod tests {
    use super::*;

    #[test]
    fn disabled_immediately_unavailable_even_with_invalid_inputs() {
        let file = tempfile::tempfile().unwrap(); // deliberately not a directory
        for path in ["", "/absolute", "../parent", "x\0y"] {
            assert_eq!(
                read_bounded_source(&file, Path::new(path)),
                Err(SourceError::Unavailable)
            );
        }
    }
}
