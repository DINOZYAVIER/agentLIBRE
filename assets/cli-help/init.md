Set up a working local agentLIBRE model and workspace.

The conservative default is Gemma 4 E4B QAT Q4 with its required vision
projector. Before any large transfer, init inspects memory, disk, llama.cpp
devices, the Hugging Face cache, and unfinished setup state, then shows one
deterministic plan. Success means a bounded generation passed through the normal
chat and model-manager path.

Interrupted setup is resumed on the next invocation. Machines below the
recommended 8 GB memory floor stop before acquisition unless
--allow-low-memory is supplied. In automation, use --yes to accept the displayed
plan; no prompt is attempted without a terminal.
