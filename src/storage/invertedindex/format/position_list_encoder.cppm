module;

export module position_list_encoder;

import stl;
import byte_slice;
import byte_slice_writer;

import file_writer;
import file_reader;
import index_defines;
import posting_byte_slice;
import skiplist_writer;
import position_list_format_option;
import posting_list_format;
import inmem_position_list_decoder;

namespace infinity {

export class PositionListEncoder {
public:
    PositionListEncoder(const PostingFormatOption &format_option, const PositionListFormat *pos_list_format = nullptr);

    ~PositionListEncoder();

    void AddPosition(pos_t pos);
    void EndDocument();
    void Flush();
    void Dump(const SharedPtr<FileWriter> &file, bool spill = false);
    void Load(const SharedPtr<FileReader> &file);
    u32 GetDumpLength() const;

    InMemPositionListDecoder *GetInMemPositionListDecoder() const;

    const ByteSliceList *GetPositionList() const { return pos_list_buffer_.GetByteSliceList(); }

    const PostingByteSlice *GetBufferedByteSlice() const { return &pos_list_buffer_; }

    const PositionListFormat *GetPositionListFormat() const { return pos_list_format_; }

    inline SizeT GetSizeInBytes() const { return pos_list_buffer_.GetSizeInBytes() + pos_skiplist_writer_->GetSizeInBytes(); }

private:
    SharedPtr<SkipListWriter> GetPosSkipListWriter();
    void AddPosSkipListItem(u32 total_pos_count, u32 compressed_pos_size, bool need_flush);
    void FlushPositionBuffer();

private:
    PostingByteSlice pos_list_buffer_;
    pos_t last_pos_in_cur_doc_; // 4byte
    PostingFormatOption format_option_;
    bool is_own_format_; // 1byte
    const PositionListFormat *pos_list_format_;

    mutable std::shared_mutex rw_mutex_; // Protect total_pos_count_ and pos_skiplist_writer_
    u32 total_pos_count_; // 4byte
    SharedPtr<SkipListWriter> pos_skiplist_writer_;
};

} // namespace infinity
