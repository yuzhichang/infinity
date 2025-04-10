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

//module;
//
//module txn;
//
//namespace infinity {
//
//Status Txn::CreateDatabaseInternalLegacy(const SharedPtr<String> &db_name, ConflictType conflict_type, const SharedPtr<String> &comment) {
//    return Status::OK();
//}
//Status DropDatabaseInternalLegacy(const String &db_name, ConflictType conflict_type) { return Status::OK(); }
//Tuple<DBEntry *, Status> GetDatabaseInternalLegacy(const String &db_name) { return {nullptr, Status::OK()}; }
//Tuple<SharedPtr<DatabaseInfo>, Status> GetDatabaseInfoInternalLegacy(const String &db_name) { return {nullptr, Status::OK()}; }
//Vector<DatabaseDetail> ListDatabasesInternalLegacy() { return {}; }
//
//} // namespace infinity
