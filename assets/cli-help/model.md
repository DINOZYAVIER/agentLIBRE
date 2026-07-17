Acquire, inspect, verify, unbind, remove, and prune local GGUF models.

Hugging Face is the only remote source. Downloads use its standard cache and
standard HF_TOKEN/HF_HOME/HF_HUB_CACHE/HF_ENDPOINT configuration. agentLIBRE
stores explicit bindings in models.toml and install metadata separately; it
does not copy cached weights or scan model directories.
