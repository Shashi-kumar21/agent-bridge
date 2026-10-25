# Security policy

## Supported versions

The latest commit on `main` is the only supported version.

## Reporting a vulnerability

Please email the maintainer instead of opening a public issue. Include:

* affected version / commit,
* reproduction steps,
* impact (local privilege boundary, message injection, DoS, ...).

You should receive a response within 72 hours.

## Threat model

`agent-bridge` is a **local** tool:

* The daemon listens only on a local endpoint (filesystem socket on Unix,
  named pipe on Windows). It never binds a network interface.
* Any process running as the same OS user can talk to the daemon by design;
  the security boundary is the OS user, not the socket.
* Messages are written verbatim into another agent's PTY. Treat `sendmessage`
  input as code execution in that terminal - that is the point of the tool.
* Output buffers are kept in memory only (256 KiB per agent) and are never
  written to disk unless the daemon log is enabled.

## Hardening ideas (not yet implemented)

* Socket ACL / `0600` verification on startup.
* Optional token handshake for shared machines.
* Structured message framing (deliver as a queued inbox instead of raw typing).
