#include "llama.h"
#include "mtmd.h"

#include <cstddef>
#include <cstdint>
#include <type_traits>

static_assert(sizeof(llama_token) == sizeof(int32_t), "llama_token ABI changed");
static_assert(sizeof(llama_pos) == sizeof(int32_t), "llama_pos ABI changed");
static_assert(sizeof(llama_seq_id) == sizeof(int32_t), "llama_seq_id ABI changed");

static_assert(std::is_standard_layout<llama_batch>::value, "llama_batch must remain C layout");
static_assert(offsetof(llama_batch, n_tokens) == 0, "llama_batch.n_tokens offset changed");
static_assert(std::is_same_v<decltype(llama_batch::n_tokens), int32_t>, "llama_batch.n_tokens ABI changed");
static_assert(std::is_same_v<decltype(llama_batch::token), llama_token *>, "llama_batch.token ABI changed");
static_assert(std::is_same_v<decltype(llama_batch::embd), float *>, "llama_batch.embd ABI changed");
static_assert(std::is_same_v<decltype(llama_batch::pos), llama_pos *>, "llama_batch.pos ABI changed");
static_assert(std::is_same_v<decltype(llama_batch::n_seq_id), int32_t *>, "llama_batch.n_seq_id ABI changed");
static_assert(std::is_same_v<decltype(llama_batch::seq_id), llama_seq_id **>, "llama_batch.seq_id ABI changed");
static_assert(std::is_same_v<decltype(llama_batch::logits), int8_t *>, "llama_batch.logits ABI changed");

static_assert(std::is_same_v<decltype(llama_model_params::devices), ggml_backend_dev_t *>, "llama_model_params.devices ABI changed");
static_assert(std::is_same_v<decltype(llama_model_params::n_gpu_layers), int32_t>, "llama_model_params.n_gpu_layers ABI changed");
static_assert(std::is_same_v<decltype(llama_model_params::tensor_split), const float *>, "llama_model_params.tensor_split ABI changed");
static_assert(std::is_same_v<decltype(llama_model_params::vocab_only), bool>, "llama_model_params boolean ABI changed");
static_assert(std::is_same_v<decltype(llama_model_params::no_alloc), bool>, "llama_model_params.no_alloc ABI changed");

static_assert(std::is_same_v<decltype(llama_context_params::n_ctx), uint32_t>, "llama_context_params.n_ctx ABI changed");
static_assert(std::is_same_v<decltype(llama_context_params::n_threads), int32_t>, "llama_context_params.n_threads ABI changed");
static_assert(std::is_same_v<decltype(llama_context_params::rope_freq_base), float>, "llama_context_params.rope_freq_base ABI changed");
static_assert(std::is_same_v<decltype(llama_context_params::type_k), ggml_type>, "llama_context_params.type_k ABI changed");
static_assert(std::is_same_v<decltype(llama_context_params::type_v), ggml_type>, "llama_context_params.type_v ABI changed");
static_assert(std::is_same_v<decltype(llama_context_params::kv_unified), bool>, "llama_context_params.kv_unified ABI changed");
static_assert(std::is_same_v<decltype(llama_context_params::n_samplers), size_t>, "llama_context_params.n_samplers ABI changed");

static_assert(std::is_same_v<decltype(&llama_model_default_params), llama_model_params (*)()>, "llama_model_default_params signature changed");
static_assert(std::is_same_v<decltype(&llama_context_default_params), llama_context_params (*)()>, "llama_context_default_params signature changed");
static_assert(std::is_same_v<decltype(&llama_batch_get_one), llama_batch (*)(llama_token *, int32_t)>, "llama_batch_get_one signature changed");
static_assert(std::is_same_v<decltype(&llama_decode), int32_t (*)(llama_context *, llama_batch)>, "llama_decode signature changed");
static_assert(std::is_same_v<decltype(&llama_set_abort_callback), void (*)(llama_context *, ggml_abort_callback, void *)>, "llama_set_abort_callback signature changed");
static_assert(std::is_standard_layout_v<mtmd_input_text>, "mtmd_input_text must remain C layout");
static_assert(std::is_same_v<decltype(mtmd_input_text::text), const char *>, "mtmd_input_text.text ABI changed");
static_assert(std::is_same_v<decltype(&mtmd_free), void (*)(mtmd_context *)>, "mtmd_free signature changed");

extern "C" int agl_llama_cpp_abi_guard(void) {
    return 0;
}
