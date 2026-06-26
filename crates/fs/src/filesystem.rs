//! The actual FUSE trait implementation.
//!
//! IMPORTANT CONCEPT FOR ANYONE NEW TO FUSE (this means you, Von —
//! worth reading this comment block before debugging anything here):
//!
//! The kernel calls into `fuser::Filesystem` methods (lookup, getattr,
//! read, readdir) SYNCHRONOUSLY, one at a time, from a dedicated FUSE
//! thread. But our actual data source — the remote agent — is only
//! reachable via async gRPC calls (tonic requires an async runtime).
//!
//! The bridge is `tokio::runtime::Handle::block_on`: inside each sync
//! FUSE callback, we block the FUSE thread and run the async gRPC call
//! to completion before returning. This is the standard pattern for
//! wrapping async clients inside sync FFI-style boundaries. It's not
//! the most efficient possible design (the FUSE thread sits idle while
//! waiting on network I/O) but it's correct and simple, which is what
//! milestone 1 needs. A future optimization would use fuser's planned
//! async support directly, once that stabilizes upstream.
//!
//! Second concept: FUSE addresses everything by *inode number*, not
//! by path. The kernel calls `lookup(parent_inode, name)` to ask "what's
//! the inode for this child", caches the answer, and from then on calls
//! `getattr(inode)` / `read(inode)` etc. So we maintain an inode <-> path
//! table in memory. This is the part most people get wrong on a first
//! FUSE implementation — it's tempting to think you can work with paths
//! throughout, but the kernel API genuinely doesn't let you.

use crate::grpc_client::RemoteFs;
use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyData, ReplyDirectory, ReplyEntry, Request};
use lru::LruCache;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(2); // how long the kernel may
                                               // cache attrs before re-asking us.
                                               // Short on purpose for milestone 1 —
                                               // correctness over performance while
                                               // we don't have invalidation/Watch wired up yet.

const ROOT_INO: u64 = 1;

/// Default upper bound on how many inode<->path entries we keep resident.
const DEFAULT_INODE_CAP: usize = 10_000;

/// Maps FUSE inode numbers to the remote path they represent, bounded by an
/// LRU so a long-running mount over a large remote tree can't leak memory
/// indefinitely. The root (ino 1, "/") is pinned — handled specially outside
/// the LRU so it can never be evicted. `path_by_ino` is the authoritative
/// store (and drives eviction); `ino_by_path` is a reverse index kept in sync.
///
/// Trade-off (acceptable for a read-only milestone-1 mount): if the kernel
/// still references an inode we've evicted, a later op on it returns a
/// stale-handle error for that one entry instead of leaking forever. A
/// fully-correct FUSE impl would track per-inode lookup counts and only drop
/// on `forget`; deliberately not doing that yet (see the module header).
struct InodeTable {
    next_ino: u64,
    path_by_ino: LruCache<u64, String>,
    ino_by_path: HashMap<String, u64>,
}

impl InodeTable {
    fn new() -> Self {
        Self::with_capacity(DEFAULT_INODE_CAP)
    }

    fn with_capacity(cap: usize) -> Self {
        let cap = NonZeroUsize::new(cap).expect("inode table capacity must be > 0");
        Self {
            next_ino: 2,
            path_by_ino: LruCache::new(cap),
            ino_by_path: HashMap::new(),
        }
    }

    fn ino_for_path(&mut self, path: &str) -> u64 {
        if path == "/" {
            return ROOT_INO;
        }
        if let Some(&ino) = self.ino_by_path.get(path) {
            // Touch recency on the authoritative LRU so frequently-used paths
            // aren't the ones evicted.
            self.path_by_ino.get(&ino);
            return ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.insert(ino, path.to_string());
        ino
    }

    fn path_for_ino(&mut self, ino: u64) -> Option<String> {
        if ino == ROOT_INO {
            return Some("/".to_string());
        }
        self.path_by_ino.get(&ino).cloned()
    }

    /// Insert a fresh ino<->path pair, evicting the LRU entry if at capacity
    /// and keeping the reverse index consistent with whatever got dropped.
    fn insert(&mut self, ino: u64, path: String) {
        self.ino_by_path.insert(path.clone(), ino);
        // `push` returns the displaced entry: the evicted LRU pair when full
        // (ino is always fresh, so the same-key case can't happen here).
        if let Some((evicted_ino, evicted_path)) = self.path_by_ino.push(ino, path) {
            if evicted_ino != ino && self.ino_by_path.get(&evicted_path) == Some(&evicted_ino) {
                self.ino_by_path.remove(&evicted_path);
            }
        }
    }
}

pub struct NexusFuse {
    client: RemoteFs,
    runtime: tokio::runtime::Handle,
    inodes: Mutex<InodeTable>,
}

impl NexusFuse {
    pub fn new(client: RemoteFs) -> Self {
        Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            inodes: Mutex::new(InodeTable::new()),
        }
    }

    fn entry_to_attr(ino: u64, entry: &nexus_proto::fs::v1::FileEntry) -> FileAttr {
        let kind = if entry.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let mtime = UNIX_EPOCH + Duration::from_secs(entry.modified_unix.max(0) as u64);

        FileAttr {
            ino,
            size: entry.size_bytes,
            blocks: entry.size_bytes.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind,
            perm: if entry.is_dir { 0o555 } else { 0o444 }, // read-only mount, milestone 1
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 65536,
            flags: 0,
        }
    }
}

impl Filesystem for NexusFuse {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &std::ffi::OsStr, reply: ReplyEntry) {
        let parent_path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(parent) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };

        let name_str = name.to_string_lossy().to_string();
        let child_path = if parent_path == "/" {
            format!("/{name_str}")
        } else {
            format!("{parent_path}/{name_str}")
        };

        let client = self.client.clone();
        let result = self.runtime.block_on(client.stat(&child_path));

        match result {
            Ok(Some(entry)) => {
                let ino = self.inodes.lock().unwrap().ino_for_path(&child_path);
                let attr = Self::entry_to_attr(ino, &entry);
                reply.entry(&TTL, &attr, 0);
            }
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => {
                tracing::warn!(error = %e, path = %child_path, "lookup failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };

        if ino == ROOT_INO {
            // Root directory attrs are synthesized rather than fetched —
            // the remote root always exists by definition once connected.
            let attr = FileAttr {
                ino: ROOT_INO,
                size: 0,
                blocks: 0,
                atime: SystemTime::now(),
                mtime: SystemTime::now(),
                ctime: SystemTime::now(),
                crtime: SystemTime::now(),
                kind: FileType::Directory,
                perm: 0o555,
                nlink: 2,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                rdev: 0,
                blksize: 65536,
                flags: 0,
            };
            return reply.attr(&TTL, &attr);
        }

        let client = self.client.clone();
        match self.runtime.block_on(client.stat(&path)) {
            Ok(Some(entry)) => reply.attr(&TTL, &Self::entry_to_attr(ino, &entry)),
            Ok(None) => reply.error(libc::ENOENT),
            Err(e) => {
                tracing::warn!(error = %e, %path, "getattr failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };

        let client = self.client.clone();
        let entries = match self.runtime.block_on(client.list_dir(&path)) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, %path, "readdir failed");
                return reply.error(libc::EIO);
            }
        };

        let mut all = vec![
            (ino, FileType::Directory, ".".to_string()),
            (ino, FileType::Directory, "..".to_string()), // simplification: milestone 1
                                                            // doesn't track true parent ino
                                                            // for ".." at depth > 1 yet.
        ];

        for entry in &entries {
            let child_path = if path == "/" {
                format!("/{}", entry.name)
            } else {
                format!("{path}/{}", entry.name)
            };
            let child_ino = self.inodes.lock().unwrap().ino_for_path(&child_path);
            let kind = if entry.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            all.push((child_ino, kind, entry.name.clone()));
        }

        for (i, (ino, kind, name)) in all.into_iter().enumerate().skip(offset as usize) {
            // reply.add returns true if the buffer is full — stop early if so
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn read(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };

        let client = self.client.clone();
        let result = self
            .runtime
            .block_on(client.read_range(&path, offset as u64, size as u64));

        match result {
            Ok(data) => reply.data(&data),
            Err(e) => {
                tracing::warn!(error = %e, %path, "read failed");
                reply.error(libc::EIO);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_is_pinned_and_never_evicted() {
        let mut t = InodeTable::with_capacity(2);
        // churn many distinct paths through a tiny cache
        for i in 0..100 {
            t.ino_for_path(&format!("/file{i}"));
        }
        assert_eq!(t.path_for_ino(ROOT_INO).as_deref(), Some("/"));
        assert_eq!(t.ino_for_path("/"), ROOT_INO);
    }

    #[test]
    fn bounded_and_evicts_least_recently_used() {
        let mut t = InodeTable::with_capacity(3);
        let a = t.ino_for_path("/a");
        let b = t.ino_for_path("/b");
        let c = t.ino_for_path("/c");

        // touch a and b so c becomes the least-recently-used entry
        assert_eq!(t.path_for_ino(a).as_deref(), Some("/a"));
        assert_eq!(t.path_for_ino(b).as_deref(), Some("/b"));

        // inserting a 4th entry evicts the LRU (/c)
        let _d = t.ino_for_path("/d");
        assert!(t.path_for_ino(c).is_none(), "LRU entry should be evicted");

        // reverse index was kept consistent: /c now allocates a fresh ino,
        // not the stale one
        assert_ne!(t.ino_for_path("/c"), c, "evicted path must get a new ino");
    }

    #[test]
    fn roundtrips_retained_entries() {
        let mut t = InodeTable::with_capacity(10);
        let ino = t.ino_for_path("/dir/file.txt");
        assert_eq!(t.path_for_ino(ino).as_deref(), Some("/dir/file.txt"));
        // same path returns the same ino
        assert_eq!(t.ino_for_path("/dir/file.txt"), ino);
    }
}
