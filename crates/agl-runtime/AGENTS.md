# AGENTS.md

`agl-runtime` owns package parsing and acquisition, Function dependency locking
and activation, model metadata, Skill instructions, Extension declarations,
inference request/output codecs, and inference service lifecycle.

Keep the `agent`, `function`, `model`, `skill`, `extension`, and `inference`
modules as distinct domains inside this crate. Do not recreate one workspace
crate per manifest kind.

The runtime must not own Agent scheduling, durable Agent state, CLI rendering,
or concrete builtin Tool handlers. Child-process and PTY lifecycle belongs to
`agl-execd`; runtime package resolution and inference use the typed execution
client.

Do not infer model bindings from filenames, scan arbitrary model directories,
persist credentials, or add in-process native llama.cpp linkage.
