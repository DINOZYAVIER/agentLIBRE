# Gemma 4 E2B in unsloth's QAT UD-Q4_K_XL (AGL-050, D2): on the GPU with 128K of
# context and a q8 KV cache, multi-token prediction and n-gram drafts, the
# projector on the CPU (D3, D12, D13). A Function on the CPU or with another
# context changes `args` and `context`. The versions are pinned (D8).
{ agl, ... }:
{
  kind = "llama";
  format = agl.formats.gemma4;
  weights = "gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf";
  draft = "mtp-gemma-4-E2B-it.gguf";
  mmproj = "mmproj-gemma-4-E2B-it-qat-F16.gguf";
  mmproj_offload = false;
  spec_type = "draft-mtp,ngram-mod";
  context = 131072;
  args = agl.args.gpu;
  sampling = {
    temperature = 1.0;
    top_p = 0.95;
    top_k = 64;
  };
  source = {
    weights = {
      url = "https://huggingface.co/unsloth/gemma-4-E2B-it-qat-GGUF/resolve/66a399f68ddd113b06dff02fca9523e55465d11d/gemma-4-E2B-it-qat-UD-Q4_K_XL.gguf";
      sha256 = "e531007218dfab990486a5de7676a6932d6ea8dea233d1f698d7c21cf8a16889";
      size = 2620370976;
    };
    draft = {
      url = "https://huggingface.co/unsloth/gemma-4-E2B-it-qat-GGUF/resolve/66a399f68ddd113b06dff02fca9523e55465d11d/mtp-gemma-4-E2B-it.gguf";
      sha256 = "586f2460b909008640981ec34060aa864e03c144fbabfb3173c4335087e4aae0";
      size = 59235648;
    };
    # Saved under the local name above (AGL-039, D29).
    mmproj = {
      url = "https://huggingface.co/unsloth/gemma-4-E2B-it-qat-GGUF/resolve/66a399f68ddd113b06dff02fca9523e55465d11d/mmproj-F16.gguf";
      sha256 = "13c8966d1635d02e6727f27402880614906fa291850c07feda18dbcddf2291b6";
      size = 985654080;
    };
  };
}
