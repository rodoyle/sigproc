---
date: 2026-11-24
depends:
  - capture-service-c2 (item #1)
status: draft
---

# Channelizer ↔ Capture Service Contract

This document captures the runtime contract between the `capture` service (sole
owner of the RF front end via UHD) and the `channelizer` service(s). It covers
both the data-plane (VITA49.2 sample streams) and the control-plane (HTTP C2).
It explicitly addresses the multi-stream case where one capture feeds multiple
channelizers, each producing one narrowband output stream.

#TODO - Explicitly state if RF Agile Frequency Hopping signals are in or out of scope. Do we assume channels are static?

## Ownership model

- **capture** is the sole owner of the RF front end. It owns UHD, sets
  `center_freq`, `sample_rate`, `bandwidth`, `rx_gain`, `antenna`. 
It issues all control Vita49.2 packets if any RF retune/bandwidth change is required. <-- WRONG. Capture receives VITA49.2 control messages and acts on them. Think of capture as receiving a multi-producer channel of C2 signals and then executing them as the lock holder over UHD/SDR card. We need to build a vita49.2 C2 command issuer to test. Or reuse https://github.com/Geontech/vrtgen.git

- **channelizer** owns **only** the baseband selection within the already-captured
  wideband stream. It never touches UHD and never issues RF control packets.
- Multiple channelizer instances may run in parallel, each bound to a different
  node/port and each selecting a different channel from the same wideband input. <- Fine. This is the microservices approach 1 Tap to 1 Instance. The necessary config and k8s machinery to replicate multiple channelizer instances is then required.

## Data plane

### Input
- capture emits wideband VITA49.2 UDP multicast: sc16 I/Q at `capture.sample_rate`
  (currently ~2 MSPS, hardware supports up to ~50 MSPS).
- Each channelizer subscribes to the same multicast group/port. Subscription is
  one-to-many; N channelizers can consume the same wideband stream simultaneously
  without impacting capture throughput (UDP multicast fanout is handled by the
  network stack).

### Subscription Model and TAPS

A Channel is defined as a Tap or Subscription over some subset of the wide band capture.
The purpose of the channelizer is to execute these taps using various DSP algorithms.
Channels can change in frequency: duplex audio, bluetooth, 802.11, EPRS, and certain digital modes.
What makes something a single channel is that all "slices" of the 
wide band capture are shifted to the same output frequency (usually DC). 
The output bandwidth should be continuously occupied after channelization.

The most basic type of channelizer tap is a single offset relative to the wide band capture's center frequency.
Another common example would be two or more sub-carriers at a fixed spacing like 600 Hz.

The job of the Channelizer is to work out what complex number to multiply each input sample by so that a single output with a well defined center frequency and bandwidth is produced.


### Output
- Each channelizer emits **one** narrowband VITA49.2 UDP stream per instance.
- Stream identity (`stream_id`) DEFAULTS TO the channel's absolute centre frequency in kHz
  (`round((center_freq + offset_hz) / 1000)`). RF agile signals do not have a single frequency.
For agile signals, the channelizer may choose a representative offset based on the mode, mean, or minimum center frequency of the channel plus jitter in the case two
signals overlap. 

This *should* guarantees uniqueness across
  multiple channelizers and distinguishes narrowband outputs from the wideband
  input. If it does not stream_id should be assigned based on the pod ordinal in the deployment set.
  
- Output sample rate = configured `output_rate_hz` (decimated). Default 50 kS/s.
- Timestamps are contiguous within a channelizer instance; packet framing is
  preserved across mid-stream reconfiguration (settling samples are discarded but
  timestamps continue monotonically).

### Multiple channel streams
When running N channelizers to produce N narrowband channels:
- Each channelizer has its own `listen` port, `c2_listen` port, `metrics_listen`
  port, and downstream `vita49_dest_host:port`.
- All channelizers consume the same wideband multicast — no extra capture load.
- Downstream sinks subscribe to distinct destination ports (one per channelizer).
- There is no coordination between channelizers; each is independent. FOR NOW.

## Control plane

### Channelizer HTTP C2 (this service)
- Endpoints (optional, disabled when `c2_listen` is empty):
  - `GET /state` → `ChannelizerState` JSON: current `offset_hz`, `output_rate_hz`,
    `passband_hz`, `input_rate_hz`, decimation, settling count, live packet/power
    metrics.
  #TODO - this config will be much more elaborate with more complex channelization schemes
  I recommend a versioned ChannelConfig object. 
  - `POST /config` → body `{"input_stream_id": str, "offset_hz": i32?, "output_rate_hz": u64?}` →
    `ReconfigResponse` JSON `{ok, offset_hz, output_rate_hz, settling_samples, error?}`.
    Invalid/out-of-range values return HTTP 400 and leave config unchanged.
  COMMENT - channel configs must reference the parent wide band capture stream as they are defined relative to this.
  Elaborate on this - invalid means the channel is not fully contained in the input spectrum?
- Restart-free: reconfiguration applies inline in the receive loop; no process
  restart. Settling samples are discarded after a rate change; framing and
  timestamps remain continuous.
- **Scope**: offset_hz and output_rate_hz only. These select a channel within the
  already-captured wideband window. They do NOT change RF center frequency or
  capture bandwidth. Comment: this is reasonable IF the 400 error is clear about what changes the capture device needs to enact.

### Capture HTTP C2 (item #1, sole owner)
- Capture owns RF control: `center_freq`, `bandwidth`, `sample_rate`, `rx_gain`.
- If a requested channel falls outside the current capture window (e.g. channelizer
  offset_hz would place the channel beyond center_freq ± capture.bandwidth/2),
  the channelizer cannot serve it alone — the capture service must be instructed to widen bandwidth
  or retune center_freq first.
- **Coordination rule**: channelizer C2 changes are local baseband selection only.
  Any request that would require RF retune must be escalated to capture via the
  capture C2 interface (out of scope for channelizer). The channelizer should log
  such requests and return HTTP 400 with an error indicating the channel is outside
  the capture window.

## ConfigMap startup defaults

- Both services read startup configuration from a ConfigMap-mounted TOML file.
- Channelizer `[channel]` section provides startup `offset_hz` and
  `output_rate_hz`. These apply at boot when no HTTP mutation has occurred.
- HTTP C2 mutations are runtime-only and do not modify the ConfigMap; on restart
  the service reverts to ConfigMap defaults.
- `c2_listen` defaults to empty (disabled). Enable per-instance via ConfigMap when
  runtime control is required.

## Performance target

- Target throughput: 50 MSPS wideband input (hardware max), not the current 2 MSPS.
- Channelizer critical path (capture → channelizer → demod/waterfall/sink) must
  handle 16-bit I/Q at 50 MSPS over gigabit+ ethernet.
- HTTP C2 must not block sample processing: bounded request channel (depth 8),
  `try_recv` drain after each UDP receive (which has a read timeout), worker pool
  of 4 for response writing. Saturation drops excess connections rather than
  unbounded queueing.

## Open questions

- Should channelizer validate offset_hz against capture bandwidth and return a
  specific error code, or leave validation entirely to the operator? YES
- Is stream_id derived from centre frequency sufficient for downstream routing, or
  do we need an explicit channel label/metadata field? Unlikely long term

Comment - current production systems usually need a channel "phonebook" service that maps channel metadata to a single integer stream_id
