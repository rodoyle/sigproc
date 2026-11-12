# Native Windows trusted-LAN VRT control

Status: helper, tests and this plan are new files. `test/vrt_lan_control.py` and
`test/test_vrt_lan_control.py` pass the local loopback suite only. The live
`192.168.0.15`/`192.168.0.55` path, the collector's new `[control]` LAN settings and
the radio demonstration are **not** run or verified here. `deploy/configmap.yaml`
now holds the desired LAN `[control]` block but has not been applied. No deployment,
firewall change, account setup, WSL or SSH is part of this work; AI/agent roles did
not and must not run live commands. Parent/main-agent review plus operator radio
confirmation is required before any live operation.

Files: `test/vrt_lan_control.py` (client, optional relay), `test/test_vrt_lan_control.py`
(tests), this plan.

## Why direct trusted-LAN packets

The reviewed loopback path (`test/vrt_control_bridge.py`,
[vrt-control-bridge.md](vrt-control-bridge.md)) relays VRT datagrams inside SSH
stdio frames so the collector can keep a loopback-only `authorized_controller`.
The operator chose the simpler direct route instead: native Windows Python binds
the exact LAN source and sends VRT UDP straight to the collector, with no SSH,
no WSL, and no extra process on the node. The Windows host must therefore be the
one peer named in the collector configuration.

```text
Soapy plugin -> 127.0.0.1:5599            optional local relay, query-only by default
  -> 192.168.0.15:50000                   bound controller source (Ethernet4, index 17)
  -> 192.168.0.55:4991                    collector [control] ctrl_addr/ctrl_port
  -> authorized_controller = "192.168.0.15:50000", stream_id = 1
```

Desired collector settings (source only, not applied):

```toml
[control]
enabled = true
ctrl_addr = "192.168.0.55"
ctrl_port = 4991
authorized_controller = "192.168.0.15:50000"
stream_id = 1
```

Only that one source tuple is permitted; the whole subnet is not added to a
collector allowlist. The client still refuses any endpoint that is a hostname,
unspecified (`0.0.0.0`), multicast, broadcast, outside the authorized subnet, or
on any port other than the fixed pair. The LAN source/destination ports are
constants in the module, not CLI options, so a run cannot silently choose a
different interface or port.

Windows note: `192.168.0.15` is the Ethernet4 adapter (interface index 17). The
client binds that address explicitly; it never binds an unspecified address, so
there is no implicit interface fallback. Confirm the mapping with
`Get-NetIPAddress -IPAddress 192.168.0.15` before a live run.

## Trusted-LAN exposure caveat

The exact-peer allowlist is **not authentication and has no replay protection**.
Any host that can spoof or occupy `192.168.0.15:50000` on the trusted
`192.168.0.0/24` LAN can issue whatever the collector accepts. Every start
therefore requires the explicit `--trusted-lan` acknowledgement, and that flag is
an operator assertion, not a security control. Keep the collector on the
authorized trusted LAN only; do not route or expose `192.168.0.55:4991` to other
networks, and do not add subnet-wide firewall permissions for this demonstration.

The optional relay listens on loopback. Loopback peer pinning is likewise not
client authentication: only one plugin peer is remembered, after a validated
command, and its ACKs are returned from the same bound socket. Relay execute
additionally requires `--trusted-local-session` (an assertion that the desktop
session is OS-isolated), and query-only remains the default.

## Reused protocol validation

No ACK parsing or ID handling is re-implemented. The module imports:

- `parse_command`, `check_command`: the bounded type 6 profile (stream/controllee,
  CIF0 restricted to bandwidth/RF/gain/sample-rate, query-state or immediate
  execute action 2, no ACK/cancel/reserved bits) applied before any send.
- `check_relayed_ack`: declared length, echoed stream/message ID and identifier
  fields, supported field layout, positive readback, query-vs-execute outcome,
  and partial-action bit 11 when hardware readback differs from the request.
- `RemoteBridge.handle`: fresh random-session wire message ID per exchange, ACK
  correlation on that ID (stale datagrams ignored up to `--max-ignored`), full ACK
  verification, and restoration of the client message ID. The `timeout` path
  raises and never resends.
- `LocalBridge` for the relay: single pinned loopback plugin peer, policy checks,
  synthesized execution-error ACKs without receiver readback, and answers from
  the bound listener socket.

`test/vrt_lan_control.py` is therefore small: LAN transport, endpoint validation,
command builders, the tune workflow and CLI policy.

## Commands (native Windows, `cmd.exe` or PowerShell)

Run from the repository root with the native Windows Python (no WSL). Nothing
below executes unless the operator runs it.

Read-only query first (default query-only, no execute path):

```bat
python test\vrt_lan_control.py query --trusted-lan --timeout 3
```

The tune workflow requires explicit execute and a finite hold, and always tries to
restore the frequency that the initial query reported:

```bat
python test\vrt_lan_control.py tune --trusted-lan --allow-execute --frequency-mhz 145.310 --hold-seconds 60 --timeout 3
```

Alternative demonstration frequency: `--frequency-mhz 448`. Hold must be in
`(0, 120]` seconds. Optional loopback relay for the Soapy plugin (query-only):

```bat
python test\vrt_lan_control.py relay --trusted-lan --bind 127.0.0.1 --ctrl-port 5599
```

Relay mutation is opt-in and additionally needs the trusted-session assertion:

```bat
python test\vrt_lan_control.py relay --trusted-lan --allow-execute --trusted-local-session --bind 127.0.0.1 --ctrl-port 5599
```

An armed relay accepts **RF-only executes**. It rejects gain, rate, bandwidth and
mixed-setting executes without forwarding them to the collector. Unlike `tune`,
the GUI relay does not impose a hold timer or restore the initial RF automatically;
GUI changes persist until explicitly changed or the collector restarts.

Exit codes: `0` success, `1` policy/transport/timeout failure, `2` the rollback
execute failed and the collector RF is **not** confirmed. A failed query aborts the
tune before any execute.

For automation, `query_state(args)` returns the complete validated state and
`query_frequency(args)` returns the RF frequency in Hz. These return values do
not depend on best-effort diagnostic delivery; `run_query` retains CLI exit codes.

Native Windows named-mutex ownership spans each whole query/tune/relay session,
including the tune hold and fresh-socket restoration. Another cooperating process
or thread fails before sending commands. This is coordination, not authentication;
a different tool that ignores the mutex is outside that protection.

## Tune workflow and rollback contract

1. Query the collector state and require a complete readback; record the original
   RF frequency.
2. Send one immediate execute that changes **only** `rf_freq_hz`
   (`cif0 = RF`). Rate, gain and bandwidth are never part of this command.
3. Print the collector's actual ACK identity, outcome and readback; a partial-action
   bit 11 readback is shown as returned, not hidden.
4. Hold for the explicit `--hold-seconds` (at most 120 s).
5. In `finally`, if the target execute may have reached the collector, send a
   separate restore execute for the original queried frequency on a fresh socket
   with fresh session message IDs. The restore is a distinct command, never a
   resend of the target.

If the target ACK timed out, the execute may still have been applied, so the
restore is still attempted; the run exits non-zero. If the collector answered with
a definitive execution-error ACK, nothing was applied, so no extra execute (and no
extra capture interruption) is issued. If the restore is rejected, mistimed or
reads back a different frequency, the command reports it on stderr and exits `2`
instead of claiming success. No command is ever retried automatically.

An execute stops and restarts the single UHD stream, so each applied execute
creates a data discontinuity and new readback context. Settings are volatile and
return to the `[rf]` values when the collector restarts; make a persistent change
in the ConfigMap, never by re-sending this command.

## Soapy plugin and relay

The relay exists only if the plugin must keep using a loopback control address.
Configure the plugin with explicit named arguments (kwargs), for example:

```ini
ctrl_addr=127.0.0.1
ctrl_port=5599
control=on
sid=1
ctrl_timeout=4000
```

`ctrl_timeout=4000` (ms) is longer than the 3 s exchange deadline so the plugin
does not give up first. `sid=1` must match `--stream-id` (default 1). Start the
relay and confirm its diagnostic lines before opening the GUI. The relay tells the
collector nothing about the plugin: the plugin's ephemeral UDP port is pinned
locally and never becomes the authorized LAN source.

### 145.310 MHz and demodulation

`145.310 MHz` is in the 2 m amateur band, where signals are usually narrowband
**FM**. AM or USB demodulation of that activity is expected to sound wrong even
when the retune itself is correct; the alternative `448 MHz` (70 cm) is also
usually FM. Confirm the modulation with the operator and the radios before
judging the demonstration. Neither frequency proves the tuning path by itself —
the ACK/readback and the radios' observed behavior do.

## Tests

```bat
python -m unittest discover -s test -p test_vrt_lan_control.py -v
```

Covered locally with a real loopback UDP mock collector and the injected
`127.0.0.0/8` authorized subnet (never `192.168.0.0/24`):

- fixed default profile and exact endpoint rejection (hostname, unspecified,
  multicast, broadcast, out-of-subnet, ephemeral/out-of-range ports);
- query-only default policy, RF-only execute contents, and execute denial without
  `--allow-execute`;
- CLI acknowledgement/hold/trusted-session gates that fire before any socket;
- query round trip from the fixed `127.0.0.1:50000` source with a full readback,
  stale-ACK rejection by wire ID correlation, and a single timed-out send with no
  retry;
- tune flow: original query, RF-only target execute, acknowledged readback, hold,
  restore of the original frequency with a distinct message ID;
- uncertain target execute (no ACK): no resend, rollback still attempted, non-zero
  exit; definitive error ACK: non-zero exit and no needless rollback execute;
  failed restore: exit `2`, no retry;
- relay round trip: client message ID restored, wire ID hidden from the plugin,
  query-only denial of a 100 MHz startup execute (collector sees no traffic), and
  an explicitly allowed execute returning the applied ACK.

The suite does not spawn itself recursively, does not use SSH/WSL, never targets
the real LAN peers, and skips when the fixed loopback ports `50000`/`4991` are in
use. The existing reviewed relay suite still passes unchanged:

```bat
python -m unittest discover -s test -p test_vrt_control_bridge.py
```

## Acceptance checklist

| # | Check | Evidence state |
|---|-------|----------------|
| 1 | LAN client unit suite passes (loopback mock collector only) | verified locally |
| 2 | Reviewed loopback/SSH relay suite unchanged | verified locally |
| 3 | Fixed endpoints, subnet and port rejection | verified locally |
| 4 | Tune sends RF only, shows ACK readback, restores original | verified locally (mock collector) |
| 5 | No automatic retry of an uncertain execute | verified locally |
| 6 | Collector LAN `[control]` block applied and loaded | not done (desired config only) |
| 7 | Live query round trip on `192.168.0.15`/`192.168.0.55` | not verified |
| 8 | Live 145.310 MHz tune + rollback with radio confirmation | not verified (operator gate) |
| 9 | Windows adapter is Ethernet4/index 17 with `192.168.0.15` | not verified here |

Steps 6-9 need the operator/parent review gate; this work performs none of them.

## Intentional limitations

- Single controller, one outstanding exchange; the collector owns the exact
  `192.168.0.15:50000` source tuple, so a second controller cannot share it.
- No authentication, encryption or replay protection; confidentiality and peer
  identity rest entirely on the trusted-LAN boundary.
- No discovery, no device ownership, and no demodulation change: RF frequency only.
- A timed-out execute remains ambiguous. The restore distinguishes "verified
  original" from "unknown", but it cannot prove what happened during the gap;
  inspect the collector's live context before any further attempt.
- No install, deployment, firewall change, account/key setup, Soapy invocation or
  WSL/SSH dependency.
