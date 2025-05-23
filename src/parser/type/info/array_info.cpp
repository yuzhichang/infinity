// Copyright(C) 2025 InfiniFlow, Inc. All rights reserved.
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

#include "array_info.h"
#include "type/complex/array_type.h"

namespace infinity {

bool ArrayInfo::operator==(const TypeInfo &other) const {
    if (other.type() != TypeInfoType::kArray) {
        return false;
    }
    auto *array_info_ptr = dynamic_cast<const ArrayInfo *>(&other);
    return this->elem_type_ == array_info_ptr->elem_type_;
}

size_t ArrayInfo::Size() const { return sizeof(ArrayType); }

std::string ArrayInfo::ToString() const { return elem_type_.ToString(); }

nlohmann::json ArrayInfo::Serialize() const { return elem_type_.Serialize(); }

} // namespace infinity
