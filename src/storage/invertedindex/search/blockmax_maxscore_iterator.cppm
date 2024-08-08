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

export module blockmax_maxscore_iterator;
import stl;
import term_doc_iterator;
import multi_doc_iterator;
import or_iterator;
import internal_types;
import index_defines;
import loser_tree;

namespace infinity {

// equivalent to "OR" iterator
export class BlockMaxMaxscoreIterator final : public MultiDocIterator {
public:
    explicit BlockMaxMaxscoreIterator(Vector<UniquePtr<DocIterator>> &&iterators);

    ~BlockMaxMaxscoreIterator() override;

    String Name() const override { return "BlockMaxMaxscoreIterator"; }

    void UpdateScoreThreshold(float threshold) override;

    bool Next(RowID doc_id) override;

    float BM25Score() override;

private:
    void Init();
    DocIterator *GetDocIterator(u32 i) { return sorted_iterators_[i]; }

    // won't change after initialization
    Vector<TermDocIterator *> sorted_iterators_; // sort by BM25ScoreUpperBound, in ascending order
    Vector<float> sum_scores_upper_bound_;       // value at i: sum of BM25ScoreUpperBound for iter [0, i]

    UniquePtr<LoserTree<RowID, std::less<RowID>>> essentialPq_ = nullptr;
    // Index of the first essential iterator, ie. essentialHeap_ contains all iterators from
    // sorted_iterators_[firstEssential:]. All iterators below this index are non-essential.
    SizeT firstEssential = 0;
    // Index of the first iterator that is required, this iterator and all following iterators are required
    // for a document to match.
    SizeT firstRequired = sorted_iterators_.size();

    // bm25 score cache
    RowID bm25_score_cache_docid_ = INVALID_ROWID;
    float bm25_score_cache_ = 0.0f;

    // debug info
    u32 inner_pivot_loop_cnt = 0;
    u32 inner_must_have_loop_cnt_ = 0;
    u32 use_prev_candidate_cnt_ = 0;
    u32 not_use_prev_candidate_cnt_ = 0;
    Vector<Pair<u32, u64>> pivot_history_;
    Vector<Pair<u32, u64>> must_have_history_;
};

} // namespace infinity