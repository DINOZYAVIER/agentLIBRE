#!/usr/bin/env bash
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/.." && pwd)"

source_dir="${AGL_LLAMA_CPP_SOURCE_DIR:-$repo_root/vendor/llama.cpp}"
build_dir="${AGL_LLAMA_CPP_BUILD_DIR:-$repo_root/target/llama-cpp/build}"
jobs="${AGL_LLAMA_CPP_BUILD_JOBS:-$(nproc)}"

if [[ ! -f "$source_dir/CMakeLists.txt" ]]; then
  echo "missing llama.cpp source tree at $source_dir" >&2
  echo "run: git submodule update --init --recursive vendor/llama.cpp" >&2
  exit 1
fi

vulkan_include_dir="${AGL_LLAMA_CPP_VULKAN_INCLUDE_DIR:-}"
vulkan_library="${AGL_LLAMA_CPP_VULKAN_LIBRARY:-}"
vulkan_glslc="${AGL_LLAMA_CPP_VULKAN_GLSLC:-$(command -v glslc || true)}"
vulkan_glslang_validator="${AGL_LLAMA_CPP_VULKAN_GLSLANG_VALIDATOR:-$(command -v glslangValidator || true)}"
spirv_include_dir="${AGL_LLAMA_CPP_SPIRV_INCLUDE_DIR:-}"
cmake_prefixes=()

if [[ -n "$vulkan_include_dir" && ! -f "$vulkan_include_dir/vulkan/vulkan.h" ]]; then
  vulkan_include_dir=""
fi
if [[ -n "$vulkan_library" && ! -f "$vulkan_library" ]]; then
  vulkan_library=""
fi
if [[ -z "$vulkan_glslc" || ! -x "$vulkan_glslc" ]]; then
  vulkan_glslc="$(command -v glslc || true)"
fi
if [[ -z "$vulkan_glslang_validator" || ! -x "$vulkan_glslang_validator" ]]; then
  vulkan_glslang_validator="$(command -v glslangValidator || true)"
fi

if [[ -z "$vulkan_include_dir" ]]; then
  for candidate in \
    /run/current-system/sw/include \
    /usr/local/include \
    /usr/include \
    /nix/store/*-vulkan-headers-*/include
  do
    if [[ -f "$candidate/vulkan/vulkan.h" ]]; then
      vulkan_include_dir="$candidate"
      break
    fi
  done
fi

if [[ -z "$vulkan_library" ]]; then
  for candidate in \
    /run/current-system/sw/lib/libvulkan.so \
    /usr/local/lib/libvulkan.so \
    /usr/local/lib64/libvulkan.so \
    /usr/lib/libvulkan.so \
    /usr/lib64/libvulkan.so \
    /usr/lib/*/libvulkan.so \
    /nix/store/*-vulkan-loader-*/lib/libvulkan.so
  do
    if [[ -f "$candidate" ]]; then
      vulkan_library="$candidate"
      break
    fi
  done
fi

if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists vulkan; then
  if [[ -z "$vulkan_include_dir" ]]; then
    candidate="$(pkg-config --variable=includedir vulkan)"
    if [[ -f "$candidate/vulkan/vulkan.h" ]]; then
      vulkan_include_dir="$candidate"
    fi
  fi
  if [[ -z "$vulkan_library" ]]; then
    candidate="$(pkg-config --variable=libdir vulkan)/libvulkan.so"
    if [[ -f "$candidate" ]]; then
      vulkan_library="$candidate"
    fi
  fi
fi

for candidate in /nix/store/*-spirv-headers-*/share/cmake/SPIRV-Headers/SPIRV-HeadersConfig.cmake; do
  if [[ -f "$candidate" ]]; then
    spirv_prefix="${candidate%/share/cmake/SPIRV-Headers/SPIRV-HeadersConfig.cmake}"
    cmake_prefixes+=("$spirv_prefix")
    if [[ -z "$spirv_include_dir" && -f "$spirv_prefix/include/spirv/unified1/spirv.hpp" ]]; then
      spirv_include_dir="$spirv_prefix/include"
    fi
    break
  fi
done

for candidate in /nix/store/*-spirv-tools-*/lib/cmake/SPIRV-Tools/SPIRV-ToolsConfig.cmake; do
  if [[ -f "$candidate" ]]; then
    cmake_prefixes+=("${candidate%/lib/cmake/SPIRV-Tools/SPIRV-ToolsConfig.cmake}")
    break
  fi
done

if [[ ${#cmake_prefixes[@]} -gt 0 ]]; then
  cmake_prefix_path="$(IFS=:; printf '%s' "${cmake_prefixes[*]}")"
  export CMAKE_PREFIX_PATH="$cmake_prefix_path${CMAKE_PREFIX_PATH:+:$CMAKE_PREFIX_PATH}"
fi

if [[ -n "$spirv_include_dir" ]]; then
  cxx_flags="-I$spirv_include_dir${CXXFLAGS:+ $CXXFLAGS}"
  export CXXFLAGS="$cxx_flags"
fi

vulkan_enabled=OFF
if [[ -n "$vulkan_include_dir" && -n "$vulkan_library" && \
      ( -n "$vulkan_glslc" || -n "$vulkan_glslang_validator" ) ]]; then
  vulkan_enabled=ON
fi

cmake_args=(
  -S "$source_dir"
  -B "$build_dir"
  -DGGML_BACKEND_DL=ON \
  -DGGML_CPU_ALL_VARIANTS=ON \
  -DGGML_NATIVE=OFF \
  -DGGML_VULKAN="$vulkan_enabled" \
  -DLLAMA_BUILD_TESTS=OFF \
  -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_TOOLS=ON \
  -DLLAMA_BUILD_SERVER=OFF \
  -DLLAMA_BUILD_APP=OFF \
  -DMTMD_VIDEO=OFF
)

printf 'llama.cpp dynamic backends: CPU=ON Vulkan=%s\n' "$vulkan_enabled"

# The old statically linked backend build used these same output names. Remove
# them before configuring dynamic modules so a disabled backend cannot survive
# as a stale load candidate in an incremental build directory.
shopt -s nullglob
stale_backend_libraries=(
  "$build_dir"/bin/libggml-cpu.so*
  "$build_dir"/bin/libggml-vulkan.so*
)
shopt -u nullglob
if [[ ${#stale_backend_libraries[@]} -gt 0 ]]; then
  cmake -E rm -f "${stale_backend_libraries[@]}"
fi
if [[ "$vulkan_enabled" == "ON" ]]; then
  # llama.cpp configures its shader generator as a nested ExternalProject. Its
  # cache can otherwise retain extension flags after a compiler/toolchain
  # change and produce a loadable-looking module with unresolved shader data.
  cmake -E rm -rf "$build_dir/ggml/src/ggml-vulkan"
fi

if [[ -n "$vulkan_include_dir" ]]; then
  cmake_args+=("-DVulkan_INCLUDE_DIR=$vulkan_include_dir")
fi
if [[ -n "$vulkan_library" ]]; then
  cmake_args+=("-DVulkan_LIBRARY=$vulkan_library")
fi
if [[ -n "$vulkan_glslc" ]]; then
  cmake_args+=("-DVulkan_GLSLC_EXECUTABLE=$vulkan_glslc")
fi
if [[ -n "$vulkan_glslang_validator" ]]; then
  cmake_args+=("-DVulkan_GLSLANG_VALIDATOR_EXECUTABLE=$vulkan_glslang_validator")
fi
if [[ -n "$spirv_include_dir" ]]; then
  cmake_args+=("-DCMAKE_CXX_FLAGS=$cxx_flags")
fi

cmake "${cmake_args[@]}"

cmake --build "$build_dir" --target llama llama-common mtmd llama-completion --parallel "$jobs"

if [[ "$vulkan_enabled" == "ON" ]]; then
  vulkan_module="$build_dir/bin/libggml-vulkan.so"
  [[ -f "$vulkan_module" ]] || {
    echo "Vulkan backend build did not produce $vulkan_module" >&2
    exit 1
  }
  command -v ldd >/dev/null 2>&1 || {
    echo "ldd is required to validate the Vulkan backend" >&2
    exit 1
  }
  relocation_report="$(ldd -r "$vulkan_module" 2>&1)"
  if [[ "$relocation_report" == *"undefined symbol:"* ]]; then
    echo "Vulkan backend contains unresolved symbols:" >&2
    while IFS= read -r line; do
      if [[ "$line" == *"undefined symbol:"* ]]; then
        printf '%s\n' "$line" >&2
      fi
    done <<<"$relocation_report"
    exit 1
  fi
fi

printf '%s\n' "$build_dir/bin"
