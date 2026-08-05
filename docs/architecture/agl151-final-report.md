# AGL-151 Final Architecture Report

Status: technically verified; human checkpoint review and release acceptance
remain pending.

## Result

Terminal extraction is complete. The independent `agl-terminal` repository
owns eight workspace packages, 43,722 lines of Rust, the public
`agl-terminal` UI, the public `agl-terminald` service, and its private exact
`agl-process-launcher` sibling. It owns execution/PTY/terminal protocol,
client, lifecycle storage, spool, shell adapters, state, and packaging.

agentLIBRE retains 37 workspace packages and 184,010 lines of Rust. Its
composition roots remain `agl-cli`, `agl-daemon`, `agl-chat`, and
`agl-runtime`; its retained domains include canonical agent sessions, runs,
turns, transcripts, policy, approvals, tools, functions, skills, artifacts,
models, inference, connectors, workspace behavior, memory, notes, and cron.
The `agl` binary is a command client. Interactive Chat and terminal rendering
belong to `agl-terminal`.

There is one state and runtime root per product. `agl-terminald` is the only
writer of execution and terminal lifecycle data. agentLIBRE stores only agent
ownership mappings, bounded projections, and audit references.

## Dependency And Release Shape

agentLIBRE pins five terminal libraries (`agl-exec`, `agl-pty`,
`agl-terminal`, `agl-terminal-protocol`, and `agl-terminal-client`) to terminal
revision `6c653fc02bfd3ac5117d56eeca6526381e07f20d`. Terminal engine, protocol,
client, service, persistence, and launcher packages have zero Agent SDK
dependencies. Only the `agl-terminal-ui` leaf pins the four bounded Agent SDK
packages (`agl-client`, `agl-content`, `agl-ids`, and `agl-protocol`) at the
older immutable agent revision
`7af1ed0484b1214000d39d07402521446395bb3d`.

This is an acyclic immutable revision DAG:

```text
agent e5962c4 -> terminal 6c653fc -> agent 7af1ed0
```

There is no mutable branch dependency, package dependency cycle, or shared
`agl-common` repository. The intentional UI consumer edge does create a
coordinated repin cost: a compatible Agent SDK update requires a new terminal
UI revision, and adopting that terminal revision requires a later agent pin.

The terminal release surface consists of eight versioned packages, two
installed public binaries, one private launcher, one service unit, and one
socket unit. The agent release no longer packages or exposes the launcher or
terminal service. A published agent build pins one exact terminal source
revision and one exact released `agl-terminald` binary identity. A clean
source rebuild proves buildability but is not assumed byte-reproducible across
absolute checkout roots; publishing a different terminal binary requires an
explicit agent build-identity repin.

H05 changed 15 terminal files (+474/-62) and 38 agent files (+360/-472). The
net deletion on the agent side and the zero lower-layer Agent SDK edges show a
real ownership cut rather than a copied facade. The remaining maintenance
cost is two CI/release pipelines, immutable cross-repository pins, coordinated
UI SDK repins, and separate runtime generation diagnostics.

## Verification Evidence

At terminal revision `6c653fc02bfd3ac5117d56eeca6526381e07f20d`:

- the full workspace format, Clippy `-D warnings`, test, documentation,
  boundary, provenance, consumer-pin, package, install/uninstall, and systemd
  activation checks pass;
- native Linux namespace, Landlock ABI 9, seccomp, pidfd, process, PTY, and
  Bash/Zsh shell-hook smoke passes; and
- owner-death verification passes four ready-tree deaths and eight pre-exec
  races with descendant cleanup.

At agent revision `e5962c4a7111688027c2290751f8468a37f58930` plus the H06
verification patch, affected workspace tests, architecture checks,
installer/uninstaller/systemd checks, the clean cross-repository build, and a
real two-daemon CLI lifecycle smoke cover bare `agl` and
`agl session new|list|show|resume|submit|follow|cancel|finish`.

The exact released service identity pinned by this agent source is
`sha256:20d95981fb6afa6d9c6a608ebc208c03ee75ea3ee0e3c5c6137f6999576e48ab`.
Successful compilation of another checkout does not silently replace that
identity.

## Decision 16C Outcome

Stop and reassess is the selected and implemented outcome. Terminal is a
credible independent boundary, but it added nine immutable cross-repository
package pins in two directions, a second eight-package release train, three
deployed terminal binaries, separate state/runtime ownership, and coordinated
release-identity work. Those costs are justified by removal of privileged
execution, PTY, terminal persistence, and interactive UI ownership from the
agent runtime; they are not evidence that every domain should become a
repository.

Inference, connectors, and workspace are still unimplemented decomposition
candidates. None is selected as the next extraction. Moving any of them
requires a new task and human decision that measures its security/data owner,
dependency direction, release unit, and operational benefit against the
coordination cost observed here.

## Remaining Human Gate

Technical completion does not accept a checkpoint. Five review records must
name the reviewed paths, behavior, evidence, defects, and exact source
identities. Only after all five records are explicitly accepted may a human
create the DCO-bearing checkpoint version commit and approved tag. No tag,
push, or publication is part of this report.
