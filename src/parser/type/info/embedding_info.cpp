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
#include "embedding_info.h"
#include <arrow/type.h>

namespace infinity {

bool EmbeddingInfo::operator==(const TypeInfo &other) const {
    if (other.type() != TypeInfoType::kEmbedding)
        return false;

    auto *embedding_info_ptr = (EmbeddingInfo *)(&other);

    return this->dimension_ == embedding_info_ptr->dimension_ && this->embedding_data_type_ == embedding_info_ptr->embedding_data_type_;
}

bool EmbeddingInfo::operator==(const arrow::FixedSizeListType &other) const {
    if (static_cast<int32_t>(dimension_) != other.list_size()) {
        return false;
    }
    return embedding_data_type_ == other.value_type()->id();
}

bool EmbeddingInfo::operator==(const arrow::ListType &other) const { return embedding_data_type_ == other.value_type()->id(); }

nlohmann::json EmbeddingInfo::Serialize() const {
    nlohmann::json res;
    res["dimension"] = dimension_;
    res["embedding_type"] = embedding_data_type_;
    return res;
}

std::string EmbeddingInfo::EmbeddingDataTypeToString(EmbeddingDataType type) {
    switch (type) {
        case EmbeddingDataType::kElemBit:
            return "bit";
        case EmbeddingDataType::kElemUInt8:
            return "uint8";
        case EmbeddingDataType::kElemInt8:
            return "int8";
        case EmbeddingDataType::kElemInt16:
            return "int16";
        case EmbeddingDataType::kElemInt32:
            return "int32";
        case EmbeddingDataType::kElemInt64:
            return "int64";
        case EmbeddingDataType::kElemFloat:
            return "float";
        case EmbeddingDataType::kElemDouble:
            return "double";
        case EmbeddingDataType::kElemFloat16:
            return "float16";
        case EmbeddingDataType::kElemBFloat16:
            return "bfloat16";
        case EmbeddingDataType::kElemInvalid:
            ParserError("Unexpected embedding type");
    }
    return std::string();
}

} // namespace infinity
