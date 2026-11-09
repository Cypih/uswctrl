# uswctl port locate

Design note, 2026-11-06. A locate shows a person which port to use, for example "put the
cable into the blinking port". A locate must end by itself. An agent or the web page can
also start and stop it.

## Device facts (tested on the USW Pro Max 16, firmware 7.5.15.17146)

- `syswrapper.sh set-port-locate <hi> <lo>` makes the chosen ports blink blue (colour
  `0515ff`) and shows the same ports on the front touchscreen ("locating ports" with a
  port graph). All other port LEDs go dark while a locate runs.
- `<hi>` and `<lo>` are hexadecimal port bitmaps, high word first, without `0x`. Port N
  is bit N-1: ports 1 to 32 are in `<lo>`, ports 33 and up are in `<hi>`. Ports 1 and 2:
  `set-port-locate 0 3`. The LED driver documents the format in
  `/proc/led/led_port_bitmap` ("55aaa a5a5a5a5" for 52 ports).
- `syswrapper.sh unset-port-locate` ends the locate and restores the normal port LEDs.
- The functions are in `/usr/etc/syswrapper.sh` (`set-port-locate)`, `unset-port-locate)`)
  and `/usr/etc/platdep_funcs.sh` (`_set_port_locate`, `_set_port_bitmap_color`). On a
  model without `/proc/led`, the same functions use the Etherlight daemon over ubus
  (the XG 10). The XG 10 path is not tested.
- A free colour through `/proc/led/led_code` does not work while a locate runs (the
  port went dark). Free colours are out of scope.

## Decisions

- L1. One locate at a time for each switch. A new locate replaces the running one (its
  ports and its end time).
- L2. Duration: default 300 s; allowed 10 to 3600 s. The worker ends the locate at the end
  time with `unset-port-locate`. A stop request ends it at once.
- L3. A switch reboot ends a locate; the worker clears its locate state when it sees the
  reboot. A daemon start sends `unset-port-locate` once to each switch only if the
  daemon's own last state file says a locate ran. If this needs a state file, skip it and
  record it as a known issue instead.
- L4. A locate changes only LEDs and the touchscreen. A new locate respects a foreign lock
  at `lock_path` (like every action). A stop and the automatic end only restore the
  normal LEDs, so they run also while another process or person holds the lock. A locate
  needs no reservation and it is allowed on protected ports, for the agent token too.
- L5. API: `POST /api/switches/{sw}/locate` with `{"ports":[1,2],"duration_s":300}`
  answers `{"switch","ports","until","cli_lines"}`; `DELETE /api/switches/{sw}/locate`
  ends it. `DeviceState` gets `locate: Option<{ports, until, end_failed}>`; `end_failed`
  is true while a failed end waits for its retry at the next poll.
- L6. Client: `uswctl locate <sw> <ports> [--for 5m]` (ports as `1,2` or `1-4`) and
  `uswctl locate-off <sw>`; `uswctl ports` shows the locate.
- L7. MCP: tools `port_locate(switch, ports, duration_s?)` and `port_locate_off(switch)`;
  no owner and no reservation needed (L4).
- L8. Web page: a locate button on each port row, a banner with the located ports, the
  remaining time and a stop button.

## Tests

- Unit: the bitmap for ports 1, 2, 32, 33 and 52; the script text; the worker ends the
  locate at the end time (paused tokio time) and on a stop; a new locate replaces the
  old one; a reboot clears the state; a foreign lock refuses a locate.
- API, client and MCP tests against the fake devices.
- Device: one supervised run on the Pro Max 16 through the daemon, with a person who
  watches the LEDs. The XG 10 was not available for this test.
