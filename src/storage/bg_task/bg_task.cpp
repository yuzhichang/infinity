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

module bg_task;

import base_memindex;
import chunk_index_entry;
import cleanup_scanner;
import infinity_context;
import storage;
import new_txn;
import new_txn_manager;
import infinity_exception;
import txn_state;

namespace infinity {

Status NewCheckpointTask::ExecuteWithinTxn() {
    TxnTimeStamp last_checkpoint_ts = InfinityContext::instance().storage()->wal_manager()->LastCheckpointTS();
    Status status = new_txn_->Checkpoint(last_checkpoint_ts);
    new_txn_->SetWalSize(wal_size_);
    return status;
}

Status NewCheckpointTask::ExecuteWithNewTxn() {
    auto *new_txn_mgr = InfinityContext::instance().storage()->new_txn_manager();
    auto *new_txn = new_txn_mgr->BeginTxn(MakeUnique<String>("checkpoint"), TransactionType::kNewCheckpoint);
    new_txn->SetWalSize(wal_size_);
    TxnTimeStamp last_checkpoint_ts = InfinityContext::instance().storage()->wal_manager()->LastCheckpointTS();
    Status status = new_txn->Checkpoint(last_checkpoint_ts);
    if (status.ok()) {
        status = new_txn_mgr->CommitTxn(new_txn);
    }
    return status;
}

Status NewCleanupTask::Execute(TxnTimeStamp last_cleanup_ts, TxnTimeStamp &cur_cleanup_ts) {
    auto *new_txn_mgr = InfinityContext::instance().storage()->new_txn_manager();
    Status status = new_txn_mgr->Cleanup(last_cleanup_ts, &cur_cleanup_ts);
    if (!status.ok()) {
        return status;
    }
    return Status::OK();
}

NewCompactTask::NewCompactTask(NewTxn *new_txn, String db_name, String table_name)
    : BGTask(BGTaskType::kNewCompact, false), new_txn_(new_txn), db_name_(db_name), table_name_(table_name) {}

DumpIndexTask::DumpIndexTask(BaseMemIndex *mem_index, NewTxn *new_txn)
    : BGTask(BGTaskType::kDumpIndex, true), mem_index_(mem_index), new_txn_(new_txn) {}

TestCommandTask::TestCommandTask(String command_content) : BGTask(BGTaskType::kTestCommand, true), command_content_(std::move(command_content)) {}

} // namespace infinity