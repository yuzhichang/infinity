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
/// The first field is a **kind tag**, not part of the identity: `0` identifies a
/// mapping made from a path, `(u64::MAX, n)` a fallback for a descriptor whose
/// metadata could not be read (a unique key, so the mapping is never shared by
/// mistake). Every engine reader now goes through [`MappedFile::open_path`], so
/// the fallback exists only for that error path - there is no longer a
/// caller-supplied `Mmap` to invent a second kind for.
type ObjectKey = (u8, u64, u64);

/// One mapped object and the readers holding it.
struct Entry {
    map: std::sync::Arc<Mmap>,
    /// Handles currently holding this object through [`MappedFile`].
    handles: usize,
    /// `map.len()`, charged once per object.
    bytes: usize,
    /// Which tables this object belongs to, with a count each.
    ///
    /// Usually one. A persistence object packs several chunks into one file
    /// (`PersistentChunkStore::resolve` returns the same object with different
    /// offsets), and those parts can belong to different tables - so the charge
    /// is split, each table paying `bytes * mine / total`.
    ///
    /// A table's count is **cumulative**, not the number of live handles: it
    /// goes to zero only when the object is unmapped. An idle mapping is still
    /// charged to its table - that is the whole point, since a reclaim pass
    /// decides from that figure which table's idle mappings to drop. Using the
    /// live handle count would report zero for every idle object, which is the
    /// state a reclaim pass is most interested in.
    tables: Vec<(u64, usize)>,
}

impl Entry {
    fn hold(&mut self, table: Option<u64>) {
        let Some(table) = table else {
            return;
        };
        match self.tables.iter_mut().find(|(id, _)| *id == table) {
            Some((_, count)) => *count += 1,
            None => self.tables.push((table, 1)),
        }
    }
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
    table: Option<u64>,
    create: impl FnOnce() -> std::io::Result<std::sync::Arc<Mmap>>,
) -> std::io::Result<(std::sync::Arc<Mmap>, bool)> {
    let Ok(mut registry) = REGISTRY.lock() else {
        // A poisoned table cannot hand out shared mappings. A private one is
        // still correct, it just costs the VMA this table exists to avoid.
        return create().map(|map| (map, true));
    };
    if let Some(entry) = registry.entries.get_mut(&key) {
        entry.handles += 1;
        entry.hold(table);
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
    let mut entry = Entry {
        map: std::sync::Arc::clone(&map),
        handles: 1,
        bytes,
        tables: Vec::new(),
    };
    entry.hold(table);
    registry.entries.insert(key, entry);
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
        let table = crate::format::table_of_path(path);
        let (map, created) = acquire(key, table, || {
            unsafe { Mmap::map(&file) }.map(std::sync::Arc::new)
        })?;
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
        let table = crate::format::table_of_path(path);
        let (map, created) = acquire(key, table, || {
            unsafe { Mmap::map(&file) }.map(std::sync::Arc::new)
        })?;
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

/// A guard a test takes before asserting on exact mapping counts.
///
/// The table is process-wide, so two tests that map files at the same time
/// cannot both see exact numbers. `cargo test` runs a binary's tests in parallel
/// threads, so this serialises the ones that need exactness - the alternative is
/// a flaky `left: 573, right: 0`.
// `cfg(test)` would not reach a dependent crate's tests (that flag is per
// crate), so this is gated on debug assertions instead - a test binary always
// has them, and a release build drops the function.
#[cfg(debug_assertions)]
pub fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
    GUARD
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// Bytes the mapping table holds on `table`'s behalf.
///
/// The size arm of the table-level reclaim fusion: a table's footprint is what
/// it holds unsealed **plus** how much of its mappings is **resident**, because
/// a scan over a large index leaves those pages in RSS and dropping the
/// table's mappings is one of the ways to get them back.
///
/// Sums each object's [`Entry::share_of`], so an object shared by two tables
/// (a persistence object holding both their chunks) is split between them
/// instead of counted twice.
pub fn mapped_bytes_of(table: u64) -> u64 {
    resident_totals().get(&table).copied().unwrap_or(0)
}

/// Every table with at least one live mapping, and what it holds.
///
/// One pass for the whole fusion rather than a call per table: the reclaim loop
/// needs the mapped size of every candidate, and a per-table query would walk
/// the registry once per table.
pub fn mapped_bytes_by_table() -> std::collections::HashMap<u64, u64> {
    resident_totals()
}

/// Resident bytes per table.
///
/// The per-region answer where the platform has one (**Linux** `smaps`,
/// **macOS** `proc_pidinfo`). On **Windows** there is none short of walking every
/// page with `QueryWorkingSetEx`, so the answer is *zero*: a table is charged
/// for nothing it has mapped, which means the reclaim decision falls back to the
/// unsealed ledger alone on that platform. That is honest - "we do not know" -
/// and it costs no code. An apportioned estimate was considered and rejected:
/// spreading one process-wide figure by mapped length gives every table a share
/// that is not its own, so a reclaim pass would rank tables by an artefact of
/// the split rather than by what they hold.
///
/// **Snapshot the maps first, read residency outside the lock.** Parsing `smaps`
/// for a process with thousands of VMAs is not free, and holding the table's
/// lock across it would stall every reader that is opening a file. The `Arc`s
/// keep each mapping alive while the lock is not held; a mapping opened in
/// between is simply not counted this pass.
fn resident_totals() -> std::collections::HashMap<u64, u64> {
    /// One mapping taken out of the registry: the mapping itself and the tables
    /// holding it.
    type Snapshot = (std::sync::Arc<Mmap>, Vec<(u64, usize)>);
    let snapshot: Vec<Snapshot> = {
        let Ok(registry) = REGISTRY.lock() else {
            return std::collections::HashMap::new();
        };
        registry
            .entries
            .values()
            .map(|entry| (std::sync::Arc::clone(&entry.map), entry.tables.clone()))
            .collect()
    };

    let mut out: std::collections::HashMap<u64, u64> = std::collections::HashMap::new();
    // The whole `smaps` file is read once and reused for every mapping: one
    // parse, not one per mapping (a table can hold thousands).
    #[cfg(target_os = "linux")]
    let ranges = lin_smaps_ranges();
    for (map, tables) in &snapshot {
        let resident = resident_from_snapshot(
            map,
            #[cfg(target_os = "linux")]
            ranges.as_ref(),
        ) as u64;
        let live: usize = tables.iter().map(|(_, count)| *count).sum();
        if live == 0 {
            continue;
        }
        for (table, count) in tables {
            *out.entry(*table).or_default() += resident * *count as u64 / live as u64;
        }
    }
    out
}

/// One mapping's resident bytes, given an already-parsed `smaps` where the
/// platform has one.
///
/// `None` from the parser means `smaps` could not be read at all; the mapping's
/// length is then the best available upper bound.
#[cfg(target_os = "linux")]
fn resident_from_snapshot(map: &Mmap, ranges: Option<&Vec<(usize, usize, usize)>>) -> usize {
    let Some(ranges) = ranges else {
        return map.len();
    };
    let start = map.as_ptr() as usize;
    let end = start.saturating_add(map.len());
    let mut total = 0usize;
    for (lo, hi, rss) in ranges {
        if *lo < end && *hi > start {
            // A VMA partially inside the mapping contributes its whole `Rss`,
            // which over-counts at a boundary. That errs toward freeing.
            total += rss;
        }
    }
    total
}

#[cfg(not(target_os = "linux"))]
fn resident_from_snapshot(map: &Mmap, _unused: ()) -> usize {
    resident_of(map)
}

/// `proc_regioninfo` from `<libproc.h>`, up to `pri_pages_resident`.
///
/// Only the leading fields are declared. `proc_pidinfo` fills the whole struct
/// and this prefix is enough to read the one figure needed. The layout is a
/// stable public ABI - it is what `vmmap` reads - and `repr(C)` states it rather
/// than relying on field-order luck.
#[cfg(target_os = "macos")]
#[repr(C)]
#[derive(Default)]
struct ProcRegionInfo {
    pri_protection: u32,
    pri_max_protection: u32,
    pri_inheritance: u32,
    pri_flags: u32,
    pri_offset: u64,
    pri_behavior: u32,
    pri_user_wired_count: u32,
    pri_user_tag: u32,
    /// Pages of this region that are resident - the figure wanted here.
    pri_pages_resident: u32,
    /// The rest of the kernel's struct, never read. Kept so the buffer passed to
    /// `proc_pidinfo` is at least as large as the call writes.
    _rest: [u8; 128],
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn resident_of(map: &Mmap) -> usize {
    // `PROC_PIDREGIONINFO` = 7: the region containing the given address.
    use libc::{c_int, c_void, pid_t};
    const PROC_PIDREGIONINFO: c_int = 7;
    let pid = std::process::id() as pid_t;
    let page = crate::page_size();
    let start = map.as_ptr() as usize;
    let end = start.saturating_add(map.len());
    let mut cursor = start;
    let mut total = 0usize;
    let mut any = false;
    while cursor < end {
        let mut info = ProcRegionInfo::default();
        // SAFETY: `info` is a local of exactly the size passed, and the call
        // writes at most that many bytes into it.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                PROC_PIDREGIONINFO,
                cursor as u64,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<ProcRegionInfo>() as c_int,
            )
        };
        if written <= 0 {
            break;
        }
        // `pri_offset` is the region's start address in this struct's ABI.
        let len = (info.pri_offset as usize).max(page);
        total += info.pri_pages_resident as usize * page;
        any = true;
        let next = cursor.saturating_add(len);
        if next <= cursor {
            break;
        }
        cursor = next;
    }
    if any {
        total
    } else {
        // The walk failed (a sandbox without the entitlement). The mapping's
        // length over-charges rather than under-charges, which is the safe
        // direction for a reclaim decision.
        map.len()
    }
}

/// No per-region residency on Windows short of walking every page with
/// `QueryWorkingSetEx`, so this reports **zero**.
///
/// An apportioned estimate (spread one process-wide figure by mapped length) was
/// considered and rejected: the share a table gets is then an artefact of the
/// split rather than something it holds, and a reclaim pass would rank tables by
/// that. Reporting zero says "unknown" plainly, costs no code, and leaves the
/// reclaim decision on the unsealed ledger - which is what that platform had
/// before any of this.
#[cfg(windows)]
fn resident_of(_map: &Mmap) -> usize {
    0
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn resident_of(_map: &Mmap) -> usize {
    0
}

/// Every VMA's `(start, end, rss)` from `/proc/self/smaps`, parsed once.
#[cfg(target_os = "linux")]
fn lin_smaps_ranges() -> Option<Vec<(usize, usize, usize)>> {
    use std::io::BufRead;
    let file = std::fs::File::open("/proc/self/smaps").ok()?;
    let reader = std::io::BufReader::new(file);
    let mut out: Vec<(usize, usize, usize)> = Vec::new();
    for line in reader.lines().map_while(Result::ok) {
        if let Some((range, _)) = line.split_once(' ') {
            if let Some((lo, hi)) = range.split_once('-') {
                if let (Ok(lo), Ok(hi)) =
                    (usize::from_str_radix(lo, 16), usize::from_str_radix(hi, 16))
                {
                    out.push((lo, hi, 0));
                    continue;
                }
            }
        }
        if let Some(rest) = line.strip_prefix("Rss:") {
            if let Some(last) = out.last_mut() {
                last.2 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|kb| kb.parse::<usize>().ok())
                    .unwrap_or(0)
                    * 1024;
            }
        }
    }
    (!out.is_empty()).then_some(out)
}

/// Unmaps every idle mapping that belongs to `table`, returning the bytes
/// released.
///
/// The per-table counterpart of [`unmap_idle_mappings`]: a reclaim pass that has
/// picked its victim needs to give *that* table's address space back, not the
/// process's. A mapping a reader holds is left alone.
pub fn unmap_idle_of(table: u64) -> usize {
    let Ok(mut registry) = REGISTRY.lock() else {
        return 0;
    };
    let victims: Vec<ObjectKey> = registry
        .entries
        .iter()
        .filter(|(_, entry)| entry.handles == 0 && entry.tables.iter().any(|(id, _)| *id == table))
        .map(|(key, _)| *key)
        .collect();
    let mut bytes = 0usize;
    let mut evicted = 0usize;
    for key in victims {
        if let Some(entry) = registry.entries.remove(&key) {
            bytes += entry.bytes;
            evicted += 1;
            MAPPED_BYTES.fetch_sub(entry.bytes, Ordering::Relaxed);
        }
    }
    if evicted > 0 {
        MAPPINGS.fetch_sub(evicted, Ordering::Relaxed);
        let live: std::collections::HashSet<ObjectKey> = registry.entries.keys().copied().collect();
        registry.idle.retain(|key| live.contains(key));
    }
    bytes
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
