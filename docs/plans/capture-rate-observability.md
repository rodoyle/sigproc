# Capture rate and observability

The collector acquires at `[rf].sample_rate`, not at the B210's maximum. It
sends a primary unicast and, when configured, a second multicast copy. Both
copies use the same hardware rate: CI16 is 4 bytes per complex sample, so at
2 MS/s each copy has roughly 64 Mbit/s of IQ payload **before** VRT, UDP, IP,
Ethernet and context overhead. Multicast may also be replicated by switches or
access points. Local socket success does not establish delivery to receivers.
Choose a low starting rate appropriate for the network and increase gradually.

The running collector exposes immediate VITA 49.2 rate changes on the sole UHD
owner. The deployment opts in to loopback-only control; defaults in the parser
remain disabled. The allowlist is not authentication or replay protection: any
process able to bind `127.0.0.1:50000` in the node network namespace can control
the radio. Do not expose it to the LAN. Restrict who can exec into host-network
pods or run processes on the node. A rate change stops and restarts the stream,
which creates a discontinuity; both destinations get new readback context before
new IQ. Hardware may round the requested rate. A failed/missing ACK is ambiguous:
check the current metric/context before retrying. The setting is volatile and
returns to `[rf].sample_rate` at restart; change the ConfigMap for a persistent
rate and restart the collector.

From a trusted operator with Kubernetes exec access:

```sh
kubectl -n default exec deploy/sigproc -- python3 /usr/local/bin/vrt-set-rate 250000
# The ACK prints the actual hardware readback, not merely the request.
kubectl -n default exec deploy/sigproc -- python3 /usr/local/bin/vrt-set-rate 2000000
```

The client binds its authorized source port and validates the message ID,
stream ID, execution status and readback. Override `--stream-id`, `--port`,
`--source-port`, `--bind` only when the matching control configuration changes.
The service is single-owner; concurrent controllers contend for the source port.
Do not assume all consumers dynamically follow context: the current Rust
channelizer ignores context and uses its configured input rate, so reconfigure
or stop such consumers before changing rate. A change affects RF bandwidth and
timestamps as well as throughput. The radio bandwidth setting is independent.

## Scraping and capacity testing

Capture serves `/metrics` and `/healthz` on `[input].metrics_listen`; the lab
manifest uses TCP 9087 and the internal `sigproc-metrics` Service. The metric
`sigproc_capture_sample_rate_hz` is actual UHD readback;
`samples_received_total`, `receive_errors_total` and `empty_reads_total` measure
capture; `primary_*` and `multicast_*` report local UDP packets, UDP payload
bytes and drops separately, including context packets where attempted. These
are counters, so graph `rate(sigproc_capture_multicast_bytes_sent_total[5m])*8`
for UDP payload bit/s, and `rate(sigproc_capture_multicast_packets_dropped_total[5m])`
for local send failures. They exclude network headers and do not see loss after
send; compare receiver sequence/timestamp gaps and switch/port drops as well.
UHD error metadata and errors returned by receive calls share one counter;
empty reads include timeouts and are not by themselves packet loss.

Apply `deploy/sigproc-metrics-service.yaml`. The existing monitoring Alloy
configuration is owned outside this repository; `python3 deploy/configure-alloy.py`
adds this target to its existing services scrape without replacing unrelated
jobs, using a resource-version precondition. Restart Alloy to load the ConfigMap
(`kubectl -n monitoring rollout restart statefulset/alloy`) and verify
`up{job="sigproc-capture"}` and the above series in VictoriaMetrics/Grafana.
Monitoring redeployment can replace the ConfigMap: add the same target to the
canonical monitoring configuration or rerun the idempotent script after such a
redeployment. No automatic discovery or dashboard is implied.

Rollback: restore the prior collector image and ConfigMap; delete the metrics
Service if no longer needed. Remove only the sigproc target from the Alloy
services list and restart Alloy. This leaves the existing monitoring jobs intact.
