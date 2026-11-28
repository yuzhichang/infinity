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

//! Reclaim of unsealed memory (`docs/MEMORY_RECLAIM.md`).
//!
//! Both memory forms - the append buffer and every in-memory index tail - are
//! bounded by **one** budget, `[buffer] unsealed_memory_quota`. Crossing it
//! drains, and the drain is the same code whoever calls it:
//!
//! * the write path, right after a batch grew a table, and
//! * the periodic sweep, for tables nobody writes any more.
//!
//! That is deliberate. Two triggers but one implementation means the two cannot
//! disagree about what "over" means, which table pays, or when to stop; a second
//! copy would drift the first time one of them was tuned.
//!
//! The unit is a **table**. A round sorts every table holding unsealed memory by
//! two dimensions (size and age), fuses the ranks ([`rrf_order`]), and dumps
//! tables in that order until the ledger is back under **half the quota**.
//!
//! Age is measured in **commit timestamps**, not wall-clock milliseconds: a
//! table's idleness is the distance between the engine's current commit ts and
//! the ts of its last append. The ts sequence is allocated in commit order and
//! never rewinds, so it orders tables the way a clock would, while staying
//! meaningful across a restart - a wall clock would make every table look idle
//! the moment the process restarted and put them all at the head of the first
//! sweep.

use std::sync::atomic::Ordering;

use super::maintain::{rrf_order, ReclaimCandidate};
use crate::engine::state::Engine;

/// Milliseconds on a clock that never goes backwards, for retry deadlines.
///
/// The ranking's age is in commit ts on purpose (it has to survive a restart),
/// but a *retry gate* must not be: a failed round may not commit anything, so a
/// deadline expressed in commit ts is only reached when an unrelated statement
/// happens to advance the clock. An `Instant`-based deadline fires on time even
/// on an idle, failing server.
pub(crate) fn monotonic_now_ms() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    // Never 0: the value doubles as "an attempt was recorded", so a stamp taken
    // in the first millisecond of the process must not read as "never tried".
    (START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64)
        .max(1)
}

/// A byte count as a short human string: `512 B`, `3 KiB`, `42 MiB`.
///
/// MiB alone would print every sub-megabyte figure as `0 MiB`, and a reclaim
/// plan whose per-table numbers all read zero says nothing.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else if bytes >= KIB {
        format!("{} KiB", bytes / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// The monotonic deadline `wait` from now.
fn monotonic_deadline(wait: std::time::Duration) -> u64 {
    monotonic_now_ms().saturating_add(wait.as_millis() as u64)
}

/// The trigger loop's tick, in milliseconds. A drain only runs on a tick, so a
/// retry deadline shorter than one could not be observed.
const TICK_MS: u64 = 1000;

/// A table as the round's first pass sees it.
struct TableStat {
    key: String,
    /// The table's numeric id, which is what the mapping table is keyed by.
    /// `None` when the catalog cannot name it.
    table_id: Option<u64>,
    /// What the table costs the process, for the size rank: buffered rows, index
    /// tails **and the bytes it has mapped**. The mapped half is what a table
    /// nobody has written in a while can still be holding - a scan leaves the
    /// block and index pages resident, and dropping the table's mappings is one
    /// of the ways the reclaim ladder gets them back
    /// (`docs/MAP_MANAGER_PLAN.md` §6 decision 3).
    ///
    /// Cached index bytes only affect *ordering*, never the decision to reclaim.
    footprint: u64,
    /// The unsealed part of `footprint`: buffered rows plus index tails. Kept
    /// apart from the mapped part so a reclaim plan can say which lever applies
    /// to which table - a dump frees this, dropping mappings frees that.
    unsealed: u64,
    /// The mapped part of `footprint`: resident pages behind this table's
    /// mappings. Zero on a platform that cannot report per-region residency
    /// (`storage::mapped`), which is honest rather than an estimate.
    mapped: u64,
    /// The table's last touch: the newer of its last append and its last block
    /// read, in commit ts. A table under constant read load is not idle even
    /// when nothing is written, which a write-only clock would report as idle.
    last_ts: u64,
    /// When the loop last tried and failed, in **monotonic milliseconds**. The
    /// per-table retry gate compares it against the same clock, so a failed dump
    /// becomes due on time even when nothing commits in between.
    last_attempt_ts: u64,
    dump_failures: u32,
}

impl Engine {
    /// One reclaim round. Returns how many tables it dumped.
    ///
    /// Two clocks, each where it belongs. **Age** is the engine's commit
    /// timestamp - it orders appends and dumps already, survives a restart, and
    /// a test drives ageing by committing rows instead of sleeping. **Retry
    /// gates** are monotonic elapsed time, because a failed round may not commit
    /// anything at all and a commit-ts deadline would then never come due.
    ///
    /// The round runs only while the ledger is over **half the quota** and stops
    /// the moment it is back under. `budget_bytes()` is re-read before every
    /// dump: a dump may release nothing (an import in flight, another path got
    /// there first) and concurrent writes can push the total back up, so an
    /// incremental count would drift.
    pub(crate) async fn reclaim_unsealed(&self) -> usize {
        // `unsealed_memory_quota == 0` disables the budget (and, with it, this
        // loop): a zero target would otherwise read as "drain everything".
        if self.context.unsealed_memory_quota.load(Ordering::Relaxed) == 0 {
            return 0;
        }
        let guard = self.context.unsealed_reclaim_lock.lock().await;
        // The retry gate is a monotonic deadline, not a commit ts: ageing the
        // ranking on commit ts is right, but no commit has to happen for a
        // backed-off round to become due again.
        if monotonic_now_ms() < self.context.unsealed_backoff_until.load(Ordering::Relaxed) {
            return 0;
        }
        let target = self.reclaim_target();
        if self.reclaim_target_met(target) {
            return 0;
        }
        let dumped = self
            .reclaim_unsealed_until(false, || self.reclaim_target_met(target))
            .await;
        drop(guard);
        // No allocator trim here. `mi_collect` reclaims the *calling thread's*
        // heap, and this call used to run on a blocking-pool thread, which
        // allocates almost nothing - it freed the wrong heap and cost a thread
        // switch. The dump itself trims on the thread that did the work (see
        // `dump_table`'s callers).
        dumped
    }

    /// Reclaims unsealed tables (append buffers + in-memory index tails) under
    /// process RSS pressure, reusing the same RRF ranking and `dump_table` path
    /// until `process_rss_bytes() <= target_rss` or no eligible tables remain.
    pub(crate) async fn reclaim_unsealed_under_pressure(&self, target_usage: u64) -> usize {
        if self.budget_bytes() == 0 {
            return 0;
        }
        let _guard = self.context.unsealed_reclaim_lock.lock().await;
        if self.memory_usage() <= target_usage {
            return 0;
        }
        self.reclaim_unsealed_until(true, || {
            storage::trim_allocator();
            self.memory_usage() <= target_usage || self.budget_bytes() == 0
        })
        .await
    }

    async fn reclaim_unsealed_until<F>(&self, ignore_backoff: bool, mut stop: F) -> usize
    where
        F: FnMut() -> bool,
    {
        let now_ts = self.current_ts();
        let cap = if ignore_backoff {
            0
        } else {
            self.context
                .unsealed_max_tables_per_round
                .load(Ordering::Relaxed)
        };

        let trace = std::env::var_os("INFINITY_PROFILE_RECLAIM").is_some();
        if stop() {
            if trace {
                eprintln!("[reclaim] stop() before any work: goal already met");
            }
            return 0;
        }

        let stats = self.unsealed_table_stats();
        if trace {
            let total: u64 = stats.iter().map(|s| s.footprint).sum();
            eprintln!(
                "[reclaim] ignore_backoff={ignore_backoff} stats={} footprint={} MiB stop_now={}",
                stats.len(),
                total / (1024 * 1024),
                stop()
            );
        }
        if stats.is_empty() {
            if trace {
                eprintln!("[reclaim] no table holds unsealed rows, index tails or mappings");
            }
            return 0;
        }
        if trace {
            for stat in &stats {
                eprintln!(
                    "[reclaim]   stat {} id={:?} footprint={} last_ts={} failures={}",
                    stat.key, stat.table_id, stat.footprint, stat.last_ts, stat.dump_failures
                );
            }
        }
        let candidates: Vec<ReclaimCandidate> = stats
            .iter()
            .filter(|stat| {
                if ignore_backoff {
                    // Under pressure a table an import is staging into is still
                    // a candidate. The marker means "these rows are not settled
                    // yet", not "uncommitted" - the import commits before it
                    // stages - and `dump_table` only writes rows at or below
                    // `reclaim_bound_ts`, which is the table's newest *committed*
                    // append ts. So the dump publishes exactly the settled part
                    // and leaves the tail that is still being staged.
                    //
                    // Excluding them entirely is what made the ceiling
                    // unreachable on a bulk load: every table was staging, so
                    // the only lever that frees a large amount of memory had no
                    // candidates and the tick reported "nothing left to
                    // release" while RSS sat above the critical watermark.
                    self.table_has_unsealed(&stat.key) || stat.footprint > 0
                } else {
                    self.reclaim_candidate_eligible(stat)
                }
            })
            .map(|stat| ReclaimCandidate {
                key: stat.key.clone(),
                table_id: stat.table_id,
                bytes: stat.footprint,
                // Pure commit-ts age: how long since the table last changed.
                // A failing table recedes through its own backoff (which keeps
                // it out of the candidate set), not by pretending it is young -
                // mixing a monotonic stamp in here would compare two clocks.
                age_ms: now_ts.saturating_sub(stat.last_ts),
            })
            .collect();
        if trace {
            eprintln!(
                "[reclaim] candidates={} of stats={}",
                candidates.len(),
                stats.len()
            );
            if candidates.len() < stats.len() {
                for stat in &stats {
                    let eligible = if ignore_backoff {
                        self.table_has_unsealed(&stat.key)
                    } else {
                        self.reclaim_candidate_eligible(stat)
                    };
                    if !eligible {
                        eprintln!(
                            "[reclaim]   excluded {} footprint={} failures={} staging={}",
                            stat.key,
                            stat.footprint,
                            stat.dump_failures,
                            self.has_staging_import(&stat.key)
                        );
                    }
                }
            }
        }
        if candidates.is_empty() {
            if trace {
                eprintln!("[reclaim] every table was excluded; nothing to dump");
            }
            return 0;
        }
        let order = rrf_order(&candidates);
        // The round's **plan**, at info level: what it saw, who it picked and
        // what it expects to get back. A reclaim decision that only shows up as
        // an RSS curve is not checkable - "the process dropped 2 GiB" does not
        // say which lever did it, and `RssAnon` (the allocator) and `RssFile`
        // (the mappings) move for different reasons. Logging the split before
        // and the expectation after makes a round attributable.
        self.log_reclaim_plan(&stats, &candidates, &order, now_ts, cap);

        let mut dumped = 0usize;
        let mut progressed = false;
        // `attempted` counts visits, `dumped` counts successes: the cap is on
        // the former, because a table whose dump releases nothing still spent
        // block I/O and an index catalog scan. Counting only successes let a
        // widespread storage failure walk every eligible table in one tick.
        for (attempted, at) in order.into_iter().enumerate() {
            let key = candidates[at].key.clone();
            if stop() {
                break;
            }
            if cap != 0 && attempted >= cap {
                break;
            }
            let usage_before = self.memory_usage();
            let started = std::time::Instant::now();
            let ok = self.dump_table(&key).await;
            // The dump frees what was *unsealed*; the pages a read left
            // resident are given back here. Both are part of the table's
            // footprint, and the round ranks tables by the sum - so a round that
            // only dumped would leave a read-only table's size arm unaddressed
            // for ever.
            //
            // Released mappings count as progress: a table whose rows are all on
            // disk has nothing for `dump_table` to write, so it reports `false`
            // even though this round did free its pages. Without this the round
            // would call itself a failure and back off from a table it had just
            // helped.
            // Table id `0` is a real table (the first one a fresh catalog
            // creates), so this must not use it as a sentinel. `TableStat`
            // carries `Option` instead.
            //
            // The symbols go **first**: a cached `Arc<MappedBlock>` holds the
            // block's `MappedFile` handles, and a mapping with a handle is never
            // idle - so unmapping before dropping them would free nothing
            // (`docs/MAP_MANAGER_PLAN.md` §3.4, the same trap stage 3 hit). The
            // next read resolves the block again.
            let unmapped = match candidates[at].table_id {
                Some(table_id) => {
                    self.context
                        .block_maps
                        .lock()
                        .unwrap()
                        .retain_table(table_id);
                    storage::mapped::unmap_idle_of(table_id)
                }
                None => 0,
            };
            let ok = ok || unmapped > 0;
            if trace {
                let after = self.memory_usage();
                eprintln!(
                    "[reclaim]   dump {key} released={ok} in {} ms; RSS {} -> {} MiB",
                    started.elapsed().as_millis(),
                    usage_before / (1024 * 1024),
                    after / (1024 * 1024)
                );
            }
            if ok {
                progressed = true;
                dumped += 1;
            }
        }
        if trace {
            eprintln!("[reclaim] round done: dumped={dumped} progressed={progressed} cap={cap}");
        }
        // The outcome, in the same terms as the plan: the process's split after
        // the round and the two ledgers it acts on. A plan line without this one
        // cannot be checked - it says what was expected, not what happened.
        eprintln!(
            "info: reclaim result: {} table(s) dumped; after {}  unsealed={} mapped={}",
            dumped,
            crate::engine::state::process_rss_split(),
            human_bytes(self.budget_bytes() as u64),
            human_bytes(storage::mapped::mapped_bytes() as u64),
        );
        // A round that released nothing must not be repeated on the next slot: it
        // would walk the same hopeless candidates and pay one `list_indexes()`
        // per table, for ever.
        if !ignore_backoff {
            if progressed {
                self.context
                    .unsealed_backoff_until
                    .store(0, Ordering::Relaxed);
            } else {
                // A **monotonic deadline**, not a commit ts. A failed round may not
                // advance `current_ts()` at all (a block write that errors before
                // its commit), and a deadline expressed in commit ts would then
                // never be reached: once storage recovered, every later tick still
                // failed the gate until an unrelated commit moved the clock.
                //
                // The floor is the tick: a drain only runs on one, so a shorter
                // deadline could not be observed anyway. It also keeps an interval
                // of `0` (sweep off, write-triggered drains only) from retrying a
                // failure every tick.
                let interval = self
                    .context
                    .unsealed_reclaim_interval
                    .load(Ordering::Relaxed);
                let wait = std::time::Duration::from_millis(interval.max(TICK_MS));
                self.context
                    .unsealed_backoff_until
                    .store(monotonic_deadline(wait), Ordering::Relaxed);
            }
        }
        dumped
    }

    /// How long a failed table waits before it is a candidate again.
    ///
    /// Duration, not commit ts: the per-table backoff has the same problem the
    /// round's gate has - a table whose dump keeps failing may never see a
    /// commit, so a commit-ts deadline would only expire when some unrelated
    /// statement advanced the clock, instead of on time.
    fn table_backoff(&self, failures: u32) -> std::time::Duration {
        // The interval is the base; `2^failures` stretches it. The cap at 2^6
        // keeps the wait finite, while the tick floor keeps an interval of `0`
        // (sweep off) from turning into a per-tick retry.
        let base = self
            .context
            .unsealed_reclaim_interval
            .load(Ordering::Relaxed)
            .max(TICK_MS);
        std::time::Duration::from_millis(base.saturating_mul(1u64 << failures.min(6)))
    }

    /// The ledger level this round drains to: **half the quota**.
    ///
    /// Half, not the quota itself: with no headroom the next batch of writes
    /// crosses the line again immediately and the loop chases the write path
    /// forever.
    /// The unsealed half of the round's goal: **half the quota**.
    ///
    /// The round wants the buffered rows and index tails down to half, not to
    /// zero - stopping at zero reruns on the next tick and thrashes the working
    /// set (`memory.rs`'s `LOW_PERCENT` reasoning, one level down).
    pub(crate) fn reclaim_target(&self) -> u64 {
        self.context.unsealed_memory_quota.load(Ordering::Relaxed) / 2
    }

    /// Whether the round has done enough: **both** halves of the goal.
    ///
    /// Two constraints, two quantities, and they are not interchangeable
    /// (`docs/MAP_MANAGER_PLAN.md` §4):
    ///
    /// 1. `budget_bytes() <= reclaim_target()` - the unsealed ledger is down to
    ///    half the quota. This is what the drain drives.
    /// 2. `mapped + unsealed <= rss_quota` - the process's own accounting fits
    ///    the RSS ceiling. This is what releasing mappings drives.
    ///
    /// Mixing them was a defect: comparing `mapped_bytes` against
    /// `unsealed_quota / 2` pits a gigabyte-scale figure against a threshold
    /// derived from a different budget, so a round either never terminates or
    /// quits while the process is still over its ceiling. Each arm is judged
    /// against its own budget here.
    pub(crate) fn reclaim_target_met(&self, target: u64) -> bool {
        let unsealed_ok = self.budget_bytes() as u64 <= target;
        let rss_ok = match self.rss_quota() {
            0 => true,
            // The low watermark, not the ceiling: `LOW_PERCENT` of the quota is
            // the share a pass aims to leave in use (`memory_low`). Stopping at
            // the ceiling would leave the engine one tick from refusing writes.
            quota => {
                self.budget_bytes() as u64 + storage::mapped::mapped_bytes() as u64
                    <= quota * crate::memory::LOW_PERCENT / 100
            }
        };
        unsealed_ok && rss_ok
    }

    /// The RSS ceiling (`[buffer] buffer_manager_size`), `0` when ungoverned.
    pub(crate) fn rss_quota(&self) -> u64 {
        self.context.memory_limit.load(Ordering::Relaxed)
    }

    /// Whether a table should enter this round's candidate set.
    fn reclaim_candidate_eligible(&self, stat: &TableStat) -> bool {
        // A staging `IMPORT` is not a reason to skip the table. Its rows are
        // committed (the import commits before it stages), and `dump_table`
        // writes only at or below `reclaim_bound_ts`, so the dump takes the
        // settled part and leaves the tail. What a staging import *does* block
        // is the index seal, and `dump_index_transaction` checks that itself -
        // twice, the second time under `flush_lock`. Excluding the table here
        // only kept its memory resident for the whole import.
        //
        // A table with **nothing unsealed but mappings** is also a candidate:
        // its rows are all on disk, but the pages a scan left resident are a
        // real part of its footprint and dropping the mappings is how the
        // ladder gets them back (`docs/MAP_MANAGER_PLAN.md` §4 stage 4).
        if !self.table_has_unsealed(&stat.key) && stat.footprint == 0 {
            return false;
        }
        if stat.dump_failures == 0 {
            return true;
        }
        // No stamp means no attempt was recorded, so there is nothing to wait
        // for. Only reachable when the counter outlives its stamp (a table entry
        // rebuilt from the catalog); waiting from process start would keep the
        // table in memory for a failure this process never had.
        if stat.last_attempt_ts == 0 {
            return true;
        }
        // The table's own backoff. `last_attempt_ts` is a monotonic millisecond
        // stamp, so this fires on time on a failing server; the commit ts stays
        // in `age_ms`, where its meaning is "how long since the table last
        // changed" and a restart cannot reset it.
        let wait = self.table_backoff(stat.dump_failures).as_millis() as u64;
        monotonic_now_ms().saturating_sub(stat.last_attempt_ts) >= wait
    }

    /// One table's unsealed footprint snapshot, for every table.
    ///
    /// One `tables` critical section: the mark map and the buffer are both plain
    /// fields there. Index tails are read through the atomic marks rather than
    /// each index's own lock - see
    /// [`crate::engine::state::TableEntry::unsealed_marks`].
    /// One info line describing the round it is about to run.
    ///
    /// The three figures it reports are the three that can be checked against
    /// the outcome: the process's resident split **now**, what the selected
    /// tables hold (`mapped` and `unsealed` separately, because a dump frees the
    /// second and dropping mappings frees the first), and what the process
    /// should read afterwards if both levers land.
    ///
    /// The estimate subtracts every selected table's mapped bytes - the mapping
    /// release is unconditional for an idle table - and its unsealed bytes,
    /// which a dump can only realise if that table's rows are all committable at
    /// this round's bound. A table with a staging import keeps its tail, so the
    /// projection is an upper bound on the release, stated as such.
    fn log_reclaim_plan(
        &self,
        stats: &[TableStat],
        candidates: &[ReclaimCandidate],
        order: &[usize],
        now_ts: u64,
        cap: usize,
    ) {
        let before = crate::engine::state::process_rss_split();
        let quota = self.rss_quota();
        let low = quota * crate::memory::LOW_PERCENT / 100;
        // Cap the plan to what this round will actually visit.
        let visits = if cap == 0 {
            order.len()
        } else {
            order.len().min(cap)
        };
        let mut planned_unsealed = 0u64;
        let mut planned_mapped = 0u64;
        for at in order.iter().take(visits) {
            if let Some(stat) = stats.iter().find(|s| s.key == candidates[*at].key) {
                planned_unsealed = planned_unsealed.saturating_add(stat.unsealed);
                planned_mapped = planned_mapped.saturating_add(stat.mapped);
            }
        }
        eprintln!(
            "info: reclaim plan: {} table(s) hold unsealed data or mappings, {} selected{}; \
             now {}  unsealed={} mapped={}; \
             selected unsealed={} mapped={}; \
             projected {} minus >= {}",
            stats.len(),
            candidates.len(),
            if cap == 0 {
                String::new()
            } else {
                format!(" (round cap {cap})")
            },
            before,
            human_bytes(self.budget_bytes() as u64),
            human_bytes(storage::mapped::mapped_bytes() as u64),
            human_bytes(planned_unsealed),
            human_bytes(planned_mapped),
            before,
            human_bytes(planned_unsealed.saturating_add(planned_mapped)),
        );
        // Per-table detail for the ones this round will visit, so a later
        // comparison has the inputs and not just the totals.
        for at in order.iter().take(visits) {
            let key = &candidates[*at].key;
            if let Some(stat) = stats.iter().find(|s| s.key == *key) {
                eprintln!(
                    "info: reclaim plan:   {key} unsealed={} mapped={} age={} ts id={:?}",
                    human_bytes(stat.unsealed),
                    human_bytes(stat.mapped),
                    now_ts.saturating_sub(stat.last_ts),
                    stat.table_id
                );
            }
        }
        // The heap, broken down by the structures that account for it. This is
        // the line that answers "what is `RssAnon`": the resident split says
        // *how much* is anonymous, this says *whose*. Without it a ten-gigabyte
        // heap can only be guessed at from the outside, and a guess cannot
        // distinguish "the index tails are holding ten gigabytes" from "the
        // accounting misses ten gigabytes".
        //
        // Each figure is the ledger the engine keeps, not an estimate of the
        // allocator's own view, so `sum` versus `RssAnon` is itself the check:
        // a large gap means memory is held by something outside these ledgers.
        let ledger = self.budget_bytes() as u64;
        eprintln!(
            "info: reclaim plan: heap breakdown: row buffer={} index tails={} \
             (ledger total={}) | decoded blocks={} decoded chunks={} ivf segments={}",
            human_bytes(self.row_buffer_bytes()),
            human_bytes(self.memindex_bytes() as u64),
            human_bytes(ledger),
            human_bytes(self.decoded_block_bytes()),
            human_bytes(self.decoded_chunk_bytes()),
            human_bytes(self.decoded_ivf_bytes()),
        );
        // The two thresholds the round is judged against, so the numbers above
        // can be read without knowing the configuration.
        eprintln!(
            "info: reclaim plan: goals unsealed<={} (quota/2) and \
             mapped+unsealed<={} (rss_quota*{}%)",
            human_bytes(self.reclaim_target()),
            human_bytes(low),
            crate::memory::LOW_PERCENT,
        );
    }

    fn unsealed_table_stats(&self) -> Vec<TableStat> {
        // `db.table` -> numeric id, so a table's mappings can be charged to it.
        // One catalog read for the whole round rather than a lookup per table:
        // the round runs on a tick and the catalog call is a scan.
        let ids: std::collections::HashMap<String, u64> = self
            .storage
            .catalog
            .list_tables()
            .unwrap_or_default()
            .into_iter()
            // `TableMeta::name` is already qualified (`default_db.t`), which is
            // the same spelling `TableEntry` keys use.
            .map(|meta| (meta.name.to_ascii_lowercase(), meta.id))
            .collect();
        let mapped = storage::mapped::mapped_bytes_by_table();
        let tables = self.tables.lock().unwrap();
        tables
            .iter()
            .filter_map(|(key, entry)| {
                let unsealed = entry.unsealed_bytes() + entry.memindex_bytes;
                let table_id = ids.get(key).copied();
                let mapped_bytes = table_id
                    .and_then(|id| mapped.get(&id).copied())
                    .unwrap_or(0);
                // A table is a candidate when it holds anything the round could
                // release: unsealed rows, an index tail, **or** mappings. The
                // last one is what a read-only table has - nothing to dump, but
                // pages to give back (`docs/MAP_MANAGER_PLAN.md` §4 stage 4).
                if unsealed == 0 && entry.unsealed_marks_are_empty() && mapped_bytes == 0 {
                    return None;
                }
                let marks: usize = entry
                    .unsealed_marks
                    .values()
                    .map(|mark| mark.load(Ordering::Relaxed) as usize)
                    .sum();
                let memindex = if marks > 0 {
                    // The cached figure is what ordering uses; fall back to the
                    // mark count when the cache has not caught up (a
                    // `CREATE INDEX` never refreshes it).
                    entry.memindex_bytes.max(marks)
                } else {
                    entry.memindex_bytes
                };
                let footprint = (entry.unsealed_bytes() + memindex) as u64 + mapped_bytes;
                // The newer of the last append and the last block read. A table
                // read every second is not idle, and a write-only clock would
                // put it at the head of every round.
                let last_write = entry
                    .append_ts
                    .as_ref()
                    .and_then(|ts| ts.last().copied())
                    .unwrap_or(0);
                let last_ts = last_write.max(entry.last_access_ts);
                Some(TableStat {
                    key: key.clone(),
                    table_id,
                    footprint,
                    unsealed: (entry.unsealed_bytes() + memindex) as u64,
                    mapped: mapped_bytes,
                    last_ts,
                    last_attempt_ts: entry.last_attempt_ts,
                    dump_failures: entry.dump_failures,
                })
            })
            .collect()
    }

    /// Whether one table has anything the loop could release: buffered rows or a
    /// non-zero index mark.
    pub(crate) fn table_has_unsealed(&self, key: &str) -> bool {
        let tables = self.tables.lock().unwrap();
        let Some(entry) = tables.get(key) else {
            return false;
        };
        entry.unsealed_bytes() > 0 || !entry.unsealed_marks_are_empty()
    }

    /// Dumps one table's unsealed data. Returns whether it released anything.
    ///
    /// **Two phases, and the order matters.** The block write needs the table's
    /// bottom gate and the index dump needs `flush_lock`; the checkpoint takes
    /// `flush_lock` and *then* the gate (`admin.rs` -> `flush_table`). Doing both
    /// in one bottom half would make this thread hold the gate while waiting for
    /// `flush_lock` - the reverse of the checkpoint's order - which parks both
    /// without an error.
    ///
    /// The progress signal is [`Self::unsealed_live_of`] before and after:
    /// buffered rows plus the index bytes computed now. Neither is a cache, which
    /// matters because `memindex_bytes` is never refreshed by `CREATE INDEX` and
    /// would report "nothing released" for a table it just emptied.
    async fn dump_table(&self, key: &str) -> bool {
        let before = self.unsealed_live_of(key);
        // The process's resident split, taken so this table's dump can be
        // attributed: the round-level "before/after" in `reclaim result` is the
        // sum of many dumps and cannot say which one moved what.
        let rss_before = crate::engine::state::process_rss_split();
        // Phase 1: fold the buffered rows into the block files, under the gate.
        //
        // The bound is the table's own newest append ts, not `checkpoint_ts()`.
        // `checkpoint_ts()` is the last **completed** commit's ts, and the write
        // path calls this from inside the very commit that just allocated a
        // larger one - so a checkpoint bound would skip exactly the rows the
        // caller is trying to drain, and every write-triggered round would
        // report "released nothing" and back off. A batch of rows with
        // `ts <= latest append ts` is what this table owns; the gate we hold
        // keeps any later one from arriving while the write runs.
        let table = key.to_string();
        let bound_ts = self.reclaim_bound_ts(key);
        let work = table.clone();
        let _ = self
            .run_commit_bottom(key, move |engine: &Engine| {
                Box::pin(async move {
                    engine
                        .write_blocks_bounded_locked(&work, bound_ts)
                        .map(|_| ())
                })
            })
            .await;
        // Phase 2: seal every index tail, with no gate held.
        self.dump_every_table_index(Some(key)).await;
        // Only this table's charge, not a full reconciliation: the round's stop
        // check re-reads the ledger before every dump, and a round of 100 dumps
        // would otherwise walk every key (and read every index's state) 100
        // times. The checkpoint's `recompute_memindex_total` stays the safety
        // net for drift.
        self.refresh_memindex_charge(key);

        // Hand the released buffers back to the OS. Without this the dump's
        // effect is invisible: the rows and index tails leave `unsealed_live`,
        // but mimalloc keeps their arenas, so RSS does not move and
        // `reclaim_memory` - which reports `before - after` on resident size -
        // counts the pass as having released nothing. That is what let a round
        // which had drained eight tables still log "nothing left to release"
        // while the process sat above its critical watermark.
        //
        // The arenas are the ones this dump filled: encoding a block and
        // sealing an index tail both build their output before writing it, and
        // it is those temporaries RSS was still holding. `index_build` makes the
        // same call after every seal, for the same reason; `trim_allocator`
        // itself explains why nothing else reaches them.
        //
        // Measured on a 34 MiB unsealed table: RSS 149 -> 113 MiB with this
        // line, 149 -> 239 MiB without it. It costs ~1.6 ms there and ~6 ms on a
        // 139 MiB one, against a dump of 174 ms / 681 ms - the walk scales with
        // the heap, not with what was freed, so it stays a small bounded stop on
        // a path that just wrote blocks and sealed a graph. `seal_index_tail`
        // makes the same blocking call inline in an async fn for the same
        // reason.
        storage::trim_allocator();

        let after = self.unsealed_live_of(key);
        let released = after < before;
        // What this one table's dump actually did, in the same terms the plan
        // used: its own ledgers and the process's resident split. A dump that
        // reports `released` but leaves RSS unchanged is either the allocator
        // holding the arenas or the pages being page cache rather than heap -
        // and the split tells which, because only `Anon` is the allocator.
        let rss_after = crate::engine::state::process_rss_split();
        eprintln!(
            "info: reclaim dump: {key} unsealed {} -> {} (released={released}); \
             RssAnon {} -> {} ({}), RssFile {} -> {}",
            human_bytes(before as u64),
            human_bytes(after as u64),
            human_bytes(rss_before.rss_anon),
            human_bytes(rss_after.rss_anon),
            if rss_after.rss_anon <= rss_before.rss_anon {
                format!("-{}", human_bytes(rss_before.rss_anon - rss_after.rss_anon))
            } else {
                format!("+{}", human_bytes(rss_after.rss_anon - rss_before.rss_anon))
            },
            human_bytes(rss_before.rss_file),
            human_bytes(rss_after.rss_file),
        );
        let still_unsealed = self.table_has_unsealed(key);
        if let Some(entry) = self.tables.lock().unwrap().get_mut(key) {
            if released {
                // Partial success must not be recorded as a failure: the next
                // round should come back to it, not back off from it.
                entry.last_attempt_ts = 0;
                entry.dump_failures = 0;
            } else {
                // A monotonic stamp: the per-table gate compares it against a
                // monotonic clock, so a failed dump retries when its wait is
                // over even if nothing commits in between.
                entry.last_attempt_ts = monotonic_now_ms();
                entry.dump_failures = entry.dump_failures.saturating_add(1);
            }
            let _ = still_unsealed;
        }
        released
    }

    /// The commit ts up to which this table's rows may be written out.
    ///
    /// `max(checkpoint_ts(), newest append ts)`. The first keeps the historical
    /// behaviour when the table has nothing newer; the second is what the write
    /// path needs, since it drains from inside a commit whose ts has not been
    /// published yet and would otherwise be excluded.
    fn reclaim_bound_ts(&self, key: &str) -> u64 {
        let newest = {
            let tables = self.tables.lock().unwrap();
            tables
                .get(key)
                .and_then(|e| e.append_ts.as_ref().and_then(|ts| ts.last().copied()))
                .unwrap_or(0)
        };
        self.checkpoint_ts().max(newest)
    }

    /// `unsealed_bytes() + memindex_bytes_of(key)`, both read now.
    ///
    /// The plan calls this `unsealed_live`; the suffix is only there to keep it
    /// distinct from the `TableEntry` accessors it composes.
    fn unsealed_live_of(&self, key: &str) -> usize {
        let buffered = {
            let tables = self.tables.lock().unwrap();
            tables.get(key).map(|e| e.unsealed_bytes()).unwrap_or(0)
        };
        buffered + self.memindex_bytes_of(key)
    }
}
