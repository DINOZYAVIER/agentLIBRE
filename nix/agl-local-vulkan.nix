{ pkgs ? import <nixpkgs> { } }:

let
  runtimeLibraryPath = pkgs.lib.makeLibraryPath [
    pkgs.libdrm
    pkgs.libglvnd
    pkgs.libxkbcommon
    pkgs.mesa
    pkgs.vulkan-loader
    pkgs.wayland
    pkgs.libx11
    pkgs.libXau
    pkgs.libXdmcp
    pkgs.libxcb
    pkgs.libxshmfence
    pkgs.zlib
    pkgs.zstd
  ];
in
pkgs.mkShell {
  packages = with pkgs; [
    bashInteractive
    binutils
    cargo
    cmake
    curl
    gcc
    git
    glslang
    ninja
    pkg-config
    python3
    rustc
    rustfmt
    clippy
    shaderc
    spirv-headers
    spirv-tools
    vulkan-headers
    vulkan-loader
    vulkan-tools
  ];

  AGL_LLAMA_CPP_VULKAN_INCLUDE_DIR = "${pkgs.vulkan-headers}/include";
  AGL_LLAMA_CPP_VULKAN_LIBRARY = "${pkgs.vulkan-loader}/lib/libvulkan.so";
  AGL_LLAMA_CPP_VULKAN_GLSLC = "${pkgs.shaderc}/bin/glslc";
  AGL_LLAMA_CPP_VULKAN_GLSLANG_VALIDATOR = "${pkgs.glslang}/bin/glslangValidator";

  shellHook = ''
    export AGL_NIX_VULKAN_SHELL=1
    export CMAKE_PREFIX_PATH="${pkgs.spirv-headers}:${pkgs.spirv-tools}''${CMAKE_PREFIX_PATH:+:''${CMAKE_PREFIX_PATH}}"
    export LD_LIBRARY_PATH="${runtimeLibraryPath}''${LD_LIBRARY_PATH:+:''${LD_LIBRARY_PATH}}"

    if [ -d /run/opengl-driver/share ]; then
      export XDG_DATA_DIRS="/run/opengl-driver/share''${XDG_DATA_DIRS:+:''${XDG_DATA_DIRS}}"
    fi

    if [ -z "''${VK_DRIVER_FILES:-}" ]; then
      for candidate in \
        /run/opengl-driver/share/vulkan/icd.d/radeon_icd.x86_64.json \
        /run/opengl-driver/share/vulkan/icd.d/nvidia_icd.json \
        /run/opengl-driver/share/vulkan/icd.d/intel_icd.x86_64.json \
        /run/opengl-driver/share/vulkan/icd.d/intel_hasvk_icd.x86_64.json
      do
        if [ -f "$candidate" ]; then
          export VK_DRIVER_FILES="$candidate"
          break
        fi
      done
    fi

    if [ -z "''${VK_ICD_FILENAMES:-}" ] && [ -n "''${VK_DRIVER_FILES:-}" ]; then
      export VK_ICD_FILENAMES="$VK_DRIVER_FILES"
    fi
  '';
}
