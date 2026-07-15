#include "llama.h"
#include "mtmd-helper.h"
#include "mtmd.h"

#include <cstdio>
#include <exception>
#include <memory>
#include <vector>

namespace {

void set_error(char * err, size_t err_len, const char * message) {
    if (err == nullptr || err_len == 0) {
        return;
    }
    std::snprintf(err, err_len, "%s", message);
}

struct bitmap_deleter {
    void operator()(mtmd_bitmap * bitmap) const { mtmd_bitmap_free(bitmap); }
};

struct chunks_deleter {
    void operator()(mtmd_input_chunks * chunks) const { mtmd_input_chunks_free(chunks); }
};

bool non_causal_chunks_fit(const mtmd_context *      context,
                           const mtmd_input_chunks * chunks,
                           const llama_context *     llama_context,
                           int32_t                   eval_batch_size,
                           char *                    err,
                           size_t                    err_len) {
    const uint32_t context_batch_size  = llama_n_batch(llama_context);
    const uint32_t context_ubatch_size = llama_n_ubatch(llama_context);
    for (size_t index = 0; index < mtmd_input_chunks_size(chunks); ++index) {
        const mtmd_input_chunk * chunk = mtmd_input_chunks_get(chunks, index);
        if (mtmd_input_chunk_get_type(chunk) == MTMD_INPUT_CHUNK_TYPE_TEXT ||
            !mtmd_decode_use_non_causal(context, chunk)) {
            continue;
        }
        const size_t token_count = mtmd_input_chunk_get_n_tokens(chunk);
        if (token_count <= static_cast<size_t>(eval_batch_size) && token_count <= context_batch_size &&
            token_count <= context_ubatch_size) {
            continue;
        }
        if (err != nullptr && err_len > 0) {
            std::snprintf(err, err_len,
                          "non-causal media chunk requires n_batch and n_ubatch >= %zu "
                          "(eval batch %d, context n_batch %u, context n_ubatch %u)",
                          token_count, eval_batch_size, context_batch_size, context_ubatch_size);
        }
        return false;
    }
    return true;
}

}  // namespace

extern "C" mtmd_context * agl_mtmd_init(const char *        projector_path,
                                        const llama_model * model,
                                        bool                use_gpu,
                                        int32_t             threads,
                                        int32_t             flash_attn_type,
                                        char *              err,
                                        size_t              err_len) try {
    if (projector_path == nullptr || model == nullptr || threads <= 0) {
        set_error(err, err_len, "invalid mtmd initialization arguments");
        return nullptr;
    }
    mtmd_context_params params = mtmd_context_params_default();
    params.use_gpu             = use_gpu;
    params.print_timings       = true;
    params.n_threads           = threads;
    params.flash_attn_type     = static_cast<llama_flash_attn_type>(flash_attn_type);
    params.warmup              = true;
    mtmd_context * context     = mtmd_init_from_file(projector_path, model, params);
    if (context == nullptr) {
        set_error(err, err_len, "mtmd failed to load the multimodal projector");
        return nullptr;
    }
    if (!mtmd_support_vision(context)) {
        mtmd_free(context);
        set_error(err, err_len, "multimodal projector does not support vision input");
        return nullptr;
    }
    return context;
} catch (const std::exception & exception) {
    set_error(err, err_len, exception.what());
    return nullptr;
} catch (...) {
    set_error(err, err_len, "unknown mtmd initialization exception");
    return nullptr;
}

extern "C" const char * agl_mtmd_marker(const mtmd_context * context) {
    return context == nullptr ? nullptr : mtmd_get_marker(context);
}

extern "C" void agl_mtmd_free(mtmd_context * context) {
    mtmd_free(context);
}

extern "C" int32_t agl_mtmd_eval_images(mtmd_context *                context,
                                        llama_context *               llama_context,
                                        const char *                  prompt,
                                        const unsigned char * const * image_data,
                                        const size_t *                image_lengths,
                                        size_t                        image_count,
                                        int32_t                       batch_size,
                                        int32_t *                     out_positions,
                                        size_t *                      out_tokens,
                                        char *                        err,
                                        size_t                        err_len) try {
    if (context == nullptr || llama_context == nullptr || prompt == nullptr || image_data == nullptr ||
        image_lengths == nullptr || image_count == 0 || batch_size <= 0 || out_positions == nullptr ||
        out_tokens == nullptr) {
        set_error(err, err_len, "invalid mtmd evaluation arguments");
        return -1;
    }

    std::vector<std::unique_ptr<mtmd_bitmap, bitmap_deleter>> owned_bitmaps;
    std::vector<const mtmd_bitmap *>                          bitmaps;
    owned_bitmaps.reserve(image_count);
    bitmaps.reserve(image_count);
    for (size_t index = 0; index < image_count; ++index) {
        if (image_data[index] == nullptr || image_lengths[index] == 0) {
            set_error(err, err_len, "empty mtmd image buffer");
            return 1;
        }
        mtmd_helper_bitmap_wrapper wrapper =
            mtmd_helper_bitmap_init_from_buf(context, image_data[index], image_lengths[index], false);
        if (wrapper.video_ctx != nullptr) {
            mtmd_helper_video_free(wrapper.video_ctx);
        }
        if (wrapper.bitmap == nullptr) {
            set_error(err, err_len, "mtmd failed to decode an image buffer");
            return 2;
        }
        owned_bitmaps.emplace_back(wrapper.bitmap);
        bitmaps.push_back(wrapper.bitmap);
    }

    std::unique_ptr<mtmd_input_chunks, chunks_deleter> chunks(mtmd_input_chunks_init());
    if (!chunks) {
        set_error(err, err_len, "mtmd failed to allocate input chunks");
        return 3;
    }
    mtmd_input_text input{ prompt, true, true };
    const int32_t   tokenize_result = mtmd_tokenize(context, chunks.get(), &input, bitmaps.data(), bitmaps.size());
    if (tokenize_result != 0) {
        set_error(err, err_len, "mtmd prompt marker/image count mismatch or preprocessing failure");
        return 10 + tokenize_result;
    }

    const size_t    token_count    = mtmd_helper_get_n_tokens(chunks.get());
    const llama_pos position_count = mtmd_helper_get_n_pos(chunks.get());
    if (token_count == 0 || position_count <= 0 ||
        static_cast<uint32_t>(position_count) >= llama_n_ctx(llama_context)) {
        set_error(err, err_len, "mtmd prompt exceeds or empties the llama context");
        return 20;
    }
    if (!non_causal_chunks_fit(context, chunks.get(), llama_context, batch_size, err, err_len)) {
        return 21;
    }
    llama_pos     new_position = 0;
    const int32_t eval_result =
        mtmd_helper_eval_chunks(context, llama_context, chunks.get(), 0, 0, batch_size, true, &new_position);
    if (eval_result != 0) {
        set_error(err, err_len, "mtmd failed to encode or evaluate multimodal chunks");
        return 30 + eval_result;
    }
    *out_positions = new_position;
    *out_tokens    = token_count;
    return 0;
} catch (const std::exception & exception) {
    set_error(err, err_len, exception.what());
    return 100;
} catch (...) {
    set_error(err, err_len, "unknown mtmd evaluation exception");
    return 101;
}
