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
import doc_iterator;
import term_doc_iterator;
import multi_doc_iterator;
import or_iterator;
import internal_types;
import index_defines;

namespace infinity {

/**
Refers to https://engineering.nyu.edu/~suel/papers/bmm.pdf
Optimizing Top-k Document Retrieval Strategies for Block-Max Indexes, 2013.

We also implemented a Maxscore-based method
(BMM), but with some changes over the implementation
described in [7]. In particular, we removed the preprocess-
ing for aligning block boundaries and the selection of non-
essential lists in each interval as described in [7], as we found
this to be a performance bottleneck.

Finally, initial experiments found that some methods are
better for queries with few terms, and some for queries with
many terms. Since all algorithms use exactly the same data
structures, it is easy to call the best method for any number
of query terms; this combined method is called BM-OPT.

In fact, it seems that BMW spends a fair amount
of effort trying to avoid evaluations and calls, while BMM
does not spend as much time thinking about avoiding them
and instead keeps the control structure simple.

The blog on ElasticSearch bmm:
https://www.elastic.co/search-labs/blog/more-skipping-with-bm-maxscore
 */
export class BlockMaxMaxscoreIterator final : public MultiDocIterator {
public:
    explicit BlockMaxMaxscoreIterator(Vector<SharedPtr<DocIterator>> &&iterators);

    ~BlockMaxMaxscoreIterator() override;

    String Name() const override { return "BlockMaxMaxscoreIterator"; }

    void UpdateScoreThreshold(float threshold) override;

    bool Next(RowID doc_id) override;

    float BM25Score() override;

private:
    void Init();

    void EssentialPqInit();

    void EssentialPqNext(RowID doc_id);

    TermDocIterator *GetDocIterator(u32 i) { return sorted_iterators_[i]; }

    bool NextPhase1();
    bool NextPhase2();

    // won't change after initialization
    Vector<TermDocIterator *> sorted_iterators_; // sort by BM25ScoreUpperBound, in ascending order
    SizeT num_iterators_ = 0;
    Vector<float> sum_scores_upper_bound_;       // value at i: sum of BM25ScoreUpperBound for iter [0, i]

    UniquePtr<DocIteratorHeap> essentialPq_ = nullptr; // OR among iterators [firstEssential_, firstRequired_)

    // Index of the first essential iterator, ie. essentialHeap_ contains all iterators from
    // sorted_iterators_[firstEssential:]. All iterators below this index are non-essential.
    SizeT firstEssential_ = 0;
    // Index of the first iterator that is required, this iterator and all following iterators are required
    // for a document to match.
    SizeT firstRequired_ = sorted_iterators_.size();

    float non_essential_sum_score_ = 0.0f;
    float essential_sum_score_ = 0.0f;
    RowID target_doc_id_ = INVALID_ROWID;

    // bm25 score cache
    RowID bm25_score_cache_docid_ = INVALID_ROWID;
    float bm25_score_cache_ = 0.0f;

    RowID threshold_doc_id_ = INVALID_ROWID;

    // debug info
    Vector<Tuple<u64, float, u32, u32>> pivot_history_; // row_id, threshold, firstEssential, firstRequired
    u64 loop_cnt_phase1_ = 0;
    u64 loop_cnt_phase2_ = 0;
    u64 loop_cnt_init_boundaries_ = 0;
    u64 loop_cnt_nlb_ = 0;
    u64 loop_cnt_quit_ = 0;
};

} // namespace infinity