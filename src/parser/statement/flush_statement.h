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

#pragma once

#include "base_statement.h"
#include "statement/extra/create_collection_info.h"
#include "statement/extra/create_index_info.h"
#include "statement/extra/create_schema_info.h"
#include "statement/extra/create_table_info.h"
#include "statement/extra/create_view_info.h"

#include <string>

namespace infinity {

enum class FlushType {
    kCatalog,
    kData,
    kLog,
    kBuffer,
};

class FlushStatement : public BaseStatement {
public:
    explicit FlushStatement() : BaseStatement(StatementType::kFlush) {}

    [[nodiscard]] std::string ToString() const final;

    inline FlushType type() const { return type_; }

    FlushType type_;
};

} // namespace infinity