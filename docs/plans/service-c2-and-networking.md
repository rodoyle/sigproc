# Service C2 API and networking direction

**Status:** direction recorded, not implemented. Nothing here is deployed or
verified. This file records the operator's architectural decisions so later work
does not re-litigate them.

## Decision 1 — one consistent C2 pattern

Capture, Channelizer and Demod follow the same shape:

1. **Static configuration at startup** from the ConfigMap / environment
   (`config.toml`, `SIGPROC_CONFIG`, `DEMOD_*`). This remains the baseline and
   the restart-safe source of truth.
2. **A C2 API layered on top** for runtime changes, so a running service can be
   inspected and adjusted without a rollout.
3. **HTTP is the failsafe lowest common denominator** and must exist on all
   three services.
4. **VITA 49.2 is used where the service actually supports it** — today that is
   Capture only, because it is the sole B210 owner.

The two control surfaces must not be two implementations. Both enter the same
validated command handler on the owning thread, and both report **applied
readback**, never an assumed setter result.

| Service | Static config today | Runtime control today | Target |
|---|---|---|---|
| Capture (`sigproc`) | `[device] [rf] [forwarding] [control] [input]` | VITA 49.2 type-6 UDP: RF, rate, bandwidth, gain, query-state | Keep VITA 49.2; add HTTP C2 on the same handler |
| Channelizer | `[rf] [forwarding] [input] [channel]` | **None** — channel plan fixed at startup | HTTP C2 for channel offset/rate/passband; restart-free retune |
| Demod | CLI/env (`--listen`, `--metrics-listen`, `--output-dir`, segment/retention) | **None** | HTTP C2 for mode, channel, recording lifecycle |

### Shared C2 semantics

- Query is read-only and must never stop the RX streamer or restart a service.
- A mutation is validated, serialized, applied by the single owner, and
  acknowledged only after readback.
- An uncertain outcome is reported as uncertain and **never retried
  automatically**.
- Bounded request size, bounded exchange deadline, one outstanding mutation.
- Settings are volatile unless written back to the ConfigMap; a restart returns
  to the static configuration.

### HTTP surface (proposed, not implemented)

```text
GET  /v1/state      -> validated current settings + applied readback
POST /v1/settings   -> validated mutation, returns applied readback
GET  /healthz       -> existing liveness
GET  /metrics       -> existing Prometheus text
```

Constraints:

- The mutation endpoint is **not** added to the existing unauthenticated metrics
  listener. It needs its own listener and an explicit authorization decision
  (token/mTLS/NetworkPolicy) before it is exposed beyond the pod.
- No mutation endpoint may open a second device handle. Capture's HTTP path must
  reuse the existing owner-thread command handler and streamer stop/apply/restart
  sequence.
- Channelizer and Demod have no hardware to own, so their HTTP C2 is a
  configuration-and-lifecycle surface, not a device surface.

## Decision 2 — VITA 49.2 reference implementation

`https://github.com/Geontech/vrtgen` is recorded as the authoritative reference
for future VITA 49.2 work. The current Rust controllee is a deliberately bounded
subset (four settings, immediate execute, query-state, no scheduling,
cancellation, UUID addressing or state/event). Future protocol work should be
checked against vrtgen rather than against this repository's own encoder.

## Decision 3 — Demod mode: NBFM, not AM/DSB

The current antenna does not support AM/DSB well. **NBFM in a 12.5 kHz channel is
the appropriate mode** for this station.

Consequences:

- The `Demod` trait seam already exists; NBFM becomes the first *useful*
  implementation rather than an extension afterthought.
- The channelizer must produce a 12.5 kHz-class narrowband channel for NBFM
  (today it is configured for 50 kS/s at a 200 kHz offset).
- AM/DSB envelope detection stays in the tree as a tested implementation, but it
  is not the operational mode and must not be presented as a reception proof.
- Audio output rate and WAV format follow the NBFM channel, not the AM 50 kHz
  assumption.

## Decision 4 — Waterfall/Orbweaver is the packet-sync authority

Waterfall/Orbweaver is the **actual packet sync** in this chain: it is the
consumer that establishes the working VITA 49 wire contract in practice. Its
parser behaviour is therefore the reference for what the sender must emit, and
it is the first consumer that must be converted to multicast.

Current state:

- Capture sends a unicast copy to `waterfall-service.default.svc.cluster.local:4820`
  and a multicast copy to `239.255.49.2:4820` on the node LAN interface.
- The waterfall consumer binds `0.0.0.0:4820` (unicast) and does not join the
  multicast group.

Work:

- Convert the waterfall/Orbweaver consumer to join `239.255.49.2:4820` on an
  explicit interface, so one sender serves all consumers without per-consumer
  unicast fan-out.
- Keep the unicast destination until the multicast path is proven, then retire
  it deliberately rather than by accident.
- Preserve the existing wire contract: sc16 payload, one packet per datagram,
  total-word size field, type-4 context before data and periodically.

## Decision 5 — Future networking work (explicitly deferred)

Recorded so it is not mistaken for current behaviour:

- Move off `hostNetwork: true` to normal Kubernetes networking.
- Adopt Cilium, including its multicast support, once version/kernel/architecture
  behaviour is verified on this cluster.
- Add a **UDP gateway service** so LAN multicast consumers do not depend on a
  host-network pod, and so control and data can be separated cleanly.

These are future work. The current deployment remains host-network with
`Recreate` and a single B210 owner; no CNI migration is authorized by this
document.

## Sequencing

1. HTTP C2 on Capture, sharing the existing owner-thread handler (highest value:
   it is the failsafe path for a flaky VITA 49.2 control channel).
2. HTTP C2 on Channelizer, including restart-free channel retune.
3. NBFM demod plus the 12.5 kHz channel it needs.
4. Waterfall/Orbweaver multicast conversion.
5. Networking migration: Cilium, UDP gateway, host-network retirement.

## Non-goals for now

- No second UHD owner, no rolling update that could surge two collectors.
- No unauthenticated mutation endpoint.
- No claim that any of the above is implemented, deployed or verified.
