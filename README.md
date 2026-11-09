# uswctl

A daemon and a command-line client that manage UniFi switches over SSH, without the
UniFi Network application. The supported models are the USW Pro Max 16 PoE and the USW
Pro XG 10 PoE. See
`docs/superpowers/specs/2026-11-02-uswctl-design.md` for the full design, and
`docs/cutover.md` for the move away from the controller.

The project has two binaries:

- `uswctld`: the daemon. It holds one worker for each switch, serves the HTTP API, and
  runs the web page.
- `uswctl`: the command-line client for the HTTP API. `uswctl mcp` serves the same API
  as MCP tools for AI agent sessions (see "MCP server" below).

## Build

```sh
cargo build --release
```

The binaries land in `target/release/uswctld` and `target/release/uswctl`. Rust 2024
edition is required (see `Cargo.toml`).

Run the tests with `cargo test`. The tests under `tests/it_xg10.rs` need a real USW
Pro XG 10 PoE with a PoE device on port 1, and run only with `--ignored`; they do not
run in `cargo test`. They change the switch and restore it at the end. They read:

- `USWCTL_IT_HOST`: the address of the switch (required)
- `USWCTL_IT_USER`: the SSH user of the switch (required)
- `USWCTL_IT_KEY`: the SSH private key (default `~/.ssh/id_rsa`)
- `USWCTL_IT_LOCK_PATH`: the lock file on the switch. If it is set, the tests take the
  lock there for their run. If it is not set, they take no lock.

To run the daemon as the systemd user service below, install both binaries to
`~/.cargo/bin`:

```sh
cargo install --path . --locked
```

## Configure the daemon

Copy `config.example.toml` to `~/.config/uswctl/config.toml` and edit it:

```sh
mkdir -p ~/.config/uswctl
cp config.example.toml ~/.config/uswctl/config.toml
chmod 600 ~/.config/uswctl/config.toml
```

The daemon refuses to start if the file mode is not 600. Fill in:

- `agent_token` and `user_token`: two different bearer tokens. The agent token is for
  scripts and agent sessions. The user token can also touch a protected port or a
  switch-wide setting (see "Roles and safety rules" below).
- `ssh_user`: the SSH user of the switches. The UniFi Network application sets the
  device SSH user name and password for its site; use that user name, or the one you set
  by hand.
- `ssh_key`: the SSH private key for the switches (no passphrase; the daemon cannot
  answer a passphrase prompt). The public key must be in the SSH keys of the switch.
- `data_dir`: where the daemon keeps its reservations, its backups and its SSH control
  sockets. The default `~/.local/share/uswctl` survives a daemon restart.
- `poll_s` and `overlay_check_s`: the live-state poll interval and the overlay-repair
  check interval, in seconds. Neither may be `0`.
- `listen`: the address and port of the API and the web page. The example uses
  `127.0.0.1`, which serves this host only. For the web page in a browser on another
  host, set the network address of this host.
- `lock_path` (optional): the lock file on each switch. See "The switch lock" below.
- one `[[switches]]` block for each switch: `name`, `host`, `model_code` (`USPM16P` or
  `USWED77`), `protected_ports`, `tested_firmware` and the optional `lock_token`.
  `protected_ports` lists the ports that carry the network itself, for example the
  uplink and the link to the daemon host.

`tested_firmware` lists the firmware versions on which the daemon writes to the switch.
On a firmware that is not in the list, the daemon refuses every settings change and every
dry run. The example lists `7.5.15.17146` for the XG 10 only, because the integration
tests ran on the XG 10. Add a version at step 5 of `docs/cutover.md`, after a supervised
change on that firmware.

### The switch lock

The lock lets another process or a person keep the daemon away from a switch, for
example while they work on it by hand or run a test. The lock is a file on the switch
at `lock_path`, for example `/tmp/agent-lock`. If `lock_path` is not set, the daemon
reads no lock.

The daemon never writes the lock file, and it holds no lock while it changes a switch.
Before each change, it reads the lock file of the switch:

- An empty or missing file is no lock.
- A lock whose text contains `lock_token` counts as the daemon's own lock. The
  integration tests use this, to hold the lock while they drive the daemon code.
- Every other lock stops the change with `409 locked`.

Set `lock_token` to a value that no lock holder writes, for example
`uswctld-own-lock-allowance`, or leave it out. A value that a lock holder writes lets
the daemon change a switch while that holder has the lock.

## Run

```sh
uswctld --config ~/.config/uswctl/config.toml
```

Run it without a config file, against two simulated switches, for a local check:

```sh
uswctld --fake
```

`--fake` listens on `127.0.0.1:8650` with tokens `agent` and `user`, and needs no SSH
key and no device. `--config` and `--fake` cannot be given together.

The daemon writes its log to stderr. The log holds:

- the listen address at startup
- one line for each API write, with its role and its outcome
- a warning for a failed read or write on a switch.

Under the systemd user service, `journalctl --user -u uswctld` shows this log.

### Install as a systemd user service

`contrib/uswctld.service` is a systemd **user** unit, not a system unit. Install and
enable it yourself:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/uswctld.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now uswctld.service
```

Check its status and logs with `systemctl --user status uswctld` and
`journalctl --user -u uswctld -f`. The user session must stay active for the service to
run; enable lingering if it must run without a login session:
`loginctl enable-linger $USER`.

## The web page

Open `http://<listen-address>/` in a browser. The page asks for a token and uses it as
the bearer token for every API call. The page needs the user token; it refuses the agent
token. Scripts and agents use the API or the command-line client with the agent token.

## The command-line client

The client reads its settings from the environment:

| Variable | Meaning | Default |
|----------|---------|---------|
| `USWCTL_URL` | Base URL of the daemon | `http://127.0.0.1:8650` |
| `USWCTL_TOKEN` | Bearer token (required) | none |
| `USWCTL_OWNER` | Owner name for a reservation, sent as `X-Uswctl-Owner` | none |

Set `USWCTL_OWNER` to a name for the caller, for example the name of the agent
session, before the caller reserves or changes a port.

Commands (`uswctl --json` prints the raw JSON instead of a table):

```sh
uswctl switches
uswctl ports <switch>
uswctl port-set <switch> <port> [--name <text>] [--native-vlan <vlan>|none] \
    [--tagged <vlan,vlan,...>] [--poe auto|off] [--stp on|off] [--edge auto|on|off] \
    [--bpdu-guard on|off] [--bpdu-filter on|off] [--enabled on|off] [--dry-run]
uswctl link <switch> <port> down|up
uswctl poe <switch> <port> off|on|cycle
uswctl reserve <switch> <port> --ttl <n>[s|m|h|d]
uswctl extend <id> --ttl <n>[s|m|h|d]
uswctl release <id>
uswctl reservations
uswctl macs <switch> [port]
uswctl events <switch>
uswctl failure <switch>
uswctl locate <switch> <ports> [--for <n>[s|m|h|d]]
uswctl locate-off <switch>
```

`--dry-run` on `port-set` previews the change: it reports the keys and does not
write to the switch, and it does not need a reservation.

`uswctl ports` prints one line with the switch-wide settings (STP version, STP priority,
BPDU action, VLANs), then one row for each port. `TAGGED` shows `all` for a port that
carries every VLAN of the switch. While a locate runs, a line with the located ports and
the end time comes before the table. `uswctl switches` shows the locate in the `LOCATE`
column.

The client waits up to 180 s for the answer to a write (`port-set`, `link`, `poe`,
`reserve`, `extend`, `release`, `locate`, `locate-off`), because a write on a switch can
wait for the switch to get ready and for an earlier write. It waits up to 30 s for every
other request, and up to 10 s for a connection to the daemon.

Exit codes: `0` success; `1` a bad command line, a bad request (400), an unknown
resource (404), or a daemon or connection error (500); `2` a refusal (401, 403, 409,
423); `3` a device failure (422, 502, 503).

## Roles and safety rules

The daemon knows two roles, from the bearer token:

- **Agent** (the agent token, with an optional owner name from `X-Uswctl-Owner`): can
  change an unprotected port, read everything, and reserve, extend or release its own
  reservations.
- **User** (the user token, used by the web page): can do everything an agent can, plus
  change a protected port, change a switch-wide setting, reboot a switch, list and
  restore a backup, and release any reservation.

Protected ports and switch-wide settings (STP version, STP priority, the global BPDU
action, and the VLAN list) need the user token. The agent token gets `403 user_only` on
such a request. The config sets the protected ports of each switch.

## Port reservations for agents

The daemon is the only writer of the switches (spec section 6.8). Before an agent
changes a port that other agents might also use, it reserves the port:

```sh
export USWCTL_OWNER=<agent session name>
uswctl reserve <switch> <port> --ttl 30m
```

Rules:

- A port with no reservation is open to every caller with the agent token, except the
  protected ports.
- A reservation blocks every agent caller except its owner. An agent caller without the
  matching owner gets `409 reserved`, with the current owner and the expiry time. The
  user token passes a reservation; the web page shows a warning first.
- Reservations are cooperative. The daemon does not verify an owner name: a caller with
  the agent token can send any name in `X-Uswctl-Owner`, also the name of another agent.
- Only the owner can extend or release its own reservation. The user token can release
  any reservation, from any switch.
- An expired reservation is removed on its next read. Reservations survive a daemon
  restart; they are kept in `<data_dir>/reservations.json`.
- A reservation does not queue a wait: it is a yes-or-no check at request time. Reserve a
  port before the first change, not after a refusal.
- Settings for the whole switch need no reservation, because only the user token (the
  web page) can change them.

See `docs/known-issues.md` for the gap between a reservation check and the write that
follows it.

## Port locate

A locate makes chosen port LEDs of a switch blink blue, so that a person can find the
port, for example to put a cable in it. The front touchscreen of the switch shows the
same ports. All other port LEDs are dark while the locate runs.

```sh
uswctl locate <switch> 1,2 --for 5m
uswctl locate-off <switch>
```

- The ports are a list, a range, or both, for example `1,2`, `1-4` or `1,3-5`.
- The locate ends by itself after the time of `--for`. The default is 5 minutes. The
  time must be from 10 s to 1 h.
- A switch has one locate at a time. A new locate replaces the running one, with its
  ports and its end time.
- `locate-off` ends the locate at once. It also ends a locate that the daemon does not
  know, for example one from before a daemon restart.
- A switch reboot ends the locate.
- A locate changes only the LEDs and the touchscreen. It needs no reservation, and the
  agent token can locate a protected port.
- A foreign lock in the lock file at `lock_path` refuses a new locate, as for every
  other request.
- A stop and the end at the end time only restore the normal LEDs. They run also while
  another agent holds the lock.
- If the end fails, the daemon tries it again at each poll that reaches the switch.
  `uswctl ports` and the web page then show "ending the locate failed; retrying".
- The web page has a `Locate` button in each port row. It locates that port for 5
  minutes. A banner above the port table shows the located ports, the time that is
  left, and a `Stop` button.

The API is `POST /api/switches/<switch>/locate` with `{"ports":[1,2],"duration_s":300}`
(`duration_s` is optional) and `DELETE /api/switches/<switch>/locate`. The state of a
switch (`GET /api/switches/<switch>`) shows the running locate in `locate`.

## MCP server

`uswctl mcp` serves the daemon API as MCP tools for AI agent sessions. It speaks MCP
(JSON-RPC 2.0) on stdin and stdout, and it calls the HTTP API of `uswctld` with the same
client code and the same environment variables as the other commands. Its log goes to
stderr. The daemon does not change.

Read tools:

| Tool | Arguments | Result |
|------|-----------|--------|
| `switches_list` | none | The switches: name, model, firmware, firmware tested (`firmware_tested`), uptime, and the last event (`last_event`). |
| `ports_list` | `switch` | One compact row for each port: link, speed, PoE watts, VLANs, STP, BPDU filter, enabled, protected, and the reservation owner (`reserved_by`). |
| `port_get` | `switch`, `port` | The full state of one port: live data and settings. |
| `macs_get` | `switch`, optional `port` | The learned MAC addresses of each port, or of one port. |
| `events_get` | `switch` | The recent events of the switch and its last failure. |
| `reservations_list` | none | All live reservations. |

Write tools (each takes an optional `owner`):

| Tool | Arguments | Effect |
|------|-----------|--------|
| `port_set` | `switch`, `port`, settings, `dry_run` | Changes the settings of one port. The settings are `name`, `native_vlan` (a VLAN id or `null`), `tagged_vlans`, `poe` (`auto` or `off`), `stp`, `edge` (`auto`, `enabled` or `disabled`), `bpdu_guard`, `bpdu_filter` and `enabled`. |
| `port_action` | `switch`, `port`, `action` | Runs `link_down`, `link_up`, `poe_off`, `poe_on` or `poe_cycle`. |
| `port_reserve` | `switch`, `port`, `ttl_s` | Reserves the port for the owner. |
| `reservation_extend` | `id`, `ttl_s` | Sets a new expiry, `ttl_s` seconds from now. |
| `reservation_release` | `id` | Releases the reservation. |

Locate tools (optional `owner`, no reservation):

| Tool | Arguments | Effect |
|------|-----------|--------|
| `port_locate` | `switch`, `ports`, optional `duration_s` | Makes the ports blink. The default time is 300 s; the time must be from 10 s to 3600 s. |
| `port_locate_off` | `switch` | Ends the locate at once. |

Every tool has a JSON Schema for its arguments. The server refuses an unknown argument.
A result holds the JSON answer of the API as text. If the API refuses a request or
fails, the tool result is an error with the HTTP status, the error kind and the message,
for example `HTTP 409 reserved: ...`.

Safety rules:

- `port_set` is a dry run by default. The daemon checks the change and shows the keys,
  and it does not write to the switch. To write, pass `dry_run: false`.
- A `port_set` with `dry_run: false` and every `port_action` need a live reservation of
  that port by the owner. The server reads `/api/reservations` before the write and
  refuses the request if the owner does not hold the reservation. This rule is stricter
  than the HTTP API, which lets an agent change a port that has no reservation.
- Every write tool needs an owner. The server uses the `owner` argument. If the call has
  no `owner`, the server uses `USWCTL_OWNER`. If neither is set, the tool refuses the
  request.
- The daemon checks the protected ports, the switch locks and the reservations as for
  every other caller.
- `port_locate` and `port_locate_off` need no owner and no reservation, because a locate
  changes only the LEDs. The server sends an `owner` as `X-Uswctl-Owner`, so that the
  daemon log shows the session.
- The server has no tool for a switch-wide setting, a reboot, a backup or a restore.
  The agent token cannot do them.

An agent uses a port in this sequence: `port_reserve`, `port_set` with the default dry
run, `port_set` with `dry_run: false`, then `reservation_release`.

Register the server in Claude Code for all projects of the current user:

```sh
claude mcp add uswctl --scope user \
    -e USWCTL_URL=http://<listen-address> \
    -e USWCTL_TOKEN=<agent token> \
    -- ~/.cargo/bin/uswctl mcp
```

The registration keeps the two values in the Claude Code settings of the user. Use the
agent token, not the user token.

This registration sets no `USWCTL_OWNER`, because all sessions of the user share it. Each
agent session must pass its own session name as `owner` in every write tool and
reservation tool. Only then do the reservations keep the sessions apart.

`USWCTL_OWNER` stays supported for a setup with one session, for example a registration
at project scope that one session uses. Add `-e USWCTL_OWNER=<owner name>` to the command
for that setup. The `owner` argument of a call overrides it.

## Documents

- `docs/superpowers/specs/2026-11-02-uswctl-design.md`: the full design.
- `docs/superpowers/specs/2026-11-03-uswctl-mcp-design.md`: the design of the MCP server.
- `docs/device-notes.md`: the device discovery behind `src/model.rs`.
- `docs/superpowers/specs/2026-11-06-uswctl-locate-design.md`: the design of the port
  locate.
- `docs/cutover.md`: the runbook to move the switches off the UniFi Network application.
- `docs/known-issues.md`: minor faults and limits that stay open after this project.
