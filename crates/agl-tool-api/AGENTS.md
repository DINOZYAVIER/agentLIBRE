# AGENTS.md

`src/lib.rs` defines the native Tool SDK: `ToolContext`, cancellation,
handler interfaces and Extension/Tool bindings. Context carries the
admitted workspace, grants, deadline, output bound and planner isolation
settings. Concrete handlers live in `agl-tools`; daemon admission/scheduling
live in `agl-daemon`.

`src/forge_file.rs` defines the binary Forge reader and verification status.
The reader owns an opaque backend guard for its whole lifetime; backend path
resolution and content verification remain outside the SDK.

`src/image.rs` recognizes the signatures accepted by the pinned private engine;
it provides format/media-type values without decoding image pixels.
