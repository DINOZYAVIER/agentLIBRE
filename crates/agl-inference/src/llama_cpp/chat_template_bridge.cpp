#include "chat.h"
#include "llama.h"

#include <algorithm>
#include <cstdint>
#include <cstring>
#include <exception>
#include <limits>
#include <string>
#include <utility>

namespace {

void agl_copy_cstr(char * dst, size_t dst_len, const std::string & src) {
    if (dst == nullptr || dst_len == 0) {
        return;
    }

    const size_t count = std::min(dst_len - 1, src.size());
    std::memcpy(dst, src.data(), count);
    dst[count] = '\0';
}

int32_t agl_return_prompt(const std::string & prompt, char * buf, size_t buf_len, char * err, size_t err_len) {
    if (prompt.size() > static_cast<size_t>(std::numeric_limits<int32_t>::max())) {
        agl_copy_cstr(err, err_len, "rendered chat template exceeds i32");
        return -1;
    }

    agl_copy_cstr(buf, buf_len, prompt);
    return static_cast<int32_t>(prompt.size());
}

} // namespace

extern "C" int32_t agl_llama_common_chat_apply_template(
        const llama_model * model,
        const llama_chat_message * chat,
        size_t n_msg,
        bool add_assistant,
        char * buf,
        size_t buf_len,
        char * err,
        size_t err_len) {
    try {
        common_chat_templates_ptr templates = common_chat_templates_init(model, "");

        common_chat_templates_inputs inputs;
        inputs.add_generation_prompt = add_assistant;
        inputs.use_jinja = true;
        inputs.enable_thinking = false;
        inputs.messages.reserve(n_msg);

        for (size_t i = 0; i < n_msg; ++i) {
            common_chat_msg message;
            message.role = chat[i].role == nullptr ? "" : chat[i].role;
            message.content = chat[i].content == nullptr ? "" : chat[i].content;
            inputs.messages.push_back(std::move(message));
        }

        common_chat_params params = common_chat_templates_apply(templates.get(), inputs);
        return agl_return_prompt(params.prompt, buf, buf_len, err, err_len);
    } catch (const std::exception & ex) {
        agl_copy_cstr(err, err_len, ex.what());
        return -1;
    } catch (...) {
        agl_copy_cstr(err, err_len, "unknown llama.cpp common chat template error");
        return -1;
    }
}
