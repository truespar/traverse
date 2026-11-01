# Security

## Reporting a vulnerability

Please report security problems privately, not in a public issue:

- email **security@tic.io**, or
- use GitHub's private vulnerability reporting (the repository's **Security** tab, **Report a
  vulnerability**).

Tell us the Traverse version, how to reproduce the problem and what an attacker could do with it.
We'll confirm we got your report, keep you posted while we work on a fix, and credit you in the
release notes unless you'd rather stay anonymous. Please give us reasonable time to ship a fix
before you disclose the problem.

## Supported versions

Security fixes go into the latest release. Upgrade to it to get them.

## What's in scope

Everything in this repository: the server (Bolt, HTTP, WebSocket, gRPC and MCP endpoints), Studio,
the CLI and admin tools, the embedded bindings and the `.tvdb` / `.tvmodel` file formats. A
crafted file that crashes the process or loads an inconsistent graph counts: files are meant to
be refused cleanly.

## Running a server safely

- **Authentication.** HTTP and WebSocket requests need the API key (`TRAVERSE_API_KEY`, generated
  at startup when unset). Seed an admin user with `TRAVERSE_AUTH` for Bolt and gRPC.
- **Network.** Listen on a private address or behind a firewall; `TRAVERSE_ALLOWED_IPS` limits
  clients to CIDR ranges. Turn on TLS (`TRAVERSE_TLS_CERT`, `TRAVERSE_TLS_KEY`) on anything that
  leaves the machine.
- **Destructive operations.** `TRAVERSE_ALLOW_DROP_DATABASE=false` disables dropping databases
  from Studio and the HTTP API (allowed by default).
- **The banner prints the API key.** Set `TRAVERSE_QUIET=true` where stdout ends up in logs.
- **Graph Assistant.** When `TRAVERSE_AI_PROVIDER` is set, Studio sends users' questions, the
  schema and the editor's query to that provider; the provider's key stays on the server.
