// Copyright(C) 2023 InfiniFlow, Inc. All rights reserved.
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

module;

export module txn_store;

import stl;

import status;
import internal_types;
import index_base;
import extra_ddl_info;

namespace infinity {

class Txn;
struct Catalog;
struct DBEntry;
struct TableIndexEntry;
struct TableEntry;
struct SegmentEntry;
struct BlockEntry;
struct BlockColumnEntry;
struct DataBlock;
struct SegmentIndexEntry;
struct ChunkIndexEntry;
class BGTaskProcessor;
class TxnManager;
enum class CompactStatementType;
class CatalogDeltaEntry;
class BufferManager;
struct WalSegmentInfo;

struct AppendState;
struct DeleteState;

export struct TxnSegmentStore {
public:
    static TxnSegmentStore AddSegmentStore(SegmentEntry *segment_entry);

    explicit TxnSegmentStore(SegmentEntry *segment_entry);

    TxnSegmentStore() = default;

    void AddDeltaOp(CatalogDeltaEntry *local_delta_ops, AppendState *append_state, Txn *txn, bool set_sealed) const;

public:
    SegmentEntry *const segment_entry_ = nullptr;
    HashMap<BlockID, BlockEntry *> block_entries_;
    Vector<BlockColumnEntry *> block_column_entries_;
};

export struct TxnIndexStore {
public:
    explicit TxnIndexStore(TableIndexEntry *table_index_entry);
    TxnIndexStore() = default;

    void AddDeltaOp(CatalogDeltaEntry *local_delta_ops, TxnTimeStamp commit_ts) const;

    void Commit(TransactionID txn_id, TxnTimeStamp commit_ts);

    void Rollback(TxnTimeStamp abort_ts);

    void AddSegmentOptimizing(SegmentIndexEntry *segment_index_entry);

    bool TryRevert();

public:
    TableIndexEntry *const table_index_entry_{};

    HashMap<SegmentID, SegmentIndexEntry *> index_entry_map_{};
    HashMap<String, ChunkIndexEntry *> chunk_index_entries_{};

    Vector<Tuple<SegmentIndexEntry *, ChunkIndexEntry *, Vector<ChunkIndexEntry *>>> optimize_data_;

    enum struct TxnStoreStatus {
        kNone,
        kOptimizing,
    };
    TxnStoreStatus status_{TxnStoreStatus::kNone};
};

export struct TxnCompactStore {
    Vector<Pair<TxnSegmentStore, Vector<SegmentEntry *>>> compact_data_;

    CompactStatementType type_;

    TxnCompactStore();
    TxnCompactStore(CompactStatementType type);
};

export class TxnTableStore {
public:
    TxnTableStore(Txn *txn, TableEntry *table_entry);

    ~TxnTableStore();

    Tuple<UniquePtr<String>, Status> Import(SharedPtr<SegmentEntry> segment_entry, Txn *txn);

    Tuple<UniquePtr<String>, Status> Append(const SharedPtr<DataBlock> &input_block);

    void AddIndexStore(TableIndexEntry *table_index_entry);

    void AddSegmentIndexesStore(TableIndexEntry *table_index_entry, const Vector<SegmentIndexEntry *> &segment_index_entries);

    void AddChunkIndexStore(TableIndexEntry *table_index_entry, ChunkIndexEntry *chunk_index_entry);

    TxnIndexStore *GetIndexStore(TableIndexEntry *table_index_entry, bool need_lock);

    void DropIndexStore(TableIndexEntry *table_index_entry);

    Tuple<UniquePtr<String>, Status> Delete(const Vector<RowID> &row_ids);

    void SetCompactType(CompactStatementType type);

    Tuple<UniquePtr<String>, Status> Compact(Vector<Pair<SharedPtr<SegmentEntry>, Vector<SegmentEntry *>>> &&segment_data, CompactStatementType type);

    void AddSegmentStore(SegmentEntry *segment_entry);

    void AddBlockStore(SegmentEntry *segment_entry, BlockEntry *block_entry);

    void AddBlockColumnStore(SegmentEntry *segment_entry, BlockEntry *block_entry, BlockColumnEntry *block_column_entry);

    void AddSealedSegment(SegmentEntry *segment_entry);

    void AddDeltaOp(CatalogDeltaEntry *local_delta_ops, TxnManager *txn_mgr, TxnTimeStamp commit_ts, bool added) const;

public:
    // transaction related

    void Rollback(TransactionID txn_id, TxnTimeStamp abort_ts);

    bool CheckConflict(Catalog *catalog, Txn *txn) const;

    Optional<String> CheckConflict(const TxnTableStore *txn_table_store) const;

    void PrepareCommit1(const Vector<WalSegmentInfo *> &segment_infos) const;

    void PrepareCommit(TransactionID txn_id, TxnTimeStamp commit_ts, BufferManager *buffer_mgr);

    void Commit(TransactionID txn_id, TxnTimeStamp commit_ts);

    void MaintainCompactionAlg();

public: // Setter, Getter
    Pair<std::shared_lock<std::shared_mutex>, const HashMap<String, UniquePtr<TxnIndexStore>> &> txn_indexes_store() const;

    const HashMap<SegmentID, TxnSegmentStore> &txn_segments() const { return txn_segments_store_; }

    const Vector<SegmentEntry *> &flushed_segments() const { return flushed_segments_; }

    Txn *GetTxn() const { return txn_; }

    TableEntry *GetTableEntry() const { return table_entry_; }

    inline bool HasUpdate() const { return has_update_; }

    DeleteState &GetDeleteStateRef();

    DeleteState *GetDeleteStatePtr();

    inline const Vector<SharedPtr<DataBlock>> &GetBlocks() const { return blocks_; }

    void SetAppendState(UniquePtr<AppendState> append_state);

    inline AppendState *GetAppendState() const { return append_state_.get(); }

    void AddWriteTxnNum() { added_txn_num_ = true; }

    bool AddedTxnNum() const { return added_txn_num_; }

private:
    mutable std::shared_mutex txn_table_store_mtx_{};

    HashMap<SegmentID, TxnSegmentStore> txn_segments_store_{};
    Vector<SegmentEntry *> flushed_segments_{};
    HashSet<SegmentEntry *> set_sealed_segments_{};

    int ptr_seq_n_;
    HashMap<TableIndexEntry *, int> txn_indexes_{};
    HashMap<String, UniquePtr<TxnIndexStore>> txn_indexes_store_{};

    TxnCompactStore compact_state_;

    Txn *const txn_{};
    Vector<SharedPtr<DataBlock>> blocks_{};

    UniquePtr<AppendState> append_state_{};
    UniquePtr<DeleteState> delete_state_{};

    SizeT current_block_id_{0};

    TableEntry *table_entry_{};
    bool added_txn_num_{false};

    bool has_update_{false};

public:
    void SetCompacting() { table_status_ = TxnStoreStatus::kCompacting; }

    void SetCreatingIndex() { table_status_ = TxnStoreStatus::kCreatingIndex; }

    void TryRevert();

private:
    enum struct TxnStoreStatus {
        kNone = 0,
        kCreatingIndex,
        kCompacting,
    };
    TxnStoreStatus table_status_{TxnStoreStatus::kNone};
};

export class TxnStore {
public:
    explicit TxnStore(Txn *txn);

    void AddDBStore(DBEntry *db_entry);

    void DropDBStore(DBEntry *dropped_db_entry);

    void AddTableStore(TableEntry *table_entry);

    void DropTableStore(TableEntry *dropped_table_entry);

    TxnTableStore *GetTxnTableStore(TableEntry *table_entry);

    TxnTableStore *GetExistTxnTableStore(TableEntry *table_entry) const;

    void AddDeltaOp(CatalogDeltaEntry *local_delta_opsm, TxnManager *txn_mgr) const;

    void MaintainCompactionAlg() const;

    bool CheckConflict(Catalog *catalog);

    Optional<String> CheckConflict(const TxnStore &txn_store);

    void PrepareCommit1();

    void PrepareCommit(TransactionID txn_id, TxnTimeStamp commit_ts, BufferManager *buffer_mgr);

    void CommitBottom(TransactionID txn_id, TxnTimeStamp commit_ts);

    void Rollback(TransactionID txn_id, TxnTimeStamp abort_ts);

    bool ReadOnly() const;

    std::mutex mtx_{};

    void RevertTableStatus();

    void SetCompacting(TableEntry *table_entry);

    void SetCreatingIndex(TableEntry *table_entry);

    void AddSemaphore(UniquePtr<std::binary_semaphore> sema) { semas_.push_back(std::move(sema)); }

    const Vector<UniquePtr<std::binary_semaphore>> &semas() const { return semas_; }

private:
    // Txn store
    Txn *txn_{}; // TODO: remove this
    int ptr_seq_n_{};
    HashMap<DBEntry *, int> txn_dbs_{};
    HashMap<TableEntry *, int> txn_tables_{};
    // Key: table name Value: TxnTableStore
    HashMap<String, UniquePtr<TxnTableStore>> txn_tables_store_{};

    Vector<UniquePtr<std::binary_semaphore>> semas_{};
};

} // namespace infinity
