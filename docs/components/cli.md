# CLI

The CLI is the command-line entrypoint and operator surface for `agl`.

Top-level runtime commands are function-first:

```bash
agl run --prompt "Summarize this repo."
agl chat
agl serve
agl run --function coding --prompt "Summarize this repo."
```

`agl init` writes the workspace default function in `.agl/workspace.toml`:

```toml
[functions]
default = "gemma4-12b"
```

Direct model/config execution is reserved for explicit low-level inference
commands:

```bash
agl inference run --config /path/to/local.toml --prompt "Reply once."
agl inference chat --config /path/to/local.toml
agl inference serve --config /path/to/local.toml
```
