#include "common.h"
#include "llama.h"
#include "speculative.h"

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <exception>
#include <memory>
#include <vector>

namespace {

constexpr llama_seq_id AGL_LLAMA_MTP_SEQ_ID = 0;

void agl_mtp_copy_cstr(char * dst, size_t dst_len, const char * src) {
    if (dst == nullptr || dst_len == 0) {
        return;
    }

    if (src == nullptr) {
        src = "unknown llama.cpp MTP error";
    }

    const size_t src_len = std::strlen(src);
    const size_t count = std::min(dst_len - 1, src_len);
    std::memcpy(dst, src, count);
    dst[count] = '\0';
}

bool agl_mtp_batch_compatible(const llama_batch & batch) {
    if (batch.n_tokens <= 0 || batch.token == nullptr || batch.embd != nullptr ||
        batch.pos == nullptr || batch.n_seq_id == nullptr || batch.seq_id == nullptr) {
        return false;
    }

    for (int32_t i = 0; i < batch.n_tokens; ++i) {
        if (batch.n_seq_id[i] != 1 || batch.seq_id[i] == nullptr ||
            batch.seq_id[i][0] != AGL_LLAMA_MTP_SEQ_ID) {
            return false;
        }
    }

    return true;
}

void agl_mtp_assign_tokens(
    std::vector<llama_token> & dst,
    const llama_token * tokens,
    size_t count) {
    if (count == 0) {
        dst.clear();
        return;
    }
    dst.assign(tokens, tokens + count);
}

} // namespace

extern "C" {

enum agl_llama_mtp_status {
    AGL_LLAMA_MTP_OK = 0,
    AGL_LLAMA_MTP_INVALID_ARGUMENT = 1,
    AGL_LLAMA_MTP_INIT_FAILED = 2,
    AGL_LLAMA_MTP_DECODE_FAILED = 3,
    AGL_LLAMA_MTP_OVERFLOW = 4,
    AGL_LLAMA_MTP_EXCEPTION = 5,
};

struct agl_llama_mtp_stats {
    uint64_t draft_calls;
    uint64_t empty_drafts;
    uint64_t drafted_tokens;
    uint64_t accepted_tokens;
};

struct agl_llama_mtp_speculative {
    common_params_speculative params;
    common_speculative * spec = nullptr;
    std::vector<llama_token> prompt;
    std::vector<llama_token> draft;
    size_t last_draft_len = 0;
    bool draft_pending = false;
    agl_llama_mtp_stats stats {};
};

agl_llama_mtp_speculative * agl_llama_mtp_init(
    llama_context * ctx_tgt,
    llama_context * ctx_dft,
    int32_t n_max,
    int32_t n_min,
    float p_min,
    char * err,
    size_t err_len) {
    if (ctx_tgt == nullptr || ctx_dft == nullptr || n_max <= 0 || n_min < 0 || n_min > n_max) {
        agl_mtp_copy_cstr(err, err_len, "invalid MTP speculative parameters");
        return nullptr;
    }

    try {
        auto wrapper = std::make_unique<agl_llama_mtp_speculative>();
        wrapper->params.types = { COMMON_SPECULATIVE_TYPE_DRAFT_MTP };
        wrapper->params.draft.ctx_tgt = ctx_tgt;
        wrapper->params.draft.ctx_dft = ctx_dft;
        wrapper->params.draft.n_max = n_max;
        wrapper->params.draft.n_min = n_min;
        wrapper->params.draft.p_min = p_min;

        wrapper->spec = common_speculative_init(wrapper->params, 1);
        if (wrapper->spec == nullptr) {
            agl_mtp_copy_cstr(err, err_len, "llama.cpp failed to initialize MTP speculative decoding");
            return nullptr;
        }

        return wrapper.release();
    } catch (const std::exception & ex) {
        agl_mtp_copy_cstr(err, err_len, ex.what());
        return nullptr;
    } catch (...) {
        agl_mtp_copy_cstr(err, err_len, "unknown llama.cpp MTP initialization error");
        return nullptr;
    }
}

void agl_llama_mtp_free(agl_llama_mtp_speculative * spec) {
    if (spec == nullptr) {
        return;
    }
    if (spec->spec != nullptr) {
        common_speculative_free(spec->spec);
        spec->spec = nullptr;
    }
    delete spec;
}

agl_llama_mtp_status agl_llama_mtp_begin(
    agl_llama_mtp_speculative * spec,
    const llama_token * prompt_tokens,
    size_t prompt_tokens_count) {
    if (spec == nullptr || spec->spec == nullptr || (prompt_tokens == nullptr && prompt_tokens_count > 0)) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }

    try {
        agl_mtp_assign_tokens(spec->prompt, prompt_tokens, prompt_tokens_count);
        spec->draft.clear();
        spec->last_draft_len = 0;
        spec->draft_pending = false;
        common_speculative_begin(spec->spec, AGL_LLAMA_MTP_SEQ_ID, spec->prompt);
        return AGL_LLAMA_MTP_OK;
    } catch (...) {
        return AGL_LLAMA_MTP_EXCEPTION;
    }
}

agl_llama_mtp_status agl_llama_mtp_process(
    agl_llama_mtp_speculative * spec,
    const llama_batch * batch) {
    if (spec == nullptr || spec->spec == nullptr || batch == nullptr) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }
    if (!agl_mtp_batch_compatible(*batch)) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }

    try {
        return common_speculative_process(spec->spec, *batch)
            ? AGL_LLAMA_MTP_OK
            : AGL_LLAMA_MTP_DECODE_FAILED;
    } catch (...) {
        return AGL_LLAMA_MTP_EXCEPTION;
    }
}

agl_llama_mtp_status agl_llama_mtp_draft(
    agl_llama_mtp_speculative * spec,
    llama_pos n_past,
    llama_token id_last,
    const llama_token * prompt_tokens,
    size_t prompt_tokens_count,
    llama_token * out_tokens,
    size_t out_tokens_capacity,
    size_t * out_tokens_count) {
    if (spec == nullptr || spec->spec == nullptr || (prompt_tokens == nullptr && prompt_tokens_count > 0) ||
        out_tokens_count == nullptr || n_past < 0) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }

    try {
        if (spec->draft_pending) {
            return AGL_LLAMA_MTP_INVALID_ARGUMENT;
        }

        agl_mtp_assign_tokens(spec->prompt, prompt_tokens, prompt_tokens_count);
        spec->draft.clear();
        spec->last_draft_len = 0;
        spec->stats.draft_calls++;

        auto & params = common_speculative_get_draft_params(spec->spec, AGL_LLAMA_MTP_SEQ_ID);
        params = {
            true,
            spec->params.draft.n_max,
            n_past,
            id_last,
            &spec->prompt,
            &spec->draft,
        };

        common_speculative_draft(spec->spec);

        *out_tokens_count = spec->draft.size();
        spec->last_draft_len = spec->draft.size();
        spec->draft_pending = !spec->draft.empty();
        if (spec->draft.empty()) {
            spec->stats.empty_drafts++;
            return AGL_LLAMA_MTP_OK;
        }
        if (spec->draft.size() > out_tokens_capacity) {
            return AGL_LLAMA_MTP_OVERFLOW;
        }
        if (out_tokens == nullptr) {
            return AGL_LLAMA_MTP_INVALID_ARGUMENT;
        }

        std::memcpy(out_tokens, spec->draft.data(), spec->draft.size() * sizeof(llama_token));
        spec->stats.drafted_tokens += spec->draft.size();
        return AGL_LLAMA_MTP_OK;
    } catch (...) {
        return AGL_LLAMA_MTP_EXCEPTION;
    }
}

agl_llama_mtp_status agl_llama_mtp_accept(
    agl_llama_mtp_speculative * spec,
    uint16_t n_accepted) {
    if (spec == nullptr || spec->spec == nullptr) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }
    if (!spec->draft_pending && n_accepted == 0) {
        return AGL_LLAMA_MTP_OK;
    }
    if (!spec->draft_pending && n_accepted > 0) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }
    if (n_accepted > spec->last_draft_len) {
        return AGL_LLAMA_MTP_INVALID_ARGUMENT;
    }

    try {
        common_speculative_accept(spec->spec, AGL_LLAMA_MTP_SEQ_ID, n_accepted);
        spec->stats.accepted_tokens += n_accepted;
        spec->last_draft_len = 0;
        spec->draft_pending = false;
        return AGL_LLAMA_MTP_OK;
    } catch (...) {
        return AGL_LLAMA_MTP_EXCEPTION;
    }
}

agl_llama_mtp_stats agl_llama_mtp_get_stats(const agl_llama_mtp_speculative * spec) {
    if (spec == nullptr) {
        return {};
    }
    return spec->stats;
}

}
