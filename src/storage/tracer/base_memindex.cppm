// Copyright(C) 2024 InfiniFlow, Inc. All rights reserved.
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

export module base_memindex;

import stl;
import memindex_tracer;

namespace infinity {

struct ChunkIndexMetaInfo;

export class BaseMemIndex : public EnableSharedFromThis<BaseMemIndex> {
public:
    virtual ~BaseMemIndex() = default;

    virtual MemIndexTracerInfo GetInfo() const = 0;

    virtual const ChunkIndexMetaInfo GetChunkIndexMetaInfo() const = 0;

protected:
    void IncreaseMemoryUsageBase(SizeT mem);
    void DecreaseMemoryUsageBase(SizeT mem);

public:
    String db_name_;
    String table_name_;
    String index_name_;
    SegmentID segment_id_ = 0;
};

} // namespace infinity
