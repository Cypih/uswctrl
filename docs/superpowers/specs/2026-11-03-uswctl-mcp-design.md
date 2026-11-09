# uswctl MCP server

Design note, 2026-11-03. This note describes the MCP server of `uswctl`.

## Intent

AI agent sessions use the switches during their tests. Without an MCP server, they run
the `uswctl` client through Bash. An MCP server gives them typed tools with schemas and
JSON results, with no environment setup in each session.

## Decisions

- D1. The server is a subcommand of the existing client: `uswctl mcp`. It speaks MCP over
  stdio and calls the existing HTTP API of `uswctld`. The daemon does not change.
- D2. It reuses the client's HTTP code and settings: `USWCTL_URL`, `USWCTL_TOKEN` (the
  agent token) and `USWCTL_OWNER`. It must not duplicate the request code.
- D3. Library: the official Rust MCP SDK (crate `rmcp`), if its API supports a stdio
  server with tools in the current stable release. The builder reads the crate source
  before use. If it does not fit, a small hand-written JSON-RPC 2.0 stdio server that
  implements `initialize`, `tools/list` and `tools/call` is acceptable; the builder
  records the reason.
- D4. Read tools:
  - `switches_list`
  - `ports_list(switch)`: compact rows (idx, cli name, link, speed, PoE watts, native
    VLAN, tagged VLANs, STP, BPDU filter, enabled, protected, reservation owner)
  - `port_get(switch, port)`: the full PortView
  - `macs_get(switch, port?)`
  - `events_get(switch)`
  - `reservations_list`
- D5. Write tools:
  - `port_set(switch, port, <patch fields>, dry_run)`: `dry_run` defaults to `true`; the
    tool applies the change only when the caller passes `dry_run: false`.
  - `port_action(switch, port, action)`: `link_down`, `link_up`, `poe_off`, `poe_on`,
    `poe_cycle`.
  - `port_reserve(switch, port, ttl_s)`, `reservation_extend(id, ttl_s)`,
    `reservation_release(id)`.
- D6. Owner: every write tool takes an optional `owner` argument; if it is absent, the
  server uses `USWCTL_OWNER`. A write tool with no owner fails with a clear message. The
  owner is sent as `X-Uswctl-Owner`.
- D7. Safety: `port_set` with `dry_run: false` and `port_action` require that the owner
  holds an active reservation on that port. The server checks this with
  `GET /api/reservations` before the write and refuses otherwise. This is stricter than
  the HTTP API, so that agents that share the MCP server do not change each other's ports
  by mistake. The daemon's own checks (protected ports, locks, reservations) stay in
  force.
- D8. No tool for global settings, reboot, restore or backups. The agent token cannot do
  them anyway (spec section 8).
- D9. Results: the API's JSON as text content. An API error becomes a tool error whose
  text holds the error kind and message, and the HTTP status.
- D10. The README documents the registration (the `mcp add` command of the MCP client,
  with the three environment variables). Nothing registers it automatically.

## Tests

- Drive `uswctl mcp` over stdio against the router with fake devices, as the CLI tests
  do: `initialize`, `tools/list` holds every tool of D4 and D5, a read tool returns JSON,
  `port_set` without `dry_run` makes no write, `port_set` with `dry_run: false` without a
  reservation is refused, and with a reservation reaches the daemon.
- No device test: the tools call the same API that the device tests cover.

## Non-goals

- An HTTP or SSE transport for MCP.
- MCP resources or prompts; tools only.
