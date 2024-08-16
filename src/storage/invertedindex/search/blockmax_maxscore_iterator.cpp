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
module blockmax_maxscore_iterator;
import stl;
import index_defines;
import doc_iterator;
import term_doc_iterator;
import multi_doc_iterator;
import or_iterator;
import internal_types;
import logger;
import third_party;

namespace infinity {

BlockMaxMaxscoreIterator::~BlockMaxMaxscoreIterator() {
    if (SHOULD_LOG_TRACE()) {
        String msg = "BlockMaxMaxscoreIterator pivot_history:";
        for (const auto &p : pivot_history_) {
            u64 row_id = std::get<0>(p);
            float threshold = std::get<1>(p);
            u32 firstEssential = std::get<2>(p);
            u32 firstRequired = std::get<3>(p);
            msg += fmt::format(" ({}, {:6f}, {}, {})", row_id, threshold, firstEssential, firstRequired);
        }
        msg += fmt::format("\nloop_cnt_phase1_ {}, loop_cnt_skip_boundaries_ {}, loop_cnt_nlb_ {}, loop_cnt_phase2_ {}, loop_cnt_quit_ {}",
                           loop_cnt_phase1_,
                           loop_cnt_skip_boundaries_,
                           loop_cnt_nlb_,
                           loop_cnt_phase2_,
                           loop_cnt_quit_);
        LOG_TRACE(msg);
    }
}

BlockMaxMaxscoreIterator::BlockMaxMaxscoreIterator(Vector<SharedPtr<DocIterator>> &&iterators)
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
        return a->BM25ScoreUpperBound() > b->BM25ScoreUpperBound();
    });
    firstEssential_ = 0;
    firstRequired_ = sorted_iterators_.size();
    Init();
}

void BlockMaxMaxscoreIterator::Init() {
    sum_scores_upper_bound_.resize(sorted_iterators_.size());
    sum_scores_upper_bound_[0] = sorted_iterators_[0]->BM25ScoreUpperBound();
    for (SizeT i = 1; i < sorted_iterators_.size(); i++) {
        sum_scores_upper_bound_[i] = sum_scores_upper_bound_[i - 1] + sorted_iterators_[i]->BM25ScoreUpperBound();
    }
}

void BlockMaxMaxscoreIterator::EssentialPqInit() {
    if (firstRequired_ > 0) {
        essentialPq_ = MakeUnique<DocIteratorHeap>();
        for (SizeT i = firstEssential_; i < firstRequired_; ++i) {
            DocIteratorEntry entry = {sorted_iterators_[i]->DocID(), i};
            essentialPq_->AddEntry(entry);
        }
        essentialPq_->BuildHeap();
    } else {
        essentialPq_.reset();
    }
}

void BlockMaxMaxscoreIterator::EssentialPqNext(RowID doc_id) {
    assert(essentialPq_.get() != nullptr);
    while (essentialPq_->TopEntry().doc_id_ < doc_id) {
        DocIterator *top = GetDocIterator(essentialPq_->TopEntry().entry_id_);
        top->Next(doc_id);
        essentialPq_->UpdateTopEntry(top->DocID());
    }
}

void BlockMaxMaxscoreIterator::BlockBoundariesInit() {
    if (firstRequired_ > 0) {
        block_boundaries_ = MakeUnique<DocIteratorHeap>();
        for (u32 i = 0; i < firstRequired_; ++i) {
            DocIteratorEntry entry = {sorted_iterators_[i]->BlockLastDocID(), i};
            block_boundaries_->AddEntry(entry);
        }
        block_boundaries_->BuildHeap();
        block_last_doc_id_ = block_boundaries_->TopEntry().doc_id_;
    } else {
        block_boundaries_.reset();
        block_last_doc_id_ = INVALID_ROWID;
    }
}

void BlockMaxMaxscoreIterator::BlockBoundariesNext(RowID doc_id) {
    assert(block_boundaries_.get() != nullptr);
    while (block_boundaries_->TopEntry().doc_id_ < doc_id) {
        TermDocIterator *top = GetDocIterator(block_boundaries_->TopEntry().entry_id_);
        bool ok = top->NextShallow(doc_id);
        RowID child_doc_id = ok ? top->BlockLastDocID() : INVALID_ROWID;
        block_boundaries_->UpdateTopEntry(child_doc_id);
    }
    block_last_doc_id_ = block_boundaries_->TopEntry().doc_id_;
}

bool BlockMaxMaxscoreIterator::Next(RowID doc_id) {
    assert(doc_id != INVALID_ROWID);
    if (doc_id_ == INVALID_ROWID && num_iterators_ > 0) {
        assert(firstEssential_ == 0 && firstRequired_ == num_iterators_);
        assert(essentialPq_.get() == nullptr);
        for (SizeT i = 0; i < num_iterators_; i++) {
            auto &it = sorted_iterators_[i];
            it->Next(doc_id);
        }
        BlockBoundariesInit();
        EssentialPqInit();
    }
    if (doc_id_ != INVALID_ROWID && doc_id_ >= doc_id)
        return true;

    if (firstRequired_ < num_iterators_) {
        assert(firstEssential_ == 0);
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
                bool possible =
                    firstRequired_ > 0 ? (sum_score + sum_scores_upper_bound_[firstRequired_ - 1] > threshold_) : (sum_score > threshold_);
                if (possible && essentialPq_.get() != nullptr) {
                    EssentialPqNext(target_doc_id);
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
                }
                target_doc_id++;
            }
        } while (1);
    } else {
        assert(firstRequired_ == num_iterators_);
        assert(firstEssential_ < num_iterators_ && essentialPq_.get() != nullptr);
        target_doc_id_ = doc_id;
        do {
            if (target_doc_id_ == 0 || target_doc_id_ > block_last_doc_id_) {
                bool ok = NextPhase1();
                if (!ok)
                    return false;
            }
            bool ok = NextPhase2();
            if (ok)
                return true;
            target_doc_id_++;
        } while (1);
    }
    doc_id_ = INVALID_ROWID;
    return false;
}

bool BlockMaxMaxscoreIterator::NextPhase1() {
    loop_cnt_phase1_++;
    non_essential_sum_bm_score_ = 0.0f;
    essential_sum_bm_score_ = 0.0f;
    float sum_score = 0.0f;
    // we first check if the (precomputed) sum of the list maxscores of the non-essential lists plus the sum of the block maxscores of the
    // essential lists is above the threshold.
    BlockBoundariesNext(target_doc_id_);
    for (SizeT i = firstEssential_; i < num_iterators_; i++) {
        essential_sum_bm_score_ += sorted_iterators_[i]->BlockMaxBM25Score();
    }
    if (firstEssential_ > 0)
        non_essential_sum_bm_score_ = sum_scores_upper_bound_[firstEssential_ - 1];
    if (non_essential_sum_bm_score_ + essential_sum_bm_score_ > threshold_) {
        // If yes, then we replace the list maxscores of the non-essential lists with block maxscores, and check again.
        non_essential_sum_bm_score_ = 0.0f;
        for (SizeT i = 0; i < firstEssential_; i++) {
            non_essential_sum_bm_score_ += sorted_iterators_[i]->BlockMaxBM25Score();
        }
        sum_score = non_essential_sum_bm_score_ + essential_sum_bm_score_;
        if (sum_score > threshold_) {
            return true;
        }
    }
    // In case the first (or second) filter above fails, we can in fact not just skip the current posting, but move directly to the end of
    // the shortest (or shortest essential) block. We refer to the version of BMM that uses this optimization as Block-Max Maxscore-Next
    // Live Block (BMM-NLB).
    loop_cnt_skip_boundaries_++;
    TermDocIterator *top = GetDocIterator(block_boundaries_->TopEntry().entry_id_);
    while (block_boundaries_->TopEntry().doc_id_ != INVALID_ROWID) {
        loop_cnt_nlb_++;
        sum_score -= top->BlockMaxBM25Score();
        target_doc_id_ = block_boundaries_->TopEntry().doc_id_ + 1;
        bool ok = top->NextShallow(target_doc_id_);
        RowID block_last_doc_id = ok ? top->BlockLastDocID() : INVALID_ROWID;
        block_boundaries_->UpdateTopEntry(block_last_doc_id);
        top = GetDocIterator(block_boundaries_->TopEntry().entry_id_);
        sum_score += top->BlockMaxBM25Score();
        if (sum_score > threshold_) {
            block_last_doc_id_ = block_boundaries_->TopEntry().doc_id_;
            return true;
        }
    }
    doc_id_ = INVALID_ROWID;
    return false;
}

bool BlockMaxMaxscoreIterator::NextPhase2() {
    loop_cnt_phase2_++;
    assert(target_doc_id_ != INVALID_ROWID && target_doc_id_ <= block_last_doc_id_ &&
           non_essential_sum_bm_score_ + essential_sum_bm_score_ > threshold_);
    // Next, we get the term scores of the candidate in the essential lists, and check if the potential score is still above the
    // threshold.
    float essential_sum_score = 0.0f;
    float non_essential_sum_score = 0.0f;
    RowID min_doc_id = INVALID_ROWID;
    for (SizeT i = firstEssential_; i < num_iterators_; i++) {
        const auto &it = sorted_iterators_[i];
        if (it->BlockMinPossibleDocID() <= target_doc_id_) {
            bool ok = it->Next(target_doc_id_);
            if (ok && it->DocID() < min_doc_id) {
                min_doc_id = it->DocID();
            }
        }
    }
    assert(target_doc_id_ <= min_doc_id);
    target_doc_id_ = min_doc_id;
    for (SizeT i = firstEssential_; i < num_iterators_; i++) {
        const auto &it = sorted_iterators_[i];
        if (it->DocID() == target_doc_id_) {
            essential_sum_score += it->BM25Score();
        }
    }
    if (non_essential_sum_bm_score_ + essential_sum_score > threshold_) {
        // If so, we then start scoring the non-essential lists until all terms have been evaluated or the candidate has
        // been ruled out.
        for (SizeT i = 0; i < firstEssential_; i++) {
            const auto &it = sorted_iterators_[i];
            if (it->BlockMinPossibleDocID() <= target_doc_id_) {
                bool ok = it->Next(target_doc_id_);
                if (ok && it->DocID() == target_doc_id_) {
                    non_essential_sum_score += it->BM25Score();
                }
            }
        }
        if (non_essential_sum_score + essential_sum_score > threshold_) {
            loop_cnt_quit_++;
            doc_id_ = target_doc_id_;
            bm25_score_cache_docid_ = doc_id_;
            bm25_score_cache_ = non_essential_sum_score + essential_sum_score;
            return true;
        }
    }
    return false;
}

float BlockMaxMaxscoreIterator::BM25Score() {
    if (bm25_score_cache_docid_ == doc_id_) {
        return bm25_score_cache_;
    }
    float sum_score = 0.0f;
    for (SizeT i = 0; i < firstRequired_; i++) {
        auto &it = sorted_iterators_[i];
        assert(doc_id_ <= it->DocID());
        if (doc_id_ == it->DocID())
            sum_score += it->BM25Score();
    }
    for (SizeT i = firstRequired_; i < num_iterators_; i++) {
        const auto &it = sorted_iterators_[i];
        assert(doc_id_ == it->DocID());
        sum_score += it->BM25Score();
    }
    bm25_score_cache_docid_ = doc_id_;
    bm25_score_cache_ = sum_score;
    return sum_score;
}

void BlockMaxMaxscoreIterator::UpdateScoreThreshold(const float threshold) {
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
        firstEssential_ = 0;
    }
    while (firstEssential_ < firstRequired_ && sum_scores_upper_bound_[firstEssential_] < threshold) {
        firstEssential_++;
        updated_firstEssential = true;
    }
    if (updated_firstRequired) {
        BlockBoundariesInit();
        EssentialPqInit();
        pivot_history_.emplace_back(doc_id_.ToUint64(), threshold, firstEssential_, firstRequired_);
    } else if (updated_firstEssential) {
        EssentialPqInit();
        pivot_history_.emplace_back(doc_id_.ToUint64(), threshold, firstEssential_, firstRequired_);
    }
}

} // namespace infinity