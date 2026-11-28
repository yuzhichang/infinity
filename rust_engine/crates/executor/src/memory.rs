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

//! The process's memory ceiling, and the pass that keeps the process under it.
//!
//! Four budgets already existed and none of them is the process's:
//! `buffer_budget` (the block mappings and the decoded index chunks),
//! `unsealed_memory_quota` (the index tails), `spann_page_budget` /
//! `spann_dirty_budget` (one delta write). The only place that compared a number
//! with the process was the cleanup task - and it ran only when a transaction
//! had committed since the last pass, so a read-only server that grew its page
//! cache was never reclaimed. This module is the one place that reads the
//! process's own figure and decides.
//!
//! The component budgets bound what the engine *asks for*; this one covers the
//! rest (the WAL, the page cache of files read without a mapping, the allocator)
//! and is the only one that can hand memory back while a query is running.
//!
//! ## What the feed is, per platform
//!
//! * **Linux**: the cgroup's own `memory.current`, which is the number the OOM
//!   killer uses; it includes the page cache, i.e. exactly the bytes the reclaim
//!   pass can give back. `memory.max` is the ceiling and `memory.high`, when it
//!   is set, is the kernel's own soft watermark - better than a percentage of
//!   ours. `memory.current` is only read when the cgroup has a limit of its own:
//!   a container gets one cgroup per container, but a host whose root cgroup is
//!   unlimited reports the whole machine's page cache there, and a ceiling that
//!   came from the configuration would then measure the machine instead of the
//!   process (`Engine::memory_usage`).
//! * **macOS / Windows**: no cgroup. There is no per-process ceiling to read, so
//!   the ceiling comes from the configuration alone ([`Ceiling::source`] says
//!   `None` otherwise) and the usage is the process's resident size. Guessing a
//!   fraction of physical memory would turn a healthy process into one that
//!   rejects writes, which is worse than not governing at all.
//!
//! Windows is the one non-Linux target with a real hard limit (`JobObject`
//! `ProcessMemoryLimit`, which is what a Windows container sets); reading it is
//! the next step there, not a guess.

use std::sync::atomic::Ordering;

/// The share of the ceiling a reclaim pass aims to leave in use.
///
/// It is also the floor: below it nothing is touched. A pass stops here rather
/// than at the high watermark because stopping at `high` reruns on the next
/// tick and thrashes the working set.
///
/// 50% is deliberate headroom, not thrift. The governor is polled on a 1s tick
/// (and `memory_pressure` reuses its verdict for 500ms), so between two ticks a
/// burst of concurrent writers can allocate whatever their rate allows. Aiming
/// at half the ceiling absorbs one tick's worth of that burst instead of
/// discovering it at the high watermark and overshooting the ceiling before the
/// next pass runs - measured: 64 clients at 16 GiB reached 20 GiB resident with
/// a 70% reclaim target, and still 16.8 GiB with an 80% high watermark.
pub(crate) const LOW_PERCENT: u64 = 70;
/// Above this share a reclaim pass starts.
///
/// 80 leaves a 10-point band to `critical` - the only room a burst has before
/// writes are refused - while still using most of the ceiling. Lowering `low`
/// further (to 50) kept RSS far below the ceiling, but it also meant the engine
/// never used the memory it was given and paid for a reclaim pass every tick.
pub(crate) const HIGH_PERCENT: u64 = 80;
/// Above this share writes are refused once a pass has failed to get under it.
pub(crate) const CRITICAL_PERCENT: u64 = 90;

/// The ceiling the process is governed by, and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ceiling {
    pub bytes: u64,
    pub source: &'static str,
}

impl Ceiling {
    pub(crate) fn disabled() -> Self {
        Self {
            bytes: 0,
            source: "none",
        }
    }
}

/// How full the process is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum Pressure {
    Ok,
    /// Over the high watermark: reclaim on the next tick.
    High,
    /// Over the critical watermark: reclaim, and refuse new statements.
    Critical,
}

impl Pressure {
    pub(crate) fn refuses_new_work(self) -> bool {
        matches!(self, Self::Critical)
    }

    fn from_byte(value: u8) -> Self {
        match value {
            2 => Self::Critical,
            1 => Self::High,
            _ => Self::Ok,
        }
    }
}

/// How long a measured pressure is reused.
///
/// The measurement is not free: on Linux it reads the cgroup's `memory.current`
/// (or `/proc/self/status` under a configured ceiling), and on macOS the
/// resident size comes from `ps` - a fork per call. Throttling keeps that off
/// every write.
///
/// 200ms is the compromise between two measurements. At 500ms a burst of 64
/// clients crossed from below `high` to above the ceiling inside one cached
/// window, so the write gate admitted exactly the allocations it exists to stop
/// (RSS reached 16.8 GiB against a 16 GiB ceiling). Measuring on every write
/// instead removed the staleness but stat'ed `/proc` once per batch. At 200ms
/// the window is five times shorter than the 1s tick that reclaims, so a burst
/// can overshoot by at most a fifth of a tick's allocation.
const CHECK_INTERVAL_MS: u64 = 200;

/// The message [`Engine::admit_statement`] refuses with. One constant, because a
/// batched insert recognises the refusal by it ([`Engine::is_admission_refusal`]).
pub(crate) const ADMISSION_REFUSED: &str = "memory limit exceeded";

/// Milliseconds since the first call, monotonic, and never `0`.
///
/// Both users are *intervals* - the cgroup refresh and the pressure-sample
/// throttle - and a wall clock is the wrong instrument for those: an NTP step
/// back would stall a refresh for the size of the step, a step forward would run
/// the throttle out in a burst. The `+ 1` keeps `0` meaning "never", which is
/// what a zeroed counter reads as.
fn elapsed_millis() -> u64 {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    START
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis() as u64
        + 1
}

/// The cgroup's memory ceiling and the cgroup that owns it.
///
/// A limit inherited from an ancestor is still the process's limit, but the
/// charge it is compared against is that ancestor's `memory.current` - a leaf
/// with 8 GiB under a 4 GiB parent is a 4 GiB process - so the directory travels
/// with the number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CgroupLimit {
    pub bytes: u64,
    /// The cgroup whose charge is compared with `bytes`.
    pub dir: std::path::PathBuf,
    /// cgroup v2 (`memory.current`) or v1 (`memory.usage_in_bytes`).
    pub v2: bool,
}

#[cfg(target_os = "linux")]
mod proc {
    use std::path::{Path, PathBuf};

    use super::CgroupLimit;

    /// A cgroup file holding one number, or `None` when it says `max` / holds
    /// the "unlimited" sentinel (cgroup v1 writes a huge page-aligned value).
    fn read_number(path: &Path) -> Option<u64> {
        let text = std::fs::read_to_string(path).ok()?;
        let text = text.trim();
        let value: u64 = text.parse().ok()?;
        if value >= 1 << 62 {
            return None;
        }
        Some(value)
    }

    /// The cgroup this process belongs to, from `/proc/self/cgroup`: `0::/path`
    /// on v2, the `memory` controller's line on v1.
    fn cgroup_path(v2: bool) -> Option<String> {
        let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
        for line in text.lines() {
            let mut parts = line.splitn(3, ':');
            let (Some(hierarchy), Some(controllers), Some(path)) =
                (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if (v2 && hierarchy == "0" && controllers.is_empty())
                || (!v2 && controllers.split(',').any(|c| c == "memory"))
            {
                return Some(path.trim().to_string());
            }
        }
        None
    }

    /// Which hierarchy `/sys/fs/cgroup` mounts.
    ///
    /// Asked of the hierarchy's own interface - `cgroup.controllers` exists only
    /// on v2 - and not of a controller file at the mount root. A namespace root
    /// is a cgroup of its own and need not carry `memory.max` at all (the limit
    /// sits on an ancestor, or nowhere), and reading that absence as "this must
    /// be v1" sent the lookup to `/sys/fs/cgroup/memory/...`, which does not
    /// exist on a v2 host: the process then ran ungoverned even though an
    /// ancestor had a limit.
    fn is_v2() -> bool {
        Path::new("/sys/fs/cgroup/cgroup.controllers").exists()
    }

    /// The mount point of the hierarchy holding the memory controller.
    fn mount(v2: bool) -> PathBuf {
        PathBuf::from(if v2 {
            "/sys/fs/cgroup"
        } else {
            "/sys/fs/cgroup/memory"
        })
    }

    /// The process's own cgroup directory, then every ancestor up to the mount.
    ///
    /// The effective limit is the smallest on that path, so all of them have to
    /// be read: a leaf may be granted more than its parent allows.
    fn own_and_ancestors(v2: bool) -> Vec<PathBuf> {
        let mount = mount(v2);
        let mut here = match cgroup_path(v2) {
            Some(path) if !path.is_empty() && path != "/" => {
                mount.join(path.trim_start_matches('/'))
            }
            _ => mount.clone(),
        };
        let mut out = vec![here.clone()];
        while let Some(parent) = here.parent().map(Path::to_path_buf) {
            if parent == here || !parent.starts_with(&mount) {
                break;
            }
            here = parent;
            out.push(here.clone());
        }
        out
    }

    /// The finite limit that is under the most pressure, with the cgroup it came
    /// from.
    ///
    /// The smallest number is not the effective one: a leaf at 100% of its own
    /// limit is what a write would push over, even when the parent that grants it
    /// is at 10% of a larger one. So every finite limit is compared with the
    /// charge of **its own** cgroup and the highest share wins - the limit and
    /// the directory its `memory.current` is read from travel together, which is
    /// what makes the pairing consistent. An unreadable charge counts as idle.
    pub(super) fn limit() -> Option<CgroupLimit> {
        let v2 = is_v2();
        let (max_file, usage_file) = if v2 {
            ("memory.max", "memory.current")
        } else {
            ("memory.limit_in_bytes", "memory.usage_in_bytes")
        };
        let mut best: Option<(CgroupLimit, u64)> = None;
        for dir in own_and_ancestors(v2) {
            let Some(bytes) = read_number(&dir.join(max_file)) else {
                continue;
            };
            let usage = read_number(&dir.join(usage_file)).unwrap_or(0);
            let tighter = match &best {
                // `usage/bytes > found_usage/found_bytes`, in integers.
                Some((found, found_usage)) => {
                    u128::from(usage) * u128::from(found.bytes)
                        > u128::from(*found_usage) * u128::from(bytes)
                }
                None => true,
            };
            if tighter {
                best = Some((CgroupLimit { bytes, dir, v2 }, usage));
            }
        }
        best.map(|(limit, _)| limit)
    }

    /// The process's own cgroup's soft limit: v2's `memory.high`, which the
    /// kernel throttles at instead of killing, or v1's `memory.soft_limit_in_bytes`.
    pub(super) fn high() -> Option<u64> {
        let v2 = is_v2();
        let own = own_and_ancestors(v2).into_iter().next()?;
        read_number(&own.join(if v2 {
            "memory.high"
        } else {
            "memory.soft_limit_in_bytes"
        }))
    }

    /// The charge under `limit`: `memory.current` (v2) / `memory.usage_in_bytes`
    /// (v1) of the cgroup the limit was taken from.
    pub(super) fn usage(limit: &CgroupLimit) -> Option<u64> {
        read_number(&limit.dir.join(if limit.v2 {
            "memory.current"
        } else {
            "memory.usage_in_bytes"
        }))
    }
}

#[cfg(not(target_os = "linux"))]
mod proc {
    use super::CgroupLimit;

    // No cgroup: macOS has no per-process ceiling (jetsam is iOS; a desktop
    // daemon is only ever slowed by swap), and Windows exposes one through a
    // `JobObject`, not a file. The ceiling comes from the configuration there.
    pub(super) fn limit() -> Option<CgroupLimit> {
        None
    }

    pub(super) fn high() -> Option<u64> {
        None
    }

    pub(super) fn usage(_limit: &CgroupLimit) -> Option<u64> {
        None
    }
}

/// How often the ceiling is looked up again.
///
/// The write path cannot re-walk `/proc/self/cgroup` and the hierarchy per
/// statement, so the answer is cached - but "discovered once" was wrong in both
/// directions: a process that started without a limit never picked up one an
/// orchestrator added later, and a limit that was raised kept refusing writes.
/// One refresh per interval keeps the cached read at a single atomic load while
/// covering both transitions.
const CGROUP_REFRESH_MS: u64 = 5_000;

/// The one view of the cgroup the governor works from.
///
/// The limit, the cgroup whose charge it is compared against, and the generation
/// that names the pair live together so that a reader cannot see one without the
/// other: a new generation published next to a stale limit is what makes a
/// verdict look current after it was measured against a ceiling that no longer
/// exists.
struct CgroupState {
    /// Moves whenever `limit` changes; a pressure verdict is stamped with it and
    /// reused only while it is still current.
    generation: u64,
    limit: Option<CgroupLimit>,
    /// Monotonic stamp of the last discovery, in [`elapsed_millis`] units; `0`
    /// means the hierarchy has not been looked at yet.
    discovered_at: u64,
}

static CGROUP: std::sync::Mutex<CgroupState> = std::sync::Mutex::new(CgroupState {
    generation: 0,
    limit: None,
    discovered_at: 0,
});

/// The current snapshot, rediscovered when it is older than
/// [`CGROUP_REFRESH_MS`].
///
/// The walk runs under the same lock a reader takes, so the generation and the
/// limit it names are published and read as one pair; it is a handful of small
/// file reads once per interval, and a concurrent reader waits for it instead of
/// observing a half-updated state.
fn cgroup_snapshot() -> (u64, Option<CgroupLimit>) {
    let mut state = match CGROUP.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = elapsed_millis();
    if state.discovered_at == 0 || now.saturating_sub(state.discovered_at) >= CGROUP_REFRESH_MS {
        let found = proc::limit();
        if found != state.limit {
            // The level `memory_pressure` cached belongs to the ceiling that was
            // in force when it was measured.
            state.generation += 1;
            state.limit = found;
        }
        state.discovered_at = now;
    }
    (state.generation, state.limit.clone())
}

/// The charge under `limit` - the number the kernel kills on, page cache
/// included - read from the cgroup the limit was taken from.
fn cgroup_usage_at(limit: &CgroupLimit) -> Option<u64> {
    proc::usage(limit)
}

/// The cgroup's soft limit (`memory.high`): the kernel throttles at it instead
/// of killing, so it is the best high watermark when it is set.
pub(crate) fn cgroup_high() -> Option<u64> {
    proc::high()
}

/// `(low, high, critical)` for `ceiling`, with `memory.high` overriding the
/// high watermark when the kernel has one.
fn watermarks(ceiling: u64, kernel_high: Option<u64>) -> (u64, u64, u64) {
    let low = ceiling * LOW_PERCENT / 100;
    let critical = ceiling * CRITICAL_PERCENT / 100;
    let high = kernel_high
        .filter(|high| *high > low && *high < critical)
        .unwrap_or(ceiling * HIGH_PERCENT / 100);
    (low, high, critical)
}

/// The figure the governor compares against the ceiling.
///
/// Each ceiling is paired with the charge it is about. A ceiling the
/// configuration set asks about **this process**, so it is compared with the
/// resident size; a ceiling the cgroup set asks about the **cgroup**, so it is
/// compared with that cgroup's `memory.current` - the number the kernel kills
/// on, page cache included. Reading the cgroup's charge under a configured
/// ceiling compared the process against the whole host, and on a CI runner that
/// had just generated the fixtures that was gigabytes of someone else's page
/// cache. `rss` is a function because reading it forks on macOS.
fn usage_feed(from_cgroup: bool, cgroup: Option<u64>, rss: fn() -> u64) -> u64 {
    match (from_cgroup, cgroup) {
        (true, Some(bytes)) => bytes,
        _ => rss(),
    }
}

/// The process's cgroup memory limit, when the host imposes one.
pub fn cgroup_memory_limit_bytes() -> Option<u64> {
    cgroup_snapshot().1.map(|limit| limit.bytes)
}

impl super::Engine {
    /// The ceiling this engine runs under: the configuration's
    /// `buffer_manager_size` (`memory_limit`) when it is set, the cgroup's
    /// `memory.max` otherwise. A container that sized neither is not governed.
    pub(crate) fn memory_ceiling(&self) -> Ceiling {
        let (_, cgroup) = cgroup_snapshot();
        self.ceiling_from(cgroup.as_ref())
    }

    /// [`Self::memory_ceiling`] against one snapshot, so the limit and the charge
    /// it is compared with come from the same discovery.
    fn ceiling_from(&self, cgroup: Option<&CgroupLimit>) -> Ceiling {
        let configured = self
            .context
            .memory_limit
            .load(std::sync::atomic::Ordering::Relaxed);
        if configured > 0 {
            return Ceiling {
                bytes: configured,
                source: "config",
            };
        }
        match cgroup {
            // Nine tenths: the kernel kills at the limit, and the last tenth is
            // what a reclaim pass needs to actually get below it.
            Some(limit) => Ceiling {
                bytes: limit.bytes / 10 * 9,
                source: "cgroup",
            },
            None => Ceiling::disabled(),
        }
    }

    /// What the process is using, as the platform's own account of it, paired
    /// with the ceiling that is in force: the resident size under a configured
    /// ceiling, the cgroup's `memory.current` (page cache included, which is what
    /// the kernel kills on) under a cgroup one.
    ///
    /// The pairing is the point. `memory.current` is a figure about the cgroup -
    /// a container is its only tenant, so it is the container's number; on a host
    /// whose root cgroup is unlimited the same file adds up every process and the
    /// machine's page cache, which is not what a `buffer_manager_size` asks
    /// about. A CI runner that had just generated and copied the fixtures had
    /// gigabytes of that, and a 4 GiB ceiling refused the engine's first write
    /// while the engine itself held a few hundred MiB.
    pub(crate) fn memory_usage(&self) -> u64 {
        let (_, cgroup) = cgroup_snapshot();
        self.usage_against(cgroup.as_ref())
    }

    /// The charge paired with the ceiling `cgroup` produces: the cgroup's own
    /// when that limit is the one in force, the process's resident size when the
    /// ceiling came from the configuration (a configured ceiling is about this
    /// process, not about whatever else shares the cgroup).
    fn usage_against(&self, cgroup: Option<&CgroupLimit>) -> u64 {
        let from_cgroup = self
            .context
            .memory_limit
            .load(std::sync::atomic::Ordering::Relaxed)
            == 0
            && cgroup.is_some();
        usage_feed(
            from_cgroup,
            cgroup.and_then(cgroup_usage_at),
            crate::engine::state::process_rss_bytes,
        )
    }

    /// How full the process is against [`Self::memory_ceiling`], measured at most
    /// once per [`CHECK_INTERVAL_MS`].
    pub(crate) fn memory_pressure(&self) -> Pressure {
        // One snapshot for the whole decision: the limit, the cgroup its charge is
        // read from, and the generation the verdict is stamped with. The refresh
        // has to happen before the throttle decides to reuse the level - the
        // level belongs to the ceiling that was in force when it was measured.
        let (generation, cgroup) = cgroup_snapshot();
        let now = elapsed_millis();
        let checked = self
            .context
            .memory_checked_at
            .load(std::sync::atomic::Ordering::Relaxed);
        // `0` is "never measured": a level that was never taken cannot be reused.
        if checked != 0
            && self
                .context
                .memory_ceiling_generation
                .load(std::sync::atomic::Ordering::Relaxed)
                == generation
            && now.saturating_sub(checked) < CHECK_INTERVAL_MS
        {
            return Pressure::from_byte(
                self.context
                    .memory_pressure_level
                    .load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        let pressure = self.measure_pressure(cgroup.as_ref());
        self.context
            .memory_checked_at
            .store(now, std::sync::atomic::Ordering::Relaxed);
        self.context
            .memory_pressure_level
            .store(pressure as u8, std::sync::atomic::Ordering::Relaxed);
        self.context
            .memory_ceiling_generation
            .store(generation, std::sync::atomic::Ordering::Relaxed);
        pressure
    }

    /// [`Self::memory_pressure`] without the throttle, against one snapshot.
    fn measure_pressure(&self, cgroup: Option<&CgroupLimit>) -> Pressure {
        let ceiling = self.ceiling_from(cgroup);
        if ceiling.bytes == 0 {
            return Pressure::Ok;
        }
        let (_, high, critical) = watermarks(ceiling.bytes, cgroup_high());
        match self.usage_against(cgroup) {
            usage if usage >= critical => Pressure::Critical,
            usage if usage >= high => Pressure::High,
            _ => Pressure::Ok,
        }
    }

    /// One pass of the governor's levers, cheapest first, until `want` bytes are
    /// given back.
    ///
    /// The ladder, in the order it may be stopped at:
    ///
    /// 1. **Allocator arenas** (`mi_collect`) - free pages the allocator is
    ///    holding. No I/O, no lock.
    /// 2. **Resident pages of the live mappings** (`madvise(DONTNEED)`) - the
    ///    file stays mapped, only its pages go. A later scan faults them back
    ///    in from a page cache that is usually still warm.
    /// 3. **Pure copies**, when `forced`: SPANN's page bookkeeping, the result
    ///    and plan caches. Recomputable by definition, and the two steps that
    ///    cost the most to redo, which is why they wait for the critical
    ///    watermark.
    /// 4. **The block mappings themselves** - `madvise` leaves the mapping and
    ///    its address space in place, so a table larger than the budget would
    ///    keep one mapping per block ever read. Dropping the entry unmaps it; a
    ///    reader already inside a scan keeps its `Arc`, and the next one remaps.
    /// 5. **Reloadable table caches** (`[`Self::evict_table_caches`]`).
    /// 6. **Unsealed tables** - buffered rows and index tails, drained by
    ///    writing a block and sealing the tail. This is the only step that
    ///    awaits, and it is also the one that holds the most: a bulk insert
    ///    accumulates rows in the append buffer and vectors in the index tail,
    ///    neither of which any of the levers above can touch.
    ///
    /// Steps 1-5 are synchronous and step 6 is not, but they are one ladder, not
    /// two passes: the async step runs before the ladder escalates a second
    /// time, because draining unsealed tables frees more than unmapping and
    /// evicting could, and re-running the cheap steps afterwards is cheaper than
    /// having escalated past them.
    ///
    /// `want` is the target for the whole pass; `forced` turns on the steps that
    /// are expensive to redo and is the critical watermark.
    /// One reclaim pass: the levers that need no drain, then - only if the
    /// process is still over - the unsealed drain.
    ///
    /// The drain runs **once** and only when the cheap levers did not get the
    /// process under the high watermark. It used to run on every pass that found
    /// `budget_bytes() > 0`, which meant a tick could drain twice: once from
    /// here and once from the tick's own `reclaim_unsealed`, and both queued on
    /// `unsealed_reclaim_lock` (`docs/MAP_MANAGER_PLAN.md` §13.4).
    ///
    /// The two steps also read different thresholds and that is deliberate -
    /// this one compares the process's resident size against the ceiling, the
    /// drain compares the unsealed ledger against half its quota. A pass that
    /// satisfies neither is what leaves a table holding memory while the process
    /// looks fine.
    pub(crate) async fn reclaim_memory(&self, want: u64, forced: bool) -> u64 {
        let mut released = self.reclaim_cheap_levers(want, forced);

        let (_, cgroup) = cgroup_snapshot();
        let over = forced || self.measure_pressure(cgroup.as_ref()) != Pressure::Ok;
        if !over {
            return released;
        }
        // Under pressure the unsealed rows are the largest reachable figure, so
        // the drain is worth a pass even when the cheap levers met `want`: it is
        // what returns the heap, and the levers above only return pages.
        let before = self.memory_usage();
        let dumped = self
            .reclaim_unsealed_under_pressure(self.memory_low())
            .await;
        if dumped > 0 {
            // The drain freed rows and index tails; what is left is now memory
            // the earlier steps can reach, so run them once more rather than
            // reusing the pre-drain figures.
            released += self.reclaim_cheap_levers(0, forced);
            let after = self.memory_usage();
            released = released.saturating_add(before.saturating_sub(after));
        }
        released
    }

    /// Steps 1-5 of [`Self::reclaim_memory`]: everything that releases memory
    /// without awaiting.
    ///
    /// Separate because the ladder is called twice - once up front, once after
    /// the unsealed drain - and because a caller holding a borrow that cannot
    /// cross an `await` (a `Mutex` guard, a synchronous build loop) may still
    /// want these. It is deliberately *not* named or shaped as a complete pass:
    /// the unsealed drain is the step that frees the most, and it is not here.
    fn reclaim_cheap_levers(&self, want: u64, forced: bool) -> u64 {
        // Each lever reports what it freed, and the report is logged rather than
        // inferred. The alternative - reading the process's resident size before
        // and after - cannot tell a lever that worked from one that did nothing
        // while another moved the same pages, and cannot tell either from an
        // allocator that simply returned them late.
        let mut released = 0u64;
        storage::trim_allocator();
        let pages = storage::mapped::release_mapped_pages(want as usize) as u64;
        released += pages;
        if pages > 0 {
            eprintln!(
                "info: reclaim lever: dropped resident pages of {} of mappings",
                crate::engine::reclaim::human_bytes(pages)
            );
        }
        if forced {
            // No separate SPANN page release: a SPANN chunk is a mapping like
            // any other and `release_mapped_pages` above already advised every
            // mapping in the registry (`docs/MAP_MANAGER_PLAN.md` §13.2).
            // Pure copies: a query that needs them recomputes them.
            let cached = self.result_cache.lock().unwrap().len();
            self.result_cache.lock().unwrap().clear();
            self.plan_cache.clear();
            if cached > 0 {
                eprintln!("info: reclaim lever: cleared {cached} result-cache entries");
            }
        }
        // The page advice reports the mappings' length, which is an upper bound
        // on what was actually resident (and a cgroup keeps charging a page
        // cache it has not reclaimed yet), so the decision to escalate is the
        // process's own figure - not the sum above. Escalate when the cheap
        // pass did not meet the target, or when the process is still over.
        released += self.escalate(released, want, forced);
        released
    }

    /// Step 4: unmapping idle block mappings. Shared by both calls of
    /// [`Self::reclaim_cheap_levers`].
    ///
    /// `already` is what the steps above released; the target is "meet `want`,
    /// or get under the low watermark, whichever asks for more".
    ///
    /// This used to have a second step - evicting decoded table caches - which
    /// is gone: the loaded HNSW chunk keeps its vectors, graph, labels and
    /// levels in the mapping, the IVF segment's parts are mapping windows, and
    /// the decoded row cache no longer exists
    /// (`docs/MAP_MANAGER_PLAN.md` §13.2). There is nothing left in those caches
    /// that is not page cache.
    fn escalate(&self, already: u64, want: u64, forced: bool) -> u64 {
        let (_, cgroup) = cgroup_snapshot();
        let pressure = self.measure_pressure(cgroup.as_ref());
        if !(forced || pressure != Pressure::Ok || already < want) {
            return 0;
        }
        let over = self.memory_usage().saturating_sub(self.memory_low());
        let remaining = want.saturating_sub(already).max(over);
        self.release_idle_mappings(remaining as usize) as u64
    }

    /// The low watermark: the level a pass reclaims down to. `0` when no
    /// ceiling governs the process.
    fn memory_low(&self) -> u64 {
        let ceiling = self.memory_ceiling();
        if ceiling.bytes == 0 {
            return 0;
        }
        watermarks(ceiling.bytes, cgroup_high()).0
    }

    /// Unmaps idle mappings the process no longer needs, until `want` bytes are
    /// released or nothing idle is left.
    ///
    /// The block cache holds symbols, not mappings (`docs/MAP_MANAGER_PLAN.md`
    /// §3.0), so the address space this gives back comes from
    /// `storage::mapped` - the one lever that returns address space and not
    /// just resident pages. A reader in the middle of a scan keeps its `Arc`,
    /// so only idle objects go; the next read remaps from the page cache.
    pub(crate) fn release_idle_mappings(&self, want: usize) -> usize {
        if want == 0 {
            return 0;
        }
        // The block symbols hold `Arc<MappedBlock>`, which holds the mappings -
        // so a mapping can never become idle while its symbol is cached, and
        // unmapping alone would free nothing. Drop the symbols first: the next
        // read resolves them again (one directory walk per block), which is the
        // price of getting the address space back.
        //
        // Only blocks whose table is over its share are dropped, so a reclaim
        // pass does not throw away the working set of every table at once. The
        // sharing is by *size*: the largest holders lose their symbols first,
        // which is what the table-level fusion ranks anyway.
        let dropped = self.drop_block_symbols_for_reclaim(want);
        let released = storage::mapped::unmap_idle_mappings();
        if released >= want {
            return released.max(dropped);
        }
        // Idle objects were not enough; drop resident pages too, which is still
        // a release the caller can count even though the mapping stays.
        released.max(dropped) + storage::mapped::release_mapped_pages(want - released)
    }

    /// Drops block symbols, largest table first, until the maps they held could
    /// cover `want` bytes (or no symbol is left), returning the bytes the
    /// dropped symbols covered.
    ///
    /// This is what makes the mappings reclaimable at all: a symbol holds its
    /// block's `MappedFile` handles, and a mapping with a handle is never idle.
    fn drop_block_symbols_for_reclaim(&self, want: usize) -> usize {
        let mut tables: Vec<(u64, u64)> = storage::mapped::mapped_bytes_by_table()
            .into_iter()
            .collect();
        tables.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
        let mut dropped = 0usize;
        for (table_id, mapped) in tables {
            if dropped >= want {
                break;
            }
            // One lock per table: dropping a whole table's symbols is cheap (a
            // hash map drain) and keeps the critical section short.
            if let Ok(mut maps) = self.context.block_maps.lock() {
                maps.retain_table(table_id);
            }
            dropped += mapped as usize;
        }
        dropped
    }

    /// Write admission: the last lever, tried after a reclaim pass has failed.
    ///
    /// Refusing the write is the only answer that keeps the process alive - the
    /// alternative is the kernel's, and it takes the whole engine with it. Reads
    /// keep working: they need no new memory.
    ///
    /// Every write entry point calls this before it allocates or commits, not
    /// only the statement path: the Thrift `Insert` RPC, the REST handler and
    /// `COPY` insert through the primitives in `dml`, which never pass through
    /// `execute_statements`.
    /// Whether `error` is the refusal [`Self::admit_statement`] makes, as opposed
    /// to any other `Error::Invalid` a statement can return. A batched insert
    /// asks so it can tell "nothing was written" from "the batches before this
    /// one are durable".
    pub(crate) fn is_admission_refusal(error: &::common::Error) -> bool {
        matches!(error, ::common::Error::Invalid(message) if message.starts_with(ADMISSION_REFUSED))
    }

    /// Admission for one statement, read or write.
    ///
    /// It is not only writes that allocate beyond the reclaim ladder's reach.
    /// A search builds a `top-k` heap and a candidate set per statement; a
    /// plain `SELECT` materialises its result set. Neither is a block mapping
    /// or an unsealed table, so neither is released when the ceiling is hit -
    /// which makes either one a way for a process already near the ceiling to
    /// go past it. The caller decides which statements are subject to this
    /// (`execute_stmt`), so the name states what it does rather than who calls
    /// it.
    pub(crate) async fn admit_statement(&self) -> ::common::Result<()> {
        // The cached verdict, reused for at most `CHECK_INTERVAL_MS` (200ms).
        //
        // Measuring on every statement was tried and works, but it costs a
        // `/proc/self/status` read per 1024-row batch for a figure that moves
        // over hundreds of milliseconds. Shortening the window instead keeps
        // that read off the statement path while staying well inside the 1s
        // tick that reclaims: a burst can overshoot by a fifth of what it could
        // before, rather than by a full tick.
        //
        // The loop below re-measures after each pass, so a stale reading costs
        // at most one reclaim attempt, never a missed refusal.
        if !self.memory_pressure().refuses_new_work() {
            return Ok(());
        }
        // Reclaim now; if memory is actively draining (released > 0 or RSS is
        // falling across a transient dump/compact spike), retry briefly before
        // turning a burst into a user-visible error.
        for attempt in 0..5 {
            let before = self.memory_usage();
            let released = self.memory_tick().await;
            let (generation, cgroup) = cgroup_snapshot();
            let after_usage = self.usage_against(cgroup.as_ref());
            let after = self.measure_pressure(cgroup.as_ref());
            self.context
                .memory_pressure_level
                .store(after as u8, Ordering::Relaxed);
            self.context
                .memory_checked_at
                .store(elapsed_millis(), Ordering::Relaxed);
            self.context
                .memory_ceiling_generation
                .store(generation, Ordering::Relaxed);
            if !after.refuses_new_work() {
                return Ok(());
            }
            if attempt == 4 || (released == 0 && after_usage >= before) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let ceiling = self.memory_ceiling();
        Err(::common::Error::Invalid(format!(
            "{ADMISSION_REFUSED}: {} bytes in use against a {} ceiling ({}); \
             the write was refused",
            self.memory_usage(),
            ceiling.bytes,
            ceiling.source
        )))
    }

    /// One pass of the governor, run on the periodic tick - deliberately **not**
    /// behind `needs_cleanup`: a read-only server can grow its page cache without
    /// a single transaction. Returns the bytes it released.
    pub(crate) async fn memory_tick(&self) -> u64 {
        // One snapshot: the pass measures and stamps against the same limit.
        let (generation, cgroup) = cgroup_snapshot();
        let ceiling = self.ceiling_from(cgroup.as_ref());
        if ceiling.bytes == 0 {
            // No ceiling: nothing to govern, but the tick still *ran*, and the
            // stamp says so. Leaving it unset made an ungoverned engine look
            // like one whose tick had stopped - which is exactly what a
            // "is the memory chain alive" probe reads.
            self.context
                .memory_checked_at
                .store(elapsed_millis(), Ordering::Relaxed);
            return 0;
        }
        let usage = self.usage_against(cgroup.as_ref());
        let (low, high, critical) = watermarks(ceiling.bytes, cgroup_high());
        if usage < high {
            self.context
                .memory_pressure_level
                .store(Pressure::Ok as u8, Ordering::Relaxed);
            self.context
                .memory_checked_at
                .store(elapsed_millis(), Ordering::Relaxed);
            self.context
                .memory_ceiling_generation
                .store(generation, Ordering::Relaxed);
            return 0;
        }
        // Down to the low watermark, not by one page: a pass that stops at `high`
        // reruns on the next tick and thrashes the working set.
        let want = usage.saturating_sub(low);
        let released = self.reclaim_memory(want, usage >= critical).await;
        // Measure again rather than assuming the pass worked: when nothing can
        // be released (a ceiling below the working set) the level is still
        // critical, and the next write must see that.
        let after = self.measure_pressure(cgroup.as_ref());
        self.context
            .memory_pressure_level
            .store(after as u8, Ordering::Relaxed);
        self.context
            .memory_checked_at
            .store(elapsed_millis(), Ordering::Relaxed);
        self.context
            .memory_ceiling_generation
            .store(generation, Ordering::Relaxed);
        self.context
            .memory_reclaim_passes
            .fetch_add(1, Ordering::Relaxed);
        self.context
            .memory_reclaimed_bytes
            .fetch_add(released, Ordering::Relaxed);
        if released == 0 && usage >= critical {
            // Nothing left to give: the ceiling is smaller than the working set.
            // Said once per pass because the next lever is the operator's.
            eprintln!(
                "memory: {usage} bytes in use against a {} ceiling ({}) and nothing left to \
                 release; writes are refused until it drops",
                ceiling.bytes, ceiling.source
            );
        }
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cached verdict expires, so a burst cannot hide behind it forever.
    ///
    /// `admit_statement` reads the cache (see `CHECK_INTERVAL_MS` for why it is not
    /// measured per write), and the guarantee it needs is that the cache is
    /// short-lived enough to see a burst. This pins that the window is bounded
    /// and that a verdict older than it is re-measured rather than reused.
    #[tokio::test]
    async fn a_pressure_verdict_expires_after_the_configured_window() {
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute("CREATE TABLE stale (id INT);").await.unwrap();

        // A fresh measurement is `Critical`, and it is what gets cached.
        e.set_memory_limit(1);
        assert_eq!(e.memory_pressure(), Pressure::Critical);

        // Poison the cache with a verdict that would admit the write, but leave
        // it inside the window: this is the case the cache is *allowed* to serve,
        // and it must be reclaimed by the loop below rather than trusted.
        e.context
            .memory_pressure_level
            .store(Pressure::Ok as u8, Ordering::Relaxed);
        e.context
            .memory_checked_at
            .store(elapsed_millis(), Ordering::Relaxed);

        // Past the window the verdict is dropped and measured again, which is
        // what keeps a burst from hiding behind a stale `Ok`.
        e.context.memory_checked_at.store(
            elapsed_millis().saturating_sub(CHECK_INTERVAL_MS + 1),
            Ordering::Relaxed,
        );
        assert_eq!(
            e.memory_pressure(),
            Pressure::Critical,
            "an expired verdict must be re-measured, not reused"
        );

        // And the gate refuses once the verdict is current again.
        let err = e
            .execute("INSERT INTO stale VALUES (1);")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory limit exceeded"), "{err}");
    }

    /// A ceiling below the working set is the one case the levers cannot fix, so
    /// it is the one that decides the process's fate: writes are refused (the
    /// front end renders that as an error) while reads keep working.
    #[tokio::test]
    async fn a_write_is_refused_when_the_ceiling_is_below_the_working_set() {
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute("CREATE TABLE meml (id INT);").await.unwrap();
        // One byte: every pass runs the levers and still cannot get under it,
        // which is exactly the state that must not take the process down.
        e.set_memory_limit(1);
        assert!(e.memory_pressure().refuses_new_work());
        let _ = e.memory_tick().await;
        assert_eq!(
            e.context.memory_reclaim_passes.load(Ordering::Relaxed),
            1,
            "the tick ran a pass"
        );
        let err = e
            .execute("INSERT INTO meml VALUES (1);")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory limit exceeded"), "{err}");
        // A plain `SELECT` still answers: it is what an operator reads the
        // process with while it is in trouble, and its result set is bounded by
        // the table and the `LIMIT`. Only a search is admitted like a write.
        e.execute("SELECT COUNT(*) FROM meml;").await.unwrap();
    }

    /// A search is refused at the critical watermark; a plain `SELECT` is not.
    ///
    /// The two allocate differently. A search builds a `top-k` heap and a
    /// candidate set per statement, neither of which the reclaim ladder can see
    /// or free, so starting one while over the watermark is how a process that
    /// was already at the ceiling goes past it. A plain `SELECT` is bounded by
    /// its table and is the instrument an operator uses to look at a process in
    /// trouble - refusing it would take that away exactly when it is needed.
    #[tokio::test]
    async fn a_search_is_refused_at_the_critical_watermark_but_a_select_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute("CREATE TABLE mems (id INT, v EMBEDDING(4));")
            .await
            .unwrap();
        e.set_memory_limit(1);

        // A search carries a `SEARCH` clause, so it is admitted like a write.
        let err = e
            .execute(
                "SELECT id FROM mems SEARCH MATCH VECTOR (v, [0.1, 0.2, 0.3, 0.4], 'float', 'l2', 3)",
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("memory limit exceeded"), "{err}");

        // A plain aggregate does not.
        e.execute("SELECT COUNT(*) FROM mems;").await.unwrap();
    }

    /// A pass that cannot release enough by advising pages has to drop the
    /// mappings themselves: `madvise(DONTNEED)` leaves the mapping (and its
    /// address space) in place, and a table larger than the budget would keep
    /// one mapping per block ever read. Dropping the entry unmaps the block;
    /// the next scan maps it again from a page cache that is still warm.
    #[tokio::test]
    async fn a_reclaim_pass_unmaps_blocks_when_dropping_pages_is_not_enough() {
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute("CREATE TABLE memu (id INT);").await.unwrap();
        e.execute("INSERT INTO memu VALUES (1);").await.unwrap();
        e.flush().await.unwrap();
        let root = e.blocks_root();
        let id = e.visible_block_ids("default_db.memu")[0];
        let _ = e.block_map("default_db.memu", &root, id).unwrap();
        assert_eq!(e.buffer_objects().len(), 1);

        // A terabyte the page advice cannot produce on its own: the pass has to
        // go on to the block map.
        let released = e.reclaim_memory(1 << 40, false).await;
        assert!(released > 0);
        assert!(
            e.buffer_objects().is_empty(),
            "the block mapping was unmapped"
        );

        // The rows are untouched: the next read maps the block again.
        assert_eq!(
            e.execute("SELECT COUNT(*) FROM memu;").await.unwrap().len(),
            1
        );
    }

    /// The periodic tick is the path a read-only server relies on: with the
    /// process over its ceiling it gives the block mappings back even though no
    /// transaction committed since the last pass.
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // the guard serialises the *whole* test
    async fn a_tick_over_the_ceiling_gives_the_block_mappings_back() {
        let _serialised = storage::mapped::test_guard();
        storage::mapped::unmap_idle_mappings();
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute("CREATE TABLE memt (id INT);").await.unwrap();
        e.execute("INSERT INTO memt VALUES (1);").await.unwrap();
        e.flush().await.unwrap();
        let root = e.blocks_root();
        let id = e.visible_block_ids("default_db.memt")[0];
        let _ = e.block_map("default_db.memt", &root, id).unwrap();
        assert_eq!(e.buffer_objects().len(), 1);

        // One byte: every measurement is critical, which is the state that must
        // reclaim rather than wait for a write.
        e.set_memory_limit(1);
        e.memory_tick().await;
        // The tick gives the mapping back, and the way it does that is by
        // dropping the table's block symbols first - a symbol holds the
        // `MappedFile` handles, so a mapping with a symbol is never idle
        // (`docs/MAP_MANAGER_PLAN.md` §3.4). `buffer_objects` is this engine's
        // own view, unlike the resident figures, which are process-wide and move
        // with any concurrent test's page cache.
        assert!(
            e.buffer_objects().is_empty(),
            "the tick dropped the table's block symbols, which is what makes its \
             mappings releasable"
        );
        assert!(e.context.memory_reclaim_passes.load(Ordering::Relaxed) >= 1);
    }

    /// The exact scan no longer builds a heap copy of the column: it reads the
    /// mapped blocks one batch at a time, so there is no budget to refuse and
    /// no cache to keep. What must still hold is the answer.
    #[tokio::test]
    async fn an_exact_scan_answers_without_building_a_cache() {
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute("CREATE TABLE memv (id INT, v EMBEDDING(FLOAT,4));")
            .await
            .unwrap();
        e.execute("INSERT INTO memv VALUES (1,[0.0,0.0,0.0,0.0]),(2,[1.0,1.0,1.0,1.0]);")
            .await
            .unwrap();
        // A quarter of 64 bytes would refuse any heap cache; the scan must
        // still answer from the mapped blocks.
        e.set_buffer_budget(64);
        let mut out = Vec::new();
        for r in e
            .execute(
                "SELECT id FROM memv SEARCH MATCH VECTOR \
                 (v, [1.0,1.0,1.0,1.0], 'float', 'l2', 1);",
            )
            .await
            .unwrap()
        {
            if let crate::QueryResult::Rows { .. } = r {
                out = r.rows_text();
            }
        }
        assert_eq!(out, vec![vec!["2".to_string()]]);
    }

    /// **Experiment, not a gate**: what one reclaim pass costs the query that
    /// runs right after it.
    ///
    /// Dropping the block *mapping* is cheap (the pages stay in the page cache,
    /// so the next access only re-faults them), but the tick also advises the
    /// pages themselves away - `madvise(DONTNEED)` throws them out, so the next
    /// access reads from the file. That is the price of the memory the pass
    /// returns, and it is why a ceiling should not sit exactly on the working
    /// set: the pass would then run every second and every query would pay it.
    ///
    /// Run with `cargo test --release -p executor --lib experiment_a_reclaim_pass -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "measurement, run with --ignored --nocapture"]
    async fn experiment_a_reclaim_pass_costs_the_next_query() {
        let dim = 128usize;
        let rows = 20_000usize;
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute(&format!(
            "CREATE TABLE rq (id INT, v EMBEDDING(FLOAT,{dim}));"
        ))
        .await
        .unwrap();
        let vector = |i: usize| -> String {
            let body: Vec<String> = (0..dim)
                .map(|d| format!("{}", (i * 7 + d * 13) % 991))
                .collect();
            format!("[{}]", body.join(","))
        };
        let mut sql = String::from("INSERT INTO rq VALUES ");
        for i in 0..rows {
            sql.push_str(&format!("({i}, {})", vector(i)));
            if i + 1 < rows {
                sql.push(',');
            }
        }
        sql.push(';');
        e.execute(&sql).await.unwrap();
        e.flush().await.unwrap();
        let q = format!(
            "SELECT id FROM rq SEARCH MATCH VECTOR (v, {}, 'float', 'l2', 10);",
            vector(rows / 2)
        );
        // One query, timed. It is an `async` closure body inlined at each call
        // site: a closure cannot be `async`, and the query it measures must be
        // awaited.
        macro_rules! time {
            () => {{
                let t0 = std::time::Instant::now();
                e.execute(&q).await.unwrap();
                t0.elapsed().as_secs_f64() * 1000.0
            }};
        }

        // Warm: every block mapped, every page resident.
        let mut warm = Vec::new();
        for _ in 0..8 {
            warm.push(time!());
        }
        warm.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "warm:            p50 {:.3} ms, mapped {} MB",
            warm[warm.len() / 2],
            storage::mapped::mapped_bytes() / (1024 * 1024)
        );

        // One pass over the ceiling: pages advised away, mappings dropped.
        e.set_memory_limit(1);
        let released = e.memory_tick().await;
        println!(
            "reclaim:         released {} MB, mapped {} MB left",
            released / (1024 * 1024),
            storage::mapped::mapped_bytes() / (1024 * 1024)
        );

        // The first query has to read the blocks back; the rest re-warm.
        let mut cold = Vec::new();
        for _ in 0..8 {
            cold.push(time!());
        }
        println!("first after:     {:.3} ms", cold[0]);
        let mut rest = cold[1..].to_vec();
        rest.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!("next seven (p50): {:.3} ms", rest[rest.len() / 2]);
    }

    /// **Experiment, not a gate**: the shape of the latency when the ceiling
    /// sits *under* the working set, so the tick reclaims on every pass.
    ///
    /// A pass drops the pages a query is about to read; the query reads them
    /// back, the next pass drops them again. That is a sawtooth, not a one-off
    /// cost - this run is what sizes the ceiling: fit the working set, or pay
    /// the re-read once per tick.
    ///
    /// Run with `cargo test --release -p executor --lib experiment_a_tick_under -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore = "measurement, run with --ignored --nocapture"]
    async fn experiment_a_tick_under_a_tight_ceiling_costs_every_cycle() {
        let dim = 128usize;
        let rows = 20_000usize;
        let dir = tempfile::tempdir().unwrap();
        let e = super::super::Engine::open_async(dir.path(), Default::default())
            .await
            .unwrap();
        e.execute(&format!(
            "CREATE TABLE rt (id INT, v EMBEDDING(FLOAT,{dim}));"
        ))
        .await
        .unwrap();
        let vector = |i: usize| -> String {
            let body: Vec<String> = (0..dim)
                .map(|d| format!("{}", (i * 7 + d * 13) % 991))
                .collect();
            format!("[{}]", body.join(","))
        };
        let mut sql = String::from("INSERT INTO rt VALUES ");
        for i in 0..rows {
            sql.push_str(&format!("({i}, {})", vector(i)));
            if i + 1 < rows {
                sql.push(',');
            }
        }
        sql.push(';');
        e.execute(&sql).await.unwrap();
        e.flush().await.unwrap();
        let q = format!(
            "SELECT id FROM rt SEARCH MATCH VECTOR (v, {}, 'float', 'l2', 10);",
            vector(rows / 2)
        );
        // Warm, so the first measured query is not paying for a cold file.
        for _ in 0..3 {
            e.execute(&q).await.unwrap();
        }
        e.set_memory_limit(1);
        let mut lat = Vec::new();
        for i in 0..24 {
            // The tick runs once a second; queries here are a few ms, so every
            // eighth query stands in for "one query fell in the tick's window".
            if i % 8 == 0 {
                e.memory_tick().await;
            }
            let t0 = std::time::Instant::now();
            e.execute(&q).await.unwrap();
            lat.push(t0.elapsed().as_secs_f64() * 1000.0);
        }
        let mut sorted = lat.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "tick every 8th: p50 {:.3} ms, p95 {:.3} ms, max {:.3} ms",
            sorted[sorted.len() / 2],
            sorted[(sorted.len() as f64 * 0.95) as usize],
            sorted[sorted.len() - 1]
        );
        println!(
            "  first three: {:.3} / {:.3} / {:.3} ms (query right after each pass)",
            lat[0], lat[8], lat[16]
        );
    }

    /// The watermarks come from the ceiling, and a kernel `memory.high` wins over
    /// the percentage - it is the number the kernel itself throttles at.
    #[test]
    fn watermarks_follow_the_ceiling_and_the_kernels_soft_limit() {
        let (low, high, critical) = watermarks(1000, None);
        assert_eq!((low, high, critical), (700, 800, 900));
        // A kernel soft limit inside (low, critical) replaces the percentage.
        let (low, high, critical) = watermarks(1000, Some(850));
        assert_eq!((low, high, critical), (700, 850, 900));
        // Outside that open interval it is not a watermark this engine can use,
        // so the percentage stands: 700 is the floor, not a valid high mark.
        assert_eq!(watermarks(1000, Some(990)).1, 800);
        assert_eq!(watermarks(1000, Some(700)).1, 800);
        assert_eq!(watermarks(1000, Some(100)).1, 800);
    }

    fn a_hundred() -> u64 {
        100
    }

    /// The feed follows the ceiling's kind. A cgroup without a limit reports the
    /// host's page cache (on a CI runner, gigabytes the engine never asked for),
    /// and measuring a configured ceiling against that refuses every write: the
    /// suite terminated on its first `CREATE TABLE` because the API rejected the
    /// table and the case used the nil handle.
    #[test]
    fn an_unlimited_cgroup_is_measured_by_the_process_not_by_the_host() {
        assert_eq!(usage_feed(false, Some(9_000), a_hundred), 100);
        assert_eq!(usage_feed(false, None, a_hundred), 100);
        // A container's own charge is the figure the kernel kills on.
        assert_eq!(usage_feed(true, Some(9_000), a_hundred), 9_000);
        // A limit without a readable `memory.current` still needs a figure.
        assert_eq!(usage_feed(true, None, a_hundred), 100);
    }
}
