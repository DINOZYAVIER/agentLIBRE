use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use agl_core::agent::RelativePath;
use anyhow::{Context, Result, ensure};

const MASK: u32 = libc::IN_MODIFY
    | libc::IN_ATTRIB
    | libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

#[derive(Clone, Copy, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
    changed_at: (i64, i64),
    bytes: u64,
}

pub(super) struct FileEvents {
    fd: OwnedFd,
    root: File,
    targets: Vec<PathBuf>,
    watches: BTreeMap<i32, PathBuf>,
    baseline: Vec<Option<FileIdentity>>,
}

impl FileEvents {
    pub fn new(root: &Path, events: &[String]) -> Result<Self> {
        let root = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .context("cannot watch schedule workspace")?;
        // SAFETY: inotify_init1 takes only validated flag bits and returns an owned descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        ensure!(
            fd >= 0,
            "cannot initialize schedule file events: {}",
            std::io::Error::last_os_error()
        );
        // SAFETY: the successful syscall returns a fresh descriptor, adopted once.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let targets = events
            .iter()
            .map(|e| {
                let path = e.strip_prefix("file:").context("invalid scheduled event")?;
                Ok(RelativePath::try_from(path.to_owned())
                    .map_err(|error| anyhow::anyhow!(error))?
                    .as_path()
                    .to_path_buf())
            })
            .collect::<Result<Vec<_>>>()?;
        let mut source = Self {
            fd,
            root,
            targets,
            watches: BTreeMap::new(),
            baseline: Vec::new(),
        };
        source.refresh_watches()?;
        source.baseline = source.targets.iter().map(|p| source.metadata(p)).collect();
        Ok(source)
    }

    fn metadata(&self, path: &Path) -> Option<FileIdentity> {
        let file = open_beneath(&self.root, path, libc::O_PATH).ok()?;
        let metadata = file.metadata().ok()?;
        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
            changed_at: (metadata.ctime(), metadata.ctime_nsec()),
            bytes: metadata.len(),
        })
    }

    fn refresh_watches(&mut self) -> Result<()> {
        let mut parents = BTreeSet::from([PathBuf::from(".")]);
        for target in &self.targets {
            let mut parent = target.parent();
            while let Some(path) = parent {
                if !path.as_os_str().is_empty() {
                    parents.insert(path.to_path_buf());
                }
                parent = path.parent();
            }
        }
        let mut current = BTreeMap::new();
        for parent in parents {
            let directory =
                match open_beneath(&self.root, &parent, libc::O_PATH | libc::O_DIRECTORY) {
                    Ok(directory) => directory,
                    Err(error)
                        if matches!(
                            error.raw_os_error(),
                            Some(libc::ENOENT | libc::ENOTDIR | libc::ELOOP)
                        ) =>
                    {
                        continue;
                    }
                    Err(error) => return Err(error).context("cannot open schedule event parent"),
                };
            let path = CString::new(format!("/proc/self/fd/{}", directory.as_raw_fd()))?;
            // SAFETY: path is a terminated string to our pinned directory descriptor;
            // the descriptor is alive for the syscall, which attaches to its inode.
            let wd = unsafe {
                libc::inotify_add_watch(self.fd.as_raw_fd(), path.as_ptr(), MASK | libc::IN_ONLYDIR)
            };
            ensure!(
                wd >= 0,
                "cannot watch scheduled directory: {}",
                std::io::Error::last_os_error()
            );
            let relative = if parent == Path::new(".") {
                PathBuf::new()
            } else {
                parent
            };
            current.insert(wd, relative);
        }
        for old in self.watches.keys().filter(|wd| !current.contains_key(wd)) {
            // SAFETY: fd is live; old was returned by inotify_add_watch for this instance.
            unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), *old) };
        }
        self.watches = current;
        Ok(())
    }

    pub fn changed(&mut self) -> Result<Vec<RelativePath>> {
        let mut buffer = [0u8; 64 * 1024];
        let mut changed = BTreeSet::new();
        let mut refresh = false;
        for _ in 0..8 {
            // SAFETY: buffer is valid writable storage; fd is a live nonblocking inotify descriptor.
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() == std::io::ErrorKind::WouldBlock {
                    break;
                }
                if error.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error).context("cannot read schedule file events");
            }
            if read == 0 {
                break;
            }
            let mut offset = 0;
            while offset + std::mem::size_of::<libc::inotify_event>() <= read as usize {
                // SAFETY: bounds above cover the event header; byte buffer need not be aligned.
                let event = unsafe {
                    std::ptr::read_unaligned(
                        buffer.as_ptr().add(offset).cast::<libc::inotify_event>(),
                    )
                };
                let end = offset + std::mem::size_of::<libc::inotify_event>() + event.len as usize;
                ensure!(end <= read as usize, "truncated schedule file event");
                if event.mask & libc::IN_Q_OVERFLOW != 0 {
                    anyhow::bail!("schedule file-event queue overflowed");
                }
                if event.mask & libc::IN_IGNORED != 0 {
                    self.watches.remove(&event.wd);
                    refresh = true;
                }
                if let Some(parent) = self.watches.get(&event.wd) {
                    let name = &buffer[offset + std::mem::size_of::<libc::inotify_event>()..end];
                    let name = &name[..name.iter().position(|b| *b == 0).unwrap_or(name.len())];
                    let path = parent.join(OsStr::from_bytes(name));
                    for (index, target) in self.targets.iter().enumerate() {
                        if *target == path && event.mask & MASK != 0 {
                            changed.insert(index);
                        }
                        if event.mask & libc::IN_ISDIR != 0
                            && target.starts_with(&path)
                            && self.metadata(target) != self.baseline[index]
                        {
                            changed.insert(index);
                        }
                    }
                    if event.mask & (libc::IN_ISDIR | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF)
                        != 0
                    {
                        refresh = true;
                    }
                }
                offset = end;
            }
        }
        if refresh {
            self.refresh_watches()?;
        }
        self.baseline = self.targets.iter().map(|p| self.metadata(p)).collect();
        changed
            .into_iter()
            .map(|i| {
                RelativePath::try_from(self.targets[i].to_string_lossy().into_owned())
                    .map_err(|error| anyhow::anyhow!(error))
            })
            .collect()
    }
}

fn open_beneath(root: &File, path: &Path, flags: i32) -> std::io::Result<File> {
    #[repr(C)]
    struct OpenHow {
        flags: u64,
        mode: u64,
        resolve: u64,
    }
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "event path contains NUL")
    })?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64,
        mode: 0,
        resolve: 0x08 | 0x04 | 0x02,
    };
    // SAFETY: root is a live pinned directory; path/how point to correctly sized,
    // initialized storage. The syscall returns a fresh descriptor on success.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root.as_raw_fd(),
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the successful syscall returns a descriptor exclusively owned here.
    Ok(unsafe { File::from_raw_fd(fd as i32) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_events_track_writes_atomic_replacement_creation_and_ignore_other_files() {
        let root =
            std::env::temp_dir().join(format!("agl-schedule-events-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("source.txt"), "before").unwrap();
        let mut events = FileEvents::new(
            &root,
            &["file:source.txt".into(), "file:new/created.txt".into()],
        )
        .unwrap();
        std::fs::write(root.join("other.txt"), "ignored").unwrap();
        assert!(events.changed().unwrap().is_empty());
        std::fs::write(root.join("source.txt"), "after").unwrap();
        assert_eq!(events.changed().unwrap().len(), 1);
        std::fs::write(root.join("replacement.txt"), "replacement").unwrap();
        std::fs::rename(root.join("replacement.txt"), root.join("source.txt")).unwrap();
        assert_eq!(events.changed().unwrap().len(), 1);
        std::fs::create_dir(root.join("new")).unwrap();
        assert!(events.changed().unwrap().is_empty());
        std::fs::write(root.join("new/created.txt"), "created").unwrap();
        assert_eq!(
            events.changed().unwrap()[0].as_path(),
            Path::new("new/created.txt")
        );
        std::fs::remove_file(root.join("source.txt")).unwrap();
        assert_eq!(events.changed().unwrap().len(), 1);
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_event_parents_never_follow_links_outside_the_workspace() {
        let root =
            std::env::temp_dir().join(format!("agl-schedule-event-links-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        std::fs::create_dir_all(root.join("outside")).unwrap();
        std::os::unix::fs::symlink(root.join("outside"), root.join("workspace/link")).unwrap();
        let mut events =
            FileEvents::new(&root.join("workspace"), &["file:link/secret.txt".into()]).unwrap();
        std::fs::write(root.join("outside/secret.txt"), "outside").unwrap();
        assert!(events.changed().unwrap().is_empty());
        drop(events);
        std::fs::remove_dir_all(root).unwrap();
    }
}
