# 映射管理器（MapManager）与表级内存回收改造 —— 开发计划书

状态：**提议 / 待评审**
范围：`rust_engine/crates/storage`（映射层）、`rust_engine/crates/executor`（块缓存、回收循环）
取代：`Context::block_maps`（`BlockMapCache`，CLOCK + budget）

标注：**[实测]** 本轮在代码里核对过或跑过数据；**[设计]** 是本文提出的方案；**[推算]** 是由前者推出、尚未测量的数。

---

## 1. 目标与前提

### 1.1 目标

1. 进程内**所有**文件的只读 mmap 由一个单件 `MapManager` 统一持有，按 `table_id / file_path / inode` 分层索引。
2. 内存回收以**表**为单位决策：`size`（未落盘字节 + 已映射字节）与 `age`（最后一次读/写的提交时刻）两臂做 RRF 融合。
3. 废除 `block_maps` 的 LRU/CLOCK 预算机制——它按块管理，与按表回收是两套并行的策略，会互相抵消。

### 1.2 前提（已核对，是本次方案成立的基础）

| 前提 | 核对结果 |
|---|---|
| 所有数据文件都不就地编辑，只通过 rename 发布 | **成立**。`format::write_parts` 写兄弟临时文件 → `File::create` → `fsync` → `rename`（`storage/src/definition/format.rs:699-737`）。块列文件走同一函数（`write_block_column_synced` → `write_parts*`，`:544-567`）。[实测] |
| 索引 chunk 一次性写入、ID 单调分配、从不复用 | **成立**。`take_next_chunk_id` 单调取号（`blocks/engine.rs:6923`），合并时旧 chunk 被 `remove_file` 删除而非覆盖（`:7479`）。[实测] |
| 未 seal 的块列文件会被重写 | **成立**，但重写也是 rename 发布的，inode 变化，旧映射仍然指向旧 inode 的页。[实测] |
| 因此 `(dev, ino)` 是文件内容的稳定身份 | **成立**。路径可以复用，inode 不会：rename 带来新 inode，旧映射永远不会被误当成新内容。[实测] |

> 这一条是 MapManager 能按 inode 做键、并且**不需要 mtime 校验**的全部理由。若将来有任何路径改成就地 `ftruncate` + `pwrite`，映射会 SIGBUS 且键会失效——必须在 CI 里守住「不就地编辑」，见 §7。

---

## 2. 现状评估

### 2.1 当前的映射层

`storage::buffer::mapped` 有 `MappedFile`，内部 `Arc<Mmap>`，`Drop` 减计数。当前形态 [实测]：

- **每次 `open` 都新建一个 VMA**（`open_path` 直接 `Mmap::map`，`mapped.rs:286-297`）。同一文件被两个读者打开就是两个 VMA。
- 一个 **slot 表**（`Registry { slots: Vec<Option<Weak<Mmap>>> }`，`:54-96`）只是为了给 `release_mapped_pages` 提供遍历目标，**不做去重**。
- 计数器 `MAPPINGS` / `MAPPED_BYTES` 统计的是**句柄数**，不是对象数（`:334-336`）。

上一轮实测的推论 [实测]：

```
after mmap + touch      VmRSS=269  RssFile=265  Pss=266  PssFile=263
after 2nd mmap+touch    VmRSS=525  RssFile=521  Pss=267  PssFile=263
```

两次映射同一文件：`RssFile` 翻倍，`Pss` 不动。**物理页只有一份，但 RSS 被算了两次。** 这在内存治理里是实质问题：`memory_usage()` 走 `VmRSS`（无 cgroup 时，`state.rs:7137`），重复映射会让进程看起来占了两倍。

### 2.2 当前的块缓存在管什么

`BlockMapCache`（`engine/state.rs:6591-6625`）[实测]：

| 字段 | 作用 |
|---|---|
| `entries: HashMap<BlockId, CachedBlock>` | 块 → `Arc<MappedBlock>` |
| `generations: HashMap<BlockId, u64>` | 重写代数，防「半写映射被缓存」 |
| `order: VecDeque<BlockId>` | CLOCK 手 |
| `bytes` | charge = 映射长度 + 解码 version 的堆 |

三个问题：

1. **预算与表无关**。`buffer_manager_size / hnsw_count` 是全局均分（`index_build.rs:1074-1077`），一张热表和一张死表争同一个预算。
2. **只覆盖块**。索引 chunk（HNSW/SPANN/PLAID/SMVE/EMVB）、全文 segment、secondary index 的映射各自独立，回收时没有共同视图。
3. **与回收阶梯重复**。`shrink_block_maps`（`memory.rs:756-761`）是回收阶梯第 4 步，而表级 drain 是第 6 步；两套策略对同一批页竞争，谁也不完整。

### 2.3 现有的表级 RRF

`engine/maintain.rs` 已有 `ReclaimCandidate { key, bytes, age_ms }` 与 `rrf_order`（K=3，规格明确、有 4 个测试）[实测]。本次改造**复用它**，只换两臂的取值：

| 臂 | 现在 | 改为 |
|---|---|---|
| size | `unsealed_bytes + memindex_bytes`（`reclaim.rs:398-406`） | 加上该表 `MapManager` 里所有映射的字节 |
| age | `now_ts - 最后 append ts`（`reclaim.rs:408-412`） | `now_ts - max(最后读提交时刻, 最后写提交时刻)` |

RRF 本身不改：`bytes` 与 `age_ms` 的语义变了，融合公式与 `K=3` 的取舍理由仍然成立。

### 2.4 缺失的一环：按表归属

`MapManager` 需要知道一个文件属于哪张表。路径里已经带了：

```
data/db_{db_id}/tbl_{table_id}/seg_{seg}/blk_{blk}/{col}.col      ← 块
data/db_{db_id}/tbl_{table_id}/idx_{index_id}/seg_0/chunk_{n}.idx  ← HNSW / PLAID / SMVE / EMVB
```

所以 `table_id` 可以从路径解析（`tbl_(\d+)`），无需额外注册。`data` 根在 `Engine::blocks_root()`（`blocks/engine.rs:301`），因此相对路径足以定位。[实测]

**但** `MapManager` 在 `storage` crate，`table_id` 的解析属于格式知识——应放在 `storage::format` 里，由 MapManager 调用，而不是让 MapManager 猜测布局。

---

## 3. 目标架构

### 3.1 分层结构 [设计]

```
MapManager（单件，Arc 持有）
├── by_table: HashMap<TableKey, TableMappings>
│     └── TableMappings
│          ├── path → ObjectKey        （一个表读过的所有文件）
│          ├── mapped_bytes: u64       （累计，用于 size 臂）
│          └── last_access_ts: u64     （最近一次命中的提交时刻，用于 age 臂）
├── by_object: HashMap<ObjectKey, ObjectEntry>
│     └── ObjectEntry
│          ├── map: Arc<Mmap>          （唯一的 VMA）
│          ├── bytes: usize            （map.len()，只记一次）
│          ├── handles: usize          （读者数；0 = 可回收）
│          └── tables: SmallVec<(TableKey, u32)>  （共享计数：一个文件通常只属一张表，
│                                                  但 persistence 对象里多个表的 chunk 共享同一 blob）
└── paths: HashMap<TableKey, HashMap<PathBuf, ObjectKey>>   （路径 → 对象，支撑 by_table 的查询）
```

`ObjectKey = (dev, ino)`（unix）/ `(path, len)`（非 unix）。**只按内容身份去重，不按路径**：硬链接、rename 后的旧名字都会落到同一个条目上，这正是需要的语义。

### 3.2 三类句柄

| 类型 | 用途 | 释放时 |
|---|---|---|
| `TableMapping` | 一次表级操作（一个块的读、一个 chunk 的读） | 减 `handles` 与 `TableMappings` 计数 |
| `ObjectRef` | 底层 `Arc<Mmap>` 的窗口（`range`） | 减 `handles`；归零后条目留在表里待回收 |
| `MapManager::open(table, path)` | 唯一入口 | — |

`MappedFile` 保留为**对外类型**（现有 11 处调用点、若干测试依赖它的 `Deref<[u8]>`），内部改为持有 `ObjectRef`。这样不需要改索引层的零拷贝结构（`MappedFloats`、`MappedGraph`、`MappedSlice` 等 20 余处）。

### 3.3 回收接口

```rust
impl MapManager {
    /// 该表所有仍在映射中的字节。
    fn mapped_bytes(&self, table: TableKey) -> u64;
    /// 该表最近一次命中的提交时刻。
    fn last_access_ts(&self, table: TableKey) -> u64;
    /// 释放该表空闲的映射（无读者持有的），返回释放字节数。
    fn release_idle(&self, table: TableKey, want: usize) -> usize;
    /// 全进程：释放所有空闲映射直到 want。
    fn release_idle_global(&self, want: usize) -> usize;
}
```

`release_idle` 只撤 **`handles == 0`** 的条目。一个正被查询持有的映射不能撤——这是 §7 里必须用测试锁住的不变量。

---

## 4. 分阶段实施

五个阶段，每个阶段可独立编译、独立验证、独立提交。**任一阶段若验收不过，停在上一阶段也仍然是可用的改进。**

### 阶段 1：`MapManager` 骨架与去重（不改任何调用方）

| 项 | 内容 |
|---|---|
| 新增 | `storage/src/buffer/map_manager.rs`：`ObjectKey`、`ObjectEntry`、`TableKey`、`MapManager`（`by_object` + `paths` + `acquire/release`） |
| 改造 | `MappedFile` 内部 `Arc<Mmap>` → `ObjectRef`；`open_path`/`open_range`/`from_mmap` 走 `MapManager::open` |
| 保持 | `MAPPINGS`/`MAPPED_BYTES` 语义**改为对象数**（`mapping_count()` 变成「进程内不同文件的映射数」），并修正 `mapping_lifecycle.rs` 的注释与断言 |
| 不动 | `block_maps`、回收阶梯、任何索引代码 |
| 验收 | ① 同一文件 `open` 两次只产生 1 个 VMA（`/proc/self/maps` 计数）② 现有 500+ storage 单测全绿 ③ `mapping_lifecycle` 集成测试全绿 |

**风险**：`from_mmap`（`secondary_lowcard.rs:228`、`secondary_segment_pgm.rs:2003`）由调用方自己 `Mmap::map`，拿不到 inode，只能作为独立对象登记。这一阶段允许它不去重，阶段 5 再收敛。

**实施记录（已完成）**：`mapped.rs` 内的 `Registry` 已从「slot 弱引用表」改为「按 `ObjectKey` 键的表 + 空闲队列」；`MappedFile` 的 `slot` 字段改为 `key: Option<ObjectKey>`；`open_path`/`open_range` 走 `acquire` 去重，`from_mmap` 登记为独立对象。`MAPPINGS`/`MAPPED_BYTES` 现在数的是**对象**。新增 `crates/storage/tests/mapping_identity.rs`（4 个用例，解析 `/proc/self/smaps` 按 inode 数段）：

- `a_second_open_reuses_the_mapping` — 两次 open 后该 inode 只有 1 个 VMA，且地址区间相同
- `unmap_idle_gives_the_address_space_back` — 被持有的映射不会被撤；释放后撤销并归还地址空间
- `the_counter_counts_objects_not_handles` — 两个句柄算 1 个对象
- `a_replaced_file_gets_a_fresh_mapping` — rename 后新 inode 是新对象，旧句柄仍读旧内容

**空闲上限（§6 决策 3/3b，已完成）**：`release` 在最后一个句柄释放时把 key 入空闲队列，超出 `IDLE_MAPPING_LIMIT` 就从队首 unmap；`acquire` 命中时把 key 移出队列。上限由 `/proc/sys/vm/max_map_count / 16` 推导并夹在 `[1024, 65536]`，非 Linux 用 16384。新增只读接口 `idle_mapping_count()`、`idle_mapping_limit()`、`kernel_max_map_count()`。

**启动告警（已完成）**：`cli/src/main.rs` 在 serving 模式的配置行之后，若 `max_map_count < 1048576`（ES 推荐值）打印一条 warning，带 `sysctl -w` 的修法。不改系统设置。

**`mapping_lifecycle.rs` 已调整**：原断言「最后句柄释放即 unmap」现在断言「归零后仍是对象（idle）」，再调 `unmap_idle_mappings` 断言归还。注释说明了为什么这样改。

**测试**：`mapping_identity.rs` 6 个用例。因为计数是进程级的，断言精确值的用例用一把 `Mutex` 串行，并且只断言**增量**——一度写成绝对值，被并行测试干扰而 flake，已改。

### 阶段 2：表归属与按表统计

| 项 | 内容 |
|---|---|
| 新增 | `storage::format::table_of_path(&Path) -> Option<u64>`：解析 `tbl_(\d+)`；`storage::format::db_of_path` 同理 |
| 改造 | `MapManager::open` 带上 `TableKey`（由调用方从路径解析后传入，或由 MapManager 自行解析——二选一，见 §6 决策 1） |
| 新增 | `TableMappings { mapped_bytes, last_access_ts }`，`open` 与命中时更新 |
| 新增 | `last_access_ts` 的写入接口：**`TableEntry::last_access_ts`** + `Engine::note_table_access(table)`，在块读取路径调用 |
| 验收 | ① 两张表各读自己的块，`mapped_bytes` 各自正确、互不串 ② 提交一次读后 `last_access_ts` 前进 ③ 单测覆盖「同一 persistence 对象里两个表的 chunk」 |

**实施记录（阶段 2 已完成）**

- `storage::format::table_of_path` / `db_of_path`：从路径的 `tbl_{id}` / `db_{id}` 分量解析归属。块（`…/tbl_t/seg_s/blk_b/`）与索引 chunk（`…/tbl_t/idx_i/seg_s/`）共用同一条解析。无归属（catalog、WAL、persistence blob）返回 `None`，调用方按「不可归属」处理，不是错误。
- `Entry.tables: Vec<(u64, usize)>`，按引用数**等分**：`bytes * mine / total`。`tables` 的计数是**累积**的——只有对象被 unmap 时才为零。这一点起初写成了「按活动句柄数」，导致空闲映射全部报 0 归属，而空闲正是回收最关心的状态；`unmap_idle_of_a_table_leaves_the_other_alone` 抓到了它。
- 新增 `mapped_bytes_of(table)`、`mapped_bytes_by_table()`、`unmap_idle_of(table)`。
- **access clock 放在 `TableEntry`，不是全局 map**（见下方决策 4 的修正）。`Engine::note_table_access(table)` 写 `TableEntry::last_access_ts`，取 `current_ts()`（已提交时钟），单调更新。调用点在 `Engine::block_map`，覆盖块读的命中与未命中两条路径。
- 测试：`mapping_identity.rs` 9 个用例（新增表归属、共享等分、按表 unmap 三个）；`reading_a_table_advances_its_access_clock` 验证读推进时钟。

### 阶段 2.5：`version` 文件零拷贝（已完成）

块缓存的堆开销里最大的一项不是列数据，而是 `version` 文件解码出来的 `deleted` 数组：`u64 × 8192` = **64 KiB/块**。它被 `BlockMapCache` 按 `heap_bytes()` 计入预算，于是"块在页缓存里"这个说法对每个块都不成立。

改法：`BlockVersionFile::deleted` 从 `Vec<TxnTimeStamp>` 变成 `DeletedSlots` 枚举：

| 形态 | 用于 | 存储 |
|---|---|---|
| `Owned(Vec<u64>)` | 正在写的块（可变） | 堆 |
| `Mapped(MappedSlots)` | 读取的块 | 映射 + 偏移，按需读 |

`MappedBlock::open` 与 `Engine::read_block_version` 都改用 `decode_mapped`：`version` 走 `MappedFile::open`（不再是 `chunk_store().read()`），`deleted` 段留在页缓存里。`heap_bytes()` 对映射形态报 0，块的 charge 于是几乎全是映射。

**对齐**：`deleted` 段起点是 `2 + 16 × creates + 2`，实测 `creates = 1..6` 全部 4 字节对齐、**从不** 8 字节对齐，所以用 `bytemuck::pod_read_unaligned` 逐槽读。x86-64 上非对齐 `u64` 加载无惩罚；换到有惩罚的目标应改写入端补齐对齐，而不是在这里复制。

**踩到的坑（重要）**：`DeletedSlots::get()` 对映射形态返回 `None`（拿不到引用），而 9 处可见性判断都写成 `deleted.get(slot).copied().unwrap_or(0) == 0` —— 于是映射块的墓碑全部读成"未删除"，`blocks_reconstruct_a_table_after_its_rows_are_deleted` 立刻抓到（删掉的行复活了）。现在所有可见性判断统一走 `stamp(slot)`，`get()` 的文档也写明它不能用于可见性判断。

**实测收益**：每块堆占用从 64 KiB 降到 0 —— `created` 也做成了映射（见下）。`a_sealed_block_is_mapped_once_and_shared` 断言 `heap_bytes() == 0`，并额外断言映射里的槽位数，防止"读不到所以报得少"。

### 阶段 2.6：`created` 记录也零拷贝（已完成）

`deleted` 之外还有一段：`created`（每个 append 一条，`{u64 ts, i64 cumulative}`）。它的**内存**很小，但代价是**每次开块的分配**。实测（release，`decode` 带分配 vs `decode_mapped`）：

| appends | created 堆 | decode | decode_mapped |
|---|---|---|---|
| 1 | 16 B | 6465 ns | **37 ns** |
| 8 | 128 B | 6394 ns | 39 ns |
| 64 | 1 KiB | 6456 ns | 84 ns |
| 128 | 2 KiB | 6518 ns | 146 ns |
| 512 | 8 KiB | 6891 ns | 447 ns |

改法与 `deleted` 同构：`CreatedRecords::{Owned, Mapped}`，`MappedCreates` 存 `(backing, offset, len)` 按需读；`VersionCreate` 加 `#[repr(C)] + Pod` 让磁盘布局显式。偏移同样是 4 字节对齐（create 段紧跟在 u16 计数后），继续用 `pod_read_unaligned`。

`BlockVersionFile::heap_bytes()` 对映射形态报 0：一个读出来的块现在**完全**是页缓存。

`locate_sections` 把"两个段在哪、各多长"从 `parse_header` 里分出来，映射形态因此不需要先解析出 `Vec` 再丢掉。

调用侧需要一个 `CreatedRecords::iter()`（3 处 `for create in &version.created`）和一个从后往前的查找（`created_at` 原本用 `rfind`，改成倒序索引，对映射形态更自然）。

---

### 阶段 3：区块缓存退化为纯符号缓存

#### 3.0 两层的关系（先定边界，再动手）

```
block_map(table, root, id)            ← BlockId → 符号对象的解析 + 一致性
  ├── BlockMapCache: HashMap<BlockId, Arc<MappedBlock>>   只答"在不在"
  │     └── generations: HashMap<BlockId, u64>            只答"被重写过几次"
  └── 未命中 → MappedBlock::open(root, id)
        └── 每个文件 MappedFile::open → MapManager（按 inode 去重、持有、按表回收）
```

| 关注点 | 归谁 | 理由 |
|---|---|---|
| `BlockId` 对应哪个 `MappedBlock` | `block_map` | 逻辑块号是引擎概念，`MapManager` 不知情 |
| 该 id 是否被重写过 | `block_map`（`generations`） | 与写入路径耦合，且只对块有意义 |
| 一个文件有几个 VMA | `MapManager` | 文件身份问题 |
| 映射占多少字节、何时回收 | `MapManager` | 回收按**表**决策，块级预算已废除 |
| 映射生命周期 | `MapManager` | — |

**两者不能合并的根本原因**：一个 `BlockId` 会同时对应两个 inode。块 3 被重写后 inode 从 100 变 101，读者 A 仍持有旧 `Arc<MappedBlock>`（inode 100），新读者要 inode 101——`MapManager` 里两个 inode 都在，`BlockMapCache` 只留最新的符号。按 `BlockId` 做键的映射表表达不了"同一逻辑块的两个物理版本共存"，而这正是 generation 存在的理由。

**`MappedBlock` 是接缝**：它持有 `MappedFile`（`MapManager` 的句柄），自身是纯符号聚合。

#### 3.1 改动清单

| 项 | 内容 |
|---|---|
| 删除 | `BlockMapCache` 的 `order`、`bytes`、`CachedBlock::referenced`、`CachedBlock::charge`，以及 `shrink_by`、`trim_to`、`shrink_to`、`evict_one`、`bytes()`；`insert` 去掉 `budget` 参数 |
| 保留 | `entries`、`generations`、`get`、`insert`（先到者胜）、`remove`、`retain_table`、`len`、`blocks` |
| **原样保留** | `block_map` 里的 3 次重试与 `current == generation` 检查；`block_written` 必须**在文件落盘之后**调用 |
| 改造 | `drop_block_maps(table_id)` 做两个动作：`retain_table(table_id)`（符号）+ `storage::mapped::unmap_idle_of(table_id)`（映射）。两者不是原子的，可接受：符号先没了，新读者会重新 `open`；旧映射短暂残留只多占一会儿地址空间 |
| 改造 | `drop_block_map(id)`（单块）保留 `remove(&id)`；底层旧映射由 idle 队列自然回收 |
| 改造 | `memory.rs` 回收阶梯第 4 步：`shrink_block_maps` → `storage::mapped::release_mapped_pages` / `unmap_idle_of` |
| 改造 | `set_buffer_budget` 不再调 `trim_to`（`buffer_budget` 仍供 HNSW 用，字段保留） |

**generation 在阶段 3 之后更重要**：预算淘汰消失后，缓存条目只由显式 drop 删除，一个坏条目可能存活很久。`index_combination_soak.rs:99-102` 记录的正是这个缺陷。

#### 3.2 对外可见面（已确认）

| 字段 | 处置 |
|---|---|
| `buffer_used_bytes()` | **删除**。`heap_bytes()` 已恒为 0，它实际等于"已缓存块映射字节之和"，被 `mapped_bytes()` 完整覆盖；`buffer_pool_usage` 在该仓库不存在 |
| `BufferedObjectCount` | **保留**，数据源改为 `storage::mapped::mapping_count()`。C++ 对应 `BufferManager::BufferedObjectCount()` = `buffer_map_.size()`；把索引 chunk 计进来数值会变大，已确认接受 |
| `MemoryCacheMiss` / `DiskCacheMiss` | 保留（命中率统计仍有意义） |
| `buffer_objects()` / `SHOW OBJECT` | 保留，仍只列**块**（按块目录查，不扩到索引 chunk） |

#### 3.3 测试

| 测试 | 处置 |
|---|---|
| `the_block_map_evicts_the_least_recently_used_mapping` | **删除**（测的是被移除的 CLOCK） |
| `a_tiny_buffer_budget_keeps_the_block_map_empty` | **改写**为"映射由 `MapManager` 持有、按表释放" |
| `buffer_obj_ut.rs` 的 `buffer_used_bytes` 断言 | 改为断言 `mapped_bytes_of(table)` 归零 |
| `index_combination_soak.rs` 注释 | 更新（失效机制仍在，实现变了） |

#### 3.4 实施记录（已完成）

实际改动的与计划一致，另有一处**计划漏掉的真问题**：

**符号缓存让映射永不空闲。** `BlockMapCache` 持有 `Arc<MappedBlock>`，`MappedBlock` 持有 `MappedFile` 的 `Arc` —— 于是 `MappedFile` 的引用计数永不归零，`handles` 永远 > 0，回收阶梯的 `unmap_idle` 一个映射也撤不掉。**回收完全失效，比原来的 LRU 更糟。** `a_tick_over_the_ceiling_gives_the_block_mappings_back` 直接抓到（`mapped_of_table` 前后不变、`idle=0`）。

修法：`release_idle_mappings` 先按表**丢符号**（`drop_block_symbols_for_reclaim`，按 `mapped_bytes_by_table` 从大到小），再 `unmap_idle_mappings`。下一次读重新解析块（每块一次目录遍历），这是拿回地址空间的代价。计划 §3.1 里"符号与映射分离"的说法漏了这一步，已在此补记。

其余改动：
- `BlockMapCache` 只留 `entries` + `generations`（+ `get`/`insert`/`remove`/`retain_table`/`blocks`，`len` 降为 `#[cfg(test)]`）。
- `drop_block_maps` = `retain_table` + `unmap_idle_of`。
- `shrink_block_maps` → `release_idle_mappings`（`memory.rs` 回收阶梯第 4 步）。
- `buffer_used_bytes()` 删除；`BufferPoolUsage` 改 `mapped_bytes()`；`BufferedObjectCount` 改 `mapping_count()`；`mem_bench` / `memory.rs` 诊断改 `mapped_bytes()`。
- 测试：`the_block_map_evicts_the_least_recently_used_mapping` → `the_block_map_keeps_one_symbol_per_block`；两个预算测试 → `dropping_a_table_releases_only_its_own_mappings`；`buffer_obj_ut` 的 LRU 断言改为"去重 + 上限"。

**测试串行化**：映射表是进程级的，断言精确值的测试会互相干扰。新增 `storage::mapped::test_guard()`（`#[cfg(debug_assertions)]`——`cfg(test)` 传不到依赖 crate），并改用**增量**断言而非绝对值：表 id 只在单个引擎内唯一，跨引擎同 id 会串。

#### 3.5 验收

① 块重写后立即读到新内容（generation 仍生效）② `index_combination_soak` 通过 ③ drop 表后该表映射归零 ④ `SHOW BUFFERS` 三个字段有数据且语义如 3.2 ⑤ 并发读同一块只产生一个 VMA

---

### 阶段 4：回收阶梯与 RRF 改造

| 项 | 内容 |
|---|---|
| 改造 | `unsealed_table_stats`：`footprint = unsealed_bytes + memindex_bytes + map_manager.mapped_bytes(table)`；`last_ts = max(last_write_ts, map_manager.last_access_ts(table))` |
| 改造 | 回收阶梯第 4 步 `shrink_block_maps` → `map_manager.release_idle_global(want)` |
| 改造 | `reclaim_unsealed_until` 的候选集：不再只筛 `table_has_unsealed`，而是「unsealed 或 has idle mappings」的表 |
| 保持 | `rrf_order` 不变（两臂语义变了，公式不变）；`K=3` 的取舍理由仍成立 |
| 验收 | ① `rrf_order` 现有 4 个测试通过 ② 新增测试：一张只有映射、没有未落盘数据的表也能被选中并释放 ③ `MEMORY_RECLAIM.md` §4 更新 |

**实施记录（阶段 4 已完成，含三处计划外的修正）**

按计划改的：`unsealed_table_stats` 的 footprint 加上 `mapped_bytes_of(table)`、age 取 `max(append_ts.last(), last_access_ts)`、候选集改为「有未落盘数据或有足迹」。`table_id` 从 `list_tables()` 一次性建 `name → id` 表（`TableMeta::name` 已是 qualified，与 `TableEntry` 的 key 同拼写）。

计划外但必须做的三处，都是"只读表"这条路径暴露的：

1. **`reclaim_target_met` 也要算映射字节。** 原来只看 `budget_bytes()`（未落盘）。一张行全落盘、映射很大的表会让 `0 <= target` 立即成立，这一轮**直接提前退出**，候选集再对也不会被遍历到。
2. **`dump_table` 返回 false 时，「释放了映射」也要算进展。** 只读表没有东西可写，`dump_table` 必然返回 false；不计入的话这一轮判定失败并退避，刚释放过的表又被推后。
3. **排水前必须先丢该表的块符号。** 与阶段 3 同一个陷阱：符号持有 `Arc<MappedFile>`，映射带着句柄永远不空闲，先 unmap 则释放 0 字节。

另外 `ReclaimCandidate.table_id` / `TableStat.table_id` 用 `Option<u64>` 而不是 `0` 作哨兵——**`0` 是真实表 id**（新 catalog 的第一张表），用 `0` 表示"未知"会让第一张表恰好被跳过。

`MEMORY_RECLAIM.md` §4.0 / §4.5 已同步（验收项 ③）。

### 阶段 4.1：回收目标的两个约束（已实现）

原 `reclaim_target_met` 把两个不同量纲的预算混用：拿 `mapped + unsealed` 去比 `unsealed_quota / 2`。GiB 级的映射量对一个由**另一个**配额推出的阈值，结果是"要么永远不达标、要么提前退出"。

现在两条臂各对自己的预算：

```
unsealed ledger <= unsealed_quota / 2                    ← drain 驱动
mapped + unsealed <= rss_quota * LOW_PERCENT / 100       ← 释放映射驱动
```

`rss_quota` 即 `[buffer] buffer_manager_size`；`LOW_PERCENT` 是 `memory.rs` 里既有的 70，回收通道本来的目标就是把占用压到该水位，而不是压到天花板——压到天花板会让引擎下一 tick 就面临拒写，通道等于立刻重跑。

`the_mapped_arm_stops_at_the_low_watermark` 构造一个介于 low 与 quota 之间的真实映射占用，断言判定在比例处翻转；把实现改回 `<= quota` 时该测试失败，确认它有鉴别力。

### 阶段 5：收敛与清理

| 项 | 内容 |
|---|---|
| **不删** | `BlockMapCache` 的 `entries` + `generations`。它是 `BlockId → 符号对象` 的解析与一致性层，与映射管理无关（§3.0）。`Context::block_maps` 保留，只是不再有预算语义 |
| 删除 | `shrink_block_maps`（阶段 3 已改为 `release_mapped_pages`） |
| 改造 | `secondary_lowcard` / `secondary_segment_pgm` 的 `from_mmap` → 走 `MappedFile::open`（按 inode 去重；这两个调用方自己 `Mmap::map`，拿不到 inode） |
| 改造 | `release_mapped_pages` 的遍历：阶段 1 已改为按对象快照后锁外 `madvise`，此处只做收尾核对 |
| 验收 | ① `cargo clippy --workspace --all-targets` 干净 ② 全量测试 ③ L 场景 RSS 曲线对比 |

**实施记录（阶段 5 已完成）**

- `secondary_lowcard::open_with_kind` 与 `secondary_segment_pgm` 的读取改为 `MappedFile::open_path`：原来各自 `Mmap::map`，拿不到 inode，同一 secondary chunk 被打开两次就是两个 VMA。改后按文件身份去重。
- `MappedFile::from_mmap` **删除**：两处调用点改完后它没有调用者，按 §7.13 删掉，`ObjectKey` 的文档同步改成"只有 path 映射 + 一个元数据读取失败的兜底键"。
- `release_mapped_pages` 的遍历核对通过：阶段 1 已改成"先锁内快照 `Arc`、锁外 `madvise`"，本次未再变动。
- `BlockMapCache` 的外壳**按 §3.0 保留**（`entries` + `generations`），不是删除项。

---

## 5. 验收标准

### 5.1 必须成立的不变量（每个阶段都要测）

1. **一个 inode 至多一个 VMA。** `/proc/self/maps` 里同一 `dev:ino` 只出现一次（除只读映射本身允许的多个同 inode 条目——本条要断言的是「引擎为同一文件只建一个映射」）。
2. **正在读的映射不会被撤。** `release_idle` 之后，持有 `ObjectRef` 的读者读到的字节不变（读一个校验和）。
3. **rename 之后，新路径读到新内容。** 写 → rename → 读，不能拿到旧 inode 的页。
4. **表的 `mapped_bytes` 等于该表所有对象字节之和**（含共享对象按归属分摊，见 §6 决策 2）。
5. **空闲映射数不超过上限。** 连续打开 N 个不同文件并全部释放后，空闲映射数 == `min(N, 上限)`，且 VMA 数同步下降（见 §6 决策 3）。

### 5.2 性能与内存

| 指标 | 现状 | 目标 | 测法 |
|---|---|---|---|
| 同文件第二次 `open` 的 VMA 增量 | +1 | **0** | 集成测试解析 `/proc/self/smaps`，按 inode 数段（见 §6.5） |
| 大量文件来回读后的 VMA 数 | 无界 | ≤ `max_map_count / 16` | 同上，打开 2×上限个文件后核对 |
| 重复映射导致的 RSS 虚高 | 2× | **1×** | 上一轮的 256 MiB probe 方法（`VmRSS`/`Pss` 对照） |
| 一次 L 场景的 RSS 峰值 | 待测（此前 17–20 GiB 被 earlyoom 杀） | 不高于现状，且回收后回落 | `.scratch/loadtest/` |
| 回收决策的候选集 | 只有有未落盘数据的表 | 有映射的表也进入 | 单测 + `INFINITY_PROFILE_RECLAIM` 追踪 |

---

## 6. 需要决策的点

### 决策 1：`table_id` 谁来解析？

| 方案 | 利 | 弊 |
|---|---|---|
| **A. 调用方传 `TableKey`** | MapManager 不依赖布局知识；`storage` 与 `format` 解耦 | 11 处调用点都要改；漏传一处就统计不到 |
| **B. MapManager 自己解析路径** | 调用方零改动；统计不会漏 | `storage` 需要 `format::table_of_path`，布局知识渗入映射层 |

**已定：A（调用方传 `TableKey`）。** 理由：`MapManager` 的语义是「按内容身份管理映射」，表归属是**策略**信息。若选 B，将来改目录布局要动映射层。

### 决策 2：一个对象被多张表共享时，`mapped_bytes` 怎么算？

**场景已核实存在。** persistence 模式下，`PersistentChunkStore::resolve` 把一个逻辑路径解析为「同一个对象文件 + `part_offset` / `part_size`」（`io/chunk_store.rs:256-267`）：多个 chunk 追加进同一个 object，`MappedFile::open_range` 就对**同一个 inode** 建多个窗口。若这些 chunk 属不同表，一个 `(dev, ino)` 就同时挂多张表。

| 方案 | 说明 |
|---|---|
| **A. 按引用数等分** | 各表记 `bytes / tables.len()`，合计等于实际 |
| **B. 每张表记全额** | 简单，但合计会超过实际（RRF 里会放大共享对象的权重） |
| **C. 只记给第一个引用者** | 合计正确，但归属不稳定 |

**已定：A（按引用数等分）。** 在 `ObjectEntry.tables: SmallVec<(TableKey, u32)>` 里记每个表对该对象的引用计数，`mapped_bytes` 求和时按引用数分摊：`bytes * mine / total`。

> 补充：**去重本身在 persistence 模式下收益最大**。一个 object 里 N 个 chunk 现在是 N 个 VMA（每次 `open_range` 都 `Mmap::map`，`mapped.rs:329`）；按 inode 去重后是 1 个，N 个窗口都指向它。

### 决策 3：读者归零的映射，立即 unmap 还是留给压力回收？

**这一条在实施中被实测推翻过一次，保留下推翻的理由。**

三个选项：

| 方案 | 问题 |
|---|---|
| **A. 引用归零就 unmap** | 每次开文件重建页表；`munmap` 在多核下触发 TLB shootdown；「unmap 后立刻又读」的抖动正好落在 seek 型扫描上 |
| **B. 只等内存压力回收** | VMA 无界增长 → 触及 `vm.max_map_count` → `mmap` 返回 `ENOMEM`，**查询报错**而非变慢 |
| **C. 归零后保留，但有界；超出上界按最久空闲回收** | 兼具两者，代价是需要一个空闲队列 |

**实测（本机，`/proc/sys/vm/max_map_count = 262144`，页 4 KiB）**

纯系统调用 vs 带缺页：

```
8 MiB (2048 页)                        64 MiB (16384 页)
  mmap+munmap only            4.96 us      7.51 us
  warm mapping, full re-read 115.81 us    917.54 us
  mmap+full fault+munmap     107.25 us    992.27 us
```

解读：第 2、3 行几乎相等 —— **软缺页本身几乎不耗时**（页仍在 page cache，只是重建页表，且与读循环重叠）。所以 A 的代价主要是**每开一次文件一次的系统调用**（5–7.5 µs），单看不大；但多核下 `munmap` 的 TLB shootdown 未计入这份单线程数据。

`munmap` 不丢 page cache（实测 `Cached` 前后 delta = 0，映射中 `RssFile` 73528 KiB，unmap 后回落 7992 KiB）：

```
RssFile while mapped 73528 KiB; Cached before munmap 22738364 after 22738364 KiB (delta 0)
```

VMA 数量是**硬上限**：

```
200 idle mappings of one file: VMAs for inode = 200
```

一张 20 列 × 1000 块的表约 2 万–4 万个 VMA；几张这样的表就碰到 262144。触顶后是错误，不是降级。

**已定：C。** 空闲上限 `IDLE_MAPPING_LIMIT = 16384`（约默认 map count 的 6%）。归零时入空闲队列，超限时从队首 unmap；`acquire` 命中时把它移出队列。

### 决策 3b：`vm.max_map_count` 的来源

系统值不保证够用（默认 262144，但容器/嵌入式可能更小；ES 文档建议 1048576）。因此：

1. **Linux 上启动时读 `/proc/sys/vm/max_map_count`**，据它推导空闲上限，而不是硬编码 16384：
   `limit = clamp(max_map_count / 16, 1024, 65536)`。除以 16 留出工作集与共享库的余量；上下夹紧避免「系统值极小 → 上限 0」和「系统值极大 → 上限膨胀」。
2. **给出建议值并在低于门槛时告警**：`max_map_count < 262144` 时在启动日志里提示「建议 1048576（ES 同款）」。不改系统设置——那是运维的事，进程无权也不应去写 `/proc/sys`。
3. **非 Linux 用默认 16384**，并在文档里注明该平台没有等价的内核上限（macOS 的 VMA 受地址空间限制，Windows 无此计数）。
4. **诊断可见**：把「当前 max_map_count / 空闲上限 / 空闲映射数 / 因超限回收的次数」放进 `SHOW BUFFERS` 或现有的内存诊断输出，压测时可核对。

### 决策 4：age 臂用哪个时间？

用户要求「最后访问时间（read/write txn commit_id）」。

- **read 的 commit_id 怎么取值**：一个 `SELECT` 有自己的 `read_ts`（快照读），不是 commit。取该 `read_ts` 即可，语义是「这张表在这个快照下被读过」。
- **要不要区分读与写**：写已经体现在 `append_ts`，读是新信息。建议 `last_access = max(last_append_ts, last_read_ts)`，两者都是引擎内的单调 ts，可与 `now_ts` 直接相减。

**`current_ts` 推进的时机已核对**（`crates/txn/src/lib.rs`）：

- 只在**提交的底半完成**时推进（`:843` 的 `fetch_max(front)`），或 WAL 重放时（`:1504`）。
- 但**后台任务也占 commit ts**：checkpoint 取 `prepare_commit_ts.fetch_add(2, …)`（`:1612`），cleanup 同样（`:1671`）。所以一个只有读、没有写的库，`current_ts` 仍会随每 10 秒的 checkpoint 前进。

由此两点设计约束：

1. **age 用 ts 差是可行的**——`now_ts` 在 checkpoint 推动下会前进，闲置表的 age 会增长。
2. **但不能假设「age 增长意味着这张表被写过」**。age 是相对量，RRF 只用它的**序**（`maintain.rs:59-105` 只排序、不做绝对判断），所以 checkpoint 推进 ts 不会破坏排序语义，只是整体一起前移。**这一点必须在阶段 4 用测试固定**：构造「只有 checkpoint、没有数据写入」的时间线，确认年龄序不变。
3. **`last_access_ts` 记录 `current_ts()`（已提交时钟），不是事务自己的 commit ts，也不是语句的 `read_ts`。**
   - 事务自己的 commit ts **不可用**：提交前未发布，回滚时永不存在，把它写进一个供回收排序的字段就是写入了无人能比较的时间。
   - 语句的 `read_ts` 是 `current_ts() + 1`，读多个快照会来回跳，且不是单调的。
   - `current_ts()` 是「本进程已完成的最近一次提交」，正是 `now_ts - last_access` 所对照的时钟，且单调。

   **存储位置**：`TableEntry::last_access_ts`，不是 `MapManager` 里的全局表。理由有三：回收本来就在遍历 `self.tables`，属性放在被遍历的对象上最直接；进程级全局表会让不同引擎实例（测试里同时存在多个）互相串号；表被 DROP 时随 `TableEntry` 一起消失，不用额外清理。

---

## 6.5 测量手段：用 `/proc/self/smaps` 做 per-VMA 断言

前几轮我用的 `VmRSS` / `RssFile` / `Pss`（`/proc/self/status` + `smaps_rollup`）只能看到**进程级聚合**。这不足以证明「一个 inode 只有一个 VMA」——聚合数字里，一个 inode 的两次映射和两个 inode 各一次映射长得一样。

`/proc/self/smaps` 给出每个 VMA 一段，段头带 `start-end perms offset dev inode pathname`，段内带 `Rss`/`Pss`/`Private_Clean`/`Shared_Clean` 等。用它可以直接断言：

```
# 同一个 inode 出现的段数（目标：1）
grep -E '^[0-9a-f]+-[0-9a-f]+ .* [0-9]{3}:[0-9]{2} ([0-9]+) ' /proc/self/smaps | awk '{print $5}' | sort | uniq -c | sort -rn | head
# 单个 VMA 的驻留字节（目标：与文件的 maps 段数 x 段内 Rss 对得上）
awk '/^[0-9a-f]+-[0-9a-f]+/ {seg=$0; rss=0} /^Rss:/ {rss+=$2; if (seg!="") {print rss*1024, seg; seg=""}}' /proc/self/smaps
```

用途分两层：

| 层 | 断言 | 用在 |
|---|---|---|
| 结构 | 引擎为某文件创建的段数 == 1 | 阶段 1 的验收 |
| 字节 | 每段的 `Rss` 之和 == 该表 `mapped_bytes` 里实际驻留的部分 | 阶段 2 / 阶段 4 的账实相符 |

**注意两点口径**（前几轮已踩过）：

1. `Rss` 在 smaps 里也是**按映射计数**的：同一页被两个 VMA 映射，两个段各自计入，而 `Pss` 会均摊。所以「去重成功」的直接证据是**段数**，不是段内 `Rss` 之和。
2. 匿名映射（堆、栈）也会出现在 smaps 里，统计必须按 `pathname` 过滤到引擎根目录下，否则会把 `[heap]` 算进来。

因此新增的测试辅助函数放在 `storage/tests/` 里：`smaps_of_pid()` 解析成 `Vec<Vma { start, end, inode, path, rss, pss }>`，断言写在这个结构上——纯文本解析，不依赖 `procfs` crate。

---

## 7. 风险与守则

| 风险 | 影响 | 应对 |
|---|---|---|
| 有代码就地 `ftruncate`/`pwrite` 数据文件 | 映射 SIGBUS；`(dev,ino)` 键失效 | CI 增加一条检查：`storage` 内除 `format::write_parts` 外不得对数据文件 `OpenOptions::write`；并保留 `write_parts` 的注释断言 |
| 阶段 3 破坏 generation 语义 | 重写后读到旧块（曾有该缺陷，`state.rs:6596-6602`） | 阶段 3 明确保留 `generations`；`index_combination_soak.rs` 必须跑 |
| 表归属解析漏掉某类文件 | 该表回收时看不到自己的映射，RSS 不降 | 阶段 2 的验收第 ①③ 项覆盖；另加「`by_object` 里无归属对象的数量」诊断计数器，生产可见 |
| 回收撤销正在使用的映射 | 查询读到 SIGSEGV 或读到被换掉的内容 | `handles` 计数 + §5.1 不变量 2；`release_idle` 只碰 `handles == 0` |
| 单件锁成为热点 | 每块每次读都过一把全局锁 | 阶段 1 用 `RwLock`，读走共享；命中路径只做一次 hash 查询。若实测有热点（`mem_bench` 并发用例，`examples/mem_bench.rs:220` 已为此设），改为分片锁（按 `table_id` 取模） |
| 与 cgroup 记账口径不一致 | `memory.current` 含 page cache，`VmRSS` 不含 | 已在 `memory.rs:469-484` 记录；本次不改口径，但回收量计算必须用同一口径（见 `reclaim.rs` 现有注释） |
| 触及 `vm.max_map_count` | `mmap` 返回 `ENOMEM`，**查询报错**而非变慢 | 空闲上限由该值推导（§6 决策 3b）；启动时低于 262144 告警；诊断输出里可见当前值与空闲映射数 |
| 空闲上限设得过大 | VMA 逼近上限，错误在压力下才暴露 | 上限取 `max_map_count / 16` 并夹在 `[1024, 65536]`；`release` 里做的是「入队即检查」，不依赖回收时机 |
| 空闲上限设得过小 | 退化成立即 unmap（决策 3 方案 A 的代价） | 下限 1024；`INFINITY_PROFILE_RECLAIM` 里输出「超限回收次数」，压测时可发现抖动 |

---

## 8. 工作量与顺序

| 阶段 | 主要文件 | 依赖 |
|---|---|---|
| 1 | `storage/src/buffer/mapped.rs`（+ 新 `map_manager.rs`） | — |
| 2 | `storage/src/format.rs`、`map_manager.rs` | 1 |
| 3 | `executor/src/engine/state.rs`、`blocks/engine.rs`、`dml.rs`、`ddl.rs` | 2 |
| 4 | `executor/src/engine/reclaim.rs`、`maintain.rs`、`memory.rs` | 3 |
| 5 | 清理 | 4 |

每阶段结束：`cargo check --workspace --all-targets`、`cargo test --workspace`、`cargo clippy --workspace --all-targets`、`cargo fmt --all`，并起服务跑一次查询（规则见 `AGENTS.md` §7.12）。

---

## 9. 对外可见面（已确认，见 §3.2）

三项已定：`buffer_used_bytes()` 删除（`buffer_pool_usage` 在该仓库不存在）；`BufferedObjectCount` 保留并改数 `mapping_count()`，数值变大已接受；`buffer_objects()` / `SHOW OBJECT` 仍只列块。

`statements.rs:12722-12726` 的注释「index chunks are mapped too, but they belong to the index structures, not to this pool」随 `BufferedObjectCount` 的语义变更一并改写。

---

## 10. 待办

决策项已全部确定（§6 决策 1–4、§3.2 对外可见面）。已核对完成的条目见 §11。

---

## 11. 已核对的事实（不必再查）

| 事实 | 结论 |
|---|---|
| 数据文件是否就地编辑 | 否，全部 temp + rename（`format::write_parts`），故 `(dev, ino)` 是稳定身份 |
| 索引 chunk 是否复用 id | 否，`take_next_chunk_id` 单调分配 |
| `current_ts` 何时推进 | 提交底半完成时（`txn/src/lib.rs:843`）；checkpoint/cleanup 也各占一个 commit ts |
| persistence 下是否跨表共享对象 | 是，`PersistentChunkStore::resolve` 把多个 part 解析到同一 blob，故需按引用数等分记账 |
| `version` 的 `deleted` 段对齐 | 4 字节对齐、从不 8 字节对齐（`creates = 1..6` 实测），用 `pod_read_unaligned` |
| 一个 `BlockId` 是否可能对应两个 inode | 是（重写期间新旧并存），故 `generations` 必须保留 |

---

## 11.5 回收计划的可观测性（已实现）

回收决策原本只体现在 RSS 曲线上——"进程掉了 2 GiB"说不清是哪个杠杆做的，而 `RssAnon`（分配器堆）与 `RssFile`（映射页缓存）因完全不同的原因变化。

现在每一轮在**确定候选集之后**打一条 info，记录三组数字：

```
info: reclaim plan: N table(s) hold unsealed data or mappings, M selected (round cap C);
     now VmRSS=.. MiB (Anon=.. File=.. Shmem=..)  unsealed=.. mapped=..;
     selected unsealed=.. mapped=..; projected <same split> minus >= ..
info: reclaim plan:   db.table unsealed=.. mapped=.. age=.. ts id=..
info: reclaim plan: goals unsealed<=.. (quota/2) and mapped+unsealed<=.. (rss_quota*70%)
info: reclaim result: K table(s) dumped; after VmRSS=.. (Anon=.. File=.. Shmem=..)  unsealed=.. mapped=..
```

- **现在的 RSS 分项**是"因"，**selected 的 unsealed/mapped** 是"计划"，**result 的分项**是"果"。三者对齐才能判断某个杠杆是否真的生效。
- 每张被选中的表单独一行，`unsealed` 与 `mapped` **分开**：dump 释放前者，丢映射释放后者，混在一起就无法归因。
- 目标阈值也打印，读日志不必回查配置。
- `projected ... minus >= X` 是**释放量的上界**：映射释放对空闲表是无条件的，而 unsealed 只有在表的所有行都能在本轮 bound 前提交时才兑现；staging import 会留尾巴。

### 跨平台的 RSS 分项

`storage::RssSplit` + `storage::process_rss_split()`，与 `page_size` / `physical_memory_bytes` 放在一起（平台探针都归 storage）：

| 平台 | 来源 | 分项 |
|---|---|---|
| Linux | `/proc/self/status` 的 `VmRSS`/`RssAnon`/`RssFile`/`RssShmem` | 全部真实 |
| macOS | `task_info(TASK_BASIC_INFO)` 的 `resident_size` | **只有总数**（分项要 `TASK_VM_INFO` 的 ledger 字段，非稳定 ABI） |
| Windows | `GetProcessMemoryInfo` 的 `WorkingSetSize` | **只有总数**（`working set = private + shareable` 未暴露） |

`has_split` 是一个**独立的布尔**而不是"三个字段都为零"：Linux 上无共享内存的进程 `RssShmem` 也是 0，靠值区分不了"没有"和"不报"。没有分项时 `Display` 打印 `(no split on this platform)`，不会打出 `Anon=0 File=0` 这种像测量结果的假数据。

Linux 上 `the_resident_split_adds_up_where_the_platform_has_one` 断言 `Anon + File + Shmem == VmRSS`——内核恒等式，这行日志的可用性就建立在这上面。

## 12. 实施完成

五个阶段全部落地，提交依次为：

| 阶段 | 提交 |
|---|---|
| 1 | `storage: key the mapping table by file identity` |
| 2 | `memory: attribute mappings and accesses to tables` |
| 2.5 | `storage: read a block's version file out of the mapping` |
| 2.6 | `storage: read a version file's create records out of the mapping too` |
| 3 | `memory: the block map holds symbols, not mappings` |
| 4 | `memory: rank a table by what it maps, not only by what it has not flushed` |
| 5 | `storage: map secondary chunks through the identity table too` |

### 验收核对（§5）

| # | 不变量 | 测试 |
|---|---|---|
| 1 | 一个 inode 至多一个 VMA | `a_second_open_reuses_the_mapping`（解析 `/proc/self/smaps` 按 inode 数段） |
| 2 | 正在读的映射不会被撤 | `unmap_idle_gives_the_address_space_back`、`unmap_idle_of_a_table_leaves_the_other_alone` |
| 3 | rename 后新路径读到新内容 | `a_replaced_file_gets_a_fresh_mapping` |
| 4 | `mapped_bytes` 等于对象字节之和 | `a_mapping_is_charged_to_its_table`、`a_shared_object_is_split_between_tables` |
| 5 | 空闲映射数不超过上限 | `the_idle_bound_follows_the_kernel_limit`、`idle_mappings_are_capped` |

性能项：`heap_bytes() == 0`（`a_sealed_block_is_mapped_once_and_shared`、`a_mapped_version_answers_like_an_owned_one`）；「只有映射的表也进候选集」（`a_table_that_only_holds_mappings_is_still_a_candidate`）。

### 未做

**L 场景 RSS 曲线**（§5.2 最后两行）没有重跑。此前几次都在 17–20 GiB 被 earlyoom 杀掉，且本轮的改动需要真实负载才能看出差别。这是唯一遗留的验收项，不是遗漏——它需要一次独立的压测窗口。

### 实施中发现的、计划里没写对的三处

1. **符号缓存让映射永不空闲**（阶段 3）：`BlockMapCache` 持有 `Arc<MappedBlock>` → `MappedFile`，`handles` 恒大于 0，回收一个字节也撤不掉。回收必须先丢符号。
2. **`reclaim_target_met` 只看未落盘字节**（阶段 4）：一张行全落盘、映射很大的表会让 `0 <= target` 立即成立，整轮提前退出。
3. **`table_id = 0` 当哨兵**（阶段 4）：`0` 是真实 id（新 catalog 的第一张表），用 `0` 表示"未知"会让那张表恰好跳过自己的映射。

三处都已修正并写入各阶段的实施记录。
