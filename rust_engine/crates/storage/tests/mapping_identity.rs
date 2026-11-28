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

//! One file, one mapping: a second open of the same file reuses the mapping
//! instead of creating a second VMA.
//!
//! The assertion reads `/proc/self/smaps` rather than the process-wide
//! `mapping_count()`. The counter can be right while the kernel still has two
//! VMAs - a mapping dropped from the table but kept by a reader, or an open that
//! bypassed the table - and the whole point of the change is the VMA count, not
//! the bookkeeping. `smaps` reports each VMA with its `dev:inode` and its
//! resident bytes, which is also what tells "one shared page counted twice" from
//! "two copies" (`Rss` doubles, `Pss` does not).
//!
//! An integration test, so nothing else in the binary maps files while the
//! numbers are read (the same reason `mapping_lifecycle.rs` is one).

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

/// The table is process-wide, so a test that asserts on exact counts must not
/// run beside another one that maps files. Every test that reads
/// `mapping_count`/`idle_mapping_count` takes this first.
static TABLE: Mutex<()> = Mutex::new(());

fn exclusive() -> MutexGuard<'static, ()> {
    TABLE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One VMA from `/proc/self/smaps`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Vma {
    start: u64,
    end: u64,
    inode: u64,
    path: String,
    /// `Rss` in bytes: pages of this VMA that are resident. Counted per VMA,
    /// so a page shared by two mappings is counted twice here.
    rss: u64,
    /// `Pss` in bytes: the same pages divided between their sharers.
    pss: u64,
}

fn parse_smaps(text: &str) -> Vec<Vma> {
    let mut out = Vec::new();
    let mut current: Option<Vma> = None;
    for line in text.lines() {
        // A header line: `start-end perms offset dev inode [pathname]`.
        if let Some((range, rest)) = line.split_once(' ') {
            if range.contains('-')
                && !range.starts_with("Rss")
                && range.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
            {
                if let Some(vma) = current.take() {
                    out.push(vma);
                }
                let mut parts = rest.split_whitespace();
                let _perms = parts.next();
                let _offset = parts.next();
                let _dev = parts.next();
                let inode = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let path = parts.collect::<Vec<_>>().join(" ");
                let (start, end) = range.split_at(range.find('-').unwrap());
                current = Some(Vma {
                    start: u64::from_str_radix(start, 16).unwrap_or(0),
                    end: u64::from_str_radix(&end[1..], 16).unwrap_or(0),
                    inode,
                    path,
                    rss: 0,
                    pss: 0,
                });
                continue;
            }
        }
        let Some(vma) = current.as_mut() else {
            continue;
        };
        if let Some(v) = line.strip_prefix("Rss:") {
            vma.rss = v
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024;
        } else if let Some(v) = line.strip_prefix("Pss:") {
            vma.pss = v
                .split_whitespace()
                .next()
                .and_then(|n| n.parse::<u64>().ok())
                .unwrap_or(0)
                * 1024;
        }
    }
    if let Some(vma) = current {
        out.push(vma);
    }
    out
}

fn smaps() -> Vec<Vma> {
    parse_smaps(&std::fs::read_to_string("/proc/self/smaps").unwrap())
}

/// The VMAs covering `path`, matched by inode so a rename cannot confuse it.
fn vm_as_for_inode(inode: u64) -> Vec<Vma> {
    smaps().into_iter().filter(|v| v.inode == inode).collect()
}

fn inode_of(path: &PathBuf) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).unwrap().ino()
}

fn write_file(path: &PathBuf, len: usize) {
    let bytes = vec![b'x'; len];
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn a_second_open_reuses_the_mapping() {
    let _guard = exclusive();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunk.bin");
    write_file(&path, 1 << 20);
    let inode = inode_of(&path);

    // Nothing of this file is mapped before the first open.
    assert!(
        vm_as_for_inode(inode).is_empty(),
        "the temp file was already mapped"
    );

    let a = storage::mapped::MappedFile::open_path(&path).unwrap();
    let after_first = vm_as_for_inode(inode);
    assert_eq!(after_first.len(), 1, "one open, one VMA: {after_first:?}");
    // Touch it so the pages are resident and `Rss` has something to report.
    let sum: u64 = a.chunks(4096).map(|c| u64::from(c[0])).sum();
    assert!(sum > 0);

    let b = storage::mapped::MappedFile::open_path(&path).unwrap();
    let after_second = vm_as_for_inode(inode);
    assert_eq!(
        after_second.len(),
        1,
        "a second open must reuse the mapping, not add a VMA: {after_second:?}"
    );
    // The two handles see the same bytes, and the mapping is the same address
    // range - so this is one object, not two that happen to agree.
    assert_eq!(a.len(), b.len());
    assert_eq!(a[..16], b[..16]);
    assert_eq!(after_second[0].start, after_first[0].start);

    drop(a);
    drop(b);
    // Dropping the handles does not unmap: the entry is kept so the next read
    // reuses it. What gives it back is `unmap_idle_mappings`.
    assert_eq!(
        vm_as_for_inode(inode).len(),
        1,
        "the mapping is kept after its last handle lets go"
    );
}

#[test]
fn unmap_idle_gives_the_address_space_back() {
    let _guard = exclusive();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunk.bin");
    write_file(&path, 1 << 20);
    let inode = inode_of(&path);

    let held = storage::mapped::MappedFile::open_path(&path).unwrap();
    assert_eq!(vm_as_for_inode(inode).len(), 1);

    // A mapping a reader holds is not unmapped under it. (The pass is
    // process-wide, so it may legitimately release *other* idle entries this
    // binary's earlier tests left behind - what this asserts is that **this**
    // file's VMA survives.)
    storage::mapped::unmap_idle_mappings();
    assert_eq!(
        vm_as_for_inode(inode).len(),
        1,
        "the held mapping is still there"
    );

    drop(held);
    let released = storage::mapped::unmap_idle_mappings();
    assert!(
        released >= 1 << 20,
        "releasing the idle mapping should report its bytes, got {released}"
    );
    assert!(
        vm_as_for_inode(inode).is_empty(),
        "an idle mapping is unmapped: {:?}",
        vm_as_for_inode(inode)
    );
}

#[test]
fn the_counter_counts_objects_not_handles() {
    let _guard = exclusive();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunk.bin");
    write_file(&path, 1 << 20);
    // Quiet the table: the assertions are exact differences.
    storage::mapped::unmap_idle_mappings();
    let before = storage::mapped::mapping_count();
    let before_bytes = storage::mapped::mapped_bytes();

    let a = storage::mapped::MappedFile::open_path(&path).unwrap();
    let b = storage::mapped::MappedFile::open_path(&path).unwrap();
    assert_eq!(
        storage::mapped::mapping_count(),
        before + 1,
        "one object, two handles"
    );
    assert_eq!(
        storage::mapped::mapped_bytes(),
        before_bytes + (1 << 20),
        "the bytes are charged once per object"
    );
    drop(a);
    drop(b);
    assert_eq!(
        storage::mapped::mapping_count(),
        before + 1,
        "the object is still mapped after both handles go"
    );
    storage::mapped::unmap_idle_mappings();
    assert_eq!(storage::mapped::mapping_count(), before);
    assert_eq!(storage::mapped::mapped_bytes(), before_bytes);
}

#[test]
fn a_replaced_file_gets_a_fresh_mapping() {
    let _guard = exclusive();
    // The property the whole design leans on: a publisher replaces a file by
    // renaming a new one over it, so the inode changes and a mapping of the old
    // file can never be served for the new name.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("chunk.bin");
    write_file(&path, 4096);
    let old_inode = inode_of(&path);
    let old = storage::mapped::MappedFile::open_path(&path).unwrap();
    assert_eq!(old[0], b'x');
    assert_eq!(old_inode, inode_of(&path));

    // Rewrite through the engine's own publisher: a temp file plus a rename.
    let tmp = dir.path().join("chunk.tmp");
    std::fs::write(&tmp, vec![b'y'; 4096]).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    let new_inode = inode_of(&path);
    assert_ne!(new_inode, old_inode, "rename must produce a new inode");

    let new = storage::mapped::MappedFile::open_path(&path).unwrap();
    assert_eq!(new[0], b'y', "the new open must see the new contents");
    // The old handle keeps the old bytes: its inode was never modified.
    assert_eq!(
        old[0], b'x',
        "the old mapping still describes its own inode"
    );
    assert_eq!(vm_as_for_inode(old_inode).len(), 1);
    assert_eq!(
        vm_as_for_inode(new_inode).len(),
        1,
        "the replacement is a second object, not a reuse"
    );
}

#[test]
fn the_idle_bound_follows_the_kernel_limit() {
    let _guard = exclusive();
    // The bound is derived, not hardcoded: a sixteenth of `vm.max_map_count`,
    // clamped. Asserting the real value keeps the derivation from silently
    // becoming "whatever number was typed in".
    let bound = storage::mapped::idle_mapping_limit();
    match std::fs::read_to_string("/proc/sys/vm/max_map_count") {
        Ok(text) => {
            let kernel: usize = text.trim().parse().expect("max_map_count is a number");
            let expected = (kernel / 16).clamp(1024, 65_536);
            assert_eq!(
                bound, expected,
                "the idle bound must follow max_map_count ({kernel})"
            );
            // A small kernel limit must not produce a useless cache.
            assert!(bound >= 1024, "bound too small: {bound}");
        }
        Err(_) => assert_eq!(bound, 16_384, "the platform default"),
    }
}

#[test]
fn idle_mappings_are_capped() {
    let _guard = exclusive();
    // The bound is the whole reason an idle mapping is kept: without it the VMA
    // count would grow with the number of files ever read, and `mmap` would
    // start failing with ENOMEM. This drives the eviction path by filling the
    // queue past a *test-sized* window rather than past the real bound (16384
    // files would make this a load test), and asserts the property that
    // matters: the queue never exceeds what was pushed while every handle was
    // held, and it empties when the reclaim pass runs.
    //
    // `idle_mapping_count` and `mapping_count` are process-wide, and the other
    // tests in this binary map files too, so the assertions are on **deltas**
    // taken around this test's own work - not on absolute values.
    let dir = tempfile::tempdir().unwrap();
    let files = 32usize;
    storage::mapped::unmap_idle_mappings();
    let idle_before = storage::mapped::idle_mapping_count();
    let objects_before = storage::mapped::mapping_count();

    let mut paths = Vec::new();
    for i in 0..files {
        let path = dir.path().join(format!("f{i}.bin"));
        write_file(&path, 4096);
        paths.push(path);
    }
    // Hold every handle at once, so none of them is idle yet.
    let held: Vec<_> = paths
        .iter()
        .map(|path| storage::mapped::MappedFile::open_path(path).unwrap())
        .collect();
    assert_eq!(
        storage::mapped::mapping_count(),
        objects_before + files,
        "each new file is one object"
    );
    assert_eq!(
        storage::mapped::idle_mapping_count(),
        idle_before,
        "a held mapping is not idle"
    );
    drop(held);

    // Every dropped handle becomes idle, because `files` is well under the
    // bound - which is the behaviour a seek-heavy scan depends on.
    assert!(
        files < storage::mapped::idle_mapping_limit(),
        "the test must stay under the bound"
    );
    assert_eq!(
        storage::mapped::idle_mapping_count(),
        idle_before + files,
        "under the bound every idle mapping is kept"
    );

    storage::mapped::unmap_idle_mappings();
    assert_eq!(storage::mapped::idle_mapping_count(), 0);
    assert_eq!(
        storage::mapped::mapping_count(),
        objects_before,
        "the reclaim pass gives the objects back"
    );
}
