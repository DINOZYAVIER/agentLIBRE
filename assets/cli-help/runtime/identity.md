# Runtime identity

Print the verified runtime identity as JSON. A sealed installation validates
its immutable manifest, executable hashes, native bundle, and embedded builtin
catalog before returning the record. A development binary reports an explicit
`development` identity instead of claiming sealed release provenance.
