//! Runtime implementation boundary for kernel-declared file-tree Artifacts.
//!
//! AGL-171 introduces the opaque handle boundary and an in-memory fixture.
//! Git binding, concrete file mutation, commit, and recovery belong to
//! AGL-172 and are intentionally absent here.

#![forbid(unsafe_code)]

// The concrete handle is added after agl-kernel owns Artifact declarations.
