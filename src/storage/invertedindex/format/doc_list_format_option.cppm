module;

import stl;
import posting_field;
import index_defines;
import infinity_exception;

export module doc_list_format_option;

namespace infinity {

export class DocListFormatOption {
public:
    explicit DocListFormatOption(optionflag_t option_flag) { Init(option_flag); }

    ~DocListFormatOption() = default;

    inline void Init(optionflag_t option_flag) {
        has_doc_payload_ = option_flag & of_doc_payload ? 1 : 0;
        if (option_flag & of_term_frequency) {
            has_tf_ = 1;
            has_tf_list_ = 1;
        } else {
            has_tf_ = 0;
            has_tf_list_ = 0;
        }
        short_list_vbyte_compress_ = 0;
        has_block_max_ = (option_flag & of_block_max) ? 1 : 0;
        unused_ = 0;
        // when has_block_max_ is set, has_tf_list_ must also be set
        if (has_block_max_ and !has_tf_list_) {
            UnrecoverableError("Invalid option flag: block_max must be used with term frequency list.");
        }
    }

    bool HasTF() const { return has_tf_ == 1; }
    bool HasTfList() const { return has_tf_list_ == 1; }
    bool HasDocPayload() const { return has_doc_payload_ == 1; }
    bool HasBlockMax() const { return has_block_max_ == 1; }
    bool operator==(const DocListFormatOption &right) const {
        return has_tf_ == right.has_tf_ && has_tf_list_ == right.has_tf_list_ && has_doc_payload_ == right.has_doc_payload_ &&
               short_list_vbyte_compress_ == right.short_list_vbyte_compress_ && has_block_max_ == right.has_block_max_;
    }
    bool IsShortListVbyteCompress() const { return short_list_vbyte_compress_ == 1; }
    void SetShortListVbyteCompress(bool flag) { short_list_vbyte_compress_ = flag ? 1 : 0; }

private:
    u8 has_tf_ : 1;
    u8 has_tf_list_ : 1;
    u8 has_doc_payload_ : 1;
    u8 short_list_vbyte_compress_ : 1;
    u8 has_block_max_ : 1;
    u8 unused_ : 3;
};

export class DocSkipListFormat : public PostingFields {
public:
    DocSkipListFormat() = default;

    ~DocSkipListFormat() = default;

    void Init(const DocListFormatOption &option) {
        AddU32Value(); // doc_id
        if (option.HasTfList()) {
            has_tf_list_ = true;
            AddU32Value(); // ttf
        }
        if (option.HasBlockMax()) {
            has_block_max_ = true;
            AddU32Value(); // block_first_doc_id
            AddU32Value(); // block_max_tf
            AddU16Value(); // block_max_percentage
        }
        AddU32Value(); // offset
    }

    bool HasTfList() const { return has_tf_list_; }
    bool HasBlockMax() const { return has_block_max_; }

private:
    bool has_tf_list_ = false;
    bool has_block_max_ = false;
};

export class DocListFormat : public PostingFields {
public:
    DocListFormat(const DocListFormatOption &option) : skiplist_format_(nullptr) { Init(option); }
    DocListFormat() : skiplist_format_(nullptr) {}

    ~DocListFormat() {
        if (skiplist_format_) {
            delete skiplist_format_;
            skiplist_format_ = nullptr;
        }
    };

    void Init(const DocListFormatOption &option) {
        AddU32Value(); // doc_id
        if (option.HasTfList()) {
            AddU32Value(); // tf
        }
        if (option.HasDocPayload()) {
            AddU16Value(); // doc_payload
        }
        skiplist_format_ = new DocSkipListFormat;
        skiplist_format_->Init(option);
    }

    const DocSkipListFormat *GetDocSkipListFormat() const { return skiplist_format_; }

private:
    DocSkipListFormat *skiplist_format_;
};

} // namespace infinity
