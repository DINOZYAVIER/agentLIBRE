#include "ggml-backend.h"
#include "llama-ext.h"

#include <cstdint>
#include <limits>
#include <type_traits>

static_assert(std::is_same_v<decltype(ggml_backend_dev_props::device_id), const char *>,
              "ggml physical device identity ABI changed");

extern "C" const char * agl_ggml_backend_dev_id(ggml_backend_dev_t device) {
    if (device == nullptr) {
        return nullptr;
    }
    ggml_backend_dev_props properties = {};
    ggml_backend_dev_get_props(device, &properties);
    return properties.device_id;
}

struct agl_llama_device_memory_breakdown {
    std::uint64_t model_bytes;
    std::uint64_t context_bytes;
    std::uint64_t compute_bytes;
    std::uint32_t found;
};

static_assert(sizeof(std::size_t) <= sizeof(std::uint64_t),
              "llama.cpp allocation sizes no longer fit the receipt ABI");

static bool checked_add(std::uint64_t & total, std::size_t value) {
    const auto converted = static_cast<std::uint64_t>(value);
    if (converted > std::numeric_limits<std::uint64_t>::max() - total) {
        return false;
    }
    total += converted;
    return true;
}

extern "C" int agl_llama_context_device_memory_breakdown(
        const llama_context * context,
        ggml_backend_dev_t device,
        agl_llama_device_memory_breakdown * output) noexcept {
    if (context == nullptr || device == nullptr || output == nullptr) {
        return -1;
    }
    *output = {};
    try {
        const llama_memory_breakdown breakdown = llama_get_memory_breakdown(context);
        for (const auto & entry : breakdown) {
            const ggml_backend_buffer_type_t buffer_type = entry.first;
            if (ggml_backend_buft_is_host(buffer_type)
                    || ggml_backend_buft_get_device(buffer_type) != device) {
                continue;
            }
            output->found = 1;
            if (!checked_add(output->model_bytes, entry.second.model)
                    || !checked_add(output->context_bytes, entry.second.context)
                    || !checked_add(output->compute_bytes, entry.second.compute)) {
                *output = {};
                return -2;
            }
        }
        return 0;
    } catch (...) {
        *output = {};
        return -3;
    }
}
