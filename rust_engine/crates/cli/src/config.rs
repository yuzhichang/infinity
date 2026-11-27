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

//! Engine configuration (`Config`).
//!
//! C++: `Config` (`src/main/config.cppm`). Defaults and the TOML overrides used
//! by `src/unit_test/main/config_ut.cpp`; validation returns the reference's
//! error categories (version, time zone, IP, byte size, log level, time info).

#![allow(dead_code)]

use std::net::IpAddr;
use std::path::Path;

/// The version this build reports and the one a config file must name.
///
/// C++: `VERSION` (`src/common/default_values.cppm`). Kept as a constant so the
/// default, the check in `Config::validate` and the accessor cannot disagree.
pub const BUILD_VERSION: &str = "0.7.5";

pub const DEFAULT_CHECKPOINT_INTERVAL_SECS: u64 = 10;
pub const DEFAULT_OPTIMIZE_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_CLEANUP_INTERVAL_SECS: u64 = 60;

pub fn default_buffer_manager_size(cgroup_memory: Option<u64>) -> u64 {
    buffer_size_for_memory(storage::physical_memory_bytes(), cgroup_memory)
}

fn buffer_size_for_memory(physical_memory: Option<u64>, cgroup_memory: Option<u64>) -> u64 {
    let Some(bytes) = physical_memory
        .into_iter()
        .chain(cgroup_memory)
        .filter(|bytes| *bytes > 0)
        .min()
    else {
        return 8 * 1024 * 1024 * 1024;
    };
    bytes / 10 * 9 + bytes % 10 * 9 / 10
}

/// C++: `ErrorCode` subset used by `Config::Init`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    InvalidConfig,
    MismatchVersion,
    InvalidTimezone,
    InvalidIpAddr,
    InvalidByteSize,
    InvalidLogLevel,
    InvalidTimeInfo,
}

/// Why `Config::set` refused one entry.
///
/// `Unknown` and `Invalid` are handled differently on purpose: see
/// [`SetOutcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetError {
    /// This build has no knob by that name (an old or a newer spelling).
    Unknown,
    /// A known knob whose value it cannot take.
    Invalid,
}

impl From<ConfigError> for SetError {
    fn from(_: ConfigError) -> Self {
        SetError::Invalid
    }
}

/// What `Config::set` did with one `key = value` line.
///
/// The three cases have to stay distinguishable: an unknown key is an operator
/// using an older or newer spelling and is only worth a warning, while a known
/// key with a malformed value is a real mistake in a setting the engine will
/// use. Collapsing them (the old `Result<(), ConfigError>`) is what made both
/// fatal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetOutcome {
    /// The value was parsed and stored.
    Applied,
    /// No such knob in this build: ignored, with a warning.
    Unknown,
    /// A known knob with a value it cannot take.
    Invalid,
}

/// C++: `LogLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warning,
    Error,
    Critical,
}

/// C++: `StorageType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageType {
    #[default]
    Local,
    Minio,
}

/// C++: `Config`.
#[derive(Debug, Clone)]
pub struct Config {
    version: String,
    time_zone: String,
    time_zone_bias: i32,
    cpu_limit: u32,

    server_address: String,
    postgres_port: u16,
    http_port: u16,
    client_port: u16,
    connection_pool_size: u32,
    peer_retry_delay: u64,
    peer_retry_count: u64,
    peer_connect_timeout: u64,
    peer_recv_timeout: u64,
    peer_send_timeout: u64,

    log_filename: String,
    log_dir: String,
    log_to_stdout: bool,
    log_file_max_size: i64,
    log_file_rotate_count: u32,
    log_level: LogLevel,

    data_dir: String,
    /// `[storage] catalog_dir`: where the catalog's RocksDB lives. Empty means
    /// `<data root>/catalog`, the reference's relative default.
    catalog_dir: String,
    wal_dir: String,
    storage_type: StorageType,
    object_storage_url: String,
    object_storage_bucket: String,
    object_storage_access_key: String,
    object_storage_secret_key: String,
    object_storage_https: bool,
    optimize_interval: u64,
    cleanup_interval: u64,
    compact_interval: u64,
    mem_index_capacity: u64,
    snapshot_dir: String,

    lru_num: usize,
    temp_dir: String,
    buffer_manager_size: i64,
    mem_index_memory_quota: Option<i64>,
    /// `[buffer] unsealed_reclaim_interval` in seconds: the sweep's period.
    unsealed_reclaim_interval: u64,
    /// `[buffer] unsealed_max_tables_per_round`: `0` means no limit.
    unsealed_max_tables_per_round: u64,

    checkpoint_interval: u64,
    wal_compact_threshold: i64,
    wal_flush: String,

    result_cache: String,
    cache_result_capacity: u64,
    resource_dir: String,
    persistence_dir: String,

    /// Entries this parser ignored while loading, each with the reason.
    ///
    /// A config file is written by an operator against whatever version they
    /// had, so it routinely carries a knob this build dropped or renamed. That
    /// is not a reason to refuse to start - the engine can run on the settings
    /// it does understand - but it must not be silent either, or the operator
    /// spends an afternoon wondering why their change had no effect. The
    /// caller drains this into the log (`configure_server`).
    warnings: Vec<String>,
}

impl Default for Config {
    /// C++: `Config` defaults (`config_ut.cpp::test1`).
    fn default() -> Self {
        Self {
            version: "0.7.5".into(),
            time_zone: "UTC".into(),
            time_zone_bias: 8,
            cpu_limit: std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(1),
            server_address: "0.0.0.0".into(),
            postgres_port: 5432,
            http_port: 23820,
            client_port: 23817,
            connection_pool_size: 128,
            peer_retry_delay: 1000,
            peer_retry_count: 2,
            peer_connect_timeout: 2000,
            peer_recv_timeout: 0,
            peer_send_timeout: 0,
            log_filename: "infinity.log".into(),
            log_dir: "/var/infinity/log".into(),
            log_to_stdout: true,
            log_file_max_size: 1024 * 1024 * 1024,
            log_file_rotate_count: 8,
            log_level: LogLevel::Info,
            data_dir: "/var/infinity/data".into(),
            catalog_dir: "/var/infinity/catalog".into(),
            wal_dir: "/var/infinity/wal".into(),
            storage_type: StorageType::Local,
            object_storage_url: "0.0.0.0:9000".into(),
            object_storage_bucket: "infinity".into(),
            object_storage_access_key: "minioadmin".into(),
            object_storage_secret_key: "minioadmin".into(),
            object_storage_https: false,
            optimize_interval: DEFAULT_OPTIMIZE_INTERVAL_SECS,
            cleanup_interval: DEFAULT_CLEANUP_INTERVAL_SECS,
            compact_interval: 120,
            mem_index_capacity: 65536,
            snapshot_dir: "/var/infinity/snapshot".into(),
            lru_num: 7,
            temp_dir: "/var/infinity/tmp".into(),
            buffer_manager_size: default_buffer_manager_size(
                executor::memory::cgroup_memory_limit_bytes(),
            ) as i64,
            mem_index_memory_quota: None,
            unsealed_reclaim_interval: 30,
            unsealed_max_tables_per_round: 100,
            checkpoint_interval: DEFAULT_CHECKPOINT_INTERVAL_SECS,
            wal_compact_threshold: 1024 * 1024 * 1024,
            wal_flush: txn::DEFAULT_WAL_FLUSH.as_str().into(),
            result_cache: "off".into(),
            cache_result_capacity: 10000,
            resource_dir: "/usr/share/infinity/resource".into(),
            persistence_dir: "/var/infinity/persistence".into(),
            warnings: Vec::new(),
        }
    }
}

/// Section name used internally for entries under a section this build has no
/// knowledge of, so they are skipped without a second warning each.
const UNUSED_SECTION: &str = "\u{0}unused";

impl Config {
    /// C++: `Config::Init(path, nullptr)`.
    pub fn init(&mut self, path: Option<&Path>) -> Result<(), ConfigError> {
        if let Some(path) = path {
            let text = std::fs::read_to_string(path).map_err(|_| ConfigError::InvalidConfig)?;
            self.apply_toml(&text)?;
        }
        self.validate()
    }

    /// Parses `text` into this config, for a caller that already has the
    /// contents and not a path (`configure_server`).
    ///
    /// It is the same parser `Config::init` runs, so the values the server
    /// reports are the ones the config tests check.
    pub fn apply_text(&mut self, text: &str) -> Result<(), ConfigError> {
        self.apply_toml(text)?;
        // `init` runs the whole-file checks after the entries; a caller that
        // has the text rather than a path needs the same pass, or a value the
        // per-entry parser accepts but the combination cannot (a version from
        // another release, an address that is not an IP) would go unreported.
        self.validate()
    }

    /// Everything the loader ignored, one entry per problem.
    ///
    /// Empty for a clean load. Callers that log use [`Self::warnings_line`]
    /// instead of joining this themselves, so the message has one shape.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Every warning as a **single** line, or `None` when there was nothing to
    /// report.
    ///
    /// One line per ignored entry made a config written against a different
    /// release produce a wall of text, and startup logs are skimmed. Folding
    /// them keeps the signal ("your config was not fully understood") while the
    /// detail stays: the count first, then each reason, so a long line can still
    /// be read for the part that matters.
    ///
    /// The wording stays stable for tests: `N config setting(s) ignored:`.
    pub fn warnings_line(&self) -> Option<String> {
        if self.warnings.is_empty() {
            return None;
        }
        Some(format!(
            "{} config setting(s) ignored: {}",
            self.warnings.len(),
            self.warnings.join("; ")
        ))
    }

    /// `[wal] wal_flush`, resolved (the file's entry or the engine default).
    pub fn wal_flush(&self) -> &str {
        &self.wal_flush
    }

    fn apply_toml(&mut self, text: &str) -> Result<(), ConfigError> {
        let mut section = String::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
                section = name.trim().to_ascii_lowercase();
                if !matches!(
                    section.as_str(),
                    "general"
                        | "network"
                        | "log"
                        | "storage"
                        | "storage.object_storage"
                        | "buffer"
                        | "wal"
                        | "resource"
                ) {
                    // A whole section this build does not have: every entry
                    // under it is unusable, so skip the section rather than
                    // complaining once per line.
                    self.warnings.push(format!(
                        "ignoring unknown config section [{name}]; this build has no such section"
                    ));
                    section = UNUSED_SECTION.to_string();
                    continue;
                }
                continue;
            }
            let (key, value) = line.split_once('=').ok_or(ConfigError::InvalidConfig)?;
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim();
            if section == UNUSED_SECTION {
                continue; // already reported with its section
            }
            match self.set(&section, &key, value) {
                Ok(()) => {}
                // An old or a newer spelling: the rest of the file still
                // applies, so this is a warning and not a failure.
                Err(SetError::Unknown) => self.warnings.push(format!(
                    "ignoring unknown config entry [{section}] {key} = {value}"
                )),
                // A known knob with a value it cannot take. Also ignored, for
                // the same reason - one bad line should not take the server
                // down - but the message says the knob was recognised, which is
                // what an operator needs to fix it.
                Err(SetError::Invalid) => self.warnings.push(format!(
                    "ignoring invalid value for [{section}] {key}: {value}"
                )),
            }
        }
        Ok(())
    }

    /// Applies one entry. `Ok` means it was stored (or deliberately accepted
    /// and ignored); `Err(UnknownKey)` means this build has no such knob, while
    /// any other error is a known knob with a value it cannot take.
    fn set(&mut self, section: &str, key: &str, raw: &str) -> Result<(), SetError> {
        let string = || raw.trim_matches('"').to_string();
        let integer = || raw.parse::<i64>().map_err(|_| ConfigError::InvalidConfig);
        let boolean = || match raw {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(ConfigError::InvalidConfig),
        };
        match (section, key) {
            ("general", "version") => self.version = string(),
            ("general", "time_zone") => {
                let (name, bias) = parse_time_zone(&string())?;
                self.time_zone = name;
                self.time_zone_bias = bias;
            }
            ("general", "cpu_limit") => self.cpu_limit = integer()? as u32,
            // Raft cluster settings (`standalone` / `leader` / `follower` /
            // `learner` and the peer address it dials). Accepted and ignored:
            // this port runs standalone only, but the shipped `leader.toml`,
            // `follower.toml`, `learner*.toml` and the pytest configs all carry
            // them, so refusing them made half of `conf/` unloadable.
            ("general", "server_mode") => {} // discard; see above
            ("network", "peer_ip") => {}     // discard; see above
            ("network", "peer_port") => {}   // discard; see above
            ("network", "server_address") => self.server_address = string(),
            ("network", "postgres_port") => self.postgres_port = integer()? as u16,
            ("network", "http_port") => self.http_port = integer()? as u16,
            ("network", "client_port") => self.client_port = integer()? as u16,
            ("network", "connection_pool_size") => self.connection_pool_size = integer()? as u32,
            ("network", "peer_retry_delay") => self.peer_retry_delay = integer()? as u64,
            ("network", "peer_retry_count") => self.peer_retry_count = integer()? as u64,
            ("network", "peer_connect_timeout") => self.peer_connect_timeout = integer()? as u64,
            ("network", "peer_recv_timeout") => self.peer_recv_timeout = integer()? as u64,
            ("network", "peer_send_timeout") => self.peer_send_timeout = integer()? as u64,
            ("log", "log_filename") => self.log_filename = string(),
            ("log", "log_dir") => self.log_dir = string(),
            ("log", "log_to_stdout") => self.log_to_stdout = boolean()?,
            ("log", "log_file_max_size") => self.log_file_max_size = parse_byte_size(&string())?,
            ("log", "log_file_rotate_count") => self.log_file_rotate_count = integer()? as u32,
            ("log", "log_level") => self.log_level = parse_log_level(&string())?,
            ("storage", "data_dir") => self.data_dir = string(),
            ("storage", "persistence_dir") => self.persistence_dir = string(),
            ("storage", "catalog_dir") => self.catalog_dir = string(),
            ("storage", "optimize_interval") => {
                self.optimize_interval = parse_time_info(&string())?
            }
            ("storage", "cleanup_interval") => self.cleanup_interval = parse_time_info(&string())?,
            ("storage", "compact_interval") => self.compact_interval = parse_time_info(&string())?,
            ("storage", "mem_index_capacity") => self.mem_index_capacity = integer()? as u64,
            ("storage", "snapshot_dir") => self.snapshot_dir = string(),
            ("storage", "storage_type") => {
                self.storage_type = match string().to_ascii_lowercase().as_str() {
                    "local" => StorageType::Local,
                    "minio" => StorageType::Minio,
                    _ => return Err(SetError::Invalid),
                }
            }
            ("storage.object_storage", "url") => self.object_storage_url = string(),
            ("storage.object_storage", "bucket_name") => self.object_storage_bucket = string(),
            ("storage.object_storage", "access_key") => self.object_storage_access_key = string(),
            ("storage.object_storage", "secret_key") => self.object_storage_secret_key = string(),
            ("storage.object_storage", "enable_https") => self.object_storage_https = boolean()?,
            // The reference's BufferManager budget. The port maps sealed blocks
            // and keeps no BufferManager, but the same number is the ceiling on
            // the decoded HNSW chunks and the block map, so it is carried here
            // (`main.rs` hands it to `Engine::set_buffer_budget`).
            ("buffer", "buffer_manager_size") => {
                let bytes = parse_byte_size(&string())?;
                if bytes > 0 {
                    self.buffer_manager_size = bytes;
                }
            }
            ("buffer", "lru_num") => self.lru_num = integer()? as usize,
            ("buffer", "temp_dir") => self.temp_dir = string(),
            ("buffer", "unsealed_memory_quota") => {
                self.mem_index_memory_quota = Some(parse_byte_size(&string())?)
            }
            // The old spelling. Kept so an existing config keeps working; the
            // name is misleading now - the quota bounds everything not yet on
            // disk (the append buffer plus every index tail), not one index.
            ("buffer", "memindex_memory_quota") => {
                self.mem_index_memory_quota = Some(parse_byte_size(&string())?)
            }
            // The reclaim loop's two settings (`docs/MEMORY_RECLAIM.md` §5).
            // This parser has no consumer of its own - the serving binary reads
            // the same file through `toml_value` - but the shipped
            // `conf/infinity_conf.toml` carries both keys and `Config::init`
            // rejects a key it does not know, so leaving them out made the
            // shipped config unloadable.
            ("buffer", "unsealed_reclaim_interval") => {
                self.unsealed_reclaim_interval = parse_time_info(&string())?
            }
            ("buffer", "unsealed_max_tables_per_round") => {
                self.unsealed_max_tables_per_round = integer()? as u64
            }
            ("buffer", "result_cache") => self.result_cache = string(),
            ("buffer", "cache_result_capacity") => self.cache_result_capacity = integer()? as u64,
            ("wal", "wal_dir") => self.wal_dir = string(),
            ("wal", "checkpoint_interval") => {
                self.checkpoint_interval = parse_time_info(&string())?
            }
            ("wal", "wal_compact_threshold") => {
                self.wal_compact_threshold = parse_byte_size(&string())?
            }
            // The reference's older spelling (`flush_method_at_commit`), which
            // `configure_server` also honoured. Both names carry the same value.
            ("wal", "flush_method_at_commit") | ("wal", "wal_flush") => {
                let value = string();
                // Delegate to the one parser the engine also uses, so the
                // accepted spellings cannot drift apart again: this list used
                // to say `only_write | flush | append`, which both rejected
                // `flush_per_second` and accepted two words `WalFlush::parse`
                // did not know (they then fell through to `flush_at_once`).
                txn::WalFlush::parse(&value).map_err(|_| ConfigError::InvalidConfig)?;
                self.wal_flush = value;
            }
            ("resource", "resource_dir") => self.resource_dir = string(),
            _ => return Err(SetError::Unknown),
        }
        Ok(())
    }

    /// Checks the values as a whole, after every entry was applied.
    ///
    /// A check here is a property of the *combination* (a version the build does
    /// not know, an address that is not an IP), not of one line's syntax, so it
    /// cannot be caught while parsing. The policy is still "keep running": a
    /// failing check restores the default for that knob and reports it, rather
    /// than refusing to start. The reference refuses the file instead; the port
    /// runs on what it understood, which is what an operator recovering from a
    /// config written for another version needs.
    ///
    /// `invalid_values()`/`out_of_range_values()` cover each of these.
    fn validate(&mut self) -> Result<(), ConfigError> {
        // The reference compares against its build-time VERSION constant; the
        // merged tree carries 0.7.5 (upstream's release bump). A config from a
        // different release is the common case for this check - it is a stale
        // file, not a broken one - so the version falls back to the build's and
        // the rest of the settings still apply.
        if self.version != BUILD_VERSION {
            let found = std::mem::replace(&mut self.version, BUILD_VERSION.to_string());
            self.warnings.push(format!(
                "config version '{found}' does not match this build ({BUILD_VERSION}); using {BUILD_VERSION}"
            ));
        }
        if self.server_address.parse::<IpAddr>().is_err() {
            let found = std::mem::replace(&mut self.server_address, "0.0.0.0".to_string());
            self.warnings
                .push(format!("invalid server_address '{found}'; using 0.0.0.0"));
        }
        if self.connection_pool_size > u16::MAX as u32 {
            let found = std::mem::take(&mut self.connection_pool_size);
            self.warnings.push(format!(
                "connection_pool_size {found} exceeds {}; using the default 128",
                u16::MAX
            ));
            self.connection_pool_size = 128;
        }
        // `log_file_max_size` must cover at least 1 MiB; `ParseByteSize`
        // rejects the unsupported TB unit.
        if self.log_file_max_size < 1024 * 1024 {
            let found = std::mem::replace(&mut self.log_file_max_size, 1024 * 1024 * 1024);
            self.warnings.push(format!(
                "log_file_max_size {found} is below 1 MiB; using 1 GiB"
            ));
        }
        if self.optimize_interval > 720 * 3600 {
            let found =
                std::mem::replace(&mut self.optimize_interval, DEFAULT_OPTIMIZE_INTERVAL_SECS);
            self.warnings.push(format!(
                "optimize_interval {found}s exceeds 720h; using {DEFAULT_OPTIMIZE_INTERVAL_SECS}s"
            ));
        }
        Ok(())
    }

    // C++ accessors below (`Config::Version()` …).
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn time_zone(&self) -> &str {
        &self.time_zone
    }
    pub fn time_zone_bias(&self) -> i32 {
        self.time_zone_bias
    }
    pub fn cpu_limit(&self) -> u32 {
        self.cpu_limit
    }
    pub fn server_address(&self) -> &str {
        &self.server_address
    }
    pub fn postgres_port(&self) -> u16 {
        self.postgres_port
    }
    pub fn http_port(&self) -> u16 {
        self.http_port
    }
    pub fn client_port(&self) -> u16 {
        self.client_port
    }
    pub fn connection_pool_size(&self) -> u32 {
        self.connection_pool_size
    }
    pub fn peer_retry_delay(&self) -> u64 {
        self.peer_retry_delay
    }
    pub fn peer_retry_count(&self) -> u64 {
        self.peer_retry_count
    }
    pub fn peer_connect_timeout(&self) -> u64 {
        self.peer_connect_timeout
    }
    pub fn peer_recv_timeout(&self) -> u64 {
        self.peer_recv_timeout
    }
    pub fn peer_send_timeout(&self) -> u64 {
        self.peer_send_timeout
    }
    pub fn log_file_name(&self) -> &str {
        &self.log_filename
    }
    pub fn log_dir(&self) -> &str {
        &self.log_dir
    }
    pub fn log_file_path(&self) -> String {
        format!("{}/{}", self.log_dir, self.log_filename)
    }
    pub fn log_to_stdout(&self) -> bool {
        self.log_to_stdout
    }
    pub fn log_file_max_size(&self) -> i64 {
        self.log_file_max_size
    }
    pub fn log_file_rotate_count(&self) -> u32 {
        self.log_file_rotate_count
    }
    pub fn log_level(&self) -> LogLevel {
        self.log_level
    }
    pub fn data_dir(&self) -> &str {
        &self.data_dir
    }
    /// `[storage] catalog_dir`. Empty means `<data root>/catalog`.
    pub fn catalog_dir(&self) -> &str {
        &self.catalog_dir
    }
    pub fn wal_dir(&self) -> &str {
        &self.wal_dir
    }
    pub fn storage_type(&self) -> StorageType {
        self.storage_type
    }
    pub fn object_storage_url(&self) -> &str {
        &self.object_storage_url
    }
    pub fn object_storage_bucket(&self) -> &str {
        &self.object_storage_bucket
    }
    pub fn object_storage_access_key(&self) -> &str {
        &self.object_storage_access_key
    }
    pub fn object_storage_secret_key(&self) -> &str {
        &self.object_storage_secret_key
    }
    pub fn object_storage_https(&self) -> bool {
        self.object_storage_https
    }
    pub fn optimize_index_interval(&self) -> u64 {
        self.optimize_interval
    }
    pub fn cleanup_interval(&self) -> u64 {
        self.cleanup_interval
    }
    pub fn compact_interval(&self) -> u64 {
        self.compact_interval
    }
    pub fn mem_index_capacity(&self) -> u64 {
        self.mem_index_capacity
    }
    pub fn snapshot_dir(&self) -> &str {
        &self.snapshot_dir
    }
    pub fn lru_num(&self) -> usize {
        self.lru_num
    }
    pub fn temp_dir(&self) -> &str {
        &self.temp_dir
    }
    pub fn mem_index_memory_quota(&self) -> i64 {
        self.mem_index_memory_quota
            .unwrap_or(self.buffer_manager_size / 2)
    }
    /// `[buffer] unsealed_reclaim_interval` in seconds; `0` stops the sweep.
    pub fn unsealed_reclaim_interval(&self) -> u64 {
        self.unsealed_reclaim_interval
    }
    /// `[buffer] unsealed_max_tables_per_round`; `0` means no limit.
    pub fn unsealed_max_tables_per_round(&self) -> u64 {
        self.unsealed_max_tables_per_round
    }
    /// `[buffer] buffer_manager_size` in bytes.
    pub fn buffer_manager_size(&self) -> i64 {
        self.buffer_manager_size
    }
    pub fn checkpoint_interval(&self) -> u64 {
        self.checkpoint_interval
    }
    pub fn wal_compact_threshold(&self) -> i64 {
        self.wal_compact_threshold
    }
    pub fn result_cache(&self) -> &str {
        &self.result_cache
    }
    pub fn cache_result_num(&self) -> u64 {
        self.cache_result_capacity
    }
    pub fn resource_path(&self) -> &str {
        &self.resource_dir
    }
    pub fn persistence_dir(&self) -> &str {
        &self.persistence_dir
    }

    /// Every knob `SHOW CONFIGS` reports, in the engine's own spelling.
    ///
    /// This is the one list of what is queryable: the values are the ones this
    /// config resolved (a file entry overrides the default above), and
    /// `configure_server` seeds the engine's config map from it, so `SET CONFIG`
    /// and `[..]` entries end up in the same place and a getter reads one map
    /// instead of reconstructing a default. Adding a knob here is what makes it
    /// both settable and visible.
    ///
    /// Integer knobs stay integers in the JSON response (`config_is_numeric`
    /// decides that from the name), so they are rendered here without padding.
    pub fn config_values(&self) -> Vec<(&'static str, String)> {
        executor::QUERYABLE_CONFIG_KEYS
            .iter()
            .filter_map(|key| {
                let value = match *key {
                    "cache_result_capacity" => self.cache_result_capacity.to_string(),
                    "checkpoint_interval" => self.checkpoint_interval.to_string(),
                    "cleanup_interval" => self.cleanup_interval.to_string(),
                    "log_level" => self.log_level.as_str().to_string(),
                    "optimize_interval" => self.optimize_index_interval().to_string(),
                    "result_cache" => self.result_cache.clone(),
                    // `wal_flush` is reported from the live transaction manager
                    // (`Engine::config_get`), because `SET CONFIG` cannot change
                    // it; leaving it out here keeps a single source for it.
                    _ => return None,
                };
                Some((*key, value))
            })
            .collect()
    }
}

/// C++: `Config::ParseTimeZoneStr`: `utc±N` → canonical name and hour bias.
fn parse_time_zone(text: &str) -> Result<(String, i32), ConfigError> {
    let lower = text.trim().to_ascii_lowercase();
    if lower == "utc" || lower == "gmt" {
        return Ok(("UTC".to_string(), 0));
    }
    let rest = lower
        .strip_prefix("utc")
        .or_else(|| lower.strip_prefix("gmt"))
        .ok_or(ConfigError::InvalidTimezone)?;
    let bias: i32 = rest.parse().map_err(|_| ConfigError::InvalidTimezone)?;
    if bias.abs() > 14 {
        return Err(ConfigError::InvalidConfig);
    }
    Ok(("UTC".to_string(), bias))
}

/// C++: `Config::ParseByteSize`: `KB`/`MB`/`GB` suffixes; `TB` is rejected.
///
/// `B` is accepted as a factor of one (`"2B"` is two bytes), which is the
/// spelling the reclaim tests use to force a drain on the first batch.
fn parse_byte_size(text: &str) -> Result<i64, ConfigError> {
    let lower = text.trim().to_ascii_lowercase();
    let (number, factor) = if let Some(v) = lower.strip_suffix("kb") {
        (v, 1024i64)
    } else if let Some(v) = lower.strip_suffix("mb") {
        (v, 1024 * 1024)
    } else if let Some(v) = lower.strip_suffix("gb") {
        (v, 1024 * 1024 * 1024)
    } else if lower.ends_with("tb") {
        return Err(ConfigError::InvalidByteSize);
    } else if let Some(v) = lower.strip_suffix('b') {
        (v, 1)
    } else {
        (lower.as_str(), 1)
    };
    let number: i64 = number.parse().map_err(|_| ConfigError::InvalidByteSize)?;
    number
        .checked_mul(factor)
        .ok_or(ConfigError::InvalidByteSize)
}

/// C++: `Config::ParseTimeInfo`: `s`/`m`/`h` suffixes.
fn parse_time_info(text: &str) -> Result<u64, ConfigError> {
    let lower = text.trim().to_ascii_lowercase();
    let (number, factor) = if let Some(v) = lower.strip_suffix('s') {
        (v, 1u64)
    } else if let Some(v) = lower.strip_suffix('m') {
        (v, 60)
    } else if let Some(v) = lower.strip_suffix('h') {
        (v, 3600)
    } else {
        return Err(ConfigError::InvalidTimeInfo);
    };
    let number: u64 = number.parse().map_err(|_| ConfigError::InvalidTimeInfo)?;
    number
        .checked_mul(factor)
        .ok_or(ConfigError::InvalidTimeInfo)
}

/// C++: `LogLevel` parsing.
fn parse_log_level(text: &str) -> Result<LogLevel, ConfigError> {
    match text.trim().to_ascii_lowercase().as_str() {
        "trace" => Ok(LogLevel::Trace),
        "debug" => Ok(LogLevel::Debug),
        "info" => Ok(LogLevel::Info),
        "warning" => Ok(LogLevel::Warning),
        "error" => Ok(LogLevel::Error),
        "critical" => Ok(LogLevel::Critical),
        _ => Err(ConfigError::InvalidLogLevel),
    }
}

impl LogLevel {
    /// The spelling `parse_log_level` accepts, so a round trip is lossless and
    /// `SHOW CONFIGS` reports what the config file said.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Trace => "trace",
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warning",
            LogLevel::Error => "error",
            LogLevel::Critical => "critical",
        }
    }
}

#[cfg(test)]
mod tests {
    //! Ports of `ConfigTest` (`src/unit_test/main/config_ut.cpp`).

    use super::*;

    fn fixture(name: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../../test/data/config")
            .join(name)
    }

    /// C++: `ConfigTest.test1`.
    #[test]
    fn defaults() {
        let mut config = Config::default();
        config.init(None).unwrap();
        assert_eq!(config.version(), "0.7.5");
        assert_eq!(config.time_zone(), "UTC");
        assert_eq!(config.time_zone_bias(), 8);
        assert_eq!(
            config.cpu_limit(),
            std::thread::available_parallelism().unwrap().get() as u32
        );
        assert_eq!(config.server_address(), "0.0.0.0");
        assert_eq!(config.postgres_port(), 5432);
        assert_eq!(config.http_port(), 23820);
        assert_eq!(config.client_port(), 23817);
        assert_eq!(config.peer_retry_delay(), 1000);
        assert_eq!(config.peer_retry_count(), 2);
        assert_eq!(config.peer_connect_timeout(), 2000);
        assert_eq!(config.peer_recv_timeout(), 0);
        assert_eq!(config.peer_send_timeout(), 0);
        assert_eq!(config.log_file_name(), "infinity.log");
        assert_eq!(config.log_dir(), "/var/infinity/log");
        assert_eq!(config.log_file_path(), "/var/infinity/log/infinity.log");
        assert!(config.log_to_stdout());
        assert_eq!(config.log_file_max_size(), 1024 * 1024 * 1024);
        assert_eq!(config.log_file_rotate_count(), 8);
        assert_eq!(config.log_level(), LogLevel::Info);
        assert_eq!(config.data_dir(), "/var/infinity/data");
        assert_eq!(config.wal_dir(), "/var/infinity/wal");
        assert_eq!(config.storage_type(), StorageType::Local);
        assert_eq!(config.lru_num(), 7);
        assert_eq!(config.temp_dir(), "/var/infinity/tmp");
        assert_eq!(
            config.mem_index_memory_quota(),
            config.buffer_manager_size() / 2
        );
        assert_eq!(config.checkpoint_interval(), 10);
        assert_eq!(config.optimize_interval, 30);
        assert_eq!(config.cleanup_interval(), 60);
        assert_eq!(config.result_cache(), "off");
        assert_eq!(config.cache_result_num(), 10000);
    }

    /// The reference's BufferManager budget keeps its meaning: the port has no
    /// BufferManager, but the same number bounds the decoded HNSW chunks and
    /// the block map, so the parsed value has to survive into the engine
    /// instead of being dropped.
    #[test]
    fn buffer_manager_size_is_carried() {
        let mut config = Config::default();
        config
            .apply_toml("[buffer]\nbuffer_manager_size = \"3GB\"\n")
            .unwrap();
        assert_eq!(config.buffer_manager_size(), 3 * 1024 * 1024 * 1024);
        assert_eq!(config.mem_index_memory_quota(), 3 * 512 * 1024 * 1024);
        assert_eq!(config.lru_num(), 7);
    }

    #[test]
    fn buffer_size_uses_nine_tenths_of_the_effective_memory() {
        assert_eq!(buffer_size_for_memory(Some(101), None), 90);
        assert_eq!(buffer_size_for_memory(Some(100), Some(60)), 54);
        assert_eq!(buffer_size_for_memory(Some(60), Some(100)), 54);
        assert_eq!(buffer_size_for_memory(None, Some(60)), 54);
    }

    /// C++: `ConfigTest.test2`.
    #[test]
    fn full_config_file() {
        let mut config = Config::default();
        config.init(Some(&fixture("infinity_conf.toml"))).unwrap();
        assert_eq!(config.version(), "0.7.5");
        assert_eq!(config.time_zone(), "UTC");
        assert_eq!(config.time_zone_bias(), -8);
        assert_eq!(config.cpu_limit(), 2);
        assert_eq!(config.server_address(), "127.0.0.1");
        assert_eq!(config.postgres_port(), 25432);
        assert_eq!(config.http_port(), 24821);
        assert_eq!(config.client_port(), 24817);
        assert_eq!(config.peer_retry_delay(), 1000);
        assert_eq!(config.peer_retry_count(), 2);
        assert_eq!(config.peer_connect_timeout(), 2000);
        assert_eq!(config.peer_recv_timeout(), 2000);
        assert_eq!(config.peer_send_timeout(), 2000);
        assert!(!config.log_to_stdout());
        assert_eq!(config.log_file_max_size(), 2 * 1024 * 1024 * 1024);
        assert_eq!(config.log_file_rotate_count(), 3);
        assert_eq!(config.log_level(), LogLevel::Trace);
        assert_eq!(config.object_storage_url(), "0.0.0.0:9000");
        assert_eq!(config.object_storage_bucket(), "infinity");
        assert_eq!(config.object_storage_access_key(), "minioadmin");
        assert_eq!(config.object_storage_secret_key(), "minioadmin");
        assert!(!config.object_storage_https());
        assert_eq!(config.lru_num(), 8);
        assert_eq!(config.mem_index_memory_quota(), 2 * 1024 * 1024 * 1024);
        assert_eq!(config.result_cache(), "off");
    }

    /// C++: `ConfigTest.TestWrongParamNames`.
    #[test]
    fn wrong_parameter_names() {
        // Every key in that fixture is a misspelling (`versi0n`, `time_z0ne`,
        // ...). They are skipped with a warning each and the defaults stand:
        // refusing the file would mean an operator cannot start a server
        // because a config carries one knob this build dropped.
        let mut config = Config::default();
        config
            .init(Some(&fixture("test_conf_invalid_param.toml")))
            .expect("a misspelled key is a warning, not a failure");
        assert_eq!(config.version(), "0.7.5", "the default stands");
        assert!(
            !config.warnings().is_empty(),
            "each ignored key has to be reported"
        );
        for warning in config.warnings() {
            // A misspelled section and a misspelled key are both "unknown",
            // and both have to say so.
            assert!(
                warning.contains("unknown config"),
                "the warning names the reason: {warning}"
            );
        }
    }

    /// C++: `ConfigTest.TestConfInvalidValues`.
    ///
    /// Each fixture carries one knob with a value it cannot take. The loader
    /// reports it and keeps the default; the file still loads. This differs
    /// from the reference, which refuses the file - the port's rule is that a
    /// single bad line must not stop a server from starting, so the operator
    /// sees the warning and the engine runs on what it understood.
    #[test]
    fn invalid_values() {
        for name in [
            "test_conf_invalid_version.toml",
            "test_conf_invalid_timezone.toml",
            "test_conf_invalid_server_address.toml",
            "test_conf_invalid_bytesize.toml",
            "test_conf_invalid_log_level.toml",
            "test_conf_invalid_timeinfo.toml",
        ] {
            let mut config = Config::default();
            let result = config.init(Some(&fixture(name)));
            assert!(
                result.is_ok(),
                "{name}: an unusable value is skipped, not fatal"
            );
            assert!(
                !config.warnings().is_empty(),
                "{name}: the skipped value has to be reported"
            );
        }
    }

    /// C++: `ConfigTest.TestOutofRangeValues`.
    ///
    /// Same policy as `invalid_values`: the offending entry is skipped with a
    /// warning and the default stays, so one out-of-range number does not keep
    /// the server from starting.
    #[test]
    fn out_of_range_values() {
        for name in [
            "test_conf_out_of_bound_bytesize.toml",
            "test_conf_out_of_bound_number.toml",
            "test_conf_out_of_bound_timeinfo.toml",
            "test_conf_out_of_bound_timezone.toml",
            "test_conf_out_of_bound_walflush.toml",
        ] {
            let mut config = Config::default();
            let result = config.init(Some(&fixture(name)));
            assert!(
                result.is_ok(),
                "{name}: an out-of-range value is skipped, not fatal"
            );
            assert!(
                !config.warnings().is_empty(),
                "{name}: the skipped value has to be reported"
            );
        }
    }

    /// C++: `ConfigTest.TestValidValues`.
    #[test]
    fn valid_values() {
        let mut config = Config::default();
        config
            .init(Some(&fixture("test_conf_valid_value.toml")))
            .unwrap();
        assert_eq!(config.version(), "0.7.5");
        assert_eq!(config.time_zone(), "UTC");
        assert_eq!(config.time_zone_bias(), -8);
        assert_eq!(config.cpu_limit(), 2);
        assert_eq!(config.server_address(), "0.0.0.0");
        assert_eq!(config.postgres_port(), 5432);
        assert_eq!(config.http_port(), 23820);
        assert_eq!(config.client_port(), 23817);
        assert_eq!(config.connection_pool_size(), 128);
        assert_eq!(config.peer_retry_delay(), 100);
        assert_eq!(config.peer_retry_count(), 1);
        assert_eq!(config.peer_connect_timeout(), 200);
        assert_eq!(config.peer_recv_timeout(), 200);
        assert_eq!(config.peer_send_timeout(), 200);
        assert_eq!(config.log_file_max_size(), 1024 * 1024 * 1024);
        assert_eq!(config.log_file_rotate_count(), 10);
        assert_eq!(config.log_level(), LogLevel::Info);
        assert_eq!(config.optimize_index_interval(), 10);
        assert_eq!(config.cleanup_interval(), 60);
        assert_eq!(config.compact_interval(), 120);
        assert_eq!(config.mem_index_capacity(), 1048576);
        assert_eq!(config.snapshot_dir(), "/var/infinity/snapshot");
        assert_eq!(config.lru_num(), 7);
        assert_eq!(config.mem_index_memory_quota(), 1024 * 1024 * 1024);
        assert_eq!(config.checkpoint_interval(), 86400);
        assert_eq!(config.wal_compact_threshold(), 1024 * 1024 * 1024);
        assert_eq!(config.resource_path(), "/usr/share/infinity/resource");
        assert_eq!(config.persistence_dir(), "/var/infinity/persistence");
    }

    /// Every ignored setting is reported on **one** line, with a count.
    ///
    /// A config written against another release can have many, and a startup
    /// log is skimmed: four separate `warning:` lines read as four unrelated
    /// failures, while one line says "this file was not fully understood" and
    /// still carries every reason.
    #[test]
    fn ignored_settings_are_reported_on_one_line() {
        let text = r#"
[general]
version = "3.0.0"
[unknown_section]
foo = "bar"
[log]
typo_key = "x"
[wal]
wal_flush = "bogus_mode"
"#;
        let mut config = Config::default();
        config.apply_text(text).expect("a bad entry is a warning");

        // Four problems: the section, the unknown key, the bad wal_flush value
        // and the version mismatch.
        assert_eq!(config.warnings().len(), 4, "{:?}", config.warnings());
        let line = config.warnings_line().expect("there were warnings");
        assert!(!line.contains('\n'), "the report is one line: {line:?}");
        assert!(
            line.starts_with("4 config setting(s) ignored: "),
            "the count leads: {line}"
        );
        for reason in [
            "unknown config section",
            "unknown config entry",
            "invalid value",
            "version",
        ] {
            assert!(line.contains(reason), "{reason} is missing from {line}");
        }

        // A clean load has nothing to report.
        let mut clean = Config::default();
        clean.apply_text("[log]\nlog_level = \"info\"\n").unwrap();
        assert_eq!(clean.warnings_line(), None);
    }

    /// The text `configure_server` hands to `Config::apply_text`.
    #[test]
    fn the_served_config_text_resolves() {
        let text = r#"
[log]
log_level = "debug"
[storage]
cleanup_interval = "30s"
catalog_dir = "/tmp/x/mycatalog"
[wal]
checkpoint_interval = "86400s"
[buffer]
result_cache = "on"
cache_result_capacity = 4096
[wal]
wal_flush = "only_write"
"#;
        // Split by section so a failure names the section that broke.
        for (name, chunk) in [
            ("log", "[log]\nlog_level = \"debug\"\n"),
            (
                "storage",
                "[storage]\ncleanup_interval = \"30s\"\ncatalog_dir = \"/tmp/x/mycatalog\"\n[wal]\ncheckpoint_interval = \"86400s\"\n",
            ),
            (
                "buffer",
                "[buffer]\nresult_cache = \"on\"\ncache_result_capacity = 4096\n",
            ),
            ("wal_flush", "[wal]\nwal_flush = \"only_write\"\n"),
        ] {
            let mut c = Config::default();
            let r = c.apply_text(chunk);
            assert!(r.is_ok(), "section {name} must resolve: {:?}", r.err());
        }
        let mut c = Config::default();
        let r = c.apply_text(text);
        assert!(
            r.is_ok(),
            "configure_server's text must resolve: {:?}",
            r.err()
        );
        assert_eq!(c.wal_flush(), "only_write");
        assert_eq!(c.log_level(), LogLevel::Debug);
    }

    /// Every shipped config must load, not just the one the round-trip test
    /// happens to name.
    ///
    /// `conf/infinity_conf.toml` carries `[storage] catalog_dir`, which this
    /// parser did not know - so `Config::init` returned `InvalidConfig` for the
    /// very file that ships with the engine. The test above only exercised
    /// `test/data/config/infinity_conf.toml`, whose key set differs, which is
    /// why nothing caught it.
    #[test]
    fn every_shipped_config_loads() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../conf");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).expect("conf/ is readable") {
            let path = entry.expect("a directory entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let mut config = Config::default();
            let result = config.init(Some(&path));
            assert!(
                result.is_ok(),
                "conf/{name} is shipped with the engine and must load: {:?}",
                result.err()
            );
            checked += 1;
        }
        assert!(checked > 0, "conf/ holds configs to check");
    }

    /// The checked-in config carries the reclaim settings, so `Config::init`
    /// has to accept them: a key it does not know is `InvalidConfig`.
    #[test]
    fn the_shipped_config_loads_its_reclaim_settings() {
        let mut config = Config::default();
        config.init(Some(&fixture("infinity_conf.toml"))).unwrap();
        assert_eq!(config.unsealed_reclaim_interval(), 30);
        assert_eq!(config.unsealed_max_tables_per_round(), 100);
    }

    /// `[wal] wal_flush` takes each of the engine's three modes; anything else
    /// is skipped with a warning and the default stands.
    ///
    /// This list used to read `only_write | flush | append`: it rejected
    /// `flush_per_second` (the shipped default) and accepted two words
    /// `WalFlush::parse` mapped to `flush_at_once`, so a config could validate
    /// and mean something else. The validation now delegates to that one parser,
    /// and the outcome here is "kept" or "ignored with a warning".
    #[test]
    fn wal_flush_accepts_the_engine_modes_and_refuses_the_rest() {
        for mode in ["only_write", "flush_at_once", "flush_per_second"] {
            let mut config = Config::default();
            assert!(
                config
                    .apply_toml(&format!("[wal]\nwal_flush = \"{mode}\"\n"))
                    .is_ok(),
                "{mode} is an engine mode and must load"
            );
            assert_eq!(config.wal_flush, mode);
            assert!(
                config.warnings().is_empty(),
                "{mode} is valid and must not warn"
            );
        }
        for bad in ["flush", "append", "only-write", "flush_every_second"] {
            let mut config = Config::default();
            assert!(
                config
                    .apply_toml(&format!("[wal]\nwal_flush = \"{bad}\"\n"))
                    .is_ok(),
                "{bad} is skipped, not a load failure"
            );
            assert_eq!(
                config.wal_flush,
                txn::DEFAULT_WAL_FLUSH.as_str(),
                "{bad} leaves the default in place"
            );
            assert!(
                config
                    .warnings()
                    .iter()
                    .any(|w| w.contains("invalid value") && w.contains("wal_flush")),
                "{bad} has to be reported: {:?}",
                config.warnings()
            );
        }
    }

    /// `"2B"` is two bytes, not an unparseable value: the end-to-end reclaim
    /// test sets it to force a drain on the first batch.
    #[test]
    fn a_bare_byte_suffix_is_a_factor_of_one() {
        assert_eq!(parse_byte_size("2B").unwrap(), 2);
        assert_eq!(parse_byte_size("2b").unwrap(), 2);
        assert_eq!(parse_byte_size("4096").unwrap(), 4096);
        assert_eq!(parse_byte_size("1KB").unwrap(), 1024);
        assert_eq!(parse_byte_size("2MB").unwrap(), 2 * 1024 * 1024);
        assert!(parse_byte_size("1TB").is_err());
        assert!(parse_byte_size("lots").is_err());
    }
}
