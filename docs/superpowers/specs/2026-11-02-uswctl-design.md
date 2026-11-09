# uswctl: a controller for UniFi switches

Design document, draft of 2026-11-02.

*Sources: SSH reads and tests on the USW Pro XG 10 PoE and the USW Pro Max 16 PoE,
firmware 7.5.15.17146; the web app bundle of UniFi Network 10.6.109.*

## 1. Intent

The design covers the two supported switch models (USW Pro Max 16 PoE and USW Pro XG 10
PoE). A UniFi Network controller (the UniFi Network application on the controller host)
can manage them. The controller does not let the user do three things that a test network
needs:

1. Stop BPDU leaks between the VLANs of devices under test (DUTs). The switch floods each
   BPDU that arrives on a port with STP off to all other ports with STP off, also across
   VLANs. This is the default global BPDU action `flooding` of the switch CLI. If a device
   under test sends BPDUs, for example with bridge priority 1, the DUTs of different test
   setups join one STP domain. A wired mesh satellite can then block its mesh uplink. The
   CLI can change the action (`spanning-tree bpdu filtering`) and can set a per-port BPDU
   filter (`spanning-tree bpdu-filter`), but the controller cannot set either. A CLI-only
   setting is lost at each switch boot and at each full provision.
2. Take a port link down and up quickly, from a test script.
3. Control PoE on a port (off, on, power cycle) from a test script.

`uswctl` replaces the UniFi controller. It manages the switches over SSH, keeps the
configuration on the switches, and adds the settings that the UniFi controller lacks.

## 2. Users

- The user (one person, the administrator of the network), through a web page in a
  browser.
- AI agents and test scripts, through an HTTP API and a command-line client.
- No other people. The tool needs no user management.

## 3. Goals, Non-goals, Anti-goals

Goals:

- G1. A daemon on the daemon host manages both switches without the UniFi controller.
- G2. The web page shows the live state of each port and lets the user change each setting
  in section 7.
- G3. The global BPDU action and the per-port BPDU filter are settable. They survive a
  switch reboot, with the gap described in section 5.4. MSTP comes in a later phase
  (section 13): it is optional, and it does not fix the BPDU problem.
- G4. Port link down and up, and PoE off, on and power cycle, are available to the web
  page, the API and the command-line client, and take effect within seconds.
- G5. The configuration lives on each switch, so that a switch boots into its correct
  configuration (VLANs included) without the daemon.

Non-goals:

- Firmware upgrades. The user upgrades firmware by hand. The daemon does not need to do it.
- Adoption of new switches, Wi-Fi access points, gateways, statistics history, alerts.
- Support for switch models other than the two supported models.
- Multi-user access, roles, audit by user.

Anti-goals (the tool fails if it does one of these, even when it otherwise works):

- A1. A switch boots with wrong VLANs or with ports open that must be closed, because the
  daemon is down. The VLAN configuration must be in the flash of the switch.
- A2. A change cuts off the network: a change on a port that carries the network itself,
  for example the uplink, the link to the daemon host or the link between the switches.
  The config sets these ports. See section 8.
- A3. The daemon writes a key that does nothing on the model, and reports success.
- A4. The daemon changes a port that another owner reserved, or changes a switch while
  another process or person that works on the switch directly over SSH holds the lock on
  it (section 6.3).

## 4. Design Decisions

- D1. Build an own controller. Do not modify the UniFi controller (it is closed source).
- D2. Replace the UniFi controller completely. It does not have to be an LXC; a daemon is
  enough.
- D3. Language: Rust. A web front end.
- D4. The configuration is stored on the switch, not in a file on the daemon host as the
  master.
- D5. Accept the gap of about 45 s after a switch boot, in which the CLI-only settings
  (BPDU filter) are missing.
- D6. The daemon does not upgrade firmware.
- D7. The code and this document live in one git repository, `uswctl`.
- D8. Port reservations: the daemon is the only writer and offers a reservation for each
  port.
- D9. Only the web page can change a protected port or a setting for the whole switch,
  after a second confirmation; the agent token cannot.
- D10. Test load: a PoE device on XG 10 port 1 (the test load).
- D11. No separate long test without a controller; the daemon watches the switch health
  instead.

## 5. Device Facts the Design Depends On

All facts were read or tested with firmware 7.5.15.17146 on both switches, unless the
text says otherwise. Section 12 lists what is not known.

### 5.1 Access

- SSH with the SSH user from `ssh_user` and a key without passphrase (`ssh_key` in the
  config). The server is dropbear. The account is root-equivalent (uid 0).
- The SSH user and keys are `system.cfg` keys (`users.N.*`, `sshd.auth.key.N.*`). While the
  UniFi controller manages a switch, the controller sends them. After the cutover, the
  daemon owns them.
- A firmware upgrade can replace the SSH host key (the XG 10 did so in one upgrade).
- `cli -c "<command>"` runs one CLI command; more `-c` arguments run more. A long `show`
  output stops at a `--More--` prompt, and one line was lost there. `cli -e <file>` runs a
  file of configuration commands (this is how `ubntconf` applies changes).
- `mca-dump` prints the status JSON that the switch sends to a controller: `port_table[]`
  with `up`, `speed`, `poe_power`, `stp_state`, `mac_table` and more.
- `swctrl poe show` prints the PoE state of each port. `swctrl poe set [COMMANDS]` changes
  it; its syntax is not read yet. `mcad` has an internal function
  `ubnt_set_port_poe_power_cycle`.

### 5.2 How a switch stores and applies configuration

1. `/tmp/system.cfg` holds the configuration as `key=value` lines. `switch.*` keys hold
   the switch settings. Example for one port:
   `switch.port.4.pvid=4`, `switch.port.4.poe=auto`, `switch.port.4.stp.port_mode=disabled`,
   and one `switch.vlan.<n>.port.4.mode=untagged|tagged|exclude` line for each VLAN.
2. `syswrapper.sh apply-config` runs `ubntconf -c /tmp/system.cfg -p /tmp/running.cfg -d
   /tmp/apply.sh`. That generates a CLI script with only the changed settings
   (`/tmp/.apply.scr`) and runs it (fast apply, 4 s in the test). If the fast apply fails,
   it runs `rc restart` (full provision), which builds `/tmp/.startup-config` from
   `system.cfg` and applies all of it.
3. `syswrapper.sh save-config` marks the configuration for saving.
   `syswrapper.sh cfg_save_check` then writes `/tmp/system.cfg` and `/etc/persistent` to
   the flash partition `cfg` (`mtd5`). It writes the backup slot, verifies it and makes it
   active. `cfgmtd -r -p <dir> -f <dir>/read.cfg` reads the active slot back.
4. At boot, the switch builds its configuration from the `system.cfg` in flash. VLANs,
   PVIDs, STP on or off per port, PoE mode and port names are all in that file. So the
   switch boots into its last saved configuration without any controller.
5. The root file system is in RAM. Only `system.cfg` and `/etc/persistent` survive a boot.
   `/etc/persistent` is a directory of files; a file of our own there survived a flash save
   and a reboot, and `mcad` did not remove it.
6. `mcad` owns `/etc/persistent/cfg/mgmt` (inform address, `cfgversion`, auth key) and
   rewrites it. Do not edit that file.

### 5.3 Keys

- `ubntconf` contains key names that the UniFi controller never sends, for example
  `switch.stp.mstp.N.vlans`, `switch.stp.mstp.N.priority`, `switch.stp.mstp.name`,
  `switch.stp.mstp.revision`, `switch.port.N.stp.root_guard`,
  `switch.port.N.stp.loopback_detection`, `switch.port.N.ingressfilter`.
- A key in `ubntconf` is not proof that it works. On the XG 10,
  `switch.port.2.stp.root_guard=enabled` produced no CLI command.
- `ubntconf` has no key for the BPDU filter or the global BPDU action, and no key that
  passes a raw CLI command (string search only).
- `ubntconf` can generate `shutdown` for a port. The key that causes it is not known yet.
  `switch.port.N.opmode` has the values `switch`, `mirror`, `aggregate`, `disabled` in the
  `ubntconf` strings; `disabled` is the likely candidate.
- The two models differ: the Pro Max 16 is `rtk,switch2` (MIPS 34Kc), the XG 10 is
  `rtk,switch1` (MIPS interAptiv). CLI port names: Pro Max ports 1-12 `gi1`-`gi12`, 13-16
  `tw1`-`tw4`, 17-18 not known yet; XG 10 ports `te1`-`te12` (te2 and te11 seen).

### 5.4 Timing

- Reboot: the switch answers ping after 92-102 s and SSH after 100-111 s (two tests).
- Ports forward from about 45 s of uptime (port 11 of the XG 10: "moving from Blocking to
  Forwarding" at 45.04 s). So after a boot the ports forward for about 45 s or more before
  the daemon can run the overlay. D5 accepts this gap.

### 5.5 Without a controller

- `mcad` retries the inform address about once each second and then also tries
  `http://unifi:8080/inform`. If a host named `unifi` resolves in the network, the switch
  can contact it. In the test network, DNS did not resolve it.
- With the controller address blocked (XG 10), the switch ran 2 h 43 min with no reboot,
  no `system.cfg` change and steady memory, until a power cut ended the test. After 17 h
  without power, `system.cfg` and a file in `/etc/persistent` were unchanged. The behaviour
  over days without a controller is not known. D11 replaces a separate test with a watch
  by the daemon (section 6.9).
- `mcad` writes snapshots to `/etc/persistent/offline_logs` while it cannot reach a
  controller (two snapshots, about 2 h 15 min apart, in the test above). These are writes
  to the flash partition `cfg`. Their rate over days is not known.

## 6. Architecture

```
 browser --HTTP--> +-------------------------+ --SSH (ssh_user, key)--> USW Pro Max 16
 agents  --HTTP--> |  uswctl daemon (Rust)   | --SSH (ssh_user, key)--> USW Pro XG 10
 uswctl CLI ------>|  on the daemon host     |
                   |  - device workers (1/switch)
                   |  - reconcile on boot    |
                   |  - backup store         |
                   +-------------------------+
```

### 6.1 Components

- `uswctl` daemon: one process on the daemon host, run by systemd. It serves the web page
  and the HTTP API on one port.
- Web front end: static files that the daemon serves. They use the HTTP API only.
- `uswctl` command-line client: a thin client of the HTTP API, for agents and scripts.
- Device worker: one per switch. It owns all SSH access to that switch and runs one
  operation at a time, so that two writes to a switch never overlap.

### 6.2 Source of truth

- The switch is the master (D4). For the settings that `ubntconf` supports, the master is
  the `system.cfg` in its flash. For the CLI-only settings, the master is the overlay file
  `/etc/persistent/uswctl-overlay.scr` in its flash.
- The daemon keeps no master copy. After each successful change it stores a backup of both
  files on the daemon host, with a timestamp. A backup is used only to restore a switch
  after a factory reset or a replacement, on explicit request.
- A restore needs a switch that the daemon can already reach with its key. A manual step
  brings a factory-reset switch to that point (default login, address, key). This step is
  not part of the daemon.

### 6.3 Change sequence (one switch, in the device worker)

1. Check the reservations (section 6.8): if another owner reserved a port that the
   request changes, refuse the request. Then check the lock file on the switch:
   - The config field `lock_path` names the lock file on each switch (example
     `/tmp/agent-lock`). If `lock_path` is absent, the daemon reads no lock.
   - If the file has text, another process or person works on the switch directly over
     SSH (for example a firmware upgrade by hand). Refuse the request and report the lock
     text (A4).
   - A lock whose text contains the optional `lock_token` of the switch counts as the
     daemon's own and does not stop the request.
   - The daemon never writes the lock file.
2. Read `/tmp/system.cfg`.
3. Change only the keys that the request names. Keep every other line and the order.
4. Write the file back to `/tmp/system.cfg`, then run `syswrapper.sh apply-config`.
5. If the request changes a CLI-only setting: write the new overlay file and run
   `cli -e` with it.
6. Run `syswrapper.sh save-config` and `syswrapper.sh cfg_save_check`.
7. Read back: `show running-config` (or `mca-dump`) must show the expected effect, and the
   flash read-back must contain the new keys. If the effect is missing, report a failure
   (A3), even when the commands returned 0.
8. Store the backup on the daemon host.

### 6.4 Transient actions

Link down and up, PoE off and on, and PoE power cycle are test actions. They take effect
at once and are not saved to flash, so a switch reboot ends them. Implementation: CLI
commands (`interface <name>`, `shutdown` / `no shutdown`) and `swctrl poe` commands. The
daemon records the active transient actions and shows them on the web page. The builder
finds the exact PoE commands (section 12).

A persistent port disable (the port stays down after a reboot) is a normal setting through
`system.cfg`, after the builder finds its key.

### 6.5 Boot detection and overlay

- The device worker reads the uptime of its switch every few seconds. If the uptime is
  lower than the last value, the switch rebooted.
- After a reboot, when SSH answers, the worker runs `cli -e
  /etc/persistent/uswctl-overlay.scr` and checks the result with `show running-config`.
- The worker also re-runs the overlay after a full provision. A full provision shows as a
  changed `/tmp/.startup-config` timestamp; the builder confirms the best signal.

### 6.6 Live state

The worker reads `mca-dump` (port table, link, speed, PoE power, STP state, MAC table) and
`swctrl poe show` on a short interval, and `show spanning-tree` when the STP view is open.
The interval is the builder's choice; one to five seconds for the port table is a
reasonable start.

### 6.7 Cutover from the UniFi controller

1. Install the daemon. Let it read both switches while the UniFi controller still runs.
2. Confirm the daemon shows the same configuration as the UniFi controller.
3. Stop the UniFi Network application on the controller host (the user does this; it is
   not the daemon's work). Do not use "Forget" in UniFi: it probably resets the switch to
   factory defaults (not tested).
4. Apply the BPDU setting through the daemon and check it.
5. Keep the UniFi controller stopped. If it starts again and the user edits a switch there,
   it overwrites `system.cfg` on that switch.

`docs/cutover.md` gives these steps as a runbook.

### 6.8 Port reservations

D8: the daemon is the only writer, and it offers a reservation for each port.

- A reservation holds the switch, the port, an owner name (an agent session name or a
  script name), a creation time and an expiry time. The daemon keeps the reservations on
  the daemon host, so that they survive a restart of the daemon.
- A change or a transient action on a reserved port is allowed only for its owner. Other
  callers get a refusal that names the owner and the expiry time.
- A port with no reservation is open to every caller with the agent token, except the
  protected ports (section 8).
- The owner can extend and release a reservation. The user can release any reservation
  from the web page.
- An expired reservation is removed. The web page shows all reservations.
- Settings for the whole switch need no reservation, because only the web page can change
  them (section 8).

### 6.9 Health watch

D11: no separate long test without a controller. The daemon watches each switch all the
time and reports, on the web page and through the API:

- an unexpected reboot (uptime lower than before, with no reboot request);
- a `system.cfg` that changed without a request from the daemon (checksum);
- memory growth (free memory from `free` or `/proc/meminfo`, trend over days);
- new files in `/etc/persistent/offline_logs`, with their rate, as a measure of flash
  writes;
- inform attempts to an address other than the one expected, for example
  `http://unifi:8080/inform` that resolves.

If a switch resets itself to factory defaults, the daemon reports it, and the user
decides on a restore from backup (section 6.2).

## 7. Web Page

One page for each switch, with a grid of its ports (18 on the Pro Max 16, 12 on the XG 10).

For each port, show:

- live: link up or down, speed, PoE state and power, STP state, MACs learned (count, with
  the list on demand);
- settings: name, native VLAN, tagged VLANs, PoE mode, STP on or off, edge, BPDU guard,
  BPDU filter, persistent enable or disable;
- actions: link down, link up, PoE off, PoE on, PoE power cycle.

For each switch, show the global settings: STP version, bridge priority, global BPDU
action, and the VLAN list. MSTP (name, revision, instances with VLANs and priority) comes
in a later phase.

Before a change is applied, show the keys and CLI lines that will change, and ask for a
confirmation. Mark the protected ports (section 8). Show the firmware version, the uptime,
the time of the last overlay run, and any failure of the last change.

The page must work at the width of a notebook computer screen. A phone layout is not
needed.

## 8. Safety Rules in the Daemon

D9: only the web page can change a protected port or a setting for the whole switch,
after a second confirmation. The agent token cannot.

- Protected ports: the ports that carry the network itself, for example the uplink, the
  link to the daemon host and the link between the switches. The config sets them for each
  switch (`protected_ports`). A change or a transient action on a protected port is
  possible only from the web page, after a second confirmation.
- Settings for the whole switch (STP version, priority, BPDU action, MSTP, VLAN list,
  SSH users and keys) are possible only from the web page, after a second confirmation
  that marks them as switch-wide.
- The daemon refuses a change on a port that another owner reserved, and any change while
  another process or person holds the lock file at `lock_path` on the switch
  (section 6.3).
- The daemon never runs a factory reset, a firmware upgrade or a reboot unless the request
  asks for it explicitly. A reboot action is allowed with a confirmation.

## 9. HTTP API and Command-Line Client

- JSON over HTTP. Resources: switches, ports, VLANs, global STP settings, actions,
  reservations, backups.
- With the agent token, the API allows reads, port reservations, and changes and actions
  on unprotected ports. It refuses protected ports and settings for the whole switch.
- Every write returns the keys and CLI lines that it changed, and the read-back result.
- The command-line client covers the agent use cases: list ports with state, reserve,
  extend and release a port, set a port setting, link down and up, PoE off, on and cycle,
  read the MAC table, show the last failure.
- Agents must be able to use the client without the web page.

## 10. Security

- The daemon holds an SSH key with root access to both switches. It runs as an
  unprivileged user on the daemon host, and its files are readable by that user only.
- The HTTP port listens on the network. There are two credentials, and no user accounts:
  - the agent token, for the API and the command-line client (section 9);
  - the user credential, for the web page. Only it allows protected ports and settings
    for the whole switch (section 8).
- The mechanism of both credentials is the builder's choice. The agent token must not be
  usable to log in to the web page.

## 11. Testing

- Unit tests: parse and change `system.cfg` (keep unknown lines and order), build overlay
  files, build CLI scripts, parse `mca-dump`, `show spanning-tree`, `swctrl poe show`.
  Use files saved from the switches as test data.
- Integration tests on the XG 10: it can be rebooted, and its only link that carries the
  network is port 11 (the uplink). Each setting in section 7 must have a test that applies
  it, reads the effect back, survives a reboot (for persistent settings), and reverts.
- Test load: a PoE device on XG 10 port 1 (link 2.5G, PoE Class 6, about 9.6 W). Link and
  PoE tests use this port. A link down must show on the switch and as a link loss on the
  test load. A PoE off must stop the test load, and a PoE on must boot it again. Only the
  power and the link of the test load are part of these tests.
- Each key mapping is valid for one model and firmware. The daemon records the tested
  model and firmware. On a firmware that is not tested, the daemon still runs the
  existing overlay after a boot and still allows the transient actions, so that the BPDU
  filter does not disappear after an upgrade. It shows a warning and refuses new
  `system.cfg` writes until the integration tests pass on that firmware.

## 12. Unknowns the Builder Must Resolve

`docs/device-notes.md` records the answers to items 1 to 4.

1. The `system.cfg` key that makes `ubntconf` generate `shutdown` (candidate:
   `switch.port.N.opmode=disabled`).
2. The `swctrl poe set` syntax for off, on and power cycle, and whether a CLI command
   exists for them.
3. The CLI names of Pro Max ports 17 and 18.
4. How to stop the `--More--` pager in `cli -c` output (or read `mca-dump` instead).
5. Which of the MSTP keys work on each model.
6. Whether the per-port `spanning-tree bpdu-filter` works on ports with STP off, and
   whether the global `spanning-tree bpdu filtering` stops the leak between DUT ports. The
   functional test needs two DUTs on two Pro Max ports with STP off, and a packet capture
   on a host port in the same VLANs.
7. The best signal for a full provision (section 6.5).
8. How to read the health values of section 6.9 at low cost (one SSH call per poll, for
   example). An own inform responder is not part of this design.

## 13. Further Design Choices

These choices complete the decisions of section 4.

- The name `uswctl`.
- One daemon process with an embedded web server, and a command-line client.
- Transient actions (link, PoE) are not saved to flash.
- The overlay file name `/etc/persistent/uswctl-overlay.scr`.
- Protected ports and the second confirmation (section 8).
- The details of the agent token and the user credential (D9 sets the split into two
  credentials).
- The firmware allowlist for key mappings, and its behaviour on an untested firmware
  (section 11).
- MSTP in a later phase (G3).
- Restore only to a switch that the daemon can reach with its key (section 6.2).
- The list of health values in section 6.9 (D11 sets the watch instead of a test).

## 14. Left to the Builder

- The Rust crates (HTTP server, SSH client, JSON), the front-end framework, and the build.
- The polling intervals.
- The format of the backup store on the daemon host.
- The layout of the web page within section 7.
- The systemd unit.
