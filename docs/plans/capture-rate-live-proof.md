# Collector runtime rate and telemetry verification

Observed on 2026-11-09 against the lab B210 on `rpi-four-2`.

## Target and build

Application source: `72b2c663d051269cfabcd9755d5abcfc5a7939b7`.
Image loaded by the running pod (container imageID):
`ghcr.io/rodoyle/sigproc@sha256:e77726b8d8f91cb07a3697eecbc428b5150abb2c1ed437eb1eb7fa19b49e5b55`.
`deploy/sigproc-deployment.yaml` pins this digest. Deployment remained one replica,
`Recreate`, with host networking; no additional UHD owner was started.

The initial Kaniko build failed at the unbounded FPGA wget (exit 4/network).
Successful Job `build/kaniko-sigproc-rate-72b2c66-v2` checked out the exact source
above, then replaced only the Dockerfile image-download steps in its ephemeral
checkout: omitted `RUN uhd_images_downloader ...` and replaced the FPGA wget with
this instruction:

```dockerfile
COPY --from=ghcr.io/rodoyle/sigproc@sha256:5a023a1b9d62578d42e12936a4d5e152ebc4af8beeadf538c3c714f552bbdb7a /usr/share/uhd/images /usr/share/uhd/images
```

This reuses the exact image set from the previously running collector; it does
not use the unrelated untracked local `usrp_b210_fpga.bin`. That previous
collector's B210 FPGA SHA-256 was
`0ab54295961c881cfe37398ce96514063a1cc7fac8035b7ae5ff63844d03bdca`.
The ordinary repository Dockerfile was not changed to depend on a prior image.
All application source and the installed rate client came from the named commit.
Kaniko compiled the release `sigproc` package with `uhd-support` and pushed the
new digest. The live logs report UHD 4.8.0.0, USB 3, B210, sample rate 2 MS/s,
and the metrics server at `0.0.0.0:9087`.

## Behavioral observations

A Python listener in the running collector's network namespace joined
`239.255.49.2:4820` on `192.168.0.55`. A single loopback VRT controller command
requested 250,000 complex samples/s. Its matched applied ACK reported exactly
250,000, and an independently decoded multicast context packet announced that
rate. This is local multicast loopback proof, **not another LAN receiver**.

During the lower-rate interval:

- 1,283,128 multicast samples / 3,759 data packets were received in the first
  approximately five seconds (includes the command/observation boundary).
- The metric's actual rate was 250,000 samples/s.
- Over a separate four-second interval, captured sample delta was 999,424,
  multicast UDP payload byte delta was 4,056,976, and local multicast drop delta
  was zero.

The test restored 2,000,000 samples/s in a `finally` block. Applied ACK and the
current metric both reported 2,000,000. A subsequent four-second interval showed
8,005,632 captured samples and 32,497,128 multicast UDP payload bytes, approximately
eight times the lower-rate throughput. The image had zero container restarts.
This verifies actual acquisition/network-volume reduction, not just a changed
configuration value. RF frequency, gain, destination and persistent startup rate
were retained.

UHD overflow events occurred at the restored/original 2 MS/s rate (12 counted
at the observation point); the logs identify receive-buffer overflow. Local
multicast drop total remained zero. This is not a loss-free or network-capacity
certification; the new metrics expose a real collector-side pressure signal.

## Monitoring path

Applied the `sigproc-metrics` Service and ran `deploy/configure-alloy.py`, which
inserted one target without replacing the existing logs, traces or other scrape
jobs. Restarted `monitoring/statefulset/alloy`; it became ready. Queried the
existing VictoriaMetrics HTTP API from the collector:

- `up{job="sigproc-capture"}` returned 1 with
  `instance="sigproc-metrics.default.svc:9087"`, `cluster="homelab"`.
- `sigproc_capture_sample_rate_hz` returned 250000 while that value was scraped.
- `min_over_time(sigproc_capture_sample_rate_hz[5m])` returned 250000.
- `sigproc_capture_multicast_bytes_sent_total` returned 241554384 at one query.
- A subsequent bounded query observed `sigproc_capture_sample_rate_hz` at
  2000000 after restoration.

This proves the Alloy-to-VictoriaMetrics ingestion path for the real collector.
Grafana can use its existing VictoriaMetrics data source and these PromQL
expressions; no dashboard or GUI check is claimed. Monitoring provisioning owned
elsewhere can overwrite the target; the runbook names the canonical-config
integration or idempotent reapplication needed after such a redeployment.
