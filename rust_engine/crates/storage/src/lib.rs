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

//! `infinity-storage`: the storage kernel (RUST_REWRITE_PLAN.md §3 D2 + §附录 A).
//!
//! Implements the three independent substrates at their trait boundaries:
//!   * [`wal`]      — custom append-only redo log (CRC-32/IEEE framed)
//!   * [`mvcc`]     — timestamp-array MVCC (`BlockVersion`)
//!   * [`metastore`] — the `MetaStore` trait, backed by RocksDB
//!
//! Invariants preserved from C++ (see plan 附录 A): the substrates are separate;
//! MVCC is timestamp-array based (not KV snapshots); WAL writes are serialized.

// `#![deny(unsafe_code)]` with per-site `#[allow(unsafe_code)]`: the `unsafe`
// here is `mmap` (mapping a segment is far cheaper than reading it into the heap
// - see PORTING.md §2.8) plus three narrow FFI entry points (`mi_collect`,
// `malloc_trim`, `posix_fadvise`) that keep RSS and the page cache bounded
// during large imports. Every call site carries a SAFETY note saying why no safe
// API can express it.
#![deny(unsafe_code)]
// The workspace pins a nightly toolchain (see `rust-toolchain.toml`) because
// `std::simd` is still unstable: the posting decoder unpacks fastpfor's four
// lanes with portable SIMD, so the crate needs neither `std::arch` intrinsics
// (which would need `unsafe`) nor a bit-at-a-time scalar loop.
#![feature(portable_simd)]

/// The system's page size in bytes.
///
/// Needed wherever a residency figure is reported in pages rather than bytes -
/// `mach_vm_region`'s `pages_resident` is the one in this crate. Not a constant:
/// Apple silicon uses 16 KiB pages while x86_64 uses 4 KiB, and a hardcoded
/// 4096 would undercount a mapping's footprint by 4x on arm64.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn page_size() -> usize {
    // SAFETY: `sysconf` takes a constant key and does not touch caller memory.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).unwrap_or(4096)
}

#[cfg(windows)]
pub fn page_size() -> usize {
    use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
    // SAFETY: `info` is a local the API fills in; it has no other requirement.
    let mut info: SYSTEM_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: as above - a valid, writable, correctly sized out pointer.
    unsafe { GetSystemInfo(&mut info) };
    info.dwPageSize as usize
}

#[cfg(not(any(unix, windows)))]
pub fn page_size() -> usize {
    4096
}

/// A process's resident memory, split into its parts where the platform
/// publishes a split.
///
/// Lives here rather than in the executor because every part of it is a
/// platform question - which `/proc` file to read, which `task_info` flavour to
/// ask for, which Windows counter to look at - and this crate is where the
/// other platform probes live (`page_size`, `physical_memory_bytes`). The
/// executor only formats the result.
///
/// **Per platform:**
///
/// * **Linux** - `/proc/self/status` publishes `VmRSS` with `RssAnon`,
///   `RssFile` and `RssShmem`; all four fields are real.
/// * **macOS** - `task_info(TASK_BASIC_INFO)` publishes the total
///   (`resident_size`) only. The parts would need `TASK_VM_INFO`'s ledger
///   fields, which are not a stable ABI.
/// * **Windows** - `GetProcessMemoryInfo` reports `WorkingSetSize`; the
///   identity `working set = private + shareable` is not exposed.
///
/// Where the split is missing, [`RssSplit::has_split`] is false and a log line
/// prints the total without inventing a breakdown - `Anon=0` would read as a
/// measurement rather than as a missing one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RssSplit {
    /// The resident total: `VmRSS` / `resident_size` / `WorkingSetSize`.
    pub vm_rss: u64,
    /// Anonymous (heap) resident bytes; 0 when `has_split` is false.
    pub rss_anon: u64,
    /// File-backed resident bytes - the page cache behind the engine's
    /// mappings; 0 when `has_split` is false.
    pub rss_file: u64,
    /// Shared-memory resident bytes; 0 when `has_split` is false.
    pub rss_shmem: u64,
    /// Whether the three parts are real or merely absent. A flag, not "all
    /// zero": a process with no shared memory has `RssShmem == 0` on Linux too.
    pub has_split: bool,
}

impl std::fmt::Display for RssSplit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const MIB: u64 = 1024 * 1024;
        if self.has_split {
            write!(
                f,
                "VmRSS={} MiB (Anon={} File={} Shmem={})",
                self.vm_rss / MIB,
                self.rss_anon / MIB,
                self.rss_file / MIB,
                self.rss_shmem / MIB
            )
        } else {
            write!(
                f,
                "VmRSS={} MiB (no split on this platform)",
                self.vm_rss / MIB
            )
        }
    }
}

/// The process's resident memory, split where the platform allows it. See
/// [`RssSplit`].
#[cfg(target_os = "linux")]
pub fn process_rss_split() -> RssSplit {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        // No `/proc/self/status`: fall back to `statm`'s resident page count,
        // the same fallback the executor's own reader uses.
        let pages = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|statm| {
                statm
                    .split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
            })
            .unwrap_or(0);
        return RssSplit {
            vm_rss: pages * page_size() as u64,
            ..Default::default()
        };
    };
    let mut split = RssSplit::default();
    for line in status.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let kb: u64 = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        match key {
            "VmRSS" => split.vm_rss = kb * 1024,
            "RssAnon" => split.rss_anon = kb * 1024,
            "RssFile" => split.rss_file = kb * 1024,
            "RssShmem" => split.rss_shmem = kb * 1024,
            _ => {}
        }
    }
    // The three `Rss*` lines arrived together in Linux 4.5; their presence is
    // what says the split is there, not a non-zero value.
    split.has_split = status.contains("RssAnon:");
    split
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn process_rss_split() -> RssSplit {
    const TASK_BASIC_INFO: u32 = 4;
    const TASK_BASIC_INFO_COUNT: u32 = 10;
    let mut info = libc::mach_task_basic_info {
        virtual_size: 0,
        resident_size: 0,
        resident_size_max: 0,
        user_time: libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        },
        system_time: libc::time_value_t {
            seconds: 0,
            microseconds: 0,
        },
        policy: 0,
        suspend_count: 0,
    };
    let mut count: libc::mach_msg_type_number_t = TASK_BASIC_INFO_COUNT;
    // SAFETY: `info` is a local of the type `TASK_BASIC_INFO` writes and
    // `count` is its word count - exactly what the call expects.
    let kr = unsafe {
        libc::task_info(
            libc::mach_task_self_,
            TASK_BASIC_INFO,
            &mut info as *mut _ as libc::task_info_t,
            &mut count,
        )
    };
    RssSplit {
        vm_rss: if kr == libc::KERN_SUCCESS {
            info.resident_size
        } else {
            0
        },
        ..Default::default()
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
pub fn process_rss_split() -> RssSplit {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters` is a correctly sized local; the pseudo-handle from
    // `GetCurrentProcess` is not owned and needs no closing.
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    RssSplit {
        vm_rss: if ok != 0 {
            counters.WorkingSetSize as u64
        } else {
            0
        },
        ..Default::default()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn process_rss_split() -> RssSplit {
    RssSplit::default()
}

/// Total physical memory reported by the operating system, in bytes.
#[cfg(unix)]
#[allow(unsafe_code)]
pub fn physical_memory_bytes() -> Option<u64> {
    // SAFETY: `sysconf` takes constant keys and does not access caller memory.
    let (pages, page_size) = unsafe {
        (
            libc::sysconf(libc::_SC_PHYS_PAGES),
            libc::sysconf(libc::_SC_PAGESIZE),
        )
    };
    u64::try_from(pages)
        .ok()?
        .checked_mul(u64::try_from(page_size).ok()?)
}

/// Total physical memory reported by the operating system, in bytes.
#[cfg(windows)]
#[allow(unsafe_code)]
pub fn physical_memory_bytes() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    let mut memory = MEMORYSTATUSEX {
        dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32,
        ..Default::default()
    };
    // SAFETY: `memory` points to writable storage with the size the API requires.
    (unsafe { GlobalMemoryStatusEx(&mut memory) } != 0).then_some(memory.ullTotalPhys)
}

#[cfg(not(any(unix, windows)))]
pub fn physical_memory_bytes() -> Option<u64> {
    None
}

/// Releases free heap pages back to the OS, on every target.
/// Long imports allocate per-batch buffers; without this the allocator retains
/// them and RSS looks like a leak (Gate C).
///
/// The shipping binary makes mimalloc the global allocator
/// (`mimalloc::MiMalloc` in `common`), whose pages `malloc_trim` cannot see and
/// whose purge timer only runs for *segments*, not for the freed arenas a
/// checkpoint or a memory-index dump leaves behind. On Linux glibc reclaims
/// those when it feels enough pressure, which is why the gate's peak looked
/// allocator-dependent; on macOS nothing ever asks, so RSS only ratcheted up.
/// `mi_collect(true)` purges the free pages of the calling thread's heap and
/// its arenas, which is mimalloc's own `mi_collect` entry point
/// (mimalloc.h) - the same call its `free` path defers.
#[allow(unsafe_code)]
pub fn trim_allocator() {
    // The workspace has exactly one allocator (`common`): mimalloc. Linking is
    // per binary, and every binary that reaches this crate pulls `common` in,
    // so the symbol resolves wherever this is called.
    unsafe extern "C" {
        fn mi_collect(force: bool);
    }
    // SAFETY: `mi_collect` only returns pages mimalloc already owns and has
    // marked free; it cannot touch a live allocation.
    unsafe {
        mi_collect(true);
    }
    // Belt and braces for a build where mimalloc is not the global allocator.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: glibc's `malloc_trim` only returns already-free heap pages.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Pins the calling OS thread to the given logical CPU indices on Linux.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn pin_current_thread_to_cpus(cpus: &[usize]) {
    // Linux `cpu_set_t` is a 1024-bit (128-byte) bitmask aligned to `usize`.
    const WORDS: usize = 1024 / (std::mem::size_of::<usize>() * 8);
    #[repr(C)]
    struct CpuSet([usize; WORDS]);
    unsafe extern "C" {
        fn sched_setaffinity(pid: i32, cpusetsize: usize, mask: *const CpuSet) -> i32;
    }
    let mut set = CpuSet([0; WORDS]);
    let bits_per_word = std::mem::size_of::<usize>() * 8;
    for &cpu in cpus {
        if cpu < 1024 {
            set.0[cpu / bits_per_word] |= 1usize << (cpu % bits_per_word);
        }
    }
    // SAFETY: `pid == 0` targets the calling thread and `set` is a valid `cpu_set_t`.
    unsafe {
        sched_setaffinity(0, std::mem::size_of::<CpuSet>(), &set);
    }
}

/// No-op on non-Linux targets.
#[cfg(not(target_os = "linux"))]
pub fn pin_current_thread_to_cpus(_cpus: &[usize]) {}

/// Drops clean page cache for one file after writing it back. Long imports
/// write gigabytes of segment/sidecar files; without this the cache grows
/// until the OOM killer fires (the reference bounds it with its buffer manager).
///
/// Linux only: `posix_fadvise(DONTNEED)` is the one call that drops pages a file
/// already has cached. Darwin exposes only `fcntl(F_NOCACHE)`, which cannot, and
/// Windows has no equivalent on a closed handle - on those targets the pages of
/// a **mapped** file are dropped through [`mapped::release_mapped_pages`]
/// instead, which is where the reclaim path goes anyway.
#[allow(unsafe_code)]
pub fn drop_file_cache(path: &std::path::Path) {
    #[cfg(not(target_os = "linux"))]
    let _ = path;
    #[cfg(target_os = "linux")]
    {
        let Ok(file) = std::fs::File::open(path) else {
            return;
        };
        use std::os::unix::io::AsRawFd;
        // No flush first: this drops **read** cache, and `DONTNEED` leaves dirty
        // pages alone anyway - a `sync_data` here only pays a write-out per file
        // on a path that runs when the process is short of memory.
        // SAFETY: `posix_fadvise` only advises the kernel about this fd's cache.
        unsafe {
            posix_fadvise(file.as_raw_fd(), 0, 0, 4 /* POSIX_FADV_DONTNEED */);
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
unsafe extern "C" {
    fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> i32;
}

/// Drops page cache under `root`, visiting at most `budget` files (best effort),
/// returning how many it visited.
///
/// The walk is `read_dir` + `open` per file, so its cost is the tree's, not the
/// caller's: a budget is what keeps one call from stalling the engine on a
/// data directory with a million files. Callers either bound it (a cache drop
/// between checkpoints) or use [`mapped::release_mapped_pages`], which reaches
/// the same pages through the mapping registry without touching the filesystem -
/// that is the one to use on a reclaim path.
pub fn drop_tree_cache(root: &std::path::Path, budget: usize) -> usize {
    let Ok(entries) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut left = budget;
    let mut visited = 0usize;
    for entry in entries.flatten() {
        if left == 0 {
            break;
        }
        let path = entry.path();
        if path.is_dir() {
            let used = drop_tree_cache(&path, left);
            left -= used;
            visited += used;
        } else {
            drop_file_cache(&path);
            left -= 1;
            visited += 1;
        }
    }
    visited
}

pub mod bg_task;
pub mod buffer;
pub mod catalog;
pub mod column_vector;
pub mod common;
pub mod data_block;
pub mod definition;
pub mod fast_rough_filter;
pub mod invertedindex;
pub mod io;
pub mod json_index;
pub mod knn_index;
pub mod new_txn;
pub mod persistence;
pub mod result_cache_manager;
pub mod secondary_index;
pub mod simd;
pub mod wal;

// The module tree above mirrors the reference's src/storage/. The re-exports
// below keep the flat names the rest of the workspace already uses, so a file
// can be moved to its reference-named directory without touching callers.
pub use bg_task::memindex_tracer;
pub use buffer::{mapped, mapped_block};
pub use catalog::{catalog_format, metastore, metastore_rocks};
pub use column_vector::selection;
pub use common::{block_index, crc, rcu_map};
pub use definition::format;
pub use fast_rough_filter::binary_fuse_filter;
pub use invertedindex::common::external_sort;
pub use invertedindex::fst::registry as fst_registry;
pub use io::{chunk_store, s3, virtual_store};
pub use knn_index::emvb::emvb_segment;
pub use knn_index::knn_format;
pub use knn_index::sparse::{bmp, bmp_segment};
pub use new_txn::{mvcc, txn_kv};
pub use secondary_index::{
    secondary_index_data, secondary_lowcard, secondary_segment_pgm as secondary_segment,
};
pub use wal::wal_entry;

/// Opens the store at the given path (creating it if needed).
///
/// RocksDB is the only substrate: it is what the C++ engine writes, so a
/// directory either engine produced opens with the same code and the same
/// bytes.
pub fn open_store(path: &std::path::Path) -> ::common::Result<std::sync::Arc<dyn MetaStore>> {
    let store = metastore_rocks::RocksMetaStore::open(path)?;
    Ok(std::sync::Arc::new(store))
}

pub use crc::crc32_ieee;
pub use format::{
    block_dir, chunk_index_file, column_file, column_layout, column_outline_file, data_file_body,
    date_to_days, datetime_to_pair, decode_data_file, element_width, embedding_column_layout,
    encode_data_file, fulltext_base_name, fulltext_files, pack_column, pack_column_with_layout,
    pack_tensor_body, parse_flat, parse_nested_f64, parse_tensorarray_f64, push_embedding_element,
    read_block, read_embedding_element, segment_index_dir, sparse_slot, split_body,
    split_body_for_layout, split_top_level, strip_one_bracket, time_to_secs,
    varlen_count_offset_slot, write_block, BlockColumn, BlockColumns, BlockId, BlockVersionFile,
    ColumnBody, ColumnLayout, FormatError, RawBlockColumn, DATA_FILE_MAGIC, DEFAULT_BLOCK_CAPACITY,
    VERSION_FILE,
};
pub use invertedindex::format::{
    append_fst_trailer, decode_column_lengths, decode_term_meta, encode_column_lengths,
    encode_term_meta, encode_term_meta_region, fst_checksum, read_vint, verify_fst, write_vint,
    TermMeta, TermMetaFlags, TermPostings, FST_TRAILER_LEN, FST_VERSION,
};
pub use io::{
    ByteSlice, ByteSliceList, ByteSliceListIterator, ByteSliceReader, ByteSliceWriter, FileReader,
    FileWriter, StreamReader,
};
pub use knn_format::{
    decode_graph_region, decode_ivf_plain_chunk, decode_ivf_pq_chunk, decode_ivf_sq_chunk,
    encode_graph_region, encode_ivf_plain_chunk, encode_ivf_pq_chunk, encode_ivf_sq_chunk,
    sq_bytes_per_vector, HnswChunk, HnswGraphVertex, HnswHeader, IvfChunkHeader, IvfPlainPart,
    IvfPqChunk, IvfPqCodebook, IvfPqPart, IvfSqChunk, IvfSqPart, HNSW_HEADER_LEN, VERTEX_L0_SIZE,
    VERTEX_LX_SIZE,
};
pub use metastore::MetaStore;
pub use metastore_rocks::RocksMetaStore;
pub use mvcc::{BlockVersion, CreateField};
pub use virtual_store::{
    string_to_storage_type, Compressor, FileAccessMode, FileHandle, StorageType, VirtualStore,
};
pub use wal::WalManager;
pub use wal_entry::{
    append_entries, KvBatchKind, WalBlockInfo, WalChunkIndexInfo, WalCmd, WalColumnDef,
    WalCommandType, WalEntry, WalEntryIterator, WalIndexBase, WalListIterator, WalRowId,
    WalSegmentIndexInfo, WalSegmentInfo, WalTableDef, WAL_ENTRY_HEADER_LEN,
};

#[cfg(test)]
mod mixed_column;
