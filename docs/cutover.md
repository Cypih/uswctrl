# Cutover from the UniFi controller

This runbook moves switches of the two supported models (USW Pro Max 16 PoE and USW Pro
XG 10 PoE) from the UniFi Network application to `uswctld`. It follows spec section 6.7.
Each step names its actor and the check that confirms the step worked. The actor is the
user (the administrator, with the user token) or an agent (a person or a script with the
agent token).

Do the steps in order. Do not skip step 3: while the UniFi controller runs, it can push
`system.cfg` to a switch and undo a change that `uswctld` made.

## 1. Install the daemon

**Actor:** user.

Build and install `uswctld` on the daemon host, write its config file, and start it. In
the config, list each switch, set `protected_ports` to the ports that carry the network
itself (for example the uplink and the link to the daemon host), and set `ssh_user`. See
the "Build", "Configure the daemon" and "Run" sections of `README.md`.

**Check:** `systemctl --user status uswctld` shows the service active, and
`uswctl switches` lists both switches with their current firmware.

## 2. Let the daemon read both switches

**Actor:** agent.

With the UniFi controller still running, read both switches through `uswctl`:

```sh
uswctl ports promax16
uswctl ports xg10
```

Use the switch names from the config; `promax16` and `xg10` are the names in
`config.example.toml`.

**Check:** the port list of each switch is not empty. With the example config,
`uswctl switches` shows `true` in the `TESTED` column for the XG 10. The Pro Max 16 shows
`false` until step 5, because the example config lists no tested firmware for it.

## 3. Confirm the daemon agrees with the controller

**Actor:** agent, then user.

Compare the settings that `uswctl ports <switch>` reports against the UniFi
controller's own view of the same switch:

- the first line: STP version, STP priority, BPDU action and VLANs
- for a sample of ports on each switch, the columns `NATIVE`, `TAGGED`, `POE`, `STP`,
  `BPDU-FILTER` and `ENABLED`.

**Check:** the user confirms that the two views agree. If they do not agree, stop and
find the cause before the next step. A mismatch means that `uswctld` read the wrong
`system.cfg`, or that the port table in `src/model.rs` (from `docs/device-notes.md`)
does not match the switch.

## 4. Stop the UniFi Network application

**Actor:** user.

Each switch keeps its own configuration (VLANs, port settings, protected ports) on its
own flash. A stop of the UniFi Network application does not change any port and does not
cut the network. Up to this step, the daemon only reads the switches; it writes nothing
until step 5.

Stop the UniFi Network application on the controller host. The daemon does not stop it:
only the user does this step.

Do not use "Forget" on a switch in UniFi. "Forget" probably resets the switch to its
factory defaults; the spec does not test this, and `uswctl` has no step that depends on
it.

**Check:** the UniFi Network application reports as stopped, or its web page does not
answer.

## 5. Apply the BPDU setting through the daemon

**Actor:** user (a switch-wide setting needs the user token).

Set the global BPDU action to filtering, through the web page or through the API with
the user token. Do the XG 10 first:

```sh
curl -s -X PATCH http://127.0.0.1:8650/api/switches/<switch>/global \
  -H "Authorization: Bearer <user_token>" \
  -H 'Content-Type: application/json' \
  -d '{"bpdu_action":"filtering"}'
```

The Pro Max 16 needs one more preparation, because this is the first change through the
daemon on that model. The daemon refuses a change and a dry run on a firmware that is not
in `tested_firmware`:

1. Add the firmware version that `uswctl switches` shows for the Pro Max 16 (for example
   `"7.5.15.17146"`) to its `tested_firmware` in `~/.config/uswctl/config.toml`.
2. Restart the daemon: `systemctl --user restart uswctld`.
3. Run the change as a dry run first: add `?dry_run=1` to the URL above, or use the web
   page, which shows the dry run before it applies.
4. Review the dry run: it must show only the BPDU overlay lines and no `system.cfg` key.
5. Apply the change without `dry_run`, and watch the uplink and the network while it
   runs.

**Check:** the first line of `uswctl ports <switch>` shows `BPDU action filtering` for
both switches, and the change result's `overlay_changed` is `true`. `uswctl switches`
shows `true` in the `TESTED` column for both switches. A BPDU capture on a port with STP
off no longer shows BPDUs from another STP domain.

## 6. Keep the UniFi controller stopped

**Actor:** user.

Leave the UniFi Network application stopped. If it starts again and the user changes a
switch there, the controller overwrites `system.cfg` on that switch and undoes the
daemon's settings, including the BPDU filter from step 5.

**Check:** none while the controller stays stopped. If the controller starts for another
reason, repeat step 3 before trusting `uswctld` again.

## Rollback

**Actor:** user.

Start the UniFi Network application again. The next time the controller pushes a change
to a switch, it sends its own stored `system.cfg` and overwrites the daemon's settings on
that switch, including the CLI-only BPDU filter.

**Check:** the UniFi Network application reports as running, and a later read of the
switch through the controller shows its own settings, not the daemon's.
