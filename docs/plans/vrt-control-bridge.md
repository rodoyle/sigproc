# VRT control bridge (loopback UDP <-> framed SSH stdio)

Status: helper, tests and this plan are new files; nothing was installed, deployed, or
run against the cluster, hardware, or a real SSH target. The Linux collector build and
its VITA 49.2 command handling were verified in earlier work; the live bridge and live
retune path are **not** verified. Local unittest coverage proves loopback/framing/policy
behaviour only, not SSH, Windows plugin, or hardware behaviour.

Files: `test/vrt_control_bridge.py` (helper, both ends), `test/test_vrt_control_bridge.py`
(tests), this plan.

## Why `ssh -L` is not enough

The collector accepts control datagrams only from the fixed ACL peer
`authorized_controller = "127.0.0.1:50000"` (`common/src/control_config.rs`,
`docs/plans/vita49.2-host-profile.md`). A TCP `-L` forward cannot:

- present that exact UDP source port to the collector;
- pin one ephemeral-port plugin client and answer from the bound listener socket;
- reject malformed or unsupported commands before they reach the controllee;
- guarantee no automatic retry of a mutation.

So the bridge relays whole VRT datagrams inside explicit frames and enforces the same
bounded profile as `common/src/vita49_control.rs` on both ends.

## Topology and fixed addresses

```text
Soapy ephemeral-UDP source (ctrl_addr=127.0.0.1, ctrl_port=<shim port>)
  -> Windows 127.0.0.1:<local_port>            local helper, single pinned peer
  -> 4-byte BE length frames over SSH stdio    dedicated account forced command
  -> node 127.0.0.1:50000 (connected UDP source; exactly the ACL peer)
  -> collector 127.0.0.1:4991 (unchanged ctrl_addr/ctrl_port/authorized_controller)
```

- Local mode binds only a numeric IPv4 loopback address (`--bind`, default `127.0.0.1`).
- Remote mode binds exactly `127.0.0.1:50000` and `connect()`s to `127.0.0.1:4991`, so
  only collector datagrams are accepted and replies cannot be sourced elsewhere.
- The collector keeps its single UHD owner, has no LAN listener, and its ACL is not
  widened by this bridge. The ACL must stay `127.0.0.1:50000`.
- `remote` writes only frames to stdout; every diagnostic goes to stderr.

## Framing

- Each frame: 4-byte big-endian length, then exactly that many bytes; 1..4096 bytes.
- Length 0, larger than the cap, trailing garbage, or a short payload is a protocol
  error. An EOF at a frame boundary is orderly shutdown (`None`); an EOF inside a frame
  is truncation and fails closed. Both are distinct classes.
- Exactly one outstanding request per direction, then one bounded exchange
  (`--timeout`, default 3 s, max 60 s). A timeout kills the transport, exits non-zero,
  and never resends. Windows pipes are read by a daemon thread feeding a bounded
  queue, so a deadline exists even though `selectors` cannot wait on Windows pipe
  handles. Remote idle time may be unlimited; once the first byte of a frame
  arrives, its remaining prefix and payload must arrive within the frame deadline.
- Frame writes and cleanup are deadline-bounded too, including a peer that stops
  reading stdout. Diagnostics use a bounded, lossy queue and raw descriptor writes
  on a daemon thread; unread stderr cannot stall commands or retain the source
  socket. Diagnostic records can be dropped under backpressure.
- Frames that are not a parseable command never reach the collector; the local end drops
  them, and the node end treats a framed malformed request as a fatal protocol error.

## Bounded command profile (both ends)

Accepted: VRT type 6 only, declared word count equal to the datagram length, no
ACK/cancel/reserved header bits, no reserved CAM bits, configured `stream_id`, optional
controllee ID equal to that stream, non-empty CIF0 restricted to the four supported
settings (`bandwidth`, `rf_freq`, `gain`, `sample_rate`), and either a query-state
command (`CAM` query bit, action 0) or an immediate execute (action 2) with no
contradictory query bit. Dry-run and other action modes are rejected.

`--allow-execute` is required on **both** ends for a mutation. Defaults are query-only;
keep the flag absent during startup/query verification. A well-framed rejected
command gets an execution ACK with its error bit set so the plugin can preserve
state instead of waiting for a timeout. Stream, message ID and ID flags are echoed,
but CIF0 is empty: the relay has no authoritative hardware readback and does not
repeat rejected setting values as if they were receiver state. A successful ACK is
never synthesized; the only success path is the collector's own correlated ACK.

Node ACKs are correlated on fixed peer (connected socket), type 6 ACK flag, declared
length, stream ID and message ID. Stale/out-of-order/non-ACK datagrams are ignored
inside the deadline up to `--max-ignored` (default 32), then the exchange fails closed.
A successful query response carries query-state ACK bit18, not execution ACK bit19
or executed bit10. Execute responses require bit19 and either an executed result or
an error result. Both ends check the response kind as well as its identity; an
independent literal query-ACK fixture protects this Rust/Python interoperability.

Every relayed ACK is parsed and checked for declared length, IDs echoed from the
command, supported field layout, state readback and outcome. A query policy denial
carries execution-error ACK bit19 plus error bit16 (with no executed bit10) and is
relayed, not mistaken for a transport failure. Successful queries have a complete
confirmed-state body; neither a truncated header-only ACK nor a successful execute
ACK can masquerade as a successful query.
Requested execute fields must match the confirmed readback unless partial-action
bit11 explicitly reports coercion. An error ACK cannot simultaneously claim query,
execution or partial-action success.

The node helper substitutes its own random-session, non-repeating wire message ID
for the client-provided ID, then restores the client ID in the returned ACK. A
delayed ACK from an earlier timed-out session cannot match a new request merely
because the GUI reused the same message ID. This reduces stale-correlation risk
within the documented single-client relay; it is not durable replay protection or
a substitute for authenticated SSH/local client isolation.

## Ephemeral plugin source and peer pinning

The Soapy plugin sends from an ephemeral UDP port. The local helper remembers the first
sender as the single pinned control peer, answers it from the same bound listener socket
(`sendto` on the listening socket, never a second socket), and drops datagrams from any
other source so two controllers can never be cross-delivered. It then serializes one
exchange at a time.

Loopback and peer pinning are **not client authentication**. Pinning happens only
after a command passes validation; malformed or policy-rejected traffic cannot
claim the client slot. Before arming execute, the operator must establish OS-level
isolation of the listener and approved GUI on a trusted host/VM or equivalent
network/process boundary with no untrusted local senders. Merely using separate
user accounts on a shared loopback network is insufficient. Local execute also
requires `--trusted-local-session` to acknowledge this prerequisite; that flag is
an operator assertion, not an authentication mechanism. If isolation cannot be
established, keep both ends query-only.

## Soapy plugin configuration (Windows)

The plugin must be configured with explicit driver kwargs, for example:

```ini
ctrl_addr=127.0.0.1
ctrl_port=<the same port as `local --local-port`>
control=on
sid=1
ctrl_timeout=4000
```

Use these as Soapy `SoapySDR.Device` named arguments / kwargs mapping entries, e.g.
`{"driver": ..., "ctrl_addr": "127.0.0.1", "ctrl_port": 5599, "control": "on",
"sid": "1"}`. `-m` is a CLI flag of other utilities and is **not** a kwargs key;
passing it in place of named arguments silently leaves the plugin with defaults. The
installed plugin ABI is SoapySDR 0.7, so the current 0.9 command-line utility cannot be
used to prove or configure this path.

Start the local bridge first and wait for its SSH remote helper to report the bound
node source endpoint before opening the GUI. The plugin's default 500 ms timeout
may be too short for a relay; the example uses 4000 ms, longer than the helper's
3 s exchange deadline. This timeout setting does not authorize any execute.

The Soapy argument set a receiver can persist is device-specific. Cubic radios must be
checked to retain `ctrl_addr`/`ctrl_port`/`control`/`sid` across a restart before
control is enabled at all.

### Cubic 100 MHz startup hazard (blocking gate)

The receiver can report 100 MHz while the stream context says 915 MHz. Enabling plugin
control may therefore issue `setFrequency(100e6)` as a startup execute, which would
retune the running capture. Required sequence, in a maintenance/security-verification
gate only:

1. Keep the bridge query-only (`--allow-execute` absent on both ends). Record the
   actual startup commands: queries may reach the collector; any startup executes
   must receive error ACKs without reaching it, even if Cubic requests 100 MHz.
2. Confirm the Cubic's persisted Soapy arguments and the stream context both name the
   intended frequency.
3. Only then consider enabling execute, with the collector ACL and a single UHD owner
   unchanged.

This work does not invoke Cubic, open a device, or enable execute anywhere.

## SSH restriction (operator-installed, dedicated account/key)

Only the target alias and identity file appear in the `ssh` argv; **no remote command
string or CLI arguments are appended**, because OpenSSH would route those through the
remote login shell. The helper builds this argv list and runs it with `shell=False`:

```text
ssh -T -o BatchMode=yes -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes -o IdentityAgent=none -o ConnectTimeout=<1..60>
    -o ClearAllForwardings=yes -o ForwardAgent=no -o ForwardX11=no
    -o PermitLocalCommand=no -i <identity> <target>
```

The dedicated account's `~/.ssh/authorized_keys` must force the node endpoint. The
entry below is illustrative and must be a single line; the operator installs it and the
key manually, nothing here deploys:

```text
command="/opt/vrt/bin/vrt-control-bridge remote --bind 127.0.0.1 --source-port 50000 --collector 127.0.0.1 --collector-port 4991",no-pty,no-port-forwarding,no-agent-forwarding,no-X11-forwarding ssh-ed25519 AAAA... vrt-control
```

The account must enforce the fixed helper as `ForceCommand` (or an equivalent
forced command) for **every** authentication method and every authorized key,
not just one illustrative key. Disable unrestricted alternate keys, password/
keyboard-interactive authentication, and other shell, forwarding, agent and PTY
access for that account. The dedicated helper and its parent directories must not
be writable by untrusted users. `IdentitiesOnly` and `IdentityAgent=none` reduce
client-side identity fallback but do not replace the server-side restrictions.
The forced command and `no-*` restrictions, not caller input, fix the node policy.
Query-only is the node's own configuration (forced-command wrapper or node config); it
is never granted by a bit inside a caller-supplied frame.

## Exact launch commands

Local (Windows side; `--local-port` must equal the plugin's `ctrl_port`):

```bat
python test\vrt_control_bridge.py local --bind 127.0.0.1 --local-port 5599 ^
  --ssh-target vrt-control --identity "%USERPROFILE%\.ssh\vrt_ctrl" ^
  --connect-timeout 5 --timeout 3
```

Remote (node side, normally started only by the forced command):

```bash
python3 test/vrt_control_bridge.py remote --bind 127.0.0.1 --source-port 50000 \
  --collector 127.0.0.1 --collector-port 4991 --timeout 3
```

Mutation is opt-in and must be set on both ends: add `--allow-execute` to both commands
only after the gate above and verified local OS isolation; the local command additionally requires `--trusted-local-session`. `--local-port 0` / `--source-port 0` exist for tests only and
must not be used for a real run because the collector ACL names port 50000.

## Tests

```bash
python -m unittest discover -s test -p test_vrt_control_bridge.py -v
```

A separate Rust/Python localhost integration test uses the actual Rust control
socket and controllee with the Python remote bridge, not a Python ACK stub:

```bash
cargo test -p sigproc python_bridge_remote_interoperates
```

It verifies the real query-state ACK, unchanged confirmed settings and denial of
a 100 MHz startup execute before it reaches the controllee. It does not open UHD,
exercise SSH authentication, or prove the deployed collector.

Covered locally: framing roundtrip/exact length/zero/oversize/truncation/orderly EOF,
hung-pipe deadline and transport kill, profile rejection (type, ACK/cancel bits, stream,
controllee ID, CIF0, contradictory query+execute, reserved CAM), Q20 field decoding,
synthesized error ACK for a denied 100 MHz startup execute with no forwarded frame,
query forwarded once with the ACK returned from the listener socket, second-client
non-delivery, mismatched-ACK fail-closed, endpoint/limits/SSH argv validation, and a
local-helper + fake-SSH-transport (a local remote-helper subprocess) end-to-end query
with a fake collector, stale/non-ACK datagrams, and no retry on timeout.

The tests do not spawn the suite recursively, do not use SSH, do not touch the cluster,
and prove nothing about hardware, the deployed collector, or SSH authentication.

## Acceptance checklist

| # | Check | Evidence state |
|---|-------|----------------|
| 1 | Local + remote regression suite passes (localhost only) | verified locally |
| 2 | Linux collector build and VITA 49.2 profile | verified earlier |
| 3 | Query-only run issues no execute to the collector | verified locally (real Rust controllee, no UHD) |
| 4 | `127.0.0.1:50000` source and `127.0.0.1:4991` ACL unchanged | design/unit only |
| 5 | Forced-command key/auth, no shell/pty/forwarding | not verified (operator gate) |
| 6 | Live bridge query round-trip on the node | not verified |
| 7 | Live retune after Cubic arguments/startup gate | not verified (maintenance gate) |
| 8 | No LAN listener, single UHD owner preserved | not verified live |

Steps 5-8 need the maintenance/security-verification gate; no other human step is
required mid-run.

## Intentional limitations

- Single pinned local client, one outstanding exchange; no multi-controller fan-in.
- Size cap 4096 bytes per frame; the control profile only needs <= 60 bytes.
- Idle `stdin` wait is unbounded (a service must be able to sit idle); only exchanges
  are deadline-bounded.
- The synthesized rejection ACK contains no state fields; it is never a readback.
- Message IDs use a random 32-bit session start plus a per-session counter; a
  random collision across restarts remains possible. Correlation is not security
  authentication and uncertain executes must never be retried automatically.
- No install, deployment, key generation, ACL change, or Cubic/Soapy invocation.
