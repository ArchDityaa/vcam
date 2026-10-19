# Security policy

## Supported versions

| Version | Supported |
| ------- | --------- |
| 0.1.x   | ✅        |

## Reporting a vulnerability

Please do **not** open a public issue for a security problem.

Use GitHub's private vulnerability reporting:

1. Open <https://github.com/ArchDityaa/vcam/security/advisories/new>.
2. Describe the issue, the affected version and, if possible, a reproduction.

You can expect an acknowledgement within a few days and a coordinated fix. Once
a fix is available, a GitHub Security Advisory and a new release will be
published, and the reporter will be credited unless they prefer otherwise.

## Scope

`vcamd` runs as an unprivileged user, but it owns a video device and a Unix
control socket. Areas worth particular attention:

- IPC message handling in `crates/vcamd/src/server.rs` and
  `crates/vcam-core/src/ipc.rs`;
- path handling for the source file and the runtime preview/socket files;
- the `PROTECT*` / sandboxing options in `packaging/systemd/vcamd.service`;
- the udev rule and device permissions;
- parsing of configuration files and process metadata.

## Hardening notes

- The daemon never runs as root and the systemd unit sets
  `NoNewPrivileges=yes`, `PrivateTmp=yes`, `ProtectSystem=strict` and
  `ProtectHome=read-only`.
- The control socket lives in the per-user runtime directory and is only
  accessible to the same user.
- The `video` group is the only privilege the daemon needs.
