# Device Notes

This file records the device discovery of spec section 12, items 1 to 4. `src/model.rs`
uses the values in this file.

- Test switch: USW Pro XG 10 PoE (`USWED77`), 192.0.2.107, firmware 7.5.15.17146
  (`/etc/version` US3.7.5.15).
- Read-only switch: USW Pro Max 16 PoE (`USPM16P`), 192.0.2.11, `/etc/version`
  US2.7.5.15.
- Duration: 10 minutes, 14:39 to 14:49 local time. The times below use the same clock.
- The UniFi controller managed the XG 10 during the test.
- Port 1 of the XG 10 has a PoE device (the test load) with a 2.5G link.
  Port 3 had no link. The test did not touch port 11 (uplink).
- All commands ran through `ssh -o BatchMode=yes -o IdentitiesOnly=yes -i <key>
  <user>@<ip> '<command>'`, with the SSH user from `ssh_user`. A lock file at
  `/tmp/agent-lock` (the example `lock_path`) was on the XG 10 for the whole test.
- Before the first change, `/tmp/system.cfg` went to `/tmp/uswctl-sys.orig`, and its MD5
  was recorded. At the end, `/tmp/system.cfg` and `/tmp/running.cfg` had the same MD5
  again.
- The test did not write the flash. It did not run `save-config` or `cfg_save_check`.

## Summary

| Item | Result |
|------|--------|
| Persistent port disable | `switch.port.N.status=disabled` gives ` shutdown`. Without the key, `ubntconf` gives ` no shutdown`. `switch.port.N.opmode=disabled` gives no command. |
| PoE off | `swctrl poe set off id N` |
| PoE on | `swctrl poe set auto id N` |
| PoE power cycle | `swctrl poe restart id N` (native; power returns about 6 s after the start) |
| PoE in `system.cfg` | `switch.port.N.poe=shutdown` makes `ubntconf` run `swctrl poe set off id N`; `auto` runs `swctrl poe set auto id N` |
| Transient link down and up | `cli -e <file>` with `interface teN`, ` shutdown` or ` no shutdown`, `exit` |
| Per-port BPDU filter | Line ` spanning-tree bpdu-filter` in the port block. Remove it with ` no spanning-tree bpdu-filter`. |
| Pager | `cli -c "terminal length 0" -c "<show command>"`. The setting holds for one `cli` call only. |
| Pro Max ports 17 and 18 | `te1` and `te2` |

## 1. Persistent Port Disable

### Candidate 1: `switch.port.3.opmode=disabled` (no effect)

`system.cfg` has `switch.port.3.opmode=switch` for each port. I changed the value of port 3.

```
$ sed -i "s/^switch\.port\.3\.opmode=switch$/switch.port.3.opmode=disabled/" /tmp/system.cfg
$ syswrapper.sh apply-config          # rc=0, 4.0 s
$ cat /tmp/.apply.scr
no ip dhcp snooping light
interface te3
exit
!
$ cli -c "show running-config interfaces te3"
interface te3
 switchport mode hybrid
 switchport hybrid pvid 3
 switchport hybrid allowed vlan add 3 untagged
 switchport hybrid allowed vlan remove 1
 switchport forbidden vlan add 1-2,4-8
 authentication guest-vlan
 flowcontrol on
 description "Port 3"
 lldp tlv-select port-desc sys-name sys-desc sys-cap management-addr
 lldp med tlv-select poe-pse
!
```

Result: `ubntconf` sees the change, but it generates no command for it. I set the value
back to `switch` and ran `apply-config` again.

### Candidate 2: `switch.port.3.status=disabled` (works)

The strings of `/usr/sbin/ubntconf` list `switch.port.%d.status` next to the other port
keys, and the formats ` shutdown` and ` %sshutdown`. `system.cfg` has no `status` key for
a port. I added it.

```
$ echo "switch.port.3.status=disabled" >> /tmp/system.cfg
$ syswrapper.sh apply-config          # rc=0
$ cat /tmp/.apply.scr
no ip dhcp snooping light
interface te3
 shutdown
exit
!
$ cli -c "show running-config interfaces te3"
interface te3
 ...
 authentication guest-vlan
 shutdown
 flowcontrol on
 ...
!
```

The reverse direction:

- `switch.port.3.status=enabled` gave ` no shutdown` in `.apply.scr`, and the
  `shutdown` line left the running configuration.
- A removal of the key (copy of the original `system.cfg`) also gave ` no shutdown`.

Result: the key is `switch.port.N.status`, value `disabled`. A missing key means the
port is enabled.

Observations:

- `mca-dump` showed `enable: true` for port 3 while the CLI had `shutdown`. Port 3 had
  no link. I did not check whether `enable` follows the `status` key on a port with a
  link.
- I did not test a full provision (`rc restart`) or a boot with this key. The fast apply
  is the only path that I saw generate the command.

## 2. PoE Commands

### Usage

```
$ swctrl poe set
[error] Command line is not complete. Try option "help"      # rc=255
$ swctrl poe
Usage: swctrl poe restart [ id PORT_ID ]
       swctrl poe show [ id PORT_ID ]
       swctrl poe set [ COMMANDS ]
Commands:
       { auto | poe | poeplus | poeplusplus | off | pasv24 | passthrough } [ id PORT_ID ]
where  PORT_ID := a list of target port, e.g., 3 or 5-7 or 1,4,9-12
```

The strings of `/lib/libclicmd.so.0.0` have no PoE configuration command. They show PoE
only in LLDP-MED commands. So `swctrl` is the tool for a transient PoE action.

### Off and on (port 1)

```
$ swctrl poe set off id 1             # 14:42:05, rc=0
$ swctrl poe show id 1
Port  OpMode      HpMode    PwrLimit   Class   PoEPwr  PwrGood  Power(W)  Voltage(V)  Current(mA)
   1     Off  Dot3bt-type4     96000  Unknown     Off      Bad      0.00        0.00         0.00
$ swctrl poe set auto id 1            # 14:42:44, rc=0
$ swctrl poe show id 1                # 5 s later
   1    Auto  Dot3bt-type4     96000  Class 6      On     Good      3.66       53.81        68.00
```

Before the test, port 1 showed `Auto`, `Class 6`, 8.23 W. The MD5 of `/tmp/system.cfg`
did not change: `swctrl` changes only the live state.

`mca-dump` showed the change some seconds later: `poe_mode: "off"`, `poe_power:
"0.00"`, `up: false`. The first `mca-dump` right after the command still showed the old
values.

### Power cycle (port 1)

```
$ swctrl poe restart id 1             # 14:45:23
[info ] port 1: power cycled          # rc=0, returned at 14:45:25
14:45:25    1     Off  ...  Unknown     Off      Bad      0.00
14:45:26    1    Auto  ...  Unknown     Off      Bad      0.00
14:45:29    1    Auto  ...  Class 6      On     Good      3.66
```

Result: a native power cycle exists. The command blocks for about 2 s, and the power
comes back about 6 s after the start. `poe_action_script(N, Cycle)` uses it. The
`ubntconf` strings show the same function (`ubnt_set_port_poe_power_cycle`).

### PoE through `system.cfg`

```
$ sed -i "s/^switch\.port\.1\.poe=auto$/switch.port.1.poe=shutdown/" /tmp/system.cfg
$ syswrapper.sh apply-config          # rc=0
$ cat /tmp/.apply.scr
no ip dhcp snooping light
interface te1
lldp med
 no lldp med tlv-select
exit
!
$ cat /tmp/apply.sh                   # extract
cli -e /tmp/.apply.scr || exit 1
	swctrl poe set off id 1
$ swctrl poe show id 1
   1     Off  Dot3bt-type4     96000  Unknown     Off      Bad      0.00
```

Then I copied the original back (`poe=auto`) and ran `apply-config`:

```
$ cat /tmp/.apply.scr
interface te1
lldp med
 lldp med tlv-select poe-pse
exit
$ grep swctrl /tmp/apply.sh
	swctrl poe set auto id 1
$ swctrl poe show id 1                # 11 s later
   1    Auto  Dot3bt-type4     96000  Class 6      On     Good      3.12
```

Result: `poe=shutdown` stops PoE, and `auto` restores it. `ubntconf` uses the same
`swctrl` commands as the transient actions.

Consequence: a fast apply only runs the commands for the changed keys. So a transient
`swctrl` change stays through a fast apply that does not change the `poe` key of that
port. I did not test this directly.

## 3. Transient Link Down and Up

```
$ cat /tmp/uswctl-link.scr
interface te1
 shutdown
exit
$ cli -e /tmp/uswctl-link.scr         # rc=0
$ cli -c "show running-config interfaces te1" | grep shutdown
 shutdown
$ swctrl poe show id 1                # PoE stays on during the link shutdown
   1    Auto  Dot3bt-type4     96000  Class 6      On     Good      7.91
$ swctrl port show id 1
   1  D/D      0H  ...
```

`mca-dump` showed port 1 `up: false`, `enable: false`. The MD5 of `system.cfg` did not
change.

With ` no shutdown` in the file, `swctrl port show id 1` showed `U/U 2500F` 2 to 3 s
after `cli -e` returned.

A second run measured the time until `mca-dump` showed the change. Each `mca-dump`
call over SSH took about 2 s:

| Action | `mca-dump` shows the change after |
|--------|-----------------------------------|
| ` shutdown` | 7 to 10 s |
| ` no shutdown` | 10 to 12 s |

Result: the commands work. The link comes up within 3 s. `mca-dump` shows the change
up to 12 s later, so it reports cached port data. I did not find its refresh interval.
A check that reads `mca-dump` must wait about 15 s. `swctrl port show id N` shows the
live link state.

`swctrl port set { up | down } id PORT_ID` is in the `ubntconf` strings too. I did not
test it.

## 4. Per-Port BPDU Filter

```
$ cat /tmp/uswctl-bpdu.scr
interface te3
 spanning-tree bpdu-filter
exit
$ cli -e /tmp/uswctl-bpdu.scr         # rc=0
$ cli -c "terminal length 0" -c "show running-config interfaces te3"
interface te3
 switchport mode hybrid
 switchport hybrid pvid 3
 switchport hybrid allowed vlan add 3 untagged
 switchport hybrid allowed vlan remove 1
 switchport forbidden vlan add 1-2,4-8
 authentication guest-vlan
 spanning-tree bpdu-filter
 flowcontrol on
 description "Port 3"
 lldp tlv-select port-desc sys-name sys-desc sys-cap management-addr
 lldp med tlv-select poe-pse
!
$ cli -c "show spanning-tree interfaces te3"
BPDU Filter: Enabled                           BPDU guard: Disabled
```

Before the change, `show spanning-tree interfaces te3` showed `BPDU Filter: Disabled`.

Revert with ` no spanning-tree bpdu-filter` in the same file layout. After it, the
running configuration had no `bpdu` line in the te3 block, and the switch showed
`BPDU Filter: Disabled`.

Result: the line in the port block is exactly ` spanning-tree bpdu-filter` (one leading
space). A port without the filter has no `bpdu` line. Port 3 had STP enabled and no link.
This test does not answer spec item 6 (filter on a port with STP off, and the leak
between DUT ports).

## 5. Pager

The strings of `/lib/libclicmd.so.0.0` have this command:

```
terminal length <0-$DEF>
Terminal configuration
Terminal length
Length value. 0 means no limit
```

Test on the XG 10:

| Command | Lines | `--More--` lines | `interface te` blocks |
|---------|-------|------------------|-----------------------|
| `cli -c "show running-config"` | 197 | 7 | 9 of 12 |
| `cli -c "terminal length 0" -c "show running-config"` | 190 | 0 | 12 of 12 |

The pager drops lines: three port blocks were missing without `terminal length 0`. A
later `cli -c "show running-config"` without the command showed 7 `--More--` lines
again. So the setting holds for one `cli` call only. The running configuration does not
show it.

`cli -c "show running-config interfaces te3"` prints one port block. A short output
like this one did not reach the pager.

Result: `CLI_PAGER_OFF` is `terminal length 0`. Send it as the first `-c` of each `cli`
call.

## 6. Pro Max 16 Port Names (read only)

Only `show` commands, `mca-dump` and a file read ran on 192.0.2.11.

`cli -c "show interfaces status"` prints nothing. The CLI wants a port list:
`show interfaces IF_PORTS status`.

```
$ cli -c "terminal length 0" -c "show running-config" | grep -n "^interface"
interface gi1 ... interface gi12
interface tw1 ... interface tw4
interface te1
interface te2
$ cli -c "show interfaces te1 status"
Port  Name                 Status      Vlan  Duplex  Speed    Type
te1   SFP_ 1               connected   1     full    10G      Fiber
$ cli -c "show interfaces te2 status"
Port  Name                 Status      Vlan  Duplex  Speed    Type
te2   SFP_ 2               notconnect  1     full    Fiber
```

`mca-dump` showed port 17 `media: "SFP+"`, up at 10000, and port 18 `media: "SFP+"`,
down. `/tmp/system.cfg` has `switch.port.17.name=SFP+ 1` and
`switch.port.18.name=SFP+ 2`.

Result: port 17 is `te1`, and port 18 is `te2`. Ports 1 to 12 are `gi1` to `gi12`.
Ports 13 to 16 are `tw1` to `tw4`.

Observation: `ubntconf` writes the name `SFP+ 1` as `description "SFP_ 1"`. A `+`
in a port name becomes `_` in the CLI. A check that compares the `description` line
with `switch.port.N.name` must apply the same change.

## Device Changes and Reverts (XG 10 only)

| Change | Revert | Check |
|--------|--------|-------|
| `switch.port.3.opmode=disabled`, `apply-config` | value `switch`, `apply-config` | MD5 equal to the original |
| `switch.port.3.status=disabled`, then `=enabled`, then `=disabled`, `apply-config` each | original `system.cfg`, `apply-config` | no `shutdown` in te3, MD5 equal |
| `swctrl poe set off id 1` | `swctrl poe set auto id 1` | `Auto`, `Class 6`, power back |
| `swctrl poe restart id 1` | none needed | power back after 6 s |
| `switch.port.1.poe=shutdown`, `apply-config` | original `system.cfg`, `apply-config` | `Auto`, power back, MD5 equal |
| `cli -e` te1 ` shutdown` (two times) | `cli -e` te1 ` no shutdown` | `U/U 2500F`, `mca-dump` `up: true` |
| `cli -e` te3 ` spanning-tree bpdu-filter` | `cli -e` te3 ` no spanning-tree bpdu-filter` | `BPDU Filter: Disabled` |
| Files `/tmp/uswctl-sys.orig`, `/tmp/uswctl-link.scr`, `/tmp/uswctl-bpdu.scr`, `/tmp/uswctl-rc-a.txt`, `/tmp/uswctl-rc-b.txt` | deleted by name | `ls` shows none |
| Lock file `/tmp/agent-lock` | deleted at the end | `ls` shows none |

End state at 14:49: port 1 PoE `Auto`, `Class 6`, 9.63 W, link `U/U 2500F`, `mca-dump`
`up: true`. Port 11 `link_down_count` was 0 at the start and at the end.

## Open Items

- Spec item 5 (MSTP keys) and item 6 (BPDU filter on a port with STP off, and the leak
  test) are not part of this discovery.
- The `status` key is not tested on the Pro Max 16, and not through a full provision or
  a boot.
