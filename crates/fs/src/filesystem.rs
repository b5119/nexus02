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

// The gRPC helpers here thread `tonic::Status` (a large ~176-byte error) through
// their Results to stay uniform with tonic's own surface; boxing it isn't worth
// it. Scoped to this module rather than crate-wide so the lint stays active
// elsewhere.
#![allow(clippy::result_large_err)]

use crate::grpc_client::RemoteFs;
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request, TimeOrNow,
};
use lru::LruCache;
use nexus_common::{ClockStore, VectorClock};
use nexus_proto::fs::v1::{delete_file_response, rename_file_response, write_file_response};
use std::collections::{HashMap, HashSet};
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
    /// Paths known to be directories (populated during `lookup`). Used to reject
    /// directory renames until subtree remapping is implemented (ADR 0009).
    dirs: HashSet<String>,
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
            dirs: HashSet::new(),
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

    /// Look up an existing path's inode WITHOUT allocating a new one (and
    /// without bumping LRU recency). Used to check for a locally-buffered file
    /// that may not exist on the host yet.
    /// Track a path as a directory so rename can reject it.
    fn track_dir(&mut self, path: &str) {
        self.dirs.insert(path.to_string());
    }

    /// Returns true if the path is known to be a directory.
    fn is_dir(&self, path: &str) -> bool {
        self.dirs.contains(path)
    }

    fn peek_ino(&self, path: &str) -> Option<u64> {
        if path == "/" {
            return Some(ROOT_INO);
        }
        self.ino_by_path.get(path).copied()
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

    /// Rename a path in-place: update the mapping so `old_path` becomes
    /// `new_path` with the SAME inode number. Does NOT delete+recreate the
    /// inode — the vector clock history is preserved (ADR 0009).
    /// Returns the inode if old_path was tracked, or None if unknown.
    fn rename_path(&mut self, old_path: &str, new_path: &str) -> Option<u64> {
        if old_path == "/" || new_path == "/" {
            return None;
        }
        let ino = self.ino_by_path.remove(old_path)?;
        // If new_path already had an inode, remove it from both maps so we
        // don't orphan a stale ino→path pointer in path_by_ino.
        if let Some(dest_ino) = self.ino_by_path.remove(new_path) {
            self.path_by_ino.pop(&dest_ino);
        }
        self.ino_by_path.insert(new_path.to_string(), ino);
        if let Some(path) = self.path_by_ino.get_mut(&ino) {
            *path = new_path.to_string();
        } else {
            self.path_by_ino.push(ino, new_path.to_string());
        }
        Some(ino)
    }
}

/// An in-memory write-back buffer for one open file. FUSE delivers writes in
/// arbitrary partial chunks; we accumulate the whole file here and flush it to
/// the host as a single WriteFile (carrying the vector clock) on flush/release.
struct FileBuf {
    data: Vec<u8>,
    /// Has unflushed local content (needs a WriteFile on flush).
    dirty: bool,
    /// Have we seeded `data` with the file's current host content? (false for a
    /// freshly-created file, which starts empty-and-loaded.)
    loaded: bool,
}

pub struct NexusFuse {
    client: RemoteFs,
    runtime: tokio::runtime::Handle,
    inodes: Mutex<InodeTable>,
    /// Per-inode write-back buffers for files currently open for writing.
    write_buffers: Mutex<HashMap<u64, FileBuf>>,
    /// This client's device id — stamped into a file's clock on local writes.
    device_id: String,
    /// Per-file vector clocks this client knows about (path -> clock).
    client_clocks: ClockStore,
}

impl NexusFuse {
    pub fn new(client: RemoteFs, device_id: String, client_clocks: ClockStore) -> Self {
        Self {
            client,
            runtime: tokio::runtime::Handle::current(),
            inodes: Mutex::new(InodeTable::new()),
            write_buffers: Mutex::new(HashMap::new()),
            device_id,
            client_clocks,
        }
    }

    /// Synthesize attrs for a file from its in-memory buffer (used while it's
    /// open / before it's been flushed to the host).
    fn buffer_attr(&self, ino: u64) -> Option<FileAttr> {
        let bufs = self.write_buffers.lock().unwrap();
        let b = bufs.get(&ino)?;
        let size = b.data.len() as u64;
        let now = SystemTime::now();
        Some(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 65536,
            flags: 0,
        })
    }

    /// Ensure inode `ino`'s buffer holds the file's current host content. A
    /// no-op if already loaded (or freshly created). Reads the whole file from
    /// the host once, so subsequent partial writes can modify-in-place.
    fn ensure_loaded(&self, ino: u64, path: &str) -> Result<(), tonic::Status> {
        {
            let bufs = self.write_buffers.lock().unwrap();
            if bufs.get(&ino).map(|b| b.loaded).unwrap_or(false) {
                return Ok(());
            }
        }
        // Fetch current content (size via stat, then a single ranged read).
        let client = self.client.clone();
        let size = match self.runtime.block_on(client.stat(path)) {
            Ok(Some(e)) => e.size_bytes,
            Ok(None) => 0,
            Err(e) => return Err(tonic::Status::internal(e.to_string())),
        };
        let data = if size > 0 {
            self.runtime
                .block_on(self.client.read_range(path, 0, size))
                .map_err(|e| tonic::Status::internal(e.to_string()))?
        } else {
            Vec::new()
        };
        let mut bufs = self.write_buffers.lock().unwrap();
        let b = bufs.entry(ino).or_insert(FileBuf {
            data: Vec::new(),
            dirty: false,
            loaded: false,
        });
        if !b.loaded {
            b.data = data;
            b.loaded = true;
        }
        Ok(())
    }

    /// Flush a dirty buffer to the host via WriteFile, carrying this client's
    /// vector clock (own counter incremented). Adopts the host's authoritative
    /// clock on return. Returns true if a conflict was reported.
    fn flush_buffer(&self, ino: u64, path: &str) -> Result<bool, tonic::Status> {
        let data = {
            let bufs = self.write_buffers.lock().unwrap();
            match bufs.get(&ino) {
                Some(b) if b.dirty => b.data.clone(),
                _ => return Ok(false), // nothing to flush
            }
        };

        let mut clock = self.client_clocks.get(path);
        clock.increment(&self.device_id);

        let resp = self
            .runtime
            .block_on(self.client.write_file_stream(path, data, &clock, &self.device_id))
            .map_err(|e| tonic::Status::internal(e.to_string()))?;

        // Adopt the host's authoritative clock so the next write builds on it.
        let host_clock = proto_to_clock(&resp.clock);
        let _ = self.client_clocks.put(path, clock.merge(&host_clock));

        {
            let mut bufs = self.write_buffers.lock().unwrap();
            if let Some(b) = bufs.get_mut(&ino) {
                b.dirty = false;
            }
        }

        let conflict = resp.result == write_file_response::Result::Conflict as i32;
        if conflict {
            if resp.conflict_path.is_empty() {
                // delete-vs-edit: this edit resurrected a concurrently-deleted
                // file — no sibling copy exists (ADR 0008).
                tracing::warn!(
                    %path,
                    "delete-vs-edit conflict — this edit resurrected a concurrently-deleted file"
                );
            } else {
                // edit-vs-edit: both versions kept, incoming saved as a sibling.
                tracing::warn!(
                    %path,
                    conflict = %resp.conflict_path,
                    "edit conflict — both versions kept (see the .conflict-* file)"
                );
            }
        }
        Ok(conflict)
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
            perm: if entry.is_dir { 0o755 } else { 0o644 }, // read-write mount (ADR 0006)
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

        // A file we've created/written locally but not yet flushed won't exist
        // on the host — serve it from its in-memory buffer instead.
        let local_ino = self.inodes.lock().unwrap().peek_ino(&child_path);
        if let Some(ino) = local_ino {
            if let Some(attr) = self.buffer_attr(ino) {
                return reply.entry(&TTL, &attr, 0);
            }
        }

        let client = self.client.clone();
        let result = self.runtime.block_on(client.stat(&child_path));

        match result {
            Ok(Some(entry)) => {
                let ino = {
                    let mut table = self.inodes.lock().unwrap();
                    if entry.is_dir {
                        table.track_dir(&child_path);
                    }
                    table.ino_for_path(&child_path)
                };
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
                perm: 0o755,
                nlink: 2,
                uid: unsafe { libc::getuid() },
                gid: unsafe { libc::getgid() },
                rdev: 0,
                blksize: 65536,
                flags: 0,
            };
            return reply.attr(&TTL, &attr);
        }

        // An open/dirty file's authoritative size lives in its write buffer,
        // not (yet) on the host — report that.
        if let Some(attr) = self.buffer_attr(ino) {
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

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        // On a read-intent open, sync this client's clock for the file from the
        // host, so a later edit builds on the version we actually read instead
        // of being flagged a spurious conflict (ADR 0007). A write-only open
        // (e.g. the truncating `>`) is deliberately NOT synced: blindly
        // overwriting a file that changed underneath us is a real conflict we
        // still want to catch.
        let access = flags & libc::O_ACCMODE;
        if access == libc::O_RDONLY || access == libc::O_RDWR {
            let path = self.inodes.lock().unwrap().path_for_ino(ino);
            if let Some(path) = path {
                if let Ok(Some((_entry, host_clock))) =
                    self.runtime.block_on(self.client.stat_full(&path))
                {
                    let merged = self.client_clocks.get(&path).merge(&host_clock);
                    let _ = self.client_clocks.put(&path, merged);
                }
            }
        }
        reply.opened(0, 0);
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &std::ffi::OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
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

        let ino = self.inodes.lock().unwrap().ino_for_path(&child_path);
        // Fresh, empty, and dirty so even a zero-byte new file is written
        // through on flush.
        self.write_buffers.lock().unwrap().insert(
            ino,
            FileBuf {
                data: Vec::new(),
                dirty: true,
                loaded: true,
            },
        );
        let attr = self.buffer_attr(ino).expect("buffer just inserted");
        reply.created(&TTL, &attr, 0, 0, 0);
    }

    fn write(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };

        if self.ensure_loaded(ino, &path).is_err() {
            return reply.error(libc::EIO);
        }

        let mut bufs = self.write_buffers.lock().unwrap();
        let b = bufs.entry(ino).or_insert(FileBuf {
            data: Vec::new(),
            dirty: false,
            loaded: true,
        });
        let off = offset as usize;
        let end = off + data.len();
        if b.data.len() < end {
            b.data.resize(end, 0);
        }
        b.data[off..end].copy_from_slice(data);
        b.dirty = true;
        reply.written(data.len() as u32);
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(ino) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };

        // The case we actually need: truncate (e.g. `>` redirection sends size=0).
        if let Some(new_size) = size {
            if new_size == 0 {
                // Truncating to empty — no need to read existing content.
                self.write_buffers.lock().unwrap().insert(
                    ino,
                    FileBuf {
                        data: Vec::new(),
                        dirty: true,
                        loaded: true,
                    },
                );
            } else {
                if self.ensure_loaded(ino, &path).is_err() {
                    return reply.error(libc::EIO);
                }
                let mut bufs = self.write_buffers.lock().unwrap();
                if let Some(b) = bufs.get_mut(&ino) {
                    b.data.resize(new_size as usize, 0);
                    b.dirty = true;
                }
            }
        }

        // Reply with current attrs: buffer if open, else host, else ENOENT.
        if let Some(attr) = self.buffer_attr(ino) {
            return reply.attr(&TTL, &attr);
        }
        let client = self.client.clone();
        match self.runtime.block_on(client.stat(&path)) {
            Ok(Some(entry)) => reply.attr(&TTL, &Self::entry_to_attr(ino, &entry)),
            _ => reply.error(libc::ENOENT),
        }
    }

    fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        // `flush` can fire multiple times per open (e.g. on each close() of a
        // dup'd fd). We do the actual write-back once, on `release` (and on
        // `fsync`), so the clock is incremented exactly once per write session.
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(p) = self.inodes.lock().unwrap().path_for_ino(ino) {
            let _ = self.flush_buffer(ino, &p);
        }
        // Drop the buffer now that the file is fully closed.
        self.write_buffers.lock().unwrap().remove(&ino);
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request, ino: u64, _fh: u64, _datasync: bool, reply: ReplyEmpty) {
        let path = self.inodes.lock().unwrap().path_for_ino(ino);
        match path {
            Some(p) => match self.flush_buffer(ino, &p) {
                Ok(_) => reply.ok(),
                Err(_) => reply.error(libc::EIO),
            },
            None => reply.ok(),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &std::ffi::OsStr, reply: ReplyEmpty) {
        let parent_path = {
            let mut table = self.inodes.lock().unwrap();
            match table.path_for_ino(parent) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            }
        };
        let name_str = name.to_string_lossy().to_string();
        let path = if parent_path == "/" {
            format!("/{name_str}")
        } else {
            format!("{parent_path}/{name_str}")
        };

        // Stamp this client's counter and send the delete; the host applies the
        // tombstone / delete-vs-edit policy (ADR 0008).
        let mut clock = self.client_clocks.get(&path);
        clock.increment(&self.device_id);
        let resp = self
            .runtime
            .block_on(self.client.delete_file(&path, &clock, &self.device_id));

        match resp {
            Ok(r) => {
                // Adopt the host's authoritative clock; drop any local buffer.
                let host_clock = proto_to_clock(&r.clock);
                let _ = self.client_clocks.put(&path, clock.merge(&host_clock));
                if let Some(ino) = self.inodes.lock().unwrap().peek_ino(&path) {
                    self.write_buffers.lock().unwrap().remove(&ino);
                }
                // On CONFLICT or STALE the host keeps the file, so the local `rm`
                // appears to succeed but the file reappears on the next lookup —
                // warn either way so that isn't a silent surprise (ADR 0008).
                if r.result == delete_file_response::Result::Conflict as i32 {
                    tracing::warn!(
                        %path,
                        "delete conflicted with a concurrent edit — the host kept the file \
                         (it will reappear on the next lookup)"
                    );
                } else if r.result == delete_file_response::Result::Stale as i32 {
                    tracing::warn!(
                        %path,
                        "delete was stale (the file has newer edits) — the host kept it \
                         (it will reappear on the next lookup)"
                    );
                }
                // The local `rm` succeeds either way.
                reply.ok();
            }
            Err(e) => {
                tracing::warn!(error = %e, %path, "delete failed");
                reply.error(libc::EIO);
            }
        }
    }

    fn rename(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &std::ffi::OsStr,
        newparent: u64,
        newname: &std::ffi::OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        // We only support plain rename (flags == 0). RENAME_NOREPLACE,
        // RENAME_EXCHANGE, and other non-zero flags are rejected until
        // we propagate them through the gRPC layer (ADR 0009).
        if flags != 0 {
            return reply.error(libc::EINVAL);
        }

        let (old_path, new_path) = {
            let mut table = self.inodes.lock().unwrap();
            let parent_path = match table.path_for_ino(parent) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            };
            let newparent_path = match table.path_for_ino(newparent) {
                Some(p) => p,
                None => return reply.error(libc::ENOENT),
            };
            let name_str = name.to_string_lossy().to_string();
            let newname_str = newname.to_string_lossy().to_string();
            let old = if parent_path == "/" {
                format!("/{name_str}")
            } else {
                format!("{parent_path}/{name_str}")
            };
            let new = if newparent_path == "/" {
                format!("/{newname_str}")
            } else {
                format!("{newparent_path}/{newname_str}")
            };
            // Reject directory renames — we don't remap subtree entries
            // in the inode table yet (ADR 0009).
            if table.is_dir(&old) {
                return reply.error(libc::EISDIR);
            }
            (old, new)
        };

        let mut clock = self.client_clocks.get(&old_path);
        clock.increment(&self.device_id);

        let resp = self.runtime.block_on(self.client.rename_file(
            &old_path,
            &new_path,
            &clock,
            &self.device_id,
        ));

        match resp {
            Ok(r) => {
                let host_clock = proto_to_clock(&r.clock);
                let merged = clock.merge(&host_clock);

                let result_code = r.result;
                if result_code == rename_file_response::Result::Renamed as i32
                    || result_code == rename_file_response::Result::Conflict as i32
                {
                    // Update inode table: same inode, new path.
                    {
                        let mut table = self.inodes.lock().unwrap();
                        table.rename_path(&old_path, &new_path);
                    }
                    // Move the client clock to the new path.
                    let _ = self.client_clocks.remove(&old_path);
                    let _ = self.client_clocks.put(&new_path, merged);
                    // If there was a write buffer for this inode, it stays with
                    // the inode (no change needed — buffers are keyed by ino).
                    if result_code == rename_file_response::Result::Conflict as i32 {
                        tracing::warn!(
                            old_path = %old_path,
                            new_path = %new_path,
                            "rename conflict — rename applied, see ADR 0009 for details"
                        );
                    }
                    reply.ok();
                } else if result_code == rename_file_response::Result::Stale as i32 {
                    tracing::warn!(%old_path, "rename stale (host has a newer version)");
                    // Adopt the host clock so the next attempt builds on it.
                    let _ = self.client_clocks.put(&old_path, merged);
                    reply.error(libc::EAGAIN);
                } else if result_code == rename_file_response::Result::NotFound as i32 {
                    reply.error(libc::ENOENT);
                } else {
                    tracing::warn!(result = %result_code, "unknown rename result");
                    reply.error(libc::EIO);
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, %old_path, "rename failed");
                reply.error(libc::EIO);
            }
        }
    }
}

fn proto_to_clock(p: &Option<nexus_proto::fs::v1::VectorClock>) -> VectorClock {
    match p {
        Some(c) => VectorClock(c.counters.iter().map(|(k, v)| (k.clone(), *v)).collect()),
        None => VectorClock::new(),
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

    #[test]
    fn rename_path_preserves_inode() {
        let mut t = InodeTable::with_capacity(10);
        let ino = t.ino_for_path("/old.txt");
        let new_ino = t.rename_path("/old.txt", "/new.txt");
        assert_eq!(new_ino, Some(ino), "same inode returned");
        // old path is gone
        assert!(t.peek_ino("/old.txt").is_none(), "old path removed");
        // new path maps to the same inode
        assert_eq!(t.peek_ino("/new.txt"), Some(ino));
        // path_by_ino roundtrips for the new name
        assert_eq!(t.path_for_ino(ino).as_deref(), Some("/new.txt"));
    }

    #[test]
    fn rename_path_unknown_returns_none() {
        let mut t = InodeTable::with_capacity(10);
        assert!(t.rename_path("/nonexistent", "/other").is_none());
    }

    #[test]
    fn rename_path_cross_directory_preserves_inode() {
        let mut t = InodeTable::with_capacity(10);
        let ino = t.ino_for_path("/dir/old.txt");
        let new_ino = t.rename_path("/dir/old.txt", "/other/new.txt");
        assert_eq!(new_ino, Some(ino), "same inode across directories");
        assert!(t.peek_ino("/dir/old.txt").is_none());
        assert_eq!(t.peek_ino("/other/new.txt"), Some(ino));
    }

    #[test]
    fn rename_path_overwrite_cleans_up_old_destination() {
        let mut t = InodeTable::with_capacity(10);
        let src_ino = t.ino_for_path("/src.txt");
        let dest_ino = t.ino_for_path("/dest.txt");
        let result = t.rename_path("/src.txt", "/dest.txt");
        assert_eq!(result, Some(src_ino));
        // Old source path is gone
        assert!(t.peek_ino("/src.txt").is_none(), "old src removed");
        // Dest maps to the moved inode
        assert_eq!(t.peek_ino("/dest.txt"), Some(src_ino));
        // Old destination inode no longer claims /dest.txt
        assert_ne!(t.path_for_ino(dest_ino).as_deref(), Some("/dest.txt"));
    }
}
