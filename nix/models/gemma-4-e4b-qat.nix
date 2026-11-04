# Gemma 4 E4B in unsloth's QAT UD-Q4_K_XL (AGL-050, D2): on the GPU with 128K of
# context and a q8 KV cache, multi-token prediction and n-gram drafts, the
# projector on the CPU (D3, D12, D13). A Function on the CPU or with another
# context changes `args` and `context`. The versions are pinned (D8).
{ agl, ... }:
{
  kind = "llama";
  format = agl.formats.gemma4;
  weights = "gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf";
  draft = "mtp-gemma-4-E4B-it.gguf";
  mmproj = "mmproj-gemma-4-E4B-it-qat-F16.gguf";
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
      url = "https://huggingface.co/unsloth/gemma-4-E4B-it-qat-GGUF/resolve/8c5a9e4fd5482e2be20fe0bf013b4c262a8f4265/gemma-4-E4B-it-qat-UD-Q4_K_XL.gguf";
      sha256 = "df0fd4ee07072c607c29a0a1cb4f98918426cca12f45a2776bdd6ee6d09a4de3";
      size = 4215695776;
    };
    draft = {
      url = "https://huggingface.co/unsloth/gemma-4-E4B-it-qat-GGUF/resolve/8c5a9e4fd5482e2be20fe0bf013b4c262a8f4265/mtp-gemma-4-E4B-it.gguf";
      sha256 = "423074e537504b4f9ec5eafed5c639fac82c96631626efccacdd3c4039b20605";
      size = 59678016;
    };
    # Saved under the local name above (AGL-039, D29).
    mmproj = {
      url = "https://huggingface.co/unsloth/gemma-4-E4B-it-qat-GGUF/resolve/8c5a9e4fd5482e2be20fe0bf013b4c262a8f4265/mmproj-F16.gguf";
      sha256 = "6a255159ee4b01b304f633a57f017dd7d5a69d30fff52abb2614bf0813cef034";
      size = 990372672;
    };
  };
}
