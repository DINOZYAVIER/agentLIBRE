# AGENTS.md

`src/lib.rs` defines the native Tool SDK: `ToolContext`, cancellation,
handler interfaces and Extension/Tool bindings. Context carries the
admitted workspace, grants, deadline, output bound and planner isolation
settings. Concrete handlers live in `agl-tools`; daemon admission/scheduling
live in `agl-daemon`.
