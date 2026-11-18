# Qwen3.8-Flash-Next, the 125B mixture of experts, as the ThinkingCap finetune
# of BottleCap AI, in its IQ4_XS on the Strata engine (AGL-089): eight shards,
# ~95 GB; its MTP block is skipped by the pack (D9), the MTP head is
# Flash-Next's own (D13). The Function names the engine's variant, its
# arguments and environment. The shards are found by their file names in the
# weights folders (AGL-039, D23, D24), else fetched at a pinned version.
{ agl, ... }:
{
  kind = "strata";
  format = agl.formats."qwen3.8";
  name = "thinkingcap-qwen3.8-flash-next-iq4_xs";
  gguf = [
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00001-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00002-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00003-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00004-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00005-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00006-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00007-of-00008.gguf"
    "ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00008-of-00008.gguf"
  ];
  context = 262144;
  # Compact before the history leaves no room for the summary (AGL-038): at
  # the 262144 of the context, at about 229k tokens.
  compact_early = true;
  sampling = {
    temperature = 1.0;
    top_p = 0.95;
    top_k = 20;
  };
  source.gguf = [
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00001-of-00008.gguf";
        sha256 = "1e3f6b8c9418fcb6fe9f19eb201d6f1cd734007d18e84fc1c6bfcc19d997ba4c";
        size = 2603719264;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00002-of-00008.gguf";
        sha256 = "be8d4b53d9d830d4992098bcaac3afd959fb54d8c4dcdfb13c0d76e53bffd177";
        size = 28800138432;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00003-of-00008.gguf";
        sha256 = "bd38993b8f1d1c99bb923b80b3db68467dae6c0b3c5b429e1fe8e3ff35beedac";
        size = 11518992192;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00004-of-00008.gguf";
        sha256 = "38e62fbe75ac9ab4bd066755f34b61d261ff7e85899b9f7151719d04bf9b3d49";
        size = 12715856224;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00005-of-00008.gguf";
        sha256 = "495de1c1d5c30261cde2f7833ff22667e2ac439871963c322ab3931a58f96880";
        size = 11923040256;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00006-of-00008.gguf";
        sha256 = "cf64e185772d41edb307e84e813d210e7993a55c1b10e8950dc5d0f71d5ec9e4";
        size = 11946745600;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00007-of-00008.gguf";
        sha256 = "269ee2f78ae25e7c9395cba34f0a15d8194f8ff482cd72cd4439d36d8be496d2";
        size = 12339872992;
      }
      {
        url = "https://huggingface.co/bottlecapai/ThinkingCap-Qwen3.8-Flash-Next-GGUF/resolve/b9474a5af09bac0fdd2ce47ca9d997541ccd4bf6/ThinkingCap-Qwen3.8-Flash-Next-IQ4_XS-00008-of-00008.gguf";
        sha256 = "ecc71c4234fc9d33270d5902871614c9cdcc1cea5dd0e694ecdbe0a1de8ddbec";
        size = 3686015648;
      }
  ];
}
