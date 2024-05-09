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

#include <cassert>

module blockmax_wand_iterator;
import stl;
import third_party;
import index_defines;
import early_terminate_iterator;
import internal_types;
import logger;

namespace infinity {

BlockMaxWandIterator::~BlockMaxWandIterator() {
    String msg = "BlockMaxWandIterator pivot_history: ";
    SizeT num_history = pivot_history_.size();
    for (SizeT i=0; i<num_history; i++) {
        auto &p = pivot_history_[i];
        u32 pivot = std::get<0>(p);
        u64 row_id = std::get<1>(p);
        float score = std::get<2>(p);
        //oss << " (" << pivot << ", " << row_id << ", " << score << ")";
        msg += fmt::format(" ({}, {}, {:6f})", pivot, row_id, score);
    }
    LOG_INFO(msg);
}

BlockMaxWandIterator::BlockMaxWandIterator(Vector<UniquePtr<EarlyTerminateIterator>> iterators) : sorted_iterators_(std::move(iterators)), pivot_(sorted_iterators_.size()) {
    backup_iterators_.reserve(sorted_iterators_.size());
    bm25_score_upper_bound_ = 0.0f;
    SizeT num_iterators = sorted_iterators_.size();
    for (SizeT i = 0; i < num_iterators; i++){
        bm25_score_upper_bound_ += sorted_iterators_[i]->BM25ScoreUpperBound();
    }
}

void BlockMaxWandIterator::UpdateScoreThreshold(const float threshold) {
    EarlyTerminateIterator::UpdateScoreThreshold(threshold);
    const float base_threshold = threshold - BM25ScoreUpperBound();
    SizeT num_iterators = sorted_iterators_.size();
    for (SizeT i = 0; i < num_iterators; i++){
        auto &it = sorted_iterators_[i];
        float new_threshold = base_threshold + it->BM25ScoreUpperBound();
        if (new_threshold > 0.0f){
            it->UpdateScoreThreshold(new_threshold);
        }
    }
}

bool BlockMaxWandIterator::NextShallow(RowID doc_id){
    assert(doc_id != INVALID_ROWID);
    assert(backup_iterators_.empty());
    RowID common_block_last_doc_id = INVALID_ROWID;
    SizeT num_iterators = sorted_iterators_.size();
    for (SizeT i = 0; i < num_iterators; i++){
        auto &it = sorted_iterators_[i];
        bool ok = it->NextShallow(doc_id);
        if (ok) {
            if (common_block_last_doc_id > it->BlockLastDocID())
                common_block_last_doc_id = it->BlockLastDocID();
            backup_iterators_.push_back(std::move(it));
        }
    }
    if (backup_iterators_.empty()){
        common_block_min_possible_doc_id_ = INVALID_ROWID;
        common_block_last_doc_id_ = INVALID_ROWID;
        return false;
    } else {
        sorted_iterators_ = std::move(backup_iterators_);
        common_block_min_possible_doc_id_ = doc_id;
        common_block_last_doc_id_ = common_block_last_doc_id;
        return true;
    }
}

bool BlockMaxWandIterator::Next(RowID doc_id){
    assert(doc_id != INVALID_ROWID);
    bm25_score_cached_ = false;
    SizeT num_iterators = sorted_iterators_.size();
    if (doc_id_ == INVALID_ROWID) {
        // Initialize children once.
        for (SizeT i = 0; i < num_iterators; i++) {
            sorted_iterators_[i]->Next(0);
        }
    } else {
        assert(pivot_ < num_iterators);
        // Move all pointers from lists[0] to lists[p] by calling Next(list, d + 1)
        for (SizeT i = 0; i <= pivot_; i++) {
            sorted_iterators_[i]->Next(doc_id_ + 1);
        }
    }
    float sum_score = 0.0f;
    while(1){
        // sort the lists by current docIDs
        std::sort(sorted_iterators_.begin(), sorted_iterators_.end(), [](const auto &a, const auto &b) {
            return a->DocID() < b->DocID();
        });
        // remove exhausted lists
        for (int i = int(num_iterators) - 1; i >= 0 && sorted_iterators_[i]->DocID() == INVALID_ROWID; i--) {
            sorted_iterators_.pop_back();
            num_iterators --;
        }
        if (num_iterators == 0) {
            doc_id_ = INVALID_ROWID;
            return false;
        }

        // same "pivoting" as in WAND using the max impact for the whole lists, use p to denote the pivot
        SizeT pivot = 0;
        sum_score = 0.0f;
        for (pivot = 0; pivot < num_iterators; pivot++) {
            sum_score += sorted_iterators_[pivot]->BM25ScoreUpperBound();
            if (sum_score > threshold_) {
                break;
            }
        }
        if (pivot >= num_iterators){
            doc_id_ = INVALID_ROWID;
            return false;
        }
        RowID d = sorted_iterators_[pivot]->DocID();
        for(; pivot < num_iterators && sorted_iterators_[pivot]->DocID() == d; pivot++){
        }

        for(SizeT i=0; i+1<pivot; i++){
            sorted_iterators_[i]->NextShallow(d);
        }
        sum_score = 0.0f;
        for (SizeT i = 0; i <= pivot; i++) {
            sum_score += sorted_iterators_[i]->BlockMaxBM25Score();
        }
        if (sum_score > threshold_) {
            if (sorted_iterators_[0]->DocID() == d) {
                // EvaluatePartial(d , p);
                sum_score = 0.0f;
                for(SizeT i=0; i<pivot; i++){
                    sum_score += sorted_iterators_[i]->BM25Score();
                }
                if(sum_score > threshold_){
                    pivot_ = pivot;
                    doc_id_ = d;
                    bm25_score_cache_ = sum_score;
                    bm25_score_cached_ = true;
                    pivot_history_.emplace_back(pivot_, doc_id_.ToUint64(), sum_score);
                    return true;
                }
            }
            // Choose one list from the lists before lists[p] with the largest IDF, move it by calling Next(list, d + 1)
            // SizeT largest_idf = 0;
            // SizeT largest_idf_idx = 0;
            // for (SizeT i = 0; i + 1 < pivot; i++) {
            //     SizeT idf = sorted_iterators_[i]->DocFreq();
            //     if (idf > largest_idf) {
            //         largest_idf = idf;
            //         largest_idf_idx = i;
            //     }
            // }
            // sorted_iterators_[largest_idf_idx]->Next(d + 1);
            for (SizeT i = 0; i <= pivot_; i++) {
                sorted_iterators_[i]->Next(d+1);
            }
        } else {
            // d′ = GetNewCandidate();
            // Choose one list from the lists before and including lists[p] with the largest IDF, move it by calling Next(list, d′)
            RowID new_candidate = INVALID_ROWID;
            if (pivot + 1 < num_iterators)
                new_candidate = sorted_iterators_[pivot + 1]->DocID();
            for (SizeT i = 0; i < pivot; i++) {
                new_candidate = std::min(new_candidate, sorted_iterators_[i]->BlockLastDocID() + 1);
            }
            SizeT largest_idf = 0;
            SizeT largest_idf_idx = 0;
            for (SizeT i = 0; i < pivot; i++) {
                SizeT idf = sorted_iterators_[i]->DocFreq();
                if (idf > largest_idf) {
                    largest_idf = idf;
                    largest_idf_idx = i;
                }
            }
            sorted_iterators_[largest_idf_idx]->Next(new_candidate + 1);
        }
    }
    return false;
}

// inherited from EarlyTerminateIterator
bool BlockMaxWandIterator::BlockSkipTo(RowID doc_id, float threshold) {
    return false;
}

float BlockMaxWandIterator::BM25Score() {
    if (bm25_score_cached_) [[unlikely]] {
        return bm25_score_cache_;
    }
    SizeT num_iterators = sorted_iterators_.size();
    if (doc_id_ == INVALID_ROWID || pivot_ >= num_iterators) [[unlikely]] {
        return 0.0f;
    }
    float sum_score = 0.0f;
    RowID d = sorted_iterators_[pivot_]->DocID();
    for(SizeT i=0; i<pivot_; i++){
        sum_score += sorted_iterators_[i]->BM25Score();
    }
    for(SizeT i=pivot_; i<num_iterators && sorted_iterators_[i]->DocID() == d; i++){
        sum_score += sorted_iterators_[i]->BM25Score();
    }
    bm25_score_cache_ = sum_score;
    bm25_score_cached_ = true;
    return sum_score;
}

Pair<bool, RowID> BlockMaxWandIterator::SeekInBlockRange(RowID doc_id, const RowID doc_id_no_beyond) {
    return {false, INVALID_ROWID};
}

Tuple<bool, float, RowID> BlockMaxWandIterator::SeekInBlockRange(RowID doc_id, const RowID doc_id_no_beyond, const float threshold) {
    return {false, 0.0F, INVALID_ROWID};
}

Pair<bool, RowID> BlockMaxWandIterator::PeekInBlockRange(RowID doc_id, RowID doc_id_no_beyond) {
    return {false, INVALID_ROWID};
}

bool BlockMaxWandIterator::NotPartCheckExist(RowID doc_id) {
    return false;
}

} // namespace infinity