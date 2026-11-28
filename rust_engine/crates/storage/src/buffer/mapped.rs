// Copyright(C) 2026 InfiniFlow, Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Read-only file mappings, shared by every reader in the process.
//!
//! A sealed block's files and an index chunk are immutable once written, so a
//! reader maps them and decodes straight out of the mapping. That is what the
//! reference's `BufferManager` achieves with an owned copy plus an LRU: here the
//! OS page cache holds the pages, the mapping (address space, not memory) is
//! shared by every reader of the same file, and no block is ever read into the
//! heap.
//!
//! The two counters below back `SHOW BUFFERS` / `buffer_pool_usage`: they say how
//! many mappings the process holds and how many bytes they cover.

use std::ops::Deref;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use memmap2::Mmap;

/// Objects this process has mapped.
///
/// One per **file identity**, not per [`MappedFile`] handle: every reader of one
/// file shares a single `Mmap`, so this is also the process's VMA count for
/// engine data.
static MAPPINGS: AtomicUsize = AtomicUsize::new(0);
/// Bytes covered by those mappings.
static MAPPED_BYTES: AtomicUsize = AtomicUsize::new(0);
/// Bytes whose resident pages have been dropped by [`MappedFile::release_pages`].
static PAGES_RELEASED: AtomicUsize = AtomicUsize::new(0);

/// What identifies one mapped object: the file's identity, not its path.
///
/// `(dev, ino)` is what the kernel itself uses, so two paths for one inode (a
/// hard link, or an object that holds several parts) share a mapping - which is
/// correct, since they are the same bytes in the same page cache. A file the
/// engine **replaces** gets a new inode: every publisher writes a sibling temp
/// file and renames it over the target (`definition::format::write_parts`), so a
/// stale mapping can never be handed out under the new name.
///
/// The second field is a **disambiguator**, not a size: `(0, n)` for a file
/// mapped through `open_path`, and `(1, n)` for an `Mmap` a caller built itself
/// (`from_mmap`), where no inode is known. Keeping the two apart means a
/// caller-made mapping can never be confused with a file whose device happens to
/// be `0`.
type ObjectKey = (u8, u64, u64);

/// One mapped object and the readers holding it.
struct Entry {
    map: std::sync::Arc<Mmap>,
    /// Handles currently holding this object through [`MappedFile`].
    handles: usize,
    /// `map.len()`, charged once per object.
    bytes: usize,
}

/// Idle mappings kept after their last handle lets go, derived once from
/// `vm.max_map_count`.
///
/// Not "none, released at once" and not "all, until memory pressure": both ends
/// were measured and both are wrong.
///
/// **Releasing at once** costs a page-table rebuild on the next open and a TLB
/// shootdown across cores at `munmap`. The syscall pair itself is cheap
/// (measured: 5 us for an 8 MiB file, 7.5 us for 64 MiB), but it is per open, and
/// a seek-heavy scan opens the same blocks repeatedly.
///
/// **Keeping everything** is not free either: a mapping is a VMA, and
/// `vm.max_map_count` bounds them. A 20-column table of 1000 blocks is 20k-40k
/// VMAs; a few of those reach the limit, and then `mmap` fails with `ENOMEM` - a
/// query error, not a slowdown. Measured: 200 idle mappings of one file are 200
/// VMAs.
///
/// So the table is bounded and unmap of the least recently released idle entry
/// is what keeps it there. The bound is a sixteenth of the kernel's own limit:
/// the rest is headroom for a working set, the shared libraries and every other
/// mapping in the process. Clamped so a host with an unusually small limit still
/// keeps a useful cache, and one with a huge limit does not grow the table
/// without need.
///
/// A limit below 262144 is worth a warning: Elasticsearch asks for 1048576, and
/// a container that inherits a small one will hit `ENOMEM` under a wide scan.
const IDLE_MAPPING_FALLBACK: usize = 16_384;
/// Lower clamp: below this the cache stops absorbing a seek-heavy scan.
const IDLE_MAPPING_MIN: usize = 1_024;
/// Upper clamp: no reason to hold more mappings than this, whatever the kernel
/// allows.
const IDLE_MAPPING_MAX: usize = 65_536;

/// `vm.max_map_count`, or `None` where the platform has no such file.
fn max_map_count() -> Option<usize> {
    let text = std::fs::read_to_string("/proc/sys/vm/max_map_count").ok()?;
    let value: usize = text.trim().parse().ok()?;
    (value > 0).then_some(value)
}

/// The idle bound, resolved once.
static IDLE_MAPPING_LIMIT: std::sync::LazyLock<usize> =
    std::sync::LazyLock::new(|| match max_map_count() {
        Some(kernel) => (kernel / 16).clamp(IDLE_MAPPING_MIN, IDLE_MAPPING_MAX),
        None => IDLE_MAPPING_FALLBACK,
    });

/// The process's mapping table, keyed by [`ObjectKey`].
///
/// A second reader of one file reuses the mapping instead of creating a second
/// VMA. That is not a page-cache optimisation - two mappings of one file already
/// share the physical pages (measured: `RssFile` doubles, `Pss` does not move) -
/// it removes the VMA and `mmap`/`munmap` churn per open, and it keeps the
/// resident figure honest: RSS counts a shared page once **per mapping**, so one
/// block mapped by two readers used to read as twice its size.
///
/// An entry survives its last handle so the next open skips the remap, but not
/// without bound: see [`IDLE_MAPPING_LIMIT`]. Pages are *not* what is kept - a
/// kept mapping's pages can still be dropped by [`release_mapped_pages`], which
/// is the cheap lever the reclaim ladder reaches for first.
struct Registry {
    entries: std::collections::HashMap<ObjectKey, Entry>,
    /// Idle keys, least recently released first. A key is pushed when its last
    /// handle goes and removed when it is acquired again, so this tracks only
    /// the entries an eviction may take.
    idle: std::collections::VecDeque<ObjectKey>,
}

static REGISTRY: std::sync::LazyLock<std::sync::Mutex<Registry>> = std::sync::LazyLock::new(|| {
    std::sync::Mutex::new(Registry {
        entries: std::collections::HashMap::new(),
        idle: std::collections::VecDeque::new(),
    })
});

/// Takes the mapping for `key`, reusing the live one when there is one.
///
/// Returns the mapping and whether this call created it, so the caller charges
/// the counters only once per object. `create` runs only on a miss.
fn acquire(
    key: ObjectKey,
    create: impl FnOnce() -> std::io::Result<std::sync::Arc<Mmap>>,
) -> std::io::Result<(std::sync::Arc<Mmap>, bool)> {
    let Ok(mut registry) = REGISTRY.lock() else {
        // A poisoned table cannot hand out shared mappings. A private one is
        // still correct, it just costs the VMA this table exists to avoid.
        return create().map(|map| (map, true));
    };
    if let Some(entry) = registry.entries.get_mut(&key) {
        entry.handles += 1;
        let map = std::sync::Arc::clone(&entry.map);
        // It is no longer a candidate for the bound.
        if entry.handles == 1 {
            registry.idle.retain(|idle| *idle != key);
        }
        return Ok((map, false));
    }
    // The lock is held across `mmap` on purpose: two readers of one file racing
    // here would otherwise both map it and one mapping would be dropped
    // immediately - exactly the churn this function removes. `mmap` is a cheap
    // syscall, not I/O.
    let map = create()?;
    let bytes = map.len();
    registry.entries.insert(
        key,
        Entry {
            map: std::sync::Arc::clone(&map),
            handles: 1,
            bytes,
        },
    );
    Ok((map, true))
}

/// Gives back one reader's reference to `key`.
///
/// The entry - and with it the mapping - stays idle, so the next open skips the
/// remap; [`IDLE_MAPPING_LIMIT`] bounds how many such entries exist.
/// [`release_mapped_pages`] can still drop its pages in the meantime, which is
/// the cheap lever the reclaim ladder reaches for first - this function keeps
/// address space, not memory.
fn release(key: ObjectKey) {
    let Ok(mut registry) = REGISTRY.lock() else {
        return;
    };
    let Some(entry) = registry.entries.get_mut(&key) else {
        return;
    };
    entry.handles = entry.handles.saturating_sub(1);
    if entry.handles > 0 {
        return;
    }
    registry.idle.push_back(key);
    // Over the bound: unmaps the oldest idle entries, `key` included when it is
    // the only one. Doing it here rather than on the next acquisition means a
    // process that stops reading also stops holding address space.
    let mut evicted = 0usize;
    while registry.idle.len() > *IDLE_MAPPING_LIMIT {
        let Some(oldest) = registry.idle.pop_front() else {
            break;
        };
        let idle = registry
            .entries
            .get(&oldest)
            .is_some_and(|entry| entry.handles == 0);
        if !idle {
            // Re-acquired while it sat on the queue; `acquire` normally removes
            // the stamp, so this is only a race it lost.
            continue;
        }
        if let Some(entry) = registry.entries.remove(&oldest) {
            evicted += 1;
            MAPPED_BYTES.fetch_sub(entry.bytes, Ordering::Relaxed);
        }
    }
    if evicted > 0 {
        MAPPINGS.fetch_sub(evicted, Ordering::Relaxed);
    }
}

/// Drops every mapping no reader holds, returning the bytes unmapped.
///
/// This is the only place a mapping is unmapped before process exit, so it is
/// what a "give the address space back" pass calls. A handle in flight keeps its
/// own `Arc`, so an entry removed here while a reader still held one would leave
/// that reader valid - the `handles` check exists to avoid that, not to paper
/// over it.
fn unmap_idle() -> usize {
    let Ok(mut registry) = REGISTRY.lock() else {
        return 0;
    };
    let idle: Vec<ObjectKey> = registry
        .entries
        .iter()
        .filter(|(_, entry)| entry.handles == 0)
        .map(|(key, _)| *key)
        .collect();
    let mut bytes = 0usize;
    for key in idle {
        if let Some(entry) = registry.entries.remove(&key) {
            bytes += entry.bytes;
            MAPPINGS.fetch_sub(1, Ordering::Relaxed);
            MAPPED_BYTES.fetch_sub(entry.bytes, Ordering::Relaxed);
        }
    }
    // Everything idle is gone, so the queue is too. (A key that was re-acquired
    // during this pass keeps its entry and is back on the queue on release.)
    registry.idle.clear();
    bytes
}

/// `(dev, ino)` of an open file.
#[cfg(unix)]
fn object_key(file: &std::fs::File, path: &Path) -> ObjectKey {
    use std::os::unix::fs::MetadataExt;
    match file.metadata() {
        Ok(meta) => (0, meta.dev(), meta.ino()),
        // A metadata read cannot fail for an open descriptor on a supported
        // filesystem. Falling back to a unique key keeps the read correct (a
        // duplicate VMA at worst) instead of failing it.
        Err(_) => {
            static FALLBACK: AtomicUsize = AtomicUsize::new(1);
            let _ = path;
            (0, u64::MAX, FALLBACK.fetch_add(1, Ordering::Relaxed) as u64)
        }
    }
}

#[cfg(not(unix))]
fn object_key(file: &std::fs::File, path: &Path) -> ObjectKey {
    let _ = file;
    // No stable identity to share by: every mapping is its own object.
    static FALLBACK: AtomicUsize = AtomicUsize::new(1);
    let _ = path;
    (0, u64::MAX, FALLBACK.fetch_add(1, Ordering::Relaxed) as u64)
}

/// Where the next [`release_mapped_pages`] pass starts.
///
/// A pass stops once it believes it has released `want` bytes, and the figure it
/// counts is the mapping's length - an upper bound, since not every page of a
/// mapping is resident. Starting from the same entry every time would therefore
/// keep advising the same few mappings while the rest of a tree larger than
/// `want` was never touched. Rotating makes successive passes sweep the whole
/// registry; the caller re-measures afterwards, so a pass that over-counted is
/// followed by one that continues where it left off.
static RELEASE_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// One read-only mapping of a file.
///
/// It derefs to the mapped bytes, so a decoder reads cells straight out of the
/// page cache. Dropping it unmaps the file and takes it off the counters.
pub struct MappedFile {
    /// `None` is the empty mapping: `Mmap::map` refuses a zero-length file
    /// (`EINVAL`), and an index chunk legitimately has an empty `.len` (a
    /// segment whose documents all carry no text). Readers slice this buffer,
    /// so the empty case has to behave like a zero-length file rather than a
    /// failure.
    /// Shared with [`REGISTRY`], which needs a weak handle to reach the pages of
    /// a mapping it does not own. `Arc` is one refcount on a mapping that is
    /// already shared by every reader of the file.
    map: Option<std::sync::Arc<Mmap>>,
    /// The slice of the mapping this reader sees.
    ///
    /// A file written by this engine is mapped whole (`0..len`). A file that
    /// lives inside a composed persistence object is one part of that object,
    /// so the reader maps the object and addresses `part_offset..part_offset +
    /// part_size` ([`Self::open_range`]). Every decoder slices whatever this
    /// buffer holds, so a range behaves exactly like a file of that length.
    range: std::ops::Range<usize>,
    /// The object this handle holds a reference to; `None` for the empty
    /// mapping, which holds nothing and charges nothing.
    key: Option<ObjectKey>,
}

impl MappedFile {
    /// A zero-length mapping, for a file that exists but holds no bytes.
    pub fn empty() -> Self {
        Self {
            map: None,
            range: 0..0,
            key: None,
        }
    }

    /// Maps `path` read-only, through the installed [`crate::chunk_store`].
    ///
    /// This is the read half of the reference's rule that every data file goes
    /// through the persistence manager (`pm->GetObjCache` +
    /// `VirtualStore::MmapFile(pm->GetObjPath(...))`). With the direct-disk
    /// store this is [`Self::open_path`]; with a persistence profile the bytes
    /// are resolved to the object that holds them when no local copy exists.
    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        crate::chunk_store::chunk_store().open(path.as_ref())
    }

    /// Maps a real file, read-only.
    ///
    /// A file the process already has mapped is **reused**: the caller gets a
    /// second handle to the same `Mmap`, not a second VMA. Two mappings of one
    /// file would share the physical pages anyway, but RSS charges each of them,
    /// so without this one block read by two readers reads as twice its size.
    ///
    /// The mapping stays valid even when the file is rewritten while it is
    /// alive: every publisher writes a sibling temp file and renames it over the
    /// target (`definition::format::write_synced`), which swaps the directory
    /// entry and leaves the inode this mapping refers to untouched. Truncating
    /// in place instead would make the mapping fault with SIGBUS.
    #[allow(unsafe_code)]
    pub fn open_path(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        // A zero-length file cannot be mapped; it is an empty mapping instead.
        // Asked before the key so an empty file never takes a table entry.
        if file.metadata()?.len() == 0 {
            return Ok(Self::empty());
        }
        let key = object_key(&file, path);
        // SAFETY: the mapping is read-only and the file is immutable while it
        // is mapped (see the function docs). `Mmap` keeps the mapping valid
        // after the descriptor is closed, so the file is dropped at once.
        let (map, created) = acquire(key, || unsafe { Mmap::map(&file) }.map(std::sync::Arc::new))?;
        if created {
            MAPPINGS.fetch_add(1, Ordering::Relaxed);
            MAPPED_BYTES.fetch_add(map.len(), Ordering::Relaxed);
        }
        let len = map.len();
        Ok(Self {
            map: Some(map),
            range: 0..len,
            key: Some(key),
        })
    }

    /// Maps `path` read-only and exposes `offset..offset + len` of it.
    ///
    /// This is how a chunk inside a composed persistence object is read: the
    /// object file holds many parts and the [`crate::persistence::ObjAddr`]
    /// names one of them. A zero `len` is an empty mapping, and a range that
    /// does not fit the file is an error rather than a short read - a truncated
    /// object means the record and the bytes disagree, which a decoder must not
    /// paper over.
    #[allow(unsafe_code)]
    pub fn open_range(path: impl AsRef<Path>, offset: usize, len: usize) -> std::io::Result<Self> {
        if len == 0 {
            return Ok(Self::empty());
        }
        let path = path.as_ref();
        let file = std::fs::File::open(path)?;
        let file_len = file.metadata()?.len() as usize;
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= file_len)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!(
                        "range {offset}..{} does not fit a {file_len}-byte file",
                        offset.saturating_add(len)
                    ),
                )
            })?;
        let key = object_key(&file, path);
        // SAFETY: the mapping is read-only and the object file is immutable
        // while it is mapped (a persisted object is replaced, never truncated
        // in place).
        let (map, created) = acquire(key, || unsafe { Mmap::map(&file) }.map(std::sync::Arc::new))?;
        if created {
            MAPPINGS.fetch_add(1, Ordering::Relaxed);
            // The whole object is what can become resident, so it is charged
            // whole: the charge is what `release_mapped_pages` reclaims, and
            // that is a property of the mapping, not of the window one reader
            // looks at.
            MAPPED_BYTES.fetch_add(map.len(), Ordering::Relaxed);
        }
        Ok(Self {
            map: Some(map),
            range: offset..end,
            key: Some(key),
        })
    }

    /// Takes an already-created mapping (a reader that mapped its own file hands
    /// the result here, so every mapping in the process is counted the same
    /// way).
    ///
    /// The caller's `Mmap` was built from a descriptor this function never sees,
    /// so there is no inode to share by: each call is its own object. Prefer
    /// [`Self::open_path`], which dedups.
    pub fn from_mmap(map: Mmap) -> Self {
        let len = map.len();
        MAPPINGS.fetch_add(1, Ordering::Relaxed);
        MAPPED_BYTES.fetch_add(len, Ordering::Relaxed);
        let map = std::sync::Arc::new(map);
        // A key no `open_path` can produce: the disambiguator is 1.
        static UNIQUE: AtomicUsize = AtomicUsize::new(1);
        let key = (1u8, u64::MAX, UNIQUE.fetch_add(1, Ordering::Relaxed) as u64);
        let Ok(mut registry) = REGISTRY.lock() else {
            return Self {
                map: Some(map),
                range: 0..len,
                key: None,
            };
        };
        registry.entries.insert(
            key,
            Entry {
                map: std::sync::Arc::clone(&map),
                handles: 1,
                bytes: len,
            },
        );
        // No idle stamp: this handle is the object's first reference, and the
        // queue is filled on release.
        Self {
            map: Some(map),
            range: 0..len,
            key: Some(key),
        }
    }

    /// Asks the kernel to drop the resident pages of this mapping.
    ///
    /// A mapped index chunk's slot pages are clean file-backed pages: dropping
    /// them costs a later re-read from the object/file and nothing else, which
    /// is the only way back to a small RSS without closing the index. Returns
    /// the bytes whose pages were dropped (0 for the empty mapping).
    ///
    /// Correct while readers hold the mapping: `MADV_DONTNEED` on a read-only
    /// file mapping only discards cache; the next access faults the page back
    /// in from the file, which is immutable once published.
    #[allow(unsafe_code)]
    pub fn release_pages(&self) -> usize {
        let Some(map) = &self.map else {
            return 0;
        };
        // The mapping is only touched by the Unix branch below: Windows has no
        // page-release primitive here, and the accounting below still applies.
        #[cfg(not(unix))]
        let _ = map;
        #[cfg(unix)]
        {
            // SAFETY: every mapping this process makes is read-only over an
            // immutable file (a publisher renames a new file over the old one,
            // never truncating or writing in place), so `MADV_DONTNEED` drops
            // clean pages and the next access faults the same bytes back in.
            // No borrow of the mapped slice is created here, and a concurrent
            // reader outside this call sees unchanged contents - the mapping is
            // never written through.
            if unsafe { map.unchecked_advise(memmap2::UncheckedAdvice::DontNeed) }.is_err() {
                return 0;
            }
        }
        let len = self.range.len();
        PAGES_RELEASED.fetch_add(len, Ordering::Relaxed);
        len
    }

    /// The file this mapping covers, while it is alive.
    pub fn len(&self) -> usize {
        self.range.len()
    }

    pub fn is_empty(&self) -> bool {
        self.range.is_empty()
    }

    fn bytes(&self) -> &[u8] {
        match &self.map {
            Some(map) => &map[self.range.clone()],
            None => &[],
        }
    }
}

impl AsRef<[u8]> for MappedFile {
    fn as_ref(&self) -> &[u8] {
        self.bytes()
    }
}

/// The bytes of a **read-only** mapping as `&[f32]`, without copying, when the
/// slice is 4-byte aligned and a whole number of `f32`.
///
/// A block stores an `EMBEDDING(FLOAT, d)` row as `d` little-endian `f32`, and
/// a mapping starts on a page boundary, so the row's bytes are already the
/// numbers a scan scores. `from_le_bytes` per element copies them into a `Vec`
/// first - 1M allocations and 512 MB moved on SIFT 1M, 64% of the query. This
/// hands back the same memory typed as `f32`.
///
/// SAFETY (of the transmute): the mapping is read-only over an immutable file
/// (a publisher renames a new file over the old one, never writes in place), so
/// no byte changes while the borrow lives; the alignment and length are checked
/// before the cast, and the lifetime is tied to `bytes`.
#[allow(unsafe_code)]
pub fn f32_slice(bytes: &[u8]) -> Option<&[f32]> {
    // SAFETY: `align_to` splits into the prefix/suffix that are not
    // `f32`-aligned; requiring both empty means every byte is in `mid`, whose
    // length is a whole number of `f32`. The source is a `&[u8]` over an
    // immutable read-only mapping, so the aliased `&[f32]` sees the same bytes
    // for its whole lifetime.
    let (head, mid, tail) = unsafe { bytes.align_to::<f32>() };
    if head.is_empty() && tail.is_empty() {
        Some(mid)
    } else {
        None
    }
}

impl Deref for MappedFile {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.bytes()
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // Only the handle goes: the object stays mapped (and counted) so the
        // next reader reuses it. See [`release`].
        if let Some(key) = self.key.take() {
            release(key);
        }
    }
}

impl std::fmt::Debug for MappedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MappedFile")
            .field("len", &self.len())
            .finish()
    }
}

/// How many files this process has mapped.
pub fn mapping_count() -> usize {
    MAPPINGS.load(Ordering::Relaxed)
}

/// How many bytes the live mappings cover.
pub fn mapped_bytes() -> usize {
    MAPPED_BYTES.load(Ordering::Relaxed)
}

/// How many mapped bytes have had their resident pages dropped.
pub fn pages_released_bytes() -> usize {
    PAGES_RELEASED.load(Ordering::Relaxed)
}

/// How many mappings are idle (no reader holds them) and kept for reuse.
///
/// Bounded by [`idle_mapping_limit`]; a reclaim pass can give them back through
/// [`unmap_idle_mappings`].
pub fn idle_mapping_count() -> usize {
    let Ok(registry) = REGISTRY.lock() else {
        return 0;
    };
    registry
        .entries
        .values()
        .filter(|entry| entry.handles == 0)
        .count()
}

/// The idle bound in force, derived from `vm.max_map_count` where the platform
/// publishes one.
pub fn idle_mapping_limit() -> usize {
    *IDLE_MAPPING_LIMIT
}

/// `vm.max_map_count` as the kernel reports it, or `None` off Linux.
///
/// Exposed for diagnostics: a value below 262144 is worth a warning at startup
/// (Elasticsearch asks for 1048576), because a wide scan over many blocks will
/// otherwise hit `ENOMEM` and fail a query rather than slow it down.
pub fn kernel_max_map_count() -> Option<usize> {
    max_map_count()
}

/// Drops the resident pages of live mappings until `want` bytes have been
/// released (all of them for [`usize::MAX`]), returning what was released.
///
/// The pass walks [`REGISTRY`] - an in-memory table - instead of the data
/// directory, so it costs one `madvise` per object and no `open`/`stat` per
/// file. Every reader in the process goes through [`MappedFile`], so one call
/// covers the block files, the index chunks, the text segments and the
/// secondary structures alike.
///
/// One `madvise` per **object**, not per handle: readers of one file now share
/// a mapping, so a file read ten times is advised once. (Before the table was
/// keyed by identity the same file could appear several times, and each copy
/// was advised separately for the same pages.)
///
/// Correct while readers hold a mapping: `MADV_DONTNEED` on a read-only mapping
/// of an immutable file only drops clean pages, and the next access faults them
/// back in.
#[allow(unsafe_code)]
pub fn release_mapped_pages(want: usize) -> usize {
    let Ok(registry) = REGISTRY.lock() else {
        return 0;
    };
    if registry.entries.is_empty() {
        return 0;
    }
    // Collect first, advise outside the lock: `madvise` on a large mapping is a
    // syscall, and holding the table's lock across it would stall every reader
    // that is opening a file. The `Arc`s keep each mapping alive while the lock
    // is not held.
    let maps: Vec<std::sync::Arc<Mmap>> = registry
        .entries
        .values()
        .map(|entry| std::sync::Arc::clone(&entry.map))
        .collect();
    drop(registry);
    let len = maps.len();
    // Start where the last pass stopped (see [`RELEASE_CURSOR`]) and wrap, so a
    // tree larger than one pass's target is covered over successive passes
    // instead of the same head being advised every second.
    let start = RELEASE_CURSOR.fetch_add(1, Ordering::Relaxed) % len;
    let mut released = 0usize;
    for step in 0..len {
        if released >= want {
            break;
        }
        let map = &maps[(start + step) % len];
        #[cfg(unix)]
        {
            // SAFETY: the mapping is read-only over an immutable file (a
            // publisher renames a new file over the old one, never truncating or
            // writing in place); no borrow of the bytes is created here, and a
            // concurrent reader sees unchanged contents - only the cached pages
            // go away.
            if unsafe { map.unchecked_advise(memmap2::UncheckedAdvice::DontNeed) }.is_err() {
                continue;
            }
        }
        released += map.len();
        PAGES_RELEASED.fetch_add(map.len(), Ordering::Relaxed);
    }
    released
}

/// Unmaps every object no reader is holding, returning the bytes released.
///
/// The counterpart of [`release_mapped_pages`] one level up: that one keeps the
/// mapping and drops its pages (cheap to undo, faults back in), this one gives
/// the address space back. A reclaim pass that cannot get far enough reaches for
/// it; ordinary steady-state reclaim should not, because the next read pays a
/// fresh `mmap`.
pub fn unmap_idle_mappings() -> usize {
    unmap_idle()
}
