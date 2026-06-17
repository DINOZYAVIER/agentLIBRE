# agentLIBRE

agentLIBRE is a local-first, event-based agentic system.

## Why agent?

It utilizes LLM-powered agents, so that's why.

## Why LIBRE?

It's free (libre) and open-source.

## Current Status

This repository is currently suitable for an alpha, not a
polished end-user release. I hope to provide a roadmap as soon as possible.

Working now:

- `agl` is the public CLI binary.
- `agl run` and `agl chat` can run a local GGUF model through the in-process
  llama.cpp backend.
- `agl config paths` prints the runtime/config/data/log locations.
- `agl config init` writes the runtime config.
- Planned commands such as `agl model pull`, `agl setup`, and `agl doctor`
  fail explicitly instead of pretending to work.

Not ready yet:

- `model pull`, `setup`, and `doctor` are not implemented.
- Standalone binary packaging is not ready. The release binary currently has an
  absolute `RUNPATH` to this checkout's `target/llama-cpp/build/bin`.
- Tool-use demos should not be promised yet. Visible tools are not rendered into
  the native prompt, the action parser is Hermes-only, and Gemma/structured tool
  paths are not end-to-end.
- `agl-loop` and `agl-events` are mostly library/test surfaces and are not wired
  into the CLI demo path.
- Event schema and privacy behavior need another pass before observability is
  treated as stable.

## Dev Demo Quickstart

Build llama.cpp and the CLI:

```sh
scripts/build-llama-cpp.sh
cargo build --release -p agl-cli
```

Create a local inference config that points at an existing GGUF file:

```toml
[backend]
kind = "llama_cpp"
model = "/absolute/path/to/model.gguf"

[runtime]
gpu_layers = 999
context_tokens = 32768
threads = 8

[model]
dialect = "qwen3"
tool_call_format = "hermes_json"
```

Run a one-shot prompt:

```sh
./target/release/agl run --config /path/to/local.toml "Say hello"
```

Start chat:

```sh
./target/release/agl chat --config /path/to/local.toml
```

If no config is provided, `agl` looks for the default path shown by:

```sh
./target/release/agl config paths
```

## License

This project is available under `LGPL-3.0-or-later`.

agentLIBRE may be offered under a separate commercial license in the future,
but no commercial license is currently offered or granted by this repository.
See `COMMERCIAL.md`.

The official project name, logos, release signing keys, update channels,
license servers, hosted services, and other project infrastructure are not
licensed under the LGPL. See `TRADEMARKS.md`.
