# Strata (AGL-089, D7, D8): the HIP engine of a Strata checkout built with
# nixpkgs' ROCm for the RDNA3 card (gfx1100), its OpenAI-compatible server
# and its tools, with a Python of nixpkgs carrying what `requirements.txt`
# pins. One function for upstream Strata and the RDNA3 fork: `src` is the
# checkout, `rdna3` the fork's own build options (its K-quant prompt kernels
# and rocWMMA's headers, as its `setup.py` builds it).
{
  lib,
  stdenv,
  cmake,
  ninja,
  python3,
  makeWrapper,
  rocmPackages,
  pname,
  version,
  src,
  # llama.cpp at the commit Strata pins (`LLAMA_CPP_COMMIT` of its setup.py):
  # ggml for the build, gguf-py for the tools.
  llamaCpp,
  rdna3 ? false,
}:
let
  python = python3.withPackages (
    ps: with ps; [
      numpy
      jinja2
      regex
      pyyaml
      tqdm
      requests
      pillow
      psutil
      truststore
    ]
  );
in
stdenv.mkDerivation {
  inherit pname version src;

  # ThinkingCap's GGUF carries the MTP (nextn) block: the pack holds the base
  # layers only, the engine takes the MTP layer from --mtp (AGL-089, D9;
  # AGL-070's patch of today's Strata). Upstream and the fork have no such
  # skip.
  patches = [ ./strata/iq-pack-skip-mtp.patch ];

  nativeBuildInputs = [
    cmake
    ninja
    python
    makeWrapper
  ];

  buildInputs = with rocmPackages; [
    clr
    hipblas
    rocblas
    hipblaslt
    rocm-runtime
    # the peer copy of experts between two cards (src/core/peer_experts.cpp)
    rccl
  ];

  cmakeFlags = [
    (lib.cmakeBool "STRATA_ENABLE_HIP" true)
    (lib.cmakeBool "STRATA_ENABLE_CUDA" false)
    (lib.cmakeBool "STRATA_BUILD_TESTS" false)
    (lib.cmakeBool "STRATA_PREFILL_MMQ" true)
    (lib.cmakeBool "STRATA_Q6K_EXPERTS" true)
    (lib.cmakeBool "STRATA_MMQ_KQUANTS" true)
    (lib.cmakeFeature "CMAKE_BUILD_TYPE" "Release")
    (lib.cmakeFeature "CMAKE_HIP_ARCHITECTURES" "gfx1100")
    (lib.cmakeFeature "CMAKE_HIP_COMPILER" "${rocmPackages.clr.hipClangPath}/clang++")
    (lib.cmakeFeature "STRATA_GGML_DIR" "${llamaCpp}")
  ]
  ++ lib.optionals rdna3 [
    (lib.cmakeFeature "STRATA_ROCWMMA_INCLUDE_DIR" "${rocmPackages.rocwmma}/include")
  ];

  ninjaFlags = [ "strata" ];

  # The CPU kernels (the experts held in RAM, computed during decode) for
  # this machine's CPU, as Strata's own setup.py builds them (AGL-089, D19):
  # nixpkgs' cc-wrapper drops -march=native otherwise, and decode is 18 %
  # slower. The result does not move to another CPU.
  NIX_ENFORCE_NO_NATIVE = "0";

  # The engine, then the checkout's server, tools and data beside it: the
  # server finds the engine through the config agl writes (D5).
  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin $out/share/strata/engine
    cp strata $out/share/strata/engine/strata
    cd $NIX_BUILD_TOP/source
    cp -r serve tools data strata_control $out/share/strata/
    for dir in rdna3 include; do
      [ -d $dir ] && cp -r $dir $out/share/strata/
    done
    mkdir -p $out/share/strata/third_party
    ln -s ${llamaCpp} $out/share/strata/third_party/llama.cpp
    ln -s $out/share/strata/engine/strata $out/bin/strata
    makeWrapper ${python}/bin/python $out/bin/strata-server \
      --add-flags $out/share/strata/serve/server.py \
      --set STRATA_GGUF_PY ${llamaCpp}/gguf-py
    makeWrapper ${python}/bin/python $out/bin/strata-iq-pack \
      --add-flags $out/share/strata/tools/iq_pack.py \
      --set STRATA_GGUF_PY ${llamaCpp}/gguf-py
    # The Python of the tools agl runs to prepare a model: mtp_fetch.py,
    # mtp_pack.py, mtp_rt.py (AGL-089, D13).
    makeWrapper ${python}/bin/python $out/bin/strata-python \
      --set STRATA_GGUF_PY ${llamaCpp}/gguf-py
    # The hipBLASLt tuning table of this hipBLASLt (AGL-089): the engine
    # refuses a table of another version and its prompt reads slow down.
    h=${rocmPackages.hipblaslt}/include/hipblaslt/hipblaslt-version.h
    # major * 100000 + minor * 100 + patch: 1.2.2 is 100202.
    v=$(awk '/define HIPBLASLT_VERSION_MAJOR/ { a = $3 } /define HIPBLASLT_VERSION_MINOR/ { b = $3 }
      /define HIPBLASLT_VERSION_PATCH/ { c = $3 } END { print a * 100000 + b * 100 + c }' $h)
    table=$out/share/strata/tools/hip/gfx1100-hipblaslt-$v.txt
    if [ -e $table ]; then
      ln -s $table $out/share/strata/hipblaslt-tuning.txt
    fi
    runHook postInstall
  '';

  passthru = { inherit python; };

  meta = {
    description = "Strata, the engine and the server of a Qwen3.8-Flash-Next on one consumer GPU, for gfx1100";
    license = lib.licenses.mit;
    platforms = [ "x86_64-linux" ];
    mainProgram = "strata-server";
  };
}
