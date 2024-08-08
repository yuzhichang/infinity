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
#include <iostream>
module maxscore_iterator;
import stl;
import index_defines;
import doc_iterator;
import term_doc_iterator;
import multi_doc_iterator;
import internal_types;
import logger;
import third_party;
import loser_tree;

namespace infinity {

MaxscoreIterator::~MaxscoreIterator() {
    if (SHOULD_LOG_TRACE()) {
        String msg = "MaxscoreIterator pivot_history:";
        for (const auto &p : pivot_history_) {
            u64 row_id = std::get<0>(p);
            float threshold = std::get<1>(p);
            u32 firstEssential = std::get<2>(p);
            u32 firstRequired = std::get<3>(p);
            msg += fmt::format(" ({}, {:6f}, {}, {})", row_id, threshold, firstEssential, firstRequired);
        }
        LOG_TRACE(msg);
    }
}

MaxscoreIterator::MaxscoreIterator(Vector<SharedPtr<DocIterator>> &&iterators)
    : MultiDocIterator(std::move(iterators)), num_iterators_(children_.size()) {
    for (SizeT i = 0; i < num_iterators_; i++) {
        TermDocIterator *tdi = dynamic_cast<TermDocIterator *>(children_[i].get());
        if (tdi == nullptr)
            continue;
        bm25_score_upper_bound_ += tdi->BM25ScoreUpperBound();
        doc_freq_ += tdi->GetDF();
        sorted_iterators_.push_back(tdi);
    }
    num_iterators_ = sorted_iterators_.size();
    std::sort(sorted_iterators_.begin(), sorted_iterators_.end(), [](const auto &a, const auto &b) {
        return a->BM25ScoreUpperBound() < b->BM25ScoreUpperBound();
    });
    firstEssential_ = 0;
    firstRequired_ = sorted_iterators_.size();
    Init();
}

void MaxscoreIterator::Init() {
    sum_scores_upper_bound_.resize(sorted_iterators_.size());
    sum_scores_upper_bound_[0] = sorted_iterators_[0]->BM25ScoreUpperBound();
    for (SizeT i = 1; i < sorted_iterators_.size(); i++) {
        sum_scores_upper_bound_[i] = sum_scores_upper_bound_[i - 1] + sorted_iterators_[i]->BM25ScoreUpperBound();
    }
}

bool MaxscoreIterator::Next(RowID doc_id) {
    assert(doc_id != INVALID_ROWID);
    if (doc_id_ == INVALID_ROWID) {
        // Initialize children once.
        essentialPq_ = MakeUnique<DocIteratorHeap>();
        for (SizeT i = 0; i < num_iterators_; ++i) {
            sorted_iterators_[i]->Next(doc_id);
            DocIteratorEntry entry = {sorted_iterators_[i]->DocID(), i};
            essentialPq_->AddEntry(entry);
        }
        essentialPq_->BuildHeap();
        doc_id_ = essentialPq_->TopEntry().doc_id_;
    }
    if (doc_id_ != INVALID_ROWID && doc_id_ >= doc_id)
        return true;

    if (firstRequired_ < num_iterators_) {
        RowID target_doc_id = doc_id;
        do {
            // AND among iterators [firstRequired_, num_iterators_)
            for (int i = (int)num_iterators_ - 1; i >= (int)firstRequired_; i--) {
                const auto &it = sorted_iterators_[i];
                bool ok = it->Next(target_doc_id);
                if (!ok) {
                    doc_id_ = INVALID_ROWID;
                    return false;
                }
                target_doc_id = it->DocID();
            }
            if (target_doc_id == INVALID_ROWID) {
                doc_id_ = INVALID_ROWID;
                return false;
            }
            if (target_doc_id == sorted_iterators_[num_iterators_ - 1]->DocID()) {
                float sum_score = 0.0f;
                for (SizeT i = firstRequired_; i < num_iterators_; i++) {
                    const auto &it = sorted_iterators_[i];
                    sum_score += it->BM25Score();
                }
                while (target_doc_id > essentialPq_->TopEntry().doc_id_) {
                    DocIterator *top = GetDocIterator(essentialPq_->TopEntry().entry_id_);
                    top->Next(target_doc_id);
                    essentialPq_->UpdateTopEntry(top->DocID());
                }
                for (SizeT i = 0; i < firstRequired_; i++) {
                    auto &it = sorted_iterators_[i];
                    if (doc_id_ == it->DocID())
                        sum_score += it->BM25Score();
                }
                if (sum_score > threshold_) {
                    doc_id_ = target_doc_id;
                    bm25_score_cache_ = sum_score;
                    bm25_score_cache_docid_ = doc_id_;
                    return true;
                }
                target_doc_id++;
            }
        } while (1);
    } else {
        assert(firstRequired_ == num_iterators_);
        assert(firstEssential_ < num_iterators_ && essentialPq_.get() != nullptr);
        RowID target_doc_id = doc_id;
        do {
            float sum_score = 0.0f;
            // OR among iterators [firstEssential_, firstRequired_)
            while (target_doc_id > essentialPq_->TopEntry().doc_id_) {
                DocIterator *top = GetDocIterator(essentialPq_->TopEntry().entry_id_);
                top->Next(target_doc_id);
                essentialPq_->UpdateTopEntry(top->DocID());
            }
            target_doc_id = essentialPq_->TopEntry().doc_id_;
            if (target_doc_id == INVALID_ROWID) {
                doc_id_ = INVALID_ROWID;
                return false;
            }
            for (SizeT i = firstEssential_; i < num_iterators_; i++) {
                auto &it = sorted_iterators_[i];
                if (target_doc_id == it->DocID())
                    sum_score += it->BM25Score();
            }
            // OR among iterators [0, firstEssential_)
            for (SizeT i = 0; i < firstEssential_; i++) {
                auto &it = sorted_iterators_[i];
                it->Next(target_doc_id);
                if (target_doc_id == it->DocID())
                    sum_score += it->BM25Score();
            }
            if (sum_score > threshold_) {
                doc_id_ = target_doc_id;
                bm25_score_cache_ = sum_score;
                bm25_score_cache_docid_ = doc_id_;
                return true;
            }
            target_doc_id++;
        } while (1);
    }

    return doc_id_ != INVALID_ROWID;
}

float MaxscoreIterator::BM25Score() {
    if (bm25_score_cache_docid_ == doc_id_) {
        return bm25_score_cache_;
    }
    float sum_score = 0.0f;
    if (firstRequired_ < num_iterators_) {
        for (SizeT i = firstRequired_; i < num_iterators_; i++) {
            const auto &it = sorted_iterators_[i];
            assert(doc_id_ == it->DocID());
            sum_score += it->BM25Score();
        }
        for (SizeT i = 0; i < firstRequired_; i++) {
            auto &it = sorted_iterators_[i];
            assert(doc_id_ <= it->DocID());
            if (doc_id_ == it->DocID())
                sum_score += it->BM25Score();
        }
    } else {
        for (SizeT i = 0; i < num_iterators_; i++) {
            const auto &it = sorted_iterators_[i];
            assert(doc_id_ <= it->DocID());
            if (doc_id_ == it->DocID())
                sum_score += it->BM25Score();
        }
    }
    bm25_score_cache_docid_ = doc_id_;
    bm25_score_cache_ = sum_score;
    return sum_score;
}

void MaxscoreIterator::UpdateScoreThreshold(const float threshold) {
    if (threshold <= threshold_)
        return;
    threshold_ = threshold;
    const float base_threshold = threshold - BM25ScoreUpperBound();
    for (SizeT i = 0; i < num_iterators_; i++) {
        float new_threshold = std::max(0.0f, base_threshold + sorted_iterators_[i]->BM25ScoreUpperBound());
        sorted_iterators_[i]->UpdateScoreThreshold(new_threshold);
    }
    bool updated_firstEssential = false;
    bool updated_firstRequired = false;
    while (firstRequired_ > 0 &&
           sum_scores_upper_bound_[num_iterators_ - 1] - sorted_iterators_[firstRequired_ - 1]->BM25ScoreUpperBound() < threshold) {
        firstRequired_--;
        updated_firstRequired = true;
    }
    while (firstEssential_ < firstRequired_ && sum_scores_upper_bound_[firstEssential_] < threshold) {
        firstEssential_++;
        updated_firstEssential = true;
    }
    if (updated_firstRequired) {
        firstEssential_ = 0;
        if (firstRequired_ > 0) {
            essentialPq_ = MakeUnique<DocIteratorHeap>();
            for (u32 i = 0; i < firstRequired_; ++i) {
                DocIteratorEntry entry = {sorted_iterators_[i]->DocID(), i};
                essentialPq_->AddEntry(entry);
            }
            essentialPq_->BuildHeap();
        } else {
            essentialPq_.reset();
        }
        pivot_history_.emplace_back(doc_id_.ToUint64(), threshold, firstEssential_, firstRequired_);
    } else if (updated_firstEssential) {
        assert(firstRequired_ == num_iterators_);
        essentialPq_ = MakeUnique<DocIteratorHeap>();
        for (u32 i = firstEssential_; i < num_iterators_; ++i) {
            DocIteratorEntry entry = {sorted_iterators_[i]->DocID(), i};
            essentialPq_->AddEntry(entry);
        }
        essentialPq_->BuildHeap();
        pivot_history_.emplace_back(doc_id_.ToUint64(), threshold, firstEssential_, firstRequired_);
    }
}

} // namespace infinity