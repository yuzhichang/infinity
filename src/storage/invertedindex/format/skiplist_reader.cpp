module;

#include <cassert>
module skiplist_reader;

import stl;
import byte_slice;
import status;
import posting_field;
import flush_info;
import infinity_exception;
import logger;
import third_party;

namespace infinity {

bool SkipListReader::SkipTo(u32 query_doc_id, u32 &block_first_doc_id, u32 &block_last_doc_id, u32 &block_offset, u32 &block_len) {
    if (query_doc_id <= current_doc_id_ && current_doc_id_ != INVALID_DOCID) [[unlikely]] {
        assert(current_block_first_doc_id_ < current_doc_id_);
        block_first_doc_id = current_block_first_doc_id_;
        block_last_doc_id = current_doc_id_;
        block_offset = prev_offset_;
        block_len = current_offset_;
        return true;
    }
    assert(current_doc_id_ == INVALID_DOCID || query_doc_id > current_doc_id_);
    if (current_doc_id_ == INVALID_DOCID) [[unlikely]] {
        assert(current_cursor_ == 0 && num_in_buffer_ == 0);
        current_block_first_doc_id_ = 0;
        current_doc_id_ = 0;
        prev_offset_ = 0;
        current_offset_ = 0;
    }
    for (;; ++current_cursor_) {
        if (current_cursor_ >= num_in_buffer_) {
            auto [status, ret] = LoadBuffer();
            assert(status == 0);
            if (!ret) {
                block_first_doc_id = current_block_first_doc_id_;
                block_last_doc_id = current_doc_id_;
                block_offset = prev_offset_;
                block_len = current_offset_ - prev_offset_;
                // current segment is exhausted
                // skip current segment
                ++skipped_item_count_;
                return false;
            }
        }
        prev_doc_id_ = current_doc_id_;
        current_doc_id_ += doc_id_buffer_[current_cursor_];
        prev_offset_ = current_offset_;
        current_offset_ += offset_buffer_[current_cursor_];
        if (has_tf_list_) {
            prev_ttf_ = current_ttf_;
            current_ttf_ += tf_buffer_[current_cursor_];
        }
        ++skipped_item_count_;
        if (current_doc_id_ >= query_doc_id) {
            if (has_block_max_) {
                current_block_first_doc_id_ = current_doc_id_ - doc_id_buffer_[current_cursor_] + block_first_doc_id_buffer_[current_cursor_];
                current_block_max_tf_ = block_max_tf_buffer_[current_cursor_];
                current_block_max_tf_percentage_ = block_max_tf_percentage_buffer_[current_cursor_];
            }
            block_first_doc_id = current_block_first_doc_id_;
            block_last_doc_id = current_doc_id_;
            block_offset = prev_offset_;
            block_len = current_offset_ - prev_offset_;
            // point to next item
            ++current_cursor_;
            return true;
        }
    }
}

u32 SkipListReader::GetLastValueInBuffer() const {
    u32 last_value_in_buffer = current_offset_;
    for (u32 current_cursor = current_cursor_; current_cursor < num_in_buffer_; ++current_cursor) {
        last_value_in_buffer += offset_buffer_[current_cursor];
    }
    return last_value_in_buffer;
}

u32 SkipListReader::GetLastKeyInBuffer() const {
    u32 last_key_in_buffer = current_doc_id_;
    for (u32 current_cursor = current_cursor_; current_cursor < num_in_buffer_; ++current_cursor) {
        last_key_in_buffer += doc_id_buffer_[current_cursor];
    }
    return last_key_in_buffer;
}

void SkipListReaderByteSlice::Load(const ByteSliceList *byte_slice_list, u32 start, u32 end) {
    if (start > byte_slice_list->GetTotalSize()) {
        UnrecoverableError("start > byte_slice_list->GetTotalSize().");
    }
    if (end > byte_slice_list->GetTotalSize()) {
        UnrecoverableError("end > byte_slice_list->GetTotalSize().");
    }
    start_ = start;
    end_ = end;
    byte_slice_reader_.Open(const_cast<ByteSliceList *>(byte_slice_list));
    byte_slice_reader_.Seek(start);
}

void SkipListReaderByteSlice::Load(ByteSlice *byte_slice, u32 start, u32 end) {
    if (start > byte_slice->size_) {
        UnrecoverableError("start > byte_slice->size_.");
    }
    if (end > byte_slice->size_) {
        UnrecoverableError("end > byte_slice->size_.");
    }

    start_ = start;
    end_ = end;
    byte_slice_reader_.Open(byte_slice);
    byte_slice_reader_.Seek(start);
}

// Note: keep sync with SkipListWriter::AddItem
Pair<int, bool> SkipListReaderByteSlice::LoadBuffer() {
    u32 end = byte_slice_reader_.Tell();
    if (end >= end_) [[unlikely]] {
        return MakePair(0, false);
    }
    const Int32Encoder *i32_encoder = GetSkipListEncoder();
    const Int16Encoder *i16_encoder = GetTermPercentageEncoder();
    u32 doc_num = i32_encoder->Decode(static_cast<u32 *>(doc_id_buffer_), SKIP_LIST_BUFFER_SIZE, byte_slice_reader_);
    if (has_tf_list_) {
        u32 ttf_num = i32_encoder->Decode(tf_buffer_.get(), SKIP_LIST_BUFFER_SIZE, byte_slice_reader_);
        if (ttf_num != doc_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} ttf_num = {}", doc_num, ttf_num));
            return MakePair(-1, false);
        }
    }
    if (has_block_max_) {
        u32 block_first_doc_id_num = i32_encoder->Decode(block_first_doc_id_buffer_.get(), SKIP_LIST_BUFFER_SIZE, byte_slice_reader_);
        if (block_first_doc_id_num != doc_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} block_first_doc_id_num = {}", doc_num, block_first_doc_id_num));
            return MakePair(-1, false);
        }

        u32 block_max_tf_num = i32_encoder->Decode(block_max_tf_buffer_.get(), SKIP_LIST_BUFFER_SIZE, byte_slice_reader_);
        if (block_max_tf_num != doc_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} block_max_tf_num = {}", doc_num, block_max_tf_num));
            return MakePair(-1, false);
        }

        u32 tf_percentage_num = i16_encoder->Decode(block_max_tf_percentage_buffer_.get(), SKIP_LIST_BUFFER_SIZE, byte_slice_reader_);
        if (tf_percentage_num != doc_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} block_max_tf_percentage_num = {}", doc_num, tf_percentage_num));
            return MakePair(-1, false);
        }
    }
    {
        u32 len_num = i32_encoder->Decode(static_cast<u32 *>(offset_buffer_), SKIP_LIST_BUFFER_SIZE, byte_slice_reader_);
        if (len_num != doc_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} offset_num = {}", doc_num, len_num));
            return MakePair(-1, false);
        }
    }
    num_in_buffer_ = doc_num;
    current_cursor_ = 0;
    return MakePair(0, true);
}

SkipListReaderPostingByteSlice::~SkipListReaderPostingByteSlice() {
    if (session_pool_) {
        skiplist_buffer_->~PostingByteSlice();
        session_pool_->Deallocate((void *)skiplist_buffer_, sizeof(PostingByteSlice));
    } else {
        delete skiplist_buffer_;
    }
    skiplist_buffer_ = nullptr;
}

void SkipListReaderPostingByteSlice::Load(const PostingByteSlice *posting_buffer) {
    PostingByteSlice *skiplist_buffer = session_pool_ ? new (session_pool_->Allocate(sizeof(PostingByteSlice)))
                                                            PostingByteSlice(session_pool_, session_pool_)
                                                      : new PostingByteSlice(session_pool_, session_pool_);
    posting_buffer->SnapShot(skiplist_buffer);

    skiplist_buffer_ = skiplist_buffer;
    skiplist_reader_.Open(skiplist_buffer_);
}

// Note: keep sync with SkipListWriter::AddItem
Pair<int, bool> SkipListReaderPostingByteSlice::LoadBuffer() {
    SizeT flush_count = skiplist_buffer_->GetTotalCount();
    FlushInfo flush_info = skiplist_buffer_->GetFlushInfo();

    SizeT decode_count = SKIP_LIST_BUFFER_SIZE;
    if (flush_info.IsValidPostingBuffer() == false) {
        decode_count = flush_count;
    }
    if (decode_count == 0) {
        return MakePair(0, false);
    }

    SizeT doc_num = 0;
    if (!skiplist_reader_.Decode(doc_id_buffer_, decode_count, doc_num)) {
        return MakePair(0, false);
    }

    if (has_tf_list_) {
        SizeT ttf_num = 0;
        if (!skiplist_reader_.Decode(tf_buffer_.get(), decode_count, ttf_num)) {
            return MakePair(0, false);
        }
        if (doc_num != ttf_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} ttf_num = {}", doc_num, ttf_num));
            return MakePair(-1, false);
        }
    }

    if (has_block_max_) {
        SizeT block_first_doc_id_num = 0;
        if (!skiplist_reader_.Decode(block_first_doc_id_buffer_.get(), decode_count, block_first_doc_id_num)) {
            return MakePair(0, false);
        }
        if (doc_num != block_first_doc_id_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} block_first_doc_id_num = {}", doc_num, block_first_doc_id_num));
            return MakePair(-1, false);
        }

        SizeT block_max_tf_num = 0;
        if (!skiplist_reader_.Decode(block_max_tf_buffer_.get(), decode_count, block_max_tf_num)) {
            return MakePair(0, false);
        }
        if (doc_num != block_max_tf_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} block_max_tf_num = {}", doc_num, block_max_tf_num));
            return MakePair(-1, false);
        }

        SizeT tf_percentage_num = 0;
        if (!skiplist_reader_.Decode(block_max_tf_percentage_buffer_.get(), decode_count, tf_percentage_num)) {
            return MakePair(0, false);
        }
        if (doc_num != tf_percentage_num) {
            UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} block_max_tf_percentage_num = {}", doc_num, tf_percentage_num));
            return MakePair(-1, false);
        }
    }

    SizeT len_num = 0;
    if (!skiplist_reader_.Decode(offset_buffer_, decode_count, len_num)) {
        return MakePair(0, false);
    }
    if (doc_num != len_num) {
        UnrecoverableError(fmt::format("SKipList decode error, doc_num = {} len_num = {}", doc_num, len_num));
        return MakePair(-1, false);
    }

    num_in_buffer_ = doc_num;
    current_cursor_ = 0;
    return MakePair(0, true);
}

} // namespace infinity
