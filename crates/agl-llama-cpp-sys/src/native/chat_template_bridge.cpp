#include "chat.h"
#include "llama.h"
#include "nlohmann/json.hpp"

#include <algorithm>
#include <cctype>
#include <cstdint>
#include <cstring>
#include <exception>
#include <limits>
#include <stdexcept>
#include <string>
#include <utility>

namespace {

struct agl_llama_chat_tool_call {
    const char * name;
    const char * arguments;
    const char * id;
};

struct agl_llama_chat_tool {
    const char * name;
    const char * description;
    const char * parameters;
};

struct agl_llama_chat_message {
    const char *                     role;
    const char *                     content;
    const char *                     name;
    const agl_llama_chat_tool_call * tool_calls;
    size_t                           n_tool_calls;
};

struct agl_llama_generation_plan {
    char *  prompt;
    size_t  prompt_len;
    char *  grammar;
    size_t  grammar_len;
    int32_t grammar_lazy;
    int32_t grammar_needs_prefill;
    char *  grammar_triggers_json;
    size_t  grammar_triggers_json_len;
    char *  grammar_prefill_tokens_json;
    size_t  grammar_prefill_tokens_json_len;
    char *  additional_stops_json;
    size_t  additional_stops_json_len;
    char *  preserved_tokens_json;
    size_t  preserved_tokens_json_len;
    char *  generation_prompt;
    size_t  generation_prompt_len;
    int32_t format;
    char *  parser;
    size_t  parser_len;
};

void agl_copy_cstr(char * dst, size_t dst_len, const std::string & src) {
    if (dst == nullptr || dst_len == 0) {
        return;
    }

    const size_t count = std::min(dst_len - 1, src.size());
    std::memcpy(dst, src.data(), count);
    dst[count] = '\0';
}

char * agl_alloc_cstr(const std::string & value) {
    auto * result = new char[value.size() + 1];
    std::memcpy(result, value.data(), value.size());
    result[value.size()] = '\0';
    return result;
}

void agl_reset_plan(agl_llama_generation_plan * plan) {
    if (plan == nullptr) {
        return;
    }
    plan->prompt                          = nullptr;
    plan->prompt_len                      = 0;
    plan->grammar                         = nullptr;
    plan->grammar_len                     = 0;
    plan->grammar_lazy                    = 0;
    plan->grammar_needs_prefill           = 0;
    plan->grammar_triggers_json           = nullptr;
    plan->grammar_triggers_json_len       = 0;
    plan->grammar_prefill_tokens_json     = nullptr;
    plan->grammar_prefill_tokens_json_len = 0;
    plan->additional_stops_json           = nullptr;
    plan->additional_stops_json_len       = 0;
    plan->preserved_tokens_json           = nullptr;
    plan->preserved_tokens_json_len       = 0;
    plan->generation_prompt               = nullptr;
    plan->generation_prompt_len           = 0;
    plan->format                          = 0;
    plan->parser                          = nullptr;
    plan->parser_len                      = 0;
}

void agl_free_plan_fields(agl_llama_generation_plan * plan) {
    if (plan == nullptr) {
        return;
    }
    delete[] plan->prompt;
    delete[] plan->grammar;
    delete[] plan->grammar_triggers_json;
    delete[] plan->grammar_prefill_tokens_json;
    delete[] plan->additional_stops_json;
    delete[] plan->preserved_tokens_json;
    delete[] plan->generation_prompt;
    delete[] plan->parser;
    agl_reset_plan(plan);
}

int32_t agl_return_prompt(const std::string & prompt, char * buf, size_t buf_len, char * err, size_t err_len) {
    if (prompt.size() > static_cast<size_t>(std::numeric_limits<int32_t>::max())) {
        agl_copy_cstr(err, err_len, "rendered chat template exceeds i32");
        return -1;
    }

    agl_copy_cstr(buf, buf_len, prompt);
    return static_cast<int32_t>(prompt.size());
}

}  // namespace

extern "C" int32_t agl_llama_common_chat_apply_template(const llama_model *            model,
                                                        const agl_llama_chat_message * chat,
                                                        size_t                         n_msg,
                                                        bool                           add_assistant,
                                                        char *                         buf,
                                                        size_t                         buf_len,
                                                        char *                         err,
                                                        size_t                         err_len) {
    try {
        common_chat_templates_ptr templates = common_chat_templates_init(model, "");

        common_chat_templates_inputs inputs;
        inputs.add_generation_prompt = add_assistant;
        inputs.use_jinja             = true;
        inputs.enable_thinking       = false;
        inputs.messages.reserve(n_msg);

        for (size_t i = 0; i < n_msg; ++i) {
            common_chat_msg message;
            message.role      = chat[i].role == nullptr ? "" : chat[i].role;
            message.content   = chat[i].content == nullptr ? "" : chat[i].content;
            message.tool_name = chat[i].name == nullptr ? "" : chat[i].name;
            if (chat[i].tool_calls != nullptr) {
                message.tool_calls.reserve(chat[i].n_tool_calls);
                for (size_t j = 0; j < chat[i].n_tool_calls; ++j) {
                    common_chat_tool_call tool_call;
                    const auto &          raw = chat[i].tool_calls[j];
                    tool_call.name            = raw.name == nullptr ? "" : raw.name;
                    tool_call.arguments       = raw.arguments == nullptr ? "{}" : raw.arguments;
                    tool_call.id              = raw.id == nullptr ? "" : raw.id;
                    message.tool_calls.push_back(std::move(tool_call));
                }
            }
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

extern "C" int32_t agl_llama_common_chat_build_generation_plan(const llama_model *            model,
                                                               const agl_llama_chat_message * chat,
                                                               size_t                         n_msg,
                                                               const agl_llama_chat_tool *    tools,
                                                               size_t                         n_tools,
                                                               bool                           add_assistant,
                                                               agl_llama_generation_plan *    plan,
                                                               char *                         err,
                                                               size_t                         err_len) {
    if (plan == nullptr) {
        agl_copy_cstr(err, err_len, "generation plan output is null");
        return -1;
    }
    agl_reset_plan(plan);

    try {
        if (model == nullptr) {
            throw std::invalid_argument("generation plan model is null");
        }
        if (n_msg != 0 && chat == nullptr) {
            throw std::invalid_argument("generation plan messages are null");
        }
        if (n_tools != 0 && tools == nullptr) {
            throw std::invalid_argument("generation plan tools are null");
        }
        common_chat_templates_ptr templates = common_chat_templates_init(model, "");

        common_chat_templates_inputs inputs;
        inputs.add_generation_prompt = add_assistant;
        inputs.use_jinja             = true;
        inputs.enable_thinking       = false;
        inputs.tool_choice           = COMMON_CHAT_TOOL_CHOICE_AUTO;
        inputs.parallel_tool_calls   = false;
        inputs.messages.reserve(n_msg);
        inputs.tools.reserve(n_tools);

        for (size_t i = 0; i < n_msg; ++i) {
            if (chat[i].n_tool_calls != 0 && chat[i].tool_calls == nullptr) {
                throw std::invalid_argument("generation plan message tool calls are null");
            }
            common_chat_msg message;
            message.role      = chat[i].role == nullptr ? "" : chat[i].role;
            message.content   = chat[i].content == nullptr ? "" : chat[i].content;
            message.tool_name = chat[i].name == nullptr ? "" : chat[i].name;
            if (chat[i].tool_calls != nullptr) {
                message.tool_calls.reserve(chat[i].n_tool_calls);
                for (size_t j = 0; j < chat[i].n_tool_calls; ++j) {
                    common_chat_tool_call tool_call;
                    const auto &          raw = chat[i].tool_calls[j];
                    tool_call.name            = raw.name == nullptr ? "" : raw.name;
                    tool_call.arguments       = raw.arguments == nullptr ? "{}" : raw.arguments;
                    tool_call.id              = raw.id == nullptr ? "" : raw.id;
                    message.tool_calls.push_back(std::move(tool_call));
                }
            }
            inputs.messages.push_back(std::move(message));
        }

        for (size_t i = 0; i < n_tools; ++i) {
            if (tools[i].name == nullptr || tools[i].description == nullptr || tools[i].parameters == nullptr) {
                throw std::invalid_argument("generation plan tool field is null");
            }
            common_chat_tool tool;
            tool.name        = tools[i].name == nullptr ? "" : tools[i].name;
            tool.description = tools[i].description == nullptr ? "" : tools[i].description;
            tool.parameters  = tools[i].parameters == nullptr ? "{}" : tools[i].parameters;
            inputs.tools.push_back(std::move(tool));
        }

        common_chat_params     params   = common_chat_templates_apply(templates.get(), inputs);
        nlohmann::ordered_json triggers = nlohmann::ordered_json::array();
        for (const auto & trigger : params.grammar_triggers) {
            triggers.push_back({
                { "type",  static_cast<int32_t>(trigger.type)  },
                { "value", trigger.value                       },
                { "token", static_cast<int32_t>(trigger.token) },
            });
        }
        nlohmann::ordered_json prefill_tokens = nlohmann::ordered_json::array();
        if (!params.generation_prompt.empty()) {
            const llama_vocab * vocab  = llama_model_get_vocab(model);
            const auto          tokens = common_tokenize(vocab, params.generation_prompt, false, true);
            for (size_t i = 0; i < tokens.size(); ++i) {
                const std::string piece = common_token_to_piece(vocab, tokens[i], true);
                if (i == 0 && !piece.empty() && std::isspace(static_cast<unsigned char>(piece[0])) &&
                    !std::isspace(static_cast<unsigned char>(params.generation_prompt[0]))) {
                    continue;
                }
                prefill_tokens.push_back(tokens[i]);
            }
        }

        const std::string triggers_json         = triggers.dump();
        const std::string prefill_tokens_json   = prefill_tokens.dump();
        const std::string additional_stops_json = nlohmann::ordered_json(params.additional_stops).dump();
        const std::string preserved_tokens_json = nlohmann::ordered_json(params.preserved_tokens).dump();

        plan->prompt                          = agl_alloc_cstr(params.prompt);
        plan->prompt_len                      = params.prompt.size();
        plan->grammar                         = agl_alloc_cstr(params.grammar);
        plan->grammar_len                     = params.grammar.size();
        plan->grammar_lazy                    = params.grammar_lazy ? 1 : 0;
        plan->grammar_needs_prefill           = !params.grammar_lazy && !params.grammar.empty() && n_tools != 0 ? 1 : 0;
        plan->grammar_triggers_json           = agl_alloc_cstr(triggers_json);
        plan->grammar_triggers_json_len       = triggers_json.size();
        plan->grammar_prefill_tokens_json     = agl_alloc_cstr(prefill_tokens_json);
        plan->grammar_prefill_tokens_json_len = prefill_tokens_json.size();
        plan->additional_stops_json           = agl_alloc_cstr(additional_stops_json);
        plan->additional_stops_json_len       = additional_stops_json.size();
        plan->preserved_tokens_json           = agl_alloc_cstr(preserved_tokens_json);
        plan->preserved_tokens_json_len       = preserved_tokens_json.size();
        plan->generation_prompt               = agl_alloc_cstr(params.generation_prompt);
        plan->generation_prompt_len           = params.generation_prompt.size();
        plan->format                          = static_cast<int32_t>(params.format);
        plan->parser                          = agl_alloc_cstr(params.parser);
        plan->parser_len                      = params.parser.size();
        return 0;
    } catch (const std::exception & ex) {
        agl_free_plan_fields(plan);
        agl_copy_cstr(err, err_len, ex.what());
        return -1;
    } catch (...) {
        agl_free_plan_fields(plan);
        agl_copy_cstr(err, err_len, "unknown llama.cpp generation plan error");
        return -1;
    }
}

extern "C" void agl_llama_generation_plan_free(agl_llama_generation_plan * plan) {
    agl_free_plan_fields(plan);
}
