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

if [[ -z "$vulkan_include_dir" ]]; then
  for candidate in /run/current-system/sw/include /nix/store/*-vulkan-headers-*/include; do
    if [[ -f "$candidate/vulkan/vulkan.h" ]]; then
      vulkan_include_dir="$candidate"
      break
    fi
  done
fi

if [[ -z "$vulkan_library" ]]; then
  for candidate in /run/current-system/sw/lib/libvulkan.so /nix/store/*-vulkan-loader-*/lib/libvulkan.so; do
    if [[ -f "$candidate" ]]; then
      vulkan_library="$candidate"
      break
    fi
  done
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

cmake_args=(
  -S "$source_dir"
  -B "$build_dir"
  -DGGML_VULKAN=ON \
  -DLLAMA_BUILD_TESTS=OFF \
  -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_TOOLS=ON \
  -DLLAMA_BUILD_SERVER=OFF \
  -DLLAMA_BUILD_APP=OFF \
  -DMTMD_VIDEO=OFF
)

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

cmake --build "$build_dir" --target llama llama-common mtmd --parallel "$jobs"

printf '%s\n' "$build_dir/bin"
