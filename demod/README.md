# Standalone NBFM/AM service

`sigproc-demod` consumes a **dedicated unicast narrowband UDP feed**; it never
opens UHD, changes RF tuning, or alters CubicSDR, waterfall, or Orbweaver.
NBFM is the default for the selected 12.5 kHz-class channel carried at 50 kS/s.
AM/DSB is available explicitly with `--mode am`; suppressed-carrier USB/SSB is
not silently mapped to either mode and remains a separate future implementation.

Example (paths are examples, no deployment is performed):

```sh
cargo run -p sigproc-demod -- --mode nbfm --fm-deviation-hz 2500 \
  --listen 0.0.0.0:4930 --metrics-listen 0.0.0.0:9093 \
  --output-dir recordings --segment-samples 3000000 --max-recordings 24
```

`DEMOD_MODE`, `DEMOD_FM_DEVIATION_HZ`, `DEMOD_LISTEN`, `DEMOD_METRICS_LISTEN`,
and `DEMOD_OUTPUT_DIR` provide equivalent environment configuration. Mount a PVC
or emptyDir at the configured output path. Run only one writer per dedicated
directory; the directory must be trusted and writable. CLI bounds reject zero
retention, zero segment length, invalid FM deviation, and segment lengths
exceeding RIFF limits.

## Wire contract

Supported context is **only the repository's fixed 60-byte IF context dialect**
from `sigproc_common::vita49::build_context_packet`: stream ID, free-running
seconds plus sample-count fraction, CIF0 bandwidth/RF/gain/sample-rate/SC16
payload-format fields, no class ID/trailer/extensions. The change indicator may
be set or clear. This is not a general VITA49/VITA49.2 context implementation.
Rate must be exactly 50,000 Hz, bandwidth in (0, 50,000] Hz, RF frequency
nonnegative, and payload format exactly complex Cartesian signed SC16.

Data uses the shared common parser with additional native header/timestamp and
stream-ID checks. Data before valid context is rejected. A malformed/unsupported
context revokes acceptance until another supported context arrives; a changed
valid context starts a new stream epoch. Periodic identical context does not
interrupt audio. Each rejected datagram closes an active segment and resets DSP
history, without crashing ingestion. This conservative policy avoids splicing
across ambiguous input. The feed is not authenticated or source-address pinned;
use a dedicated network endpoint, not a multiplexed multi-producer port.

The shared `GapTracker` tracks **sample-count timestamps**, not modulo-16 packet
counts. Forward gaps count exact missing samples; backward/duplicate/reordered
timestamps count resets, not missing samples. All interrupt DSP history and
start a new segment. No missing-sample silence is synthesized. Backward packets
are recorded as a new discontinuous segment rather than silently discarded.

## Recording lifecycle and evidence

WAV is 50,000 Hz mono PCM16LE, fixed gain, rounded/clamped from DSP float output.
Sidecars count clipping. Segment length is sample-bounded (default 60 seconds of
received audio); size rotation alone preserves DSP history. With no packets an
open segment remains open but cannot grow. SIGINT/SIGTERM request shutdown,
polled every packet and at most about 100 ms into an idle receive. Normal shutdown
and receive errors finalize the current segment; disk errors fail the service
closed instead of continuing to claim successful recording. Drop is a best-effort
fallback, not a substitute for explicit finalization.

Each WAV has a matching JSON sidecar with demodulation mode, stream ID, RF frequency,
rate/encoding,
first input counter, first output index, sample count, clipping count, start
boundary (if any), and end reason. Output indices count only received/written
audio samples over this process; input counters preserve source time. Boundary
indices identify the resumed input counter, exact missing samples when known,
process output index, and recording-local index (zero because boundaries rotate).
Context/rejection boundaries have unknown input/missing counts (`null`); the
record's first input counter supplies the next accepted sample. A previous
segment's end reason remains evidence even if no subsequent samples arrive.

Unique timestamp/PID/serial names and exclusive `.part` creation avoid restart
truncation. Both files are synced, WAV sizes patched, then WAV renamed before
JSON. **A completed pair is a WAV plus its final JSON completion marker**. Never
consume `.part` files or a WAV lacking final JSON as complete. IO failure poisons
the writer so Drop cannot publish a partially written segment. Two renames are
not an atomic pair or a power-loss durability guarantee (directory fsync is not
implemented). A crash, SIGKILL, or disk failure may leave parts/orphan WAVs;
they are deliberately preserved for operator recovery, not silently repaired.

Retention runs on startup and completion and removes only regular completed
pairs with the service naming pattern and matching owner/filename metadata.
Unrelated files, symlinks, incomplete files and unrecognized sidecars are not
removed. Default steady-state retained audio is about 144 MB plus one active
6 MB segment, metadata, and a transient pair during completion/pruning. This is
not a filesystem quota: preserved crash remnants and foreign files require
operator cleanup. Failed cleanup is fatal, so a live process cannot keep growing
recordings while retention is broken.

## `/metrics` and `/healthz`

The service implements the common `MetricsRenderer` and uses its bounded HTTP
worker pool. `/healthz` means HTTP process liveness, **not RF lock or recording
readiness**. Gauge/counter names are prefixed `demod_`:

- `rssi_dbfs`: latest contiguous 4096-sample IQ mean power, 0 dBFS = unit complex
  mean power. Not calibrated dBm and not audio RMS. Silence is `-inf`.
- `frequency_offset_estimate_hz`: strongest IQ spectral peak, **including DC and
  the AM carrier**, using the common Hann-windowed, zero-padded/interpolated FFT
  estimator. Not a modulation-tone estimate or a tuning feedback controller.
  Native 4096-sample window spacing is ~12.2 Hz (zero-padded bins ~6.1 Hz).
- These two gauges start at NaN and clear on discontinuities/rejects/context
  changes. They update once per non-overlapping block (~82 ms of received IQ),
  not per datagram. They describe the latest window, not an all-time average;
  absent input they retain their last value. Monitor receive-counter deltas to
  detect idle input. All-zero blocks have no frequency estimate (NaN).
- `snr_db` is always NaN and `snr_available` always 0: there is **no validated SNR
  estimator**. No inferred or fabricated measured SNR is reported.
- `vita49_packets_received_total`: all datagrams, including context and rejects.
- `vita49_packets_dropped_total`: local rejects only, **not inferred network
  packets lost**. Sample loss cannot honestly be converted to packet count with
  variable packet sizes; kernel socket-drop counters are not collected here.
- `vita49_seq_gaps_total` / `vita49_gap_samples_total`: forward timestamp events /
  exact missing samples, including losses hidden by 4-bit sequence wrap.
- `vita49_resets_total`: backward timestamps; context epochs reset the tracker
  without fabricating a gap. `context_valid` describes metadata acceptance only.
- `samples_out_total`: audio samples successfully written per complete processing
  call. An IO failure mid-call may leave partial artifacts, not claimed samples.
  `recording_errors_total`: fatal recording/retention errors while serving.


## Kubernetes and deterministic cluster gate

Build and push the UHD-free images from the pushed source with
`deploy/kaniko-channelizer-job.yaml`, then apply `demod-configmap.yaml`,
`demod-pvc.yaml`, `demod-service.yaml`, and `demod-deployment.yaml`. The channelizer
ConfigMap routes its selected 50 kS/s narrowband stream to `vita49-demod:4930`.
The deployment uses a PVC; `demod-emptydir-patch.yaml` replaces it with emptyDir
for non-persistent operation.

`bash test/demod-cluster-gate.sh` pauses only the production channelizer (not the
B210 capture), replays a two-second deterministic NBFM fixture through the real
channelizer image and UDP Service, checks metrics, gracefully restarts demod to
finalize its active segment, copies completed recordings from the PVC, and
independently measures the recovered 1 kHz tone. A trap restores the original
channelizer replica count.

## Verification
`cargo test -p sigproc-demod -- --test-threads=2` runs NBFM tone recovery from a
deterministic constant-envelope VITA49 fixture, default-service WAV/sidecar tone
verification, preserved AM tone recovery within ±5 Hz, and service tests for
context invalidation, malformed native headers, timestamp gaps and backwards
data, 16-packet loss hidden by sequence wrap, exact WAV header and PCM content,
rotation without resetting DSP, selective retention after restart, poisoned
writes, open/finalization errors, Drop fallback, real loopback UDP shutdown and
HTTP metrics, RSSI/frequency estimates, and unavailable SNR. Test artifacts are
created only under this repository's `target/demod-tests`. No live RF or
power-failure validation is claimed.
