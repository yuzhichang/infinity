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

//! Mapping lifecycle: a block maps only its own files, and dropping the last
//! handle leaves the mapping **idle** - kept for reuse, bounded, and given back
//! by an explicit reclaim (`unmap_idle_mappings`).
//!
//! That the mapping outlives its last handle is the point of the identity table
//! (`buffer/mapped.rs`, `IDLE_MAPPING_LIMIT`): unmapping at once costs a
//! page-table rebuild on the next open and a TLB shootdown per `munmap`, and a
//! seek-heavy scan opens the same blocks over and over. The counter therefore
//! tracks objects, and only a reclaim pass or the idle bound removes one.
//!
//! This is an integration test rather than a module unit test on purpose. It
//! asserts on `storage::mapped::mapping_count()` / `mapped_bytes()`, which are
//! **process-wide**, while `cargo test -p storage --lib` runs its ~500 unit
//! tests in parallel threads: any of them mapping a file at the same moment
//! shifts those counters, so the exact `before + n` / `before` comparison is a
//! race there (observed on a many-core Linux amd64 runner as `left: 1,
//! right: 0` - another test's mapping opened between the "held" assertion and
//! the "released" one). A test binary of its own runs nothing else, so the
//! counters here are quiet and the numbers are exact.

use storage::block_index::BlockIndex;
use storage::format::ColumnBody;
use storage::{BlockColumn, BlockColumns, BlockId};

fn id(segment: u32, block: u16) -> BlockId {
    BlockId {
        db_id: 1,
        table_id: 2,
        segment_id: segment,
        block_id: block,
    }
}

fn column(column_id: u64, values: &[i32]) -> BlockColumn {
    let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let valid = vec![true; values.len()];
    let body = ColumnBody::build(values.len(), 4, &data, &valid).unwrap();
    BlockColumn {
        column_id,
        body,
        outline: Vec::new(),
    }
}

#[test]
fn every_block_maps_only_its_own_files() {
    let dir = tempfile::tempdir().unwrap();
    let before = storage::mapped::mapping_count();
    let before_bytes = storage::mapped::mapped_bytes();
    let mut index = BlockIndex::new(dir.path());
    for b in 0..4u16 {
        index
            .create_block(id(0, b), b as usize, 4, &[column(0, &[b as i32; 4])], 1, 1)
            .unwrap();
    }
    // Everything a block maps: its one column plus the `version` file, which is
    // mapped too (its delete section is 64 KiB per 8192-slot block and copying
    // it into the heap was the largest per-block charge).
    let block_lens: Vec<u64> = (0..4u16)
        .map(|b| {
            let dir = dir.path().join(storage::block_dir(1, 2, 0, b));
            std::fs::metadata(dir.join(storage::column_file(0)))
                .unwrap()
                .len()
                + std::fs::metadata(dir.join(storage::format::VERSION_FILE))
                    .unwrap()
                    .len()
        })
        .collect();
    {
        let mut mapped = Vec::new();
        for b in 0..4u16 {
            let (_, block) = index.get_block(&id(0, b)).unwrap();
            let (body, _) = block.column(0).unwrap();
            assert_eq!(i32::from_le_bytes(body[0..4].try_into().unwrap()), b as i32);
            // The mapping covers this block's files and nothing else.
            assert_eq!(block.bytes() as u64, block_lens[b as usize]);
            mapped.push(block);
        }
        assert_eq!(
            storage::mapped::mapping_count(),
            before + 8,
            "one mapping per file: four blocks, each with its column and version"
        );
        assert_eq!(
            storage::mapped::mapped_bytes(),
            before_bytes + block_lens.iter().sum::<u64>() as usize,
            "the counter covers the mapped files, not the address space"
        );
    }
    // Dropping the handles leaves the objects mapped: they are idle, not gone.
    assert_eq!(
        storage::mapped::mapping_count(),
        before + 8,
        "an idle mapping is kept for the next open"
    );
    assert_eq!(
        storage::mapped::mapped_bytes(),
        before_bytes + block_lens.iter().sum::<u64>() as usize,
    );
    // The explicit reclaim is what gives the address space back.
    storage::mapped::unmap_idle_mappings();
    assert_eq!(
        storage::mapped::mapping_count(),
        before,
        "unmap_idle_mappings releases the idle blocks"
    );
    assert_eq!(storage::mapped::mapped_bytes(), before_bytes);
}
