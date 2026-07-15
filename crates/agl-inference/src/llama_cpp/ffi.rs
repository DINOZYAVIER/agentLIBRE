#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

use std::ffi::{c_char, c_float, c_int, c_void};

pub(crate) type llama_token = i32;
pub(crate) type llama_pos = i32;
pub(crate) type llama_seq_id = i32;
pub(crate) type llama_memory_t = *mut c_void;
pub(crate) type ggml_backend_dev_t = *mut c_void;

pub(crate) const LLAMA_SPLIT_MODE_LAYER: c_int = 1;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_AUTO: c_int = -1;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_DISABLED: c_int = 0;
pub(crate) const LLAMA_FLASH_ATTN_TYPE_ENABLED: c_int = 1;
pub(crate) const GGML_TYPE_F32: c_int = 0;
pub(crate) const GGML_TYPE_F16: c_int = 1;
pub(crate) const GGML_TYPE_Q4_0: c_int = 2;
pub(crate) const GGML_TYPE_Q4_1: c_int = 3;
pub(crate) const GGML_TYPE_Q5_0: c_int = 6;
pub(crate) const GGML_TYPE_Q5_1: c_int = 7;
pub(crate) const GGML_TYPE_Q8_0: c_int = 8;
pub(crate) const GGML_TYPE_IQ4_NL: c_int = 20;
pub(crate) const GGML_TYPE_BF16: c_int = 30;

#[repr(C)]
pub(crate) struct llama_model_params {
    pub devices: *mut ggml_backend_dev_t,
    pub tensor_buft_overrides: *const c_void,
    pub n_gpu_layers: c_int,
    pub split_mode: c_int,
    pub main_gpu: c_int,
    pub tensor_split: *const c_float,
    pub progress_callback: *mut c_void,
    pub progress_callback_user_data: *mut c_void,
    pub kv_overrides: *const c_void,
    pub vocab_only: bool,
    pub use_mmap: bool,
    pub use_direct_io: bool,
    pub use_mlock: bool,
    pub check_tensors: bool,
    pub use_extra_bufts: bool,
    pub no_host: bool,
    pub no_alloc: bool,
}

#[repr(C)]
pub(crate) struct llama_context_params {
    pub n_ctx: u32,
    pub n_batch: u32,
    pub n_ubatch: u32,
    pub n_seq_max: u32,
    pub n_rs_seq: u32,
    pub n_outputs_max: u32,
    pub n_threads: c_int,
    pub n_threads_batch: c_int,
    pub ctx_type: c_int,
    pub rope_scaling_type: c_int,
    pub pooling_type: c_int,
    pub attention_type: c_int,
    pub flash_attn_type: c_int,
    pub rope_freq_base: c_float,
    pub rope_freq_scale: c_float,
    pub yarn_ext_factor: c_float,
    pub yarn_attn_factor: c_float,
    pub yarn_beta_fast: c_float,
    pub yarn_beta_slow: c_float,
    pub yarn_orig_ctx: u32,
    pub defrag_thold: c_float,
    pub cb_eval: *mut c_void,
    pub cb_eval_user_data: *mut c_void,
    pub type_k: c_int,
    pub type_v: c_int,
    pub abort_callback: *mut c_void,
    pub abort_callback_data: *mut c_void,
    pub embeddings: bool,
    pub offload_kqv: bool,
    pub no_perf: bool,
    pub op_offload: bool,
    pub swa_full: bool,
    pub kv_unified: bool,
    pub samplers: *mut c_void,
    pub n_samplers: usize,
    pub ctx_other: *mut c_void,
}

#[repr(C)]
pub(crate) struct llama_sampler_chain_params {
    pub no_perf: bool,
}

#[repr(C)]
pub(crate) struct llama_batch {
    pub n_tokens: i32,
    pub token: *mut llama_token,
    pub embd: *mut c_float,
    pub pos: *mut llama_pos,
    pub n_seq_id: *mut i32,
    pub seq_id: *mut *mut llama_seq_id,
    pub logits: *mut i8,
}

#[repr(C)]
pub(crate) struct llama_chat_message {
    pub role: *const c_char,
    pub content: *const c_char,
}

unsafe extern "C" {
    pub(crate) fn ggml_backend_load_all_from_path(dir_path: *const c_char);
    pub(crate) fn ggml_backend_dev_count() -> usize;
    pub(crate) fn ggml_backend_dev_get(index: usize) -> ggml_backend_dev_t;
    pub(crate) fn ggml_backend_dev_by_name(name: *const c_char) -> ggml_backend_dev_t;
    pub(crate) fn ggml_backend_dev_name(device: ggml_backend_dev_t) -> *const c_char;
    pub(crate) fn ggml_backend_dev_description(device: ggml_backend_dev_t) -> *const c_char;
    pub(crate) fn ggml_backend_dev_memory(
        device: ggml_backend_dev_t,
        free: *mut usize,
        total: *mut usize,
    );

    pub(crate) fn llama_backend_init();
    pub(crate) fn llama_log_set(
        log_callback: Option<unsafe extern "C" fn(c_int, *const c_char, *mut c_void)>,
        user_data: *mut c_void,
    );
    pub(crate) fn llama_model_default_params() -> llama_model_params;
    pub(crate) fn llama_context_default_params() -> llama_context_params;
    pub(crate) fn llama_sampler_chain_default_params() -> llama_sampler_chain_params;
    pub(crate) fn llama_model_load_from_file(
        path_model: *const c_char,
        params: llama_model_params,
    ) -> *mut c_void;
    pub(crate) fn llama_model_free(model: *mut c_void);
    pub(crate) fn llama_init_from_model(
        model: *mut c_void,
        params: llama_context_params,
    ) -> *mut c_void;
    pub(crate) fn llama_free(ctx: *mut c_void);
    pub(crate) fn llama_model_get_vocab(model: *const c_void) -> *const c_void;
    pub(crate) fn llama_model_chat_template(
        model: *const c_void,
        name: *const c_char,
    ) -> *const c_char;
    pub(crate) fn llama_model_desc(model: *const c_void, buf: *mut c_char, buf_size: usize) -> i32;
    pub(crate) fn llama_n_ctx(ctx: *const c_void) -> u32;
    pub(crate) fn llama_get_memory(ctx: *const c_void) -> llama_memory_t;
    pub(crate) fn llama_memory_seq_pos_max(mem: llama_memory_t, seq_id: llama_seq_id) -> llama_pos;
    pub(crate) fn llama_print_system_info() -> *const c_char;
    pub(crate) fn llama_supports_gpu_offload() -> bool;

    pub(crate) fn llama_chat_apply_template(
        tmpl: *const c_char,
        chat: *const llama_chat_message,
        n_msg: usize,
        add_ass: bool,
        buf: *mut c_char,
        length: i32,
    ) -> i32;
    pub(crate) fn agl_llama_common_chat_apply_template(
        model: *const c_void,
        chat: *const llama_chat_message,
        n_msg: usize,
        add_ass: bool,
        buf: *mut c_char,
        buf_len: usize,
        err: *mut c_char,
        err_len: usize,
    ) -> i32;
    pub(crate) fn llama_tokenize(
        vocab: *const c_void,
        text: *const c_char,
        text_len: i32,
        tokens: *mut llama_token,
        n_tokens_max: i32,
        add_special: bool,
        parse_special: bool,
    ) -> i32;
    pub(crate) fn llama_token_to_piece(
        vocab: *const c_void,
        token: llama_token,
        buf: *mut c_char,
        length: i32,
        lstrip: i32,
        special: bool,
    ) -> i32;
    pub(crate) fn llama_batch_get_one(tokens: *mut llama_token, n_tokens: i32) -> llama_batch;
    pub(crate) fn llama_decode(ctx: *mut c_void, batch: llama_batch) -> i32;
    pub(crate) fn llama_vocab_is_eog(vocab: *const c_void, token: llama_token) -> bool;
    pub(crate) fn llama_sampler_chain_init(params: llama_sampler_chain_params) -> *mut c_void;
    pub(crate) fn llama_sampler_chain_add(chain: *mut c_void, sampler: *mut c_void);
    pub(crate) fn llama_sampler_init_greedy() -> *mut c_void;
    pub(crate) fn llama_sampler_sample(
        sampler: *mut c_void,
        ctx: *mut c_void,
        idx: i32,
    ) -> llama_token;
    pub(crate) fn llama_sampler_free(sampler: *mut c_void);
}
