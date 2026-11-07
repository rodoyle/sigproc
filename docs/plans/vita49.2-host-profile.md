# VITA 49.2 capture host profile

The Rust/UHD capture service is the sole B210 owner. Downstream subscribers
never open UHD. This is a bounded VRT profile, not every VITA 49.2 feature.
Hardware-free protocol proof does not establish live multicast delivery or GUI compatibility.
The authorized host-network deployment and Windows group reception on the lab
LAN are recorded below. CubicSDR/GUI setup, CNI migration and gateway work
remain deferred.

## Data and context wire contract

All words/components are network byte order. Packet size in header bits 15:0
is the **total** number of 32-bit words, including header (not words minus one).
There is a stream ID, no class ID, no trailer. Stream ID defaults to 1 and is
configured as `[control].stream_id`, even when control is disabled. It stays
stable across retunes/restarts with unchanged configuration; it is not frequency.

| Word | IF data (type 1) / context (type 4) |
|---|---|
| 0 | Type in 31:28; TSI=3 (other/free-running), TSF=1 (sample count); sequence in 19:16; total words in 15:0 |
| 1 | Stream ID |
| 2 | Integer UHD seconds (not UTC) |
| 3–4 | Fractional sample count within that second, unsigned 64-bit |
| 5+ | CI16 payload, or CIF0 and context fields |

Timestamp refers to the first sample in the packet. Fraction is rounded to
nearest sample, carrying into integer seconds if needed. The downstream sample
counter is integer_seconds × announced_sample_rate + fractional_samples. Rate
changes and streamer restarts are discontinuities, not a promise of continuity.

Data is CI16/sc16: big-endian signed I then Q, one complex sample per word.
Default 360 complex samples/packet gives 1460 UDP payload bytes, within a 1500
IPv4 MTU. A partial final packet is allowed. Data sequence is modulo 16;
context sequence is a separate modulo-16 counter. Both start at zero. Sequence
is consumed on attempted sends, so transport loss does not hide a gap. A gap
multiple of 16 is not detectable by sequence alone; timestamps also matter.

Context is 15 words (60 bytes). CIF0 uses bandwidth bit29, RF reference frequency
bit27, gain bit23, sample rate bit21 and payload format bit15. Field order:
bandwidth (2 words), RF frequency (2), gain (1), sample rate (2), format (2).
Hz fields are signed Q20. Gain stage1 is the low signed Q7 halfword; stage2 is
zero. Payload format is `0x200003cf00000000`: complex cartesian signed fixed
point, 16-bit packing and item sizes, repeat/vector counts encoded as zero.
Context change bit31 is set initially and after changed settings; periodic
context clears it. Context precedes initial/changed IQ and repeats after 256
data packets. Initial and applied context values are UHD readback, not requests.

Independent literal vectors in `capture/src/profile_tests.rs` pin data/context
size, CI16, stream ID, sequence, timestamps, CIF0 and field encodings; no encoder
produces those fixture words. The decoder consumes the hand-authored data
fixture. The sender is compared against the independent context vector.

## Four-setting commands (the 49.2 addition)

Type 6 command header uses bit26 for ACK and bit24 for cancellation. The VRT
prologue contains the stream ID and optional class ID/timestamps as indicated by
the header. CAM and Message ID follow that prologue, then optional controllee
and controller identifiers, then CIF0 and the same field encodings as context.
The old private layout (CAM action in 31:30, no Message ID) was incompatible and
is no longer used. Layout cross-check: SoapyVITA49 `src/Control.hpp` and
`src/Control.cpp`, revision `6f209e47812424fb2038a83e30ba5613f68c2725`:
https://github.com/alphafox02/SoapyVITA49/tree/6f209e47812424fb2038a83e30ba5613f68c2725

CAM action is bits24:23: execute=2. Bit19 requests/reports execution ACK.
Bits31/29 indicate controllee/controller IDs, bits30/28 their UUID form.
This endpoint accepts optional 32-bit IDs, immediate execute, and only the four
fields: RF frequency, sample rate, bandwidth, gain. It checks a supplied
controllee ID against the stream ID. It does not implement query-state, dry-run,
scheduling, cancellation, UUID addressing, state/event, or other settings.
A well-framed unsupported command receives an execution ACK with CAM error
indicator bit16, no executed bit10, echoed Message ID/IDs, and unchanged confirmed
settings. Mixed supported/unsupported requests apply **nothing**. Nonpositive Hz
or unrepresentable Q7 gain is also rejected. Malformed/truncated datagrams are
logged/dropped. Unauthorized peers receive no response.

Applied ACK has header bit26, CAM bit19 and executed bit10, and includes all four
read-back fields. If actual values differ from requested values, partial-action
bit11 is set. ACKs use their own modulo-16 sequence, zero timestamp (not a
hardware execution-time measurement), and echo the request's Message ID and
identifier fields. Do not correlate by the 4-bit count. Hardware failure emits
no applied ACK and terminates capture because a multi-field setter can have
partially changed hardware; transactional rollback is not claimed.

### Separate control endpoint and ownership

Default control is disabled. Explicit opt-in example:

```toml
[control]
enabled = true
ctrl_addr = "127.0.0.1"
ctrl_port = 4991
authorized_controller = "127.0.0.1:50000"
stream_id = 1
```

This is one exact source-IP/port allowlist, **not cryptographic authentication or
replay protection**. Remote exposure requires trusted-network/firewall controls.
Data destination is separately configured under `[forwarding]`; multicast joins
are data subscriptions and do not authorize hardware control. The controller
must bind its configured source port. SoapyVITA49's automatic query probe is not
implemented: desktop control integration remains explicitly unproven/deferred.

The sole capture thread polls bounded UDP batches between UHD reads. The Rust
UHD API borrows the device while a streamer exists: stop/drop the streamer,
apply/read back on the same owner thread, then recreate it. This interrupts
capture even for an authorized request later rejected. No second UHD device
handle is opened for control. Default RX channel remains zero.
The Deployment source uses one replica and `Recreate` (no rolling-update surge)
so replacing this collector does not start two competing B210 owners. That
source change alone is not a claim that the lab Deployment has been updated.

## Reproducible host-only gates

```
cargo test -p sigproc
cargo test -p sigproc-common
```

These need no libuhd or radio. Capture tests cover independent data/context and
external command-layout words, four-setting UDP request/ACK/readback, unsupported
no mutation, unauthorized/malformed traffic, and hardware-failure/no-ACK.
One production forwarder sends context and samples into a host UDP fanout relay;
two concurrently bound subscribers receive byte-identical packets. That is a
local UDP harness, **not evidence of multicast delivery across the lab LAN**.
The separate ARM64/UHD release build and live LAN observations are recorded in
[vita49-host-live-proof.md](vita49-host-live-proof.md). Hardware-free tests remain
independent of those lab prerequisites.

### Windows multicast listener

`test/vrt_listener.py` is an independent Python standard-library decoder and a
bounded IPv4 group listener. It imports no Rust encoder/decoder code, checks
CI16/context framing, and succeeds only after context and sample packets with
the same stream ID arrive. Group membership alone is not success.

```powershell
# Substitute the actual multicast destination and this PC's LAN IPv4 address.
python test/vrt_listener.py --group 239.255.49.2 --port 4820 --interface 192.168.0.15 --seconds 15
python -m unittest discover -s test -p test_vrt_listener.py -q
```

The live Windows interface used was `192.168.0.15`. The collector on
`rpi-four-2` transmits from `192.168.0.55`; the Windows listener runs locally,
not in a cluster pod. Before rollout the old pod was `10.42.0.40` with
`hostNetwork` unset and `RollingUpdate`. After rollout the collector uses the
node LAN address, `hostNetwork: true`, and `Recreate`. See the recorded
[build and live proof](vita49-host-live-proof.md) for exact image/source and
packet receipts. No CNI or gateway change was used.

### Authorized temporary lab deployment

The collector source now specifies `hostNetwork: true`,
`dnsPolicy: ClusterFirstWithHostNet`, one replica and `Recreate`. RF remains
915 MHz / 2 MS/s / 2 MHz bandwidth / gain 30 / RX2. The existing unicast
waterfall destination is retained. An additional forwarder in the same process
sends the same stream onto `239.255.49.2:4820`, explicitly using local interface
address `192.168.0.55` on rpi-four-2, multicast TTL 1. No gateway, second UHD
owner, CNI configuration or receiver-side multicast duplication is introduced.
`[forwarding.multicast]` is optional for other deployments and validates group,
interface, port and TTL. Control remains disabled in the lab ConfigMap.

Release build: commit/push runtime source, then override the Kaniko Job's
`GIT_REF` with that exact commit using a local kubectl patch before applying.
The distinct `host-vrt-20261107-v1` build tag did not overwrite `latest`; the live
Deployment is pinned to the resulting digest. The build receipt shows the
exact checked-out runtime source, not merely the branch name.

Rollback baseline image:
`ghcr.io/rodoyle/sigproc@sha256:39015a363ddbe80a53a46809f5e4b8c11ad0c91a43cd8abf1bfd0469679a0ddb`.
If rollout fails, restore that image and original unicast-only configuration;
keep one replica and `Recreate` while rolling back. Build, deployment and local
Windows reception receipts are recorded in the linked live-proof document.

## Deferred path (design notes only)

- First desktop client: CubicSDR → SoapySDR → SoapyVITA49. SDR++ deferred.
  Desktop setup, GUI acceptance and control integration are later work.
- Do not migrate k3s CNI here. Cilium multicast remains beta; use VXLAN only
  after recording actual Cilium version, kernel versions and architecture.
  Required kernel baseline: ≥6.0 AArch64 or ≥5.10 AMD64. No such check is claimed
  to have been run by these hardware-free tests. The gate precedes migration.
- Later dual-homed gateway: static, allowlisted overlay group export onto a LAN
  multicast group, IQ and context only. Separate data-group join from
  `ctrl_addr`/`ctrl_port`, with one authorized hardware controller and observers.
  No gateway is implemented here. Verify IGMP snooping/querier behavior before
  considering listener-driven export. Host-network collector deployment, if
  separately authorized in the goal, does not imply Cilium or gateway changes.
