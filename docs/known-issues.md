# Known issues

This file lists minor faults and limits of uswctl. A fault stays here while it is minor
and the fix is involved, or while it is a documented limit of the design. An item that
gets its own work item leaves this file; the work item becomes its record.

## Reservations

### The reservation check and the write are not one step

A reservation check (`reservation_check`) and the write that follows it are two separate
steps. Another caller can take or release a reservation on the same port between them.

Effect: a caller can write to a port in the instant another caller's reservation starts
or ends. The MCP server (`uswctl mcp`) adds a second window: its own reservation read
and the write that follows it are two separate requests to the daemon.

Workaround: the worker runs one change at a time for each switch, so two writes never
overlap. The tool serves a small number of callers, so the design accepts the gap
(spec section 6.8).

## Switch locks

### The daemon holds no lock during its own change

If `lock_path` is set, the daemon reads the lock file before each change and refuses the
change if another holder has the lock. It does not write a lock for the time of its own
change.

Effect: a holder that reads the lock file while the daemon changes the switch finds no
lock, and can start a change by hand at the same time.

Workaround: a person or process that works on a switch by hand takes the lock first. The
daemon then refuses its own changes until the holder releases the lock. A change through
the daemon takes 14 to 21 s, so the window is short.

## Running-config reads and the pager

### A dropped port block can give a false "overlay present"

`runcfg_overlay_check` by itself reports the BPDU filter overlay as present when the CLI
pager drops a whole port block from `show running-config`.

Effect: none in the daemon. Every caller first runs `running_complete_check`, which
requires the `interface` line of every port of the model, and treats a text without it
as a failed read. Every running-config read also sends `CLI_PAGER_OFF`
(`terminal length 0`) before the `show` command. A new caller must keep the completeness
check.

### CSI stripping ends only on a letter final byte

The code that removes cursor-redraw escape sequences from CLI output assumes the
sequence ends on a letter (as seen in the fixtures, for example `\x1b[A` and `\x1b[2K`).
An escape sequence with a non-letter final byte would not get stripped.

Effect: none observed. No fixture or device read has shown this form.

Plan: fix it if a real `show` output shows such a sequence.

### `runcfg_overlay_check` cleans the text once per port

The function strips pager and escape noise from the running-config text separately for
each port, instead of once for the whole text.

Effect: none on correctness; extra CPU work only, on a text of a few kilobytes.

## Flash save and boot timing

### `cfg_save_check` has other exit-0 paths that do not write the flash

`cfg_save_check` can return 0 without a flash write in states other than the empty
`/etc/persistent/cfg/mgmt` case the daemon waits for: a busy state, no pending save, or a
logged `cfgmtd` failure.

Effect: a save can report success while the flash still holds the old configuration.

Workaround: the daemon always reads the flash back with `cfgmtd -r` after a save and
compares the new keys. A save that did not reach the flash shows as `NoEffect`
(`readback_ok` false), so the caller sees the failure.

### The early-boot save failure is not reproduced

One test run (run 4 of `it_reboot_persistence`) saw a save that returned 0 but
left the flash unchanged, soon after a boot. The likely cause is an empty
`/etc/persistent/cfg/mgmt` at that time; the process that fills it is not confirmed. A
fix (`save_ready_wait`: wait up to 90 s for that file to be non-empty, now before the
first write of a change) passed one verification reboot, but the empty `mgmt` state did
not reproduce on that reboot.

Effect: the cause stays likely, not proven.

Plan: watch for a repeat of a save with `readback_ok: false` soon after a boot, and
capture `/etc/persistent/cfg/mgmt` at that time.

### A rollback after a failed save does not save

If a change fails after the daemon put the new `system.cfg` or overlay file, the daemon
puts the old text back and runs it again (`config_write`). The error names the result:
`rollback: restored` or `rollback failed: ...`. The rollback does not run a save.

Effect: if `cfg_save_check` failed after it wrote part or all of the new text to flash,
the flash can hold the new text while the running configuration and `/tmp/system.cfg`
hold the old text. A reboot before the next successful save boots with the new text.

Workaround: after a `save_failed` error, read the flash (`cfgmtd -r`) or make the next
change, which saves the old text with it. The rollback runs no save by design: a save is
a second flash write on a path that just failed.

### A rollback can leave an overlay file that did not exist

If the switch had no overlay file before a failed change of an overlay setting, the
rollback writes the default overlay (flooding, no port filter) to the file.

Effect: none on the switch. The default overlay and a missing file have the same meaning
for the daemon, and a run of the default overlay sets the default state.

### The gap between boot and forwarding ports is about 45 seconds

Ports on both switches forward about 45 seconds after boot. The daemon's overlay (the
BPDU filter) can only run once the switch answers SSH, after the ports already forward.
For about 45 seconds after a boot, a port with STP off can pass a BPDU from another STP
domain, because the overlay is not applied yet.

Effect: a short window after every boot without the BPDU filter.

Status: accepted in the design (spec decision D5). No workaround planned.

### `mca-dump` reports a link change 10 to 12 seconds late

A port's link state in `mca-dump` lags the real link state by about 10 to 12 seconds.

Effect: the web page and the API can show a port as down (or up) for up to 12 seconds
after it changed. Code that waits for a link change must allow for this lag.

Workaround: the device tests use a 30 second wait window for both link down and link up.

## STP forms not verified on a device

### STP mode `stp` (legacy, non-rapid)

Setting the global STP version to `stp` was not tested on a device. Testing it on the XG
10 would put its only uplink (port 11, the root port) through the legacy
listening/learning states (about 30 seconds) and send a topology change into the network
through the Pro Max 16 at the other end of the uplink.

Effect: this form of the global STP version setting is unverified. The web page shows a
warning in the review of such a change.

Plan: test it only in a window in which a network-wide disruption is acceptable.

### The BPDU leak functional test (spec section 12, item 6)

The test that two Pro Max ports with STP off, each with its own STP domain behind it, no
longer leak BPDUs between them after the filter is applied, needs a device that sends
BPDUs on each of those ports. It was not run.

Effect: the BPDU filter's effect on a real leak between two devices is unverified; it is
verified only through `show running-config` and a capture without a device behind the
port.

Plan: run it when two such devices are available on two Pro Max STP-off ports.

### Edge disabled on a port with STP off

The CLI forms of edge enabled, edge auto and BPDU guard on a port with STP off are known
from the XG 10 device tests, and the effect check uses them. The form
of edge disabled on a port with STP off was not seen.

Effect: for that one case, only the `system.cfg` and flash read-back checks the change.

Plan: add the form when a device run shows it.

### MSTP

MSTP (named instances, VLANs per instance, revision and priority) is not implemented.
The switch firmware supports it; `uswctld` does not expose it.

Status: a later phase (spec section 7).

## Transient actions

### A full provision does not clear the transient marks

The worker clears its record of transient actions (link down, PoE off) when the switch
reboots. A full provision (`rc restart`) also ends these actions, but the daemon has no
signal for a full provision yet (spec section 6.5 and open question 7).

Effect: after a full provision, the web page and the API can show a port as "link down"
or "PoE off" by the daemon, while the port is up again.

Plan: find a signal for a full provision, for example the time stamp of
`/tmp/.startup-config`, and clear the record on it.

## Port locate

### A daemon restart during a locate leaves the LEDs blinking

The daemon keeps the locate in memory only. A daemon restart loses the end time, so the
new daemon does not end the locate. Design decision L3 allows this instead of a state
file.

Effect: after a daemon restart during a locate, the chosen ports blink and all other
port LEDs stay dark until a switch reboot, a new locate, or a stop.

Workaround: run `uswctl locate-off <switch>`. A stop sends `unset-port-locate` also when
the daemon knows no locate.

### The locate path of the XG 10 is not tested

The switch scripts use the LED driver (`/proc/led`) on the Pro Max 16. On a model without
`/proc/led`, such as the XG 10, the same scripts use the Etherlight daemon over ubus. Only
the Pro Max 16 path was tested on a device (firmware 7.5.15.17146).

Effect: a locate on the XG 10 can fail, or can give another LED pattern.

Plan: run one supervised locate on an XG 10.

## Code and test gaps

- `tests/fixtures/redact.sh` adds a trailing newline to a fixture that lacks one. No
  current fixture lacks a trailing newline, so this has no effect yet.
- The running-config fixtures contain literal `--More--` pager lines. This is expected:
  the parser in `src/runcfg.rs` strips them.
- `docs/device-notes.md` does not state that XG 10 CLI port names are `te1` to `te12` as
  a fact confirmed by discovery; the fixture startup-config confirms it, and
  `src/model.rs` uses it.
- `src/syscfg.rs` has no test for a commented `#k=v` line or a value that itself contains
  a second `=`. The code handles both correctly by reading; it has no regression test.
- `syscfg_rename` does not refuse a rename into a key that already exists. Its only
  caller (`vlan_remove`) never does this, so the gap has no live path.
- A port with no VLAN membership lines ("all VLANs") reads a newly added VLAN as
  `tagged`, because `vlan_add` copies `mode=tagged` from its template VLAN. This matches
  the rule that a port without membership lines keeps the controller's "all VLANs"
  profile, and it is confirmed on a device (`it_vlan_add_remove`).
- The `Health` struct fields `lock`, `mem_avail_kb`, `offline_files`, `inform_status`,
  `firmware` and `model` have no permanent test assertion (only `version`, `uptime_s` and
  `cfg_md5` are asserted).
- `probe_sections_split` in `src/live.rs` would split on a column-0 `@@` line inside the
  `mca-dump` JSON body itself. No real `mca-dump` output has shown this.
- `process_run` reports the stdin write error (a broken pipe) instead of the remote's
  real exit status when the remote process exits early.
- `backup_save` uses a blocking `std::thread::sleep` inside an async function when two
  backups collide on the same second. This can block one runtime thread for up to 3
  seconds.
- The `DirBuilder` mode-setting idiom is repeated in `src/config.rs`, `src/backup.rs` and
  `src/transport.rs`. A shared helper would remove the repetition.
- The safety of `config_load` against a test accidentally touching the real home
  directory depends on the order of its checks, not on a dedicated guard.
- `own_write_note` marks a change as the daemon's own even when the change failed before
  any write reached the switch. This can hide one poll's `CfgChangedExternally` event
  after such a failure.
- On the web page, the switch-wide settings card and the events/reservations card are not
  the same height. The events list stays inside its own card rather than the page
  header. This is accepted, not a defect.
- The success view of a settings write and of a restore has no automated test, because
  `uswctld --fake` cannot complete a real write. A manual browser check stays open. The same applies to the reservation warnings in the review and action dialogs,
  and to the legacy STP warning.
- In `it_reboot_persistence`, the worker's own SSH control-socket reset can disturb the
  test's own concurrent SSH reads, because both share one control path. A separate
  control directory for the test's reads would remove the race.
- The check in the device tests that the test load on port 1 is back accepts a single MAC
  address when the switch's start state already had only one distinct MAC on that port.
  A stricter check would compare the actual MAC count before and after.
