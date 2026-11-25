---
date: 2026-11-25
status: proposed — blocked on one network-side entry criterion
depends_on: docs/plans/service-c2-and-networking.md (the original "prove then retire" direction)
related_repo: github.com/rodoyle/waterfall (consumer + deploy manifests)
owners: sigproc (sender, this document), waterfall (consumer), infra (node/L2, rollout)
---

# Retiring the unicast VITA49 path

The capture service sends every VITA49 stream twice: once as unicast to the
waterfall Service, and once as an additive LAN multicast copy. That was
deliberate — multicast was unproven, so unicast stayed authoritative. This
document is the procedure for turning the second path off, and the evidence that
has to exist first.

Retirement is **not** the same thing as the `waterfall-service` →
`orbweaver-service` Service rename documented in `waterfall/deploy/README.md`.
That rename moves the unicast destination; it keeps unicast. Retirement removes
it. Do not treat one as evidence for the other.

## Entry criteria: do not start until every line passes

All of these are measurable. None of them is a judgement call.

1. **The multicast join succeeded in the running consumer.** `/stats` reports
   `multicast_joined: true`, `multicast_group: "239.255.49.2"`, and
   `multicast_iface` set to the node's LAN address, with
   `multicast_join_error: null`. A failed join silently degrades to unicast, so
   this is checked, not assumed.

2. **Every duplicate the consumer drops was byte-compared, and none differed.**
   This is the core criterion, and it is the one a counter alone cannot prove:

   - `duplicates_compared` must equal
     `(duplicates_dropped + context_duplicates_dropped) - duplicates_fallback`,
     up to a handful of units for a non-atomic counter snapshot: data *and* IF
     Context duplicates are both byte-compared, and only fallbacks escape,
   - `duplicates_fallback` must be a negligible fraction of
     `duplicates_dropped` (measured: 63 of 9,604,171, i.e. 0.0007 %) — a
     fallback is a duplicate whose original had already aged out of the retained
     window, so its bytes were **not** checked,
   - `duplicate_mismatch` must be `0`.

   `duplicate_mismatch: 0` on its own proves nothing; it is also what a filter
   that never compares bytes reports. The comparison counter is what makes the
   zero meaningful.

3. **VITA49 context packets are received on the multicast path and are byte
   identical.** `context_packets` advances, `context_duplicates_dropped` is a
   substantial fraction of it (the receiver sees both copies), and
   `context_duplicate_mismatch` is `0`. A stream whose data is identical but
   whose receiver metadata is not is worse than a dead path: the consumer would
   plot correctly-labelled wrong data.

4. **Multicast costs exactly one copy per datagram.** With the unicast path
   still running, `sources` in `/stats` must show the two sender sockets —
   the multicast socket at roughly 1× and the unicast socket at 1× — and
   `packets_received / packets_analyzed` must be ~2.0, not ~3.0. A ratio of 3.0
   means the multicast copy is arriving twice, so "multicast works" is not yet
   an established fact and retirement must wait. **This criterion currently
   fails; see "Open blocker" below.**

5. **The unicast path has produced no unique packets.** Nothing analysed may
   depend on unicast. `packets_analyzed` must equal the count of distinct
   packets on the multicast socket, and `gaps`/`missing_samples` must not
   improve when unicast is restored.

## Open blocker (do not retire until resolved)

Measured 2026-11-25 at 886 s consumer uptime:

```text
packets_received 14,767,065   packets_analyzed 5,105,444   (2.89 copies/packet)
sources: 192.168.0.55:37108 = 9,643,689   (multicast)
         192.168.0.55:4508  = 5,123,376   (unicast)
```

The multicast socket delivers each datagram more than once (1.9–2.0× unicast,
both copies from the *same* source port, trailing by ~0.35–0.7 ms). The capture
send path is proven to emit exactly one datagram per packet —
`Vita49Forwarder::send_one_with` performs a single `send_to`
(`common/src/vita49.rs`), `packets_sent` only increments on a full write, and
nothing joined `239.255.49.2` could re-emit a looped copy. So the second frame is
produced below the application.

**Confirmed 2026-11-25: the second frame exists only on the wired segment.** Two
receivers counted overlapping 8 s windows at the same moment:

| Receiver | Link | Copies per datagram |
| --- | --- | --- |
| `192.168.0.15` | wired | 1.9999 |
| `192.168.0.20` (`wlp3s0`) | Wi-Fi | 1.0 |

The wireless receiver got exactly one copy of everything it received, so the
application transmits once and something on the wireless side of the L2
re-injects the frame onto the wire. That also rules out IP-layer multicast
forwarding: the stream carries `ttl = 1`, and a routed copy would be decremented
to 0 and dropped rather than delivered. Look for a bridge, repeater or WDS path
between the AP and the wired segment — not `mrouted`, `smcroute` or `igmpproxy`.

A passive `AF_PACKET` capture on the wired receiver's NIC proves the copy is
verbatim rather than rebuilt: across 39,652 duplicated datagrams the two frames
shared the same source MAC (`dc:a6:32:bc:8a:3c`, the sender's eth0), the same IP
ID, the same `ttl = 1`, and the same source port. Over the same window the frame
destined to `192.168.0.48` measured **exactly 1.0** copies. So an L2 device
duplicates the frame in place — no host re-originated it, nothing rebuilt it, and
no routed hop occurred.

That clears the collector from a second and independent direction, and it retires
the RPi-as-bridge idea as well: the AP would only deliver the group to `wlan0` if
something there had joined it, nothing has, and with no bridge and `ttl = 1`
there is no path for it to re-emit. The copier holds both a wireless and a wired
foothold in this L2 — an AP in WDS/repeater mode, a mesh node with both backhauls
live, or an extender with an Ethernet port.

When diagnosing, pair copies by **(counter, destination)**, not by counter alone.
One datagram legitimately arrives once as a unicast frame DNATed to the node
address and once as a multicast frame to the group, so counter-only pairing
invents duplicates and makes the healthy unicast path look duplicated.

Re-confirm with two receivers at once, one on each link. A single receiver
cannot distinguish L2 re-injection from a sender that transmits twice:

```bash
python test/vrt_path_probe.py --group 239.255.49.2 --port 4820 \
    --interface <wired ip> --seconds 8
python test/vrt_path_probe.py --group 239.255.49.2 --port 4820 \
    --interface <wifi ip> --seconds 8
```

`test/mcast_emit_probe.py` (emits a known number of VRT-shaped datagrams from one
pinned egress interface) and `test/vrt_path_probe.py` (counts copies per
sequence number at a remote receiver and byte-compares them) are the instruments.
Run the receiver on a host with exactly **one** LAN interface: a host that is
itself multi-homed sees one copy per NIC and will fake a duplication that is not
on the wire.

## What the sender needs

`forwarding.vita49_dest_host` and `forwarding.vita49_dest_port` are now optional
in `common/src/config.rs`, with three states that are deliberately distinct:

| Config | Meaning |
| --- | --- |
| host + port set | unicast authoritative, multicast additive (today's deployment) |
| both omitted, `[forwarding.multicast]` present | **retired** — multicast only |
| host set, port omitted (or the reverse) | refused at startup; a typo must not look like a retirement |
| neither | refused at startup; a sender with no destination would drop every packet |

Capture treats an omitted destination as retired and logs
`unicast is RETIRED, multicast is the only output path`. The channelizer has no
multicast path, so it refuses an omitted destination instead
(`ForwardingConfig::require_unicast`). Demod carries no forwarding config at all —
it consumes VITA49 and forwards nothing — so retirement cannot reach it.

To retire, remove exactly two lines from the capture ConfigMap
(`deploy/configmap.yaml`, and the live `configmap/sigproc-config`) and leave the
multicast block alone:

```toml
[forwarding]
# vita49_dest_host = "waterfall-service.default.svc.cluster.local"   # retired
# vita49_dest_port = 4820                                            # retired

[forwarding.multicast]
group = "239.255.49.2"
port = 4820
interface = "192.168.0.55"
ttl = 1
```

### Prerequisite: the running image and the live ConfigMap must have the target shape

The optional-destination code is shipped in this repo from `bdaaa16` onward, but
the running pod is not automatically that build, and a ConfigMap this tree accepts
is not automatically one an older image accepts. Settle both *before* the two-line
edit, because `Recreate` means a config the image cannot parse takes the radio and
the service down together.

| Prerequisite | Why it bites | How to check |
| --- | --- | --- |
| Capture runs a build at or after `bdaaa16` | earlier builds declare `vita49_dest_host: String` and `vita49_dest_port: u16`, both required, so omitting them is a startup parse error rather than a retirement | `kubectl -n default get deploy sigproc -o jsonpath='{.spec.template.spec.containers[0].image}'`. Image tags are commit short-SHAs, so `git merge-base --is-ancestor bdaaa16 <tag>` decides; `:latest` must be resolved to a digest and mapped back to its build. |
| The live ConfigMap's Hz fields are integers | this tree parses `center_freq`/`sample_rate`/`bandwidth` as integers (`deploy/configmap.yaml` has `center_freq = 915_000_000`), so a float form such as `915_000_000.0` exits at startup on any post-`bdaaa16` image | `kubectl -n default get cm sigproc-config -o yaml` and confirm the three fields carry no decimal point. |
| The channelizer ConfigMap keeps a destination | it has no multicast path and refuses to start without one (`ForwardingConfig::require_unicast`), so a sweep that deletes `dest_host` from every ConfigMap stops it. Demod has no forwarding config, so only the channelizer is exposed. | `grep -n dest_host deploy/channelizer-configmap.yaml` |

The image upgrade and the float→integer ConfigMap migration are prerequisites, not
part of retirement. Do them together, **with unicast still configured and both
paths live**, and confirm `/metrics` shows primary and multicast packet counters
both advancing. A pod that fails to come up then is unambiguously a config-shape
problem, and retirement keeps its single-variable footprint.

## Applying it without interrupting the RF path

Capture holds the only handle on the B210, so its Deployment is `Recreate` with
`strategy.rollingUpdate` removed, pinned to its node. A ConfigMap change does
**not** restart the pod, and the sender reads its config at startup, so the
order matters:

Step 0 is not optional. Verify the prerequisites in the previous section first:
they are the difference between a two-line retirement and a capture service that
cannot parse its config while holding the only handle on the radio.

1. Edit the live ConfigMap.
2. Confirm the partial state is harmless: the running pod keeps using its
   in-memory config, so unicast and multicast both stay live until the restart.
3. Roll the pod: `kubectl -n default rollout restart deploy/sigproc` then
   `rollout status`. `Recreate` guarantees the old pod releases the radio before
   the new one starts; never replace this with a rolling update, and never allow
   two capture pods to run at once.
4. Watch the first packets: the log must show the multicast line and the
   `unicast is RETIRED` warning, and `/metrics` must show
   `sigproc_capture_multicast_packets_sent_total` advancing while
   `sigproc_capture_primary_packets_sent_total` stays flat.

## Consumer-side cleanup (waterfall, after the sender is retired)

Do these in the consumer repo, in this order, so the fallback path is removed
only once nothing uses it:

1. **`orbweaver` Service** — the unicast path pointed at
   `waterfall-service.default.svc.cluster.local`. Once nothing dials it, the
   ClusterIP and its compat label
   (`deploy/kustomize/compat/waterfall-service.yaml`) can go. Keep it until the
   sender is retired; it is the rollback target.
2. **`DuplicateFilter`** — the consumer drops the second copy of every datagram
   and, on a retired path, there is no second copy. Leave the filter and its
   counters in place: with one path `duplicates_dropped` simply stays flat, and
   the first time it moves you have learned that a second path reappeared.
   Removing it would delete the instrument that detected the current blocker.
   Revisit only once `packets_received / packets_analyzed` has been 1.0 for a
   full day.
3. **`deploy/README.md`** — the "Retirement checklist" section currently
   describes the Service rename. It has been rewritten to point here for path
   retirement and to keep the rename as a separate step.

## Post-retirement verification

Run for at least 10 minutes after the rollout, then compare:

```bash
# Consumer: exactly one source, no duplicates, nothing missing.
curl -s http://<node>:4830/stats

# Sender: multicast advancing, unicast flat at its final value.
curl -s http://<sender>:9087/metrics | grep -E 'packets_sent_total|multicast_enabled'
```

Pass conditions:

- `sources` lists **one** live address — the multicast socket, identified by the
  port that also appears at a third host rather than by any fixed number.
- `packets_received / packets_analyzed` ≈ `1.000`; `duplicates_dropped` flat.
- `duplicate_mismatch`, `context_duplicate_mismatch` still `0`.
- `sigproc_capture_multicast_packets_sent_total` advances and
  `sigproc_capture_primary_packets_sent_total` does not.
- `gaps` and `missing_samples` do not increase relative to the two-path
  baseline. If they rise, the multicast path cannot carry the stream alone and
  that is a stop condition, not a tuning problem.

Independent confirmation, from a host with one LAN interface, that the stream is
still on the wire and internally consistent:

```bash
python test/vrt_path_probe.py --group 239.255.49.2 --port 4820 \
    --interface <that host's LAN ip> --seconds 10 --strict
```
`copies_per_packet {"1": N}` and `byte_mismatches 0` is the pass. Read that
histogram rather than the exit status: `--strict` only fails above two copies, so
a doubled multicast path alone still exits 0.

## Rollback

Restore the two ConfigMap lines and roll the pod again. `Recreate` means the
radio is re-opened cleanly either way; there is no half-retired state to unwind,
because the unicast socket is created only when a destination is configured.

The consumer needs no rollback: it never stopped accepting unicast, and the
duplicate filter simply starts seeing two copies again. That is exactly the
state the deployment is in today, which is why retirement was reversible by
construction.

## Evidence log

**2026-11-25, consumer uptime 886 s, image `ghcr.io/rodoyle/waterfall`
sha256:0b3867bb (commit 3fc93e2):**

- `duplicates_dropped 9,604,171`, `duplicates_compared 9,641,619`,
  `duplicates_fallback 63`, `duplicate_mismatch 0`.
  The identity holds exactly: `compared = (dropped − fallback) + context_dup`
  = 9,604,108 + 37,511 = 9,641,619. So 9,604,108 data duplicates were genuinely
  byte-compared, and 63 were not.
- `context_packets 57,450`, `context_duplicates_dropped 37,511`,
  `context_duplicate_mismatch 0` — context reception is byte-identical too.
- `multicast_joined true`, group `239.255.49.2`, iface `192.168.0.48`.
- Two sources, one per path: multicast `192.168.0.55:37108`,
  unicast `192.168.0.55:4508`.
  Note the source ports are ephemeral per process: each collector restart takes
  new ones, and `sources` holds cumulative lifetime counters, so a restarted
  collector lists the old and new sockets side by side. The stable rule is "the
  port that also appears at a third host is the multicast socket" — never a fixed
  number — and more than two entries means restarts, not extra paths.

  To tell live sockets from frozen ones, sample `/stats` twice about 10 s apart
  and take the delta per source address: only the sockets that are still moving
  are carrying traffic. This distinguishes "the collector restarted" from "more
  paths exist", and it is cheap enough to run before any retirement decision. It
  is also the check that was got wrong once here — the four entries were read as
  four paths when two of them were frozen lifetime totals.

  That restart left the 2:1 split unchanged, which is evidence in its own right:
  the duplication depends on the path, not on the process or the socket that
  happens to be sending. A ratio measured after a collector restart is therefore
  still valid.

Criterion 2 and 3 pass on this evidence. Criterion 4 does not: multicast is
delivered ~1.9× per datagram, a below-application duplication that exists only
on the wired segment only. It is still open, so unicast stays.
