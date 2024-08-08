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

module or_iterator;
import internal_types;

import stl;
import index_defines;
import multi_doc_iterator;
import doc_iterator;
import loser_tree;
namespace infinity {

OrIterator::OrIterator(Vector<UniquePtr<DocIterator>> iterators) : MultiDocIterator(std::move(iterators)), pq_(children_.size()) {
    doc_freq_ = 0;
    bm25_score_upper_bound_ = 0.0f;
    for (u32 i = 0; i < children_.size(); ++i) {
        doc_freq_ += children_[i]->GetDF();
        bm25_score_upper_bound_ += children_[i]->BM25ScoreUpperBound();
    }
}

bool OrIterator::Next(RowID doc_id) {
    assert(doc_id != INVALID_ROWID);
    if (doc_id_ == INVALID_ROWID) {
        for (u32 i = 0; i < children_.size(); ++i) {
            children_[i]->Next();
            RowID child_doc_id = children_[i]->DocID();
            pq_.InsertStart(&child_doc_id, i, false);
        }
        pq_.Init();
        doc_id_ = pq_.TopKey();
    }
    if (doc_id_ != INVALID_ROWID && doc_id_ >= doc_id)
        return true;
    while (doc_id > pq_.TopKey()) {
        DocIterator *top = GetDocIterator(pq_.TopSource());
        top->Next(doc_id);
        RowID child_doc_id = top->DocID();
        pq_.DeleteTopInsert(&child_doc_id, false);
    }
    doc_id_ = pq_.TopKey();
    return doc_id_ != INVALID_ROWID;
}

float OrIterator::BM25Score() {
    if (bm25_score_cache_docid_ == doc_id_) {
        return bm25_score_cache_;
    }
    float sum_score = 0;
    for (u32 i = 0; i < children_.size(); ++i) {
        if (children_[i]->DocID() == doc_id_)
            sum_score += children_[i]->BM25Score();
    }
    bm25_score_cache_docid_ = doc_id_;
    bm25_score_cache_ = sum_score;
    return sum_score;
}

void OrIterator::UpdateScoreThreshold(float threshold) {
    if (threshold <= threshold_)
        return;
    threshold_ = threshold;
    const float base_threshold = threshold - BM25ScoreUpperBound();
    for (SizeT i = 0; i < children_.size(); i++) {
        const auto &it = children_[i];
        float new_threshold = std::max(0.0f, base_threshold + it->BM25ScoreUpperBound());
        it->UpdateScoreThreshold(new_threshold);
    }
}

} // namespace infinity