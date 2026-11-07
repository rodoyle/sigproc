# Host-network collector and local Windows multicast proof

Observed 2026-11-07 after the operator approved live redeployment and Windows
packet reception. CubicSDR setup, GUI acceptance, extra control commands,
Cilium/CNI migration and gateway implementation remain outside this work.

## Source and native build

- Runtime source commit: `136eaadf6d74958c2462397dc89530602fd27a7b`, pushed to
  `origin/master` without including unrelated DSP working-tree edits.
- ARM64 build Job: `build/kaniko-build-sigproc-host-vrt-v1`. `GIT_REF` explicitly
  overridden to the commit above; git-clone printed that exact HEAD.
- Job completed at `2026-11-07T07:42:32Z`, `Complete=True`, succeeded=1.
- `capture/Dockerfile` built `cargo build --release --package sigproc --features
  uhd-support`; Kaniko copied that release binary and pushed the final image.
- Build tag: `ghcr.io/rodoyle/sigproc:host-vrt-20261107-v1` (not `latest`).
- Final pushed **and running** image:
  `ghcr.io/rodoyle/sigproc@sha256:5a023a1b9d62578d42e12936a4d5e152ebc4af8beeadf538c3c714f552bbdb7a`.
  Cache-layer digests are not used as target-image evidence.

## Applied deployment and sole ownership

Applied only `deploy/configmap.yaml` and `deploy/sigproc-deployment.yaml` to the
lab. `kubectl rollout status deployment/sigproc --timeout=140s` exited 0.

| Observation | Value |
|---|---|
| Deployment | default/sigproc, replicas=1, strategy=Recreate |
| Network | hostNetwork=true, dnsPolicy=ClusterFirstWithHostNet |
| Running pod | sigproc-777f55fb5-8rmdc, 1/1 Ready, zero restarts at inspection |
| Node / pod IP | rpi-four-2 / 192.168.0.55 |
| Node kernel / architecture | 6.18.34+rpt-rpi-v8 / arm64 |
| UHD runtime | 4.8.0.0+ds1-2, B210 detected over USB3 |
| USB owner inspection | host PID1321941 named sigproc, /dev/bus/usb/002/002 |

Ownership inspection enumerated host `/proc/*/fd` symlinks from the existing
hostPID capture pod, without opening/probing the radio. It found one process
holding a USB device handle. No second UHD receiver/probe process was launched.
Recreate prevents Deployment surge while replacing the collector.

Initial UHD readback and Windows context agree: frequency
914999999.9999988 Hz (requested 915 MHz), rate 2 MS/s, bandwidth 2 MHz, gain
30 dB, RX2. No live tuning command was needed; the four-setting control gate is
tested with a backend seam independently of the live radio. Lab control remains
disabled, so multicast observers cannot tune hardware by joining the group.

The original waterfall unicast remains
`waterfall-service.default.svc.cluster.local:4820`, resolved in the new host-net
pod to `10.43.78.205:4820`. The same owner also sends a multicast copy to
`239.255.49.2:4820`, interface `192.168.0.55`, TTL1, stream ID1. Startup logs
explicitly show both forwarders and the chosen LAN interface. No CNI settings
were installed or changed; no gateway or cluster-side listener was needed.

Waterfall peer independently confirmed ongoing ingest/row publication with no
new detected gaps or bridge loss during a four-second check. Its deployed
September image still increments legacy parser warnings (words-plus-one sizing
and context counted as wrong_type); its existing source fix is commit `be00aef`
but was not redeployed in this work. The peer independently sampled the stream:
data length1460 / size365 and context length60 / size15, both stream1. That
confirms standard sender sizing; do not alter it to satisfy the legacy consumer.
A later waterfall rollout is separate work, not a reason to weaken this profile.

## Local Windows reception and egress proof

The independent standard-library Python listener runs **locally on Windows**,
not in a cluster pod. Its interface is `192.168.0.15`; the source packet address
is `192.168.0.55:40522`. Receiving those remote-node packets on this PC, combined
with the explicit sender interface selection, establishes LAN egress rather
than merely local multicast-loop delivery.

Two separate local listener invocations each exited 0, decoded CI16 samples
and a context packet with stream ID1, and reported zero invalid datagrams.
Their observed data counts were 282 and 32 respectively (observations, not
future fixed test expectations).

For a stronger simultaneous fanout proof, both local sockets joined before
receiving and compared entire packet bytes, not only stream IDs or metadata:

```powershell
python test/vrt_fanout_probe.py --group 239.255.49.2 --port 4820 --interface 192.168.0.15 --source 192.168.0.55 --seconds 15
```

Observed exit0, runtime `win32`, subscribers2, received counts `[240,231]`,
invalid `[0,0]`, success=true. Both sockets received the same data packet and
same context packet byte-for-byte. Recorded packet examples:

| Field | Data | Context |
|---|---|---|
| Stream ID | 1 | 1 |
| Sequence | 14 | 15 (independent context sequence) |
| Integer seconds | 101 | 101 |
| Fractional samples | 1644823 | 1694695 |
| Payload | 360 CI16 samples, first IQ [5,-11] | ~915MHz / 2MS/s / 2MHz / 30dB |
| SHA256 | f602af05a0edde554f3ed99e02230ac660a89ad41048fb30724b593704ae8fef | be12a2d500d6276d981b899d50e1a17bbf4052c64047a458741c1920f9ccec25 |

Hashes/timestamps are historical observations, not expected constants on a
later run. The probe requires matching samples and context with one stream ID
from the specified source and exits nonzero on failure; group membership alone
does not pass it. It closes both sockets on success, timeout or exception.

A fresh-context read-only reviewer independently reran both regression commands,
the live Windows fanout command, build-source receipt and running-image checks.
No blocker was found. Its live observation received `[47,37]` packets with
invalid `[0,0]`, matching data and context for stream1; it confirmed one Ready
pod with zero restarts and the same pinned image. Reviewer run:
`d68822d0-6add-4bc1-afd5-1f8f578c503b`.

Single-subscriber reproduction:

```powershell
python test/vrt_listener.py --group 239.255.49.2 --port 4820 --interface 192.168.0.15 --seconds 15 --packets 32
```

## Hardware-free regression gates

- `cargo test -p sigproc -p sigproc-common --quiet`: exit0; 7 capture tests and
  98 common tests passed, 1 pre-existing common test ignored.
- `python -m unittest discover -s test -p test_vrt_listener.py -q`: exit0,
  3 independent decoder tests passed. Valid IQ payload changes remain accepted;
  wrong total length, timestamp flags, class/trailer flags and format fail.
- Independent literal data/context and external command-layout fixtures live in
  `capture/src/profile_tests.rs`. No repository encoder creates fixture words.
- Capture UDP tests cover frequency/rate/bandwidth/gain requests, ACK fields,
  applied backend readback (including coerced values), unsupported command
  rejection without backend mutation, malformed/unauthorized traffic and
  hardware failure without an applied ACK.
- Host-only one-sender/relay/two-subscriber regression proof remains available
  without lab/radio dependencies. It is separate from the live multicast proof.

## Limitations and rollback

This is not CubicSDR/SoapySDR integration, RF-quality/voice acceptance, a sustained
loss/throughput benchmark, or a live controller acceptance test. SoapyVITA49's
query-state probe remains unsupported. No claim of cryptographic authorization
or replay protection is made. Cilium/VXLAN and dual-homed gateway remain design
notes in the profile; the node kernel observation alone does not approve a CNI
migration.

The saved pre-rollout image was
`ghcr.io/rodoyle/sigproc@sha256:39015a363ddbe80a53a46809f5e4b8c11ad0c91a43cd8abf1bfd0469679a0ddb`.
If rollback is needed, use that digest, restore the unicast-only ConfigMap, and
retain one replica/Recreate to prevent two B210 owners. The RF settings above
were unchanged by this rollout. No rollback was needed for the observed proof.
