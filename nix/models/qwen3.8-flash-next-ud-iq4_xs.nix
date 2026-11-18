# Qwen3.8-Flash-Next, the 125B mixture of experts, in Unsloth's UD-IQ4_XS on the
# Strata engine (AGL-089): three shards, ~94 GB, the revision and the sums
# Strata's setup.py pins. The Function names the engine's variant, its
# arguments and environment.
{ agl, ... }:
{
  kind = "strata";
  format = agl.formats."qwen3.8";
  name = "qwen3.8-flash-next-ud-iq4_xs";
  gguf = [
    "Qwen3.8-Flash-Next-UD-IQ4_XS-00001-of-00003.gguf"
    "Qwen3.8-Flash-Next-UD-IQ4_XS-00002-of-00003.gguf"
    "Qwen3.8-Flash-Next-UD-IQ4_XS-00003-of-00003.gguf"
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
        url = "https://huggingface.co/unsloth/Qwen3.8-Flash-Next-GGUF/resolve/38bb39ee97821de2c9009abb7e93950eec396e66/UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00001-of-00003.gguf";
        sha256 = "5ce89370720f8bf90890f439361282104c1aa1482d4013bb9a50923e758e71a4";
        size = 10946624;
      }
      {
        url = "https://huggingface.co/unsloth/Qwen3.8-Flash-Next-GGUF/resolve/38bb39ee97821de2c9009abb7e93950eec396e66/UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00002-of-00003.gguf";
        sha256 = "577a38a2392b40ca2193cea502e1d92f60b8cd370675d308e0ec21885d9daaa7";
        size = 49835229856;
      }
      {
        url = "https://huggingface.co/unsloth/Qwen3.8-Flash-Next-GGUF/resolve/38bb39ee97821de2c9009abb7e93950eec396e66/UD-IQ4_XS/Qwen3.8-Flash-Next-UD-IQ4_XS-00003-of-00003.gguf";
        sha256 = "d4634e6d84f0ebb0940be15c90d3790bf6464e3dea3a1cddc567dc0e83ad8833";
        size = 43836407744;
      }
  ];
}
