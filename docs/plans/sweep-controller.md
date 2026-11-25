# Sweep controller (140-150 MHz, 12.5 kHz NBFM steps)

**Status:** implemented and covered by unit + simulated-socket tests. **Not yet run
against the live cluster** — that live sweep is the operator's step, and nothing in
this document should be read as a claim that it has happened.

This is sequencing step 4-and-5 of the approved design in
[`service-c2-and-networking.md`](service-c2-and-networking.md): fix one wideband RF
capture across 140-150 MHz, then move a 12.5 kHz NBFM slice through that band
*digitally*. A step therefore costs a channelizer retune, never a B210
stop/start, and the radio is never handed back and forth mid-sweep.

Crate: `sweep/` (package `sigproc-sweep`, binary `sigproc-sweep`). The library holds
all the logic; the binary is CLI wiring plus an HTTP client.

## Why this is a controller and not a shell loop

Four properties are acceptance criteria, and each one is a type or a test rather
than a convention:

| Property | How it is enforced |
| --- | --- |
| **Bounded** | `SweepPlan::new` refuses a zero step, an inverted range, or any range outside 140 000 000-150 000 000 Hz *before* anything moves. The band plan is 800 NBFM channels whose centres are inset by half a channel, so the channels tile 140.000-150.000 MHz edge-to-edge and none is clipped by the capture boundary. |
| **Applied readback, never an assumed setter** | Every step records the offset/rate the service says it applied. `SweepReport::applied_matching()` counts steps whose readback matched the request, so a service that accepts a retune and applies something else cannot be reported as confirmed. |
| **No retry of an uncertain execute** | `ControlError` separates *not sent* (connect failed — the setting cannot have moved) from *uncertain* (request written, reply lost — restore owed, never re-sent) from *rejected* (service answered and refused). The sweep stops on the first bad step; nothing is retried. |
| **Serialized against other controllers** | `ControllerLock` takes an exclusive lock file (`create_new`) for the whole run, including `--restore-from`. A second controller is refused. `--break-stale-lock` breaks it only when the file is older than `--stale-after-ms`, so an impatient second invocation cannot clobber a live sweep. |
| **Restore on exit** | The original channel is put back on *every* path — completion, step budget, deadline, operator interrupt, refused/unsent/uncertain step — and the restore is read back and reported. |

Frequencies are integer hertz everywhere in the controller. The channelizer's C2
JSON carries `f64` for its DSP types; an integer JSON number round-trips exactly
through those fields, so no float ever enters a request. Readback is converted back
to integers and a **fractional** value is reported as *uncertain* rather than
rounded — a value we cannot represent is a value we cannot verify.

## Prerequisites

1. **A fixed wideband capture covering the band.** Centre 145 000 000 Hz, sample
   rate 10 000 000 Hz (the plan derives this as `2 x half-width`). The controller
   checks the applied RF when `--capture-c2` is given:
   - coverage sufficient → the RF is left alone and reported as
     `AlreadySufficient`;
   - coverage insufficient, `--apply-rf` absent → the sweep is **refused** and
     nothing is mutated (`exit 5`);
   - coverage insufficient, `--apply-rf` present → the wideband is applied after
     its original value is recorded, and restored on exit.

   Without `--capture-c2` the RF is not consulted at all (`NotUsed`) and the run
   assumes an operator already put the wideband where it belongs.
2. **The channelizer's HTTP C2 plane reachable** — `GET /state`, `POST /config`
   (`--channelizer-c2 <host:port>`).
3. **The channelizer must accept the requested rate.** `POST /config` validates
   that `output_rate_hz` divides the input rate exactly; with a 10 MS/s wideband,
   12 500 Hz divides 800 times, so the NBFM channel rate is valid. A refusal is
   reported as a refused step, not as an uncertain one.

## Running it

Smoke run — 5 dwell points at the bottom of the band, no RF changes:

```bash
sigproc-sweep --channelizer-c2 10.42.0.169:8080 \
    --start-hz 140006250 --stop-hz 140031250 --step-hz 12500 \
    --max-steps 5 --report-file sweep-smoke.json
```

The full band (800 steps, the default plan), with the RF verified and moved only
if it must be:

```bash
sigproc-sweep --channelizer-c2 10.42.0.169:8080 \
    --capture-c2 192.168.0.55:8081 --capture-token "$SIGPROC_C2_TOKEN" \
    --apply-rf --settle-ms 120 --report-file sweep-full.json \
    --recovery-file sweep-original.json
```

Every step prints the applied readback and the settled observation:

```text
sweep: 140006250..149993750 Hz step 12500 Hz -> 800 dwell points, channel rate 12500 Hz, centre 145000000 Hz
offsets: -5000000..5000000 Hz relative to the fixed wideband
step 1/800 target 140006250 Hz offset -4993750 Hz -> applied offset -4993750 Hz rate 12500 Hz settling 64 samples | power -61.5 dBFS, peak 0 Hz, packets 42, gaps 0
...
stop: completed — 800 steps applied with matching readback of 800 recorded
original channel: offset -1250000 Hz rate 12500 Hz
restore: original channel read back as applied
rf: already covered the plan at 145000000 Hz / 10000000 Hz, left alone
```

Stop it cleanly with **Enter on stdin**: the current step finishes and the channel
is restored. A hard Ctrl-C kills the process without unwinding, so the recovery
file is written *before* the first mutation and repairs it afterwards:

```bash
sigproc-sweep --channelizer-c2 10.42.0.169:8080 \
    --restore-from sweep-original.json \
    --capture-c2 192.168.0.55:8081 --capture-token "$SIGPROC_C2_TOKEN"
```

## Exit codes

| Code | Meaning | Was anything mutated? |
| --- | --- | --- |
| 0 | completed, or stopped at its own step budget/deadline | yes, and restored |
| 2 | interrupted by the operator | yes, and restored |
| 3 | a step was refused, undelivered, or uncertain | up to that step, and restored |
| 4 | the restore did not read back | **unknown — apply the recovery file** |
| 5 | preflight refused to sweep | **no** |
| 6 | usage, plan, or lock error | **no** |

## Verification

`cargo test -p sigproc-sweep` (37 tests) covers:

- **endpoints** — 800 channels tiling 140.000-150.000 MHz edge-to-edge with a
  constant 12 500 Hz step, the outermost channels reaching exactly the band edges;
  offsets running -4 993 750 Hz to +4 993 750 Hz about the fixed centre; a partial
  plan still visiting its own endpoints; and the default plan fitting a 10 MS/s
  capture exactly (half a channel past the outermost centre is exactly the
  half-band);
- **channel fit** — a plan centred on the band edge is reported as clipped (with the
  inset that would fix it) while still running and restoring; an edge-aligned plan
  records no such warning;
- **errors** — zero step, inverted range, and one step outside the licensed window
  refused before any mutation; a refused step; an undelivered step (no restore
  owed); an uncertain step (restore owed, attempted exactly once); a failed
  observation that does not discard the step; a lying readback that is recorded
  rather than trusted; a restore that fails and is reported while the recovery file
  keeps the original;
- **interruption** — the stop flag honoured between steps with a clean restore;
  step-budget and deadline bounds;
- **restore** — the original channel applied and read back on completion, failure,
  interruption, and after an RF that had to move; `--restore-from` round-trip;
- **the wire** — `sweep/tests/simulated_sweep.rs` drives the real HTTP client
  against a stub channelizer on a real socket, asserting the exact sequence of
  applied retunes and that the original channel is written back, including when a
  reply is lost mid-sweep.

## Known limitations

- **The channelizer's `POST /config` has no authentication today.** The controller
  sends `--channelizer-token` if one is configured, but the service does not check
  it yet. The design's non-goal ("no unauthenticated mutation endpoint") therefore
  binds the listener being pod/LAN-scoped — do not expose it beyond the cluster
  network until the channelizer grows an authorization decision.
- **No live run yet.** These are unit and simulated-socket tests; they prove the
  controller's own behaviour, not that the cluster accepted 800 retunes.
- **The observation is per-step, not per-signal.** `output_power_dbfs` and
  `peak_offset_hz` say whether the channel was alive; they are not a detection
  record. Demod/NBFM output per step is the next layer (sequencing step 6).
