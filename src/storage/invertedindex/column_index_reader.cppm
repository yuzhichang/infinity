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

export module column_index_reader;

import stl;
import third_party;
import segment_posting;
import index_segment_reader;
import posting_iterator;
import index_defines;
// import memory_indexer;
import internal_types;
import logger;
import status;
import default_values;

namespace infinity {
class TermDocIterator;
class NewTxn;
class MemoryIndexer;
class TableIndexMeeta;
class BufferObj;
struct SegmentIndexFtInfo;

struct ColumnReaderChunkInfo {
    BufferObj *index_buffer_ = nullptr;
    RowID base_rowid_{};
    u32 row_count_{};

    SegmentID segment_id_ = 0;
    ChunkID chunk_id_ = 0;
};

export class ColumnIndexReader {
public:
    ~ColumnIndexReader();

    Status Open(optionflag_t flag, TableIndexMeeta &table_index_meta);

    UniquePtr<PostingIterator> Lookup(const String &term, bool fetch_position = true);

    Pair<u64, float> GetTotalDfAndAvgColumnLength();

    optionflag_t GetOptionFlag() const { return flag_; }

    void InvalidateSegment(SegmentID segment_id);

    void InvalidateChunk(SegmentID segment_id, ChunkID chunk_id);

    inline String GetAnalyzer() const { return analyzer_; }

    inline String GetColumnName() const { return column_name_; }

private:
    std::mutex mutex_;

    optionflag_t flag_;
    Vector<SharedPtr<IndexSegmentReader>> segment_readers_;
    Map<SegmentID, SharedPtr<SegmentIndexFtInfo>> segment_index_ft_infos_;

    u64 total_df_ = 0;
    float avg_column_length_ = 0.0f;

public:
    String column_name_;
    String analyzer_;
    // for loading column length files
    String index_dir_;
    SharedPtr<MemoryIndexer> memory_indexer_{nullptr};

    Vector<ColumnReaderChunkInfo> chunk_index_meta_infos_;
};

namespace detail {
template <class T>
struct Hash {
    inline SizeT operator()(const T &val) const { return val; }
};
} // namespace detail

export struct IndexReader {
    // Get a index reader on a column based on hints.
    // If no such column exists, return nullptr.
    // If column exists, but no index with a hint name is found, return a random one.
    ColumnIndexReader *GetColumnIndexReader(u64 column_id, const Vector<String> &hints) const {
        const auto &column_index_map = column_index_readers_->find(column_id);
        // if no fulltext index exists, or the map is empty.
        if (column_index_map == column_index_readers_->end() || column_index_map->second->size() == 0) {
            return nullptr;
        }

        auto indices_map = column_index_map->second;
        for (SizeT i = 0; i < hints.size(); i++) {
            if (auto it = indices_map->find(hints[i]); it != indices_map->end()) {
                return indices_map->at(hints[i]).get();
            }
        }
        return indices_map->begin()->second.get();
    }

    // return map: column_name -> analyzer_name based on hints.
    Map<String, String> GetColumn2Analyzer(const Vector<String> &hints) const {
        Map<String, String> rst;
        for (const auto &id_index_map : *column_index_readers_) {
            ColumnIndexReader *column_index_reader = GetColumnIndexReader(id_index_map.first, hints);
            if (column_index_reader != nullptr) {
                rst[column_index_reader->GetColumnName()] = column_index_reader->GetAnalyzer();
            }
        }
        return rst;
    }

    // column_id -> [index_name -> column_index_reader]
    SharedPtr<FlatHashMap<u64, SharedPtr<Map<String, SharedPtr<ColumnIndexReader>>>, detail::Hash<u64>>> column_index_readers_;
};

export class TableIndexReaderCache {
public:
    TableIndexReaderCache(String db_id_str, String table_id_str) : db_id_str_(db_id_str), table_id_str_(table_id_str) {}

    SharedPtr<IndexReader> GetIndexReader(NewTxn *txn);

    // User shall call this function only once when all transactions using `GetIndexReader()` have finished.
    void Invalidate();

private:
    std::mutex mutex_;
    String db_id_str_;
    String table_id_str_;

    TxnTimeStamp cache_ts_ = UNCOMMIT_TS;
    SharedPtr<FlatHashMap<u64, SharedPtr<Map<String, SharedPtr<ColumnIndexReader>>>, detail::Hash<u64>>> cache_column_readers_;
};

} // namespace infinity
