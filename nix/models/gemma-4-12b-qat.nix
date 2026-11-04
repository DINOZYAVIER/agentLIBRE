# Gemma 4 12B in unsloth's QAT UD-Q4_K_XL (AGL-050, D2): on the GPU with 128K of
# context and a q8 KV cache, multi-token prediction and n-gram drafts, the
# projector on the CPU (D3, D12, D13). A Function on the CPU or with another
# context changes `args` and `context`. The versions are pinned (D8).
{ agl, ... }:
{
  kind = "llama";
  format = agl.formats.gemma4;
  weights = "gemma-4-12B-it-qat-UD-Q4_K_XL.gguf";
  draft = "mtp-gemma-4-12B-it.gguf";
  mmproj = "mmproj-gemma-4-12B-it-qat-F16.gguf";
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
      url = "https://huggingface.co/unsloth/gemma-4-12B-it-qat-GGUF/resolve/980b060c40a8539ac159e0501a3e0f66a6365af3/gemma-4-12B-it-qat-UD-Q4_K_XL.gguf";
      sha256 = "90fd44e29e0d7cffeb0fd00dc73cfdab9ed0b0e95306ecf7821ea634c940c370";
      size = 6716356800;
    };
    draft = {
      url = "https://huggingface.co/unsloth/gemma-4-12B-it-qat-GGUF/resolve/980b060c40a8539ac159e0501a3e0f66a6365af3/mtp-gemma-4-12B-it.gguf";
      sha256 = "fcb35dea42c71333db904cee11baac525c9ef872818ee3753f6cb156f3c6f4f6";
      size = 253708800;
    };
    # Saved under the local name above (AGL-039, D29).
    mmproj = {
      url = "https://huggingface.co/unsloth/gemma-4-12B-it-qat-GGUF/resolve/980b060c40a8539ac159e0501a3e0f66a6365af3/mmproj-F16.gguf";
      sha256 = "ecc4e93128da8363b7dbf2193eab98cf1142353f52ceaa0c95c0872997aaadd3";
      size = 175115840;
    };
  };
}
