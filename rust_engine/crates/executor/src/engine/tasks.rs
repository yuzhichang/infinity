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

use super::*;

/// Background `CleanupTask` (`src/storage/bg_task/bg_task.cppm:107-114`).
///
/// The trigger submits it once a transaction has committed since the last
/// cleanup ([`Engine::needs_cleanup`]); the task itself runs
/// [`Engine::cleanup`], which records the new timestamp. A failure is reported
/// rather than propagated: the reference calls `UnrecoverableError`, and the
/// port has no process-wide abort for a background task.
pub struct CleanupTask {
    pub(crate) engine: std::sync::Weak<Engine>,
}

/// Background `NotifyOptimizeTask` (`src/storage/bg_task/bg_task.cppm:136-143`),
/// the periodic index optimizer: `NewNotifyOptimize` runs
/// `txn->OptimizeAllIndexes()` (`optimization_process_impl.cpp:91-145`), which
/// [`Engine::optimize_indexes`] mirrors for the index mirrors it ports.
pub struct OptimizeTask {
    pub(crate) engine: std::sync::Weak<Engine>,
}

/// A spann segment the checkpoint declined to rebuild on its own thread.
///
/// The checkpoint publishes deltas, which is `O(new rows)`. A rebuild is
/// `O(rows · log(rows/max) · dims)` - minutes on a large segment - so the two
/// states that force one (a chain at the layer cap, or a segment whose
/// published chain is mostly dead rows) are handed here instead: the old chain
/// keeps answering until the new one is installed, and the checkpoint does not
/// stall. `publish_index_chunks_forced` is the rebuild, and it runs on the
/// blocking pool so a query keeps its runtime worker.
pub struct SpannRebuildTask {
    pub(crate) engine: std::sync::Weak<Engine>,
    pub(crate) table: String,
    pub(crate) table_id: u64,
    pub(crate) index: String,
    /// The dump timestamp the new chunk's records carry.
    pub(crate) ts: u64,
}

/// The reference's `PeriodicTriggerThread::Run`
/// (`periodic_trigger_thread_impl.cpp:49-74`): wake every second and fire each
/// trigger whose interval elapsed.
///
/// This is a **tokio task**, not a thread. Every step it performs is either an
/// `.await` (the checkpoint's WAL swap and block mirror) or a pure enqueue, so
/// the loop costs a future between ticks instead of a parked thread. The one
/// synchronous wait left is `notify_compact`, which walks the tables and takes
/// each one's compaction lock; it is called inside `block_in_place` so the
/// runtime worker it happens to run on hands its core to a replacement first.
///
/// The engine is upgraded per tick and dropped before the sleep, so the task
/// stops as soon as the last `Arc<Engine>` goes away instead of keeping the
/// engine alive through a strong handle.
pub(crate) async fn trigger_loop(
    engine: std::sync::Weak<Engine>,
    checkpoint_interval: std::time::Duration,
    cleanup_interval: std::time::Duration,
    compact_interval: std::time::Duration,
    optimize_interval: std::time::Duration,
    reclaim_interval: std::time::Duration,
) {
    // **Three chains, not one.** Everything used to run in a single tick, so a
    // checkpoint that took two minutes also stopped the memory governor and the
    // WAL flush for two minutes - and a long reclaim stopped the checkpoint.
    // The steps fall into three groups by which lock they contend on:
    //
    //   storage: checkpoint → WAL flush. Owns `flush_lock` for writing.
    //   memory:  the ceiling tick and the unsealed drain. Takes `flush_lock`
    //            for reading while dumping, plus `unsealed_reclaim_lock`.
    //   index:   compaction, which takes each table's gate.
    //
    // Two members of one group still serialise - that is real, they share a
    // lock - but a slow member of one group no longer starves another. Each
    // chain keeps its own cadence, so a chain that overruns its interval simply
    // runs late; it does not delay the others.
    crate::scheduler::scheduler().spawn_task(storage_chain(engine.clone(), checkpoint_interval));
    crate::scheduler::scheduler().spawn_task(memory_chain(engine.clone(), reclaim_interval));
    crate::scheduler::scheduler().spawn_task(index_chain(
        engine,
        cleanup_interval,
        compact_interval,
        optimize_interval,
    ));
}

/// One tick of a chain, in milliseconds. Shared so the three chains stay in
/// step with each other and with the reference's one-second trigger.
const TICK: std::time::Duration = std::time::Duration::from_secs(1);

/// The interval for `key`, from the live config (which a `SET` can change) or
/// the value resolved at start-up.
fn chain_interval(engine: &Engine, key: &str, default: std::time::Duration) -> std::time::Duration {
    engine
        .context
        .config
        .lock()
        .unwrap()
        .get(key)
        .and_then(|value| value.parse().ok())
        .map_or(default, std::time::Duration::from_secs)
}

/// Checkpoints and the WAL flush.
///
/// Both are storage-level and both are cheap when due and idle; the checkpoint
/// is the one long step here, and nothing else waits on this chain.
async fn storage_chain(engine: std::sync::Weak<Engine>, checkpoint_interval: std::time::Duration) {
    let mut last = std::time::Instant::now();
    loop {
        let Some(engine) = engine.upgrade() else {
            return;
        };
        let interval = chain_interval(&engine, "checkpoint_interval", checkpoint_interval);
        if !interval.is_zero() && last.elapsed() >= interval {
            last = std::time::Instant::now();
            engine.checkpoint_wal_async().await;
            // A periodic checkpoint is not the operator's `FLUSH DATA`: it
            // never dumps the memory indexes, even with
            // `flush_dumps_mem_index` on.
            let _ = engine.checkpoint().await;
        }
        // `flush_per_second`'s timer half: the commit path syncs the first
        // commit of each one-second window and returns without syncing the
        // rest, so without this the last commit before a quiet period would
        // never reach the disk.
        if let Err(err) = engine.txn.flush_wal_if_due().await {
            eprintln!("wal flush_per_second: {err}");
        }
        drop(engine);
        tokio::time::sleep(TICK).await;
    }
}

/// The memory ceiling and the unsealed drain.
///
/// The ceiling check runs **every** tick, not on an interval: a read-only
/// server grows its page cache without a single transaction, and that process
/// is the one an OOM killer finds. Its levers are `madvise`/`trim`/mapping
/// release - no I/O, no hot-path lock - so a tick is cheap when nothing is over.
///
/// The drain has its own cadence (`unsealed_reclaim_interval`) plus the write
/// path's flag, and it is the one step here that can take a long time. That is
/// why it is a separate chain from the checkpoint: it takes `flush_lock` for
/// reading while dumping, so it cannot run beside a checkpoint, but it no longer
/// holds up the WAL flush or the ceiling check.
async fn memory_chain(engine: std::sync::Weak<Engine>, reclaim_interval: std::time::Duration) {
    let mut last_reclaim = std::time::Instant::now();
    loop {
        let Some(engine) = engine.upgrade() else {
            return;
        };
        engine.memory_tick().await;
        let interval = chain_interval(&engine, "unsealed_reclaim_interval", reclaim_interval);
        let due = !interval.is_zero() && last_reclaim.elapsed() >= interval;
        let requested = engine
            .context
            .unsealed_reclaim_requested
            .swap(false, std::sync::atomic::Ordering::Relaxed);
        if due || requested {
            last_reclaim = std::time::Instant::now();
            let _ = engine.reclaim_unsealed().await;
        }
        drop(engine);
        tokio::time::sleep(TICK).await;
    }
}

/// Compaction, optimization and cleanup.
///
/// The two that touch a table - compaction and optimizer submission - wait for
/// `Pressure::Ok`, which is the check this chain shares with the memory one
/// through `memory_pressure`'s cached verdict. Cleanup is a pure enqueue.
async fn index_chain(
    engine: std::sync::Weak<Engine>,
    cleanup_interval: std::time::Duration,
    compact_interval: std::time::Duration,
    optimize_interval: std::time::Duration,
) {
    let mut last_cleanup = std::time::Instant::now();
    let mut last_compact = std::time::Instant::now();
    let mut last_optimize = std::time::Instant::now();
    loop {
        let Some(engine) = engine.upgrade() else {
            return;
        };
        let cleanup = chain_interval(&engine, "cleanup_interval", cleanup_interval);
        let compact = chain_interval(&engine, "compact_interval", compact_interval);
        let optimize = chain_interval(&engine, "optimize_interval", optimize_interval);
        if !optimize.is_zero()
            && last_optimize.elapsed() >= optimize
            && engine.memory_pressure() == crate::memory::Pressure::Ok
        {
            last_optimize = std::time::Instant::now();
            engine.submit_optimize_indexes();
        }
        if !compact.is_zero()
            && last_compact.elapsed() >= compact
            && engine.memory_pressure() == crate::memory::Pressure::Ok
        {
            last_compact = std::time::Instant::now();
            let _ = engine.notify_compact().await;
        }
        if !cleanup.is_zero() && last_cleanup.elapsed() >= cleanup && engine.needs_cleanup() {
            last_cleanup = std::time::Instant::now();
            engine.submit_cleanup();
        }
        if engine.has_spann_fixups() {
            let engine = engine.clone();
            let _ = tokio::task::spawn_blocking(move || {
                engine.spann_drain_fixups(crate::spann_mem::BACKGROUND_FIXUP_BUDGET)
            })
            .await;
        }
        drop(engine);
        tokio::time::sleep(TICK).await;
    }
}
