#!/usr/bin/env bash
# Run the deterministic NBFM cluster gate and recover/analyze its WAV.
set -euo pipefail

NS=default
JOB=sigproc-demod-gate
OUT=${1:-target/demod-cluster}
if [[ -z "${PYTHON:-}" ]]; then
  if command -v python3 >/dev/null 2>&1; then
    PYTHON=python3
  else
    PYTHON=python
  fi
fi
ORIGINAL_REPLICAS=$(kubectl -n "$NS" get deploy sigproc-channelizer -o jsonpath='{.spec.replicas}')
restore() {
  kubectl -n "$NS" scale deploy sigproc-channelizer --replicas="$ORIGINAL_REPLICAS" >/dev/null || true
}
trap restore EXIT

kubectl -n "$NS" apply \
  -f deploy/demod-configmap.yaml \
  -f deploy/demod-pvc.yaml \
  -f deploy/demod-service.yaml \
  -f deploy/demod-deployment.yaml \
  -f deploy/demod-gate-configmap.yaml >/dev/null
kubectl -n "$NS" rollout status deploy/sigproc-demod --timeout=180s >/dev/null

# Isolate the deterministic stream from live RF; the B210 capture stays running.
kubectl -n "$NS" scale deploy sigproc-channelizer --replicas=0 >/dev/null
if kubectl -n "$NS" get pods -l app=sigproc-channelizer -o name | grep -q .; then
  kubectl -n "$NS" wait --for=delete pod -l app=sigproc-channelizer --timeout=120s >/dev/null
fi
# Reset demod counters after any live packets and before the deterministic run.
kubectl -n "$NS" delete pod -l app=sigproc-demod --wait --timeout=120s >/dev/null
kubectl -n "$NS" rollout status deploy/sigproc-demod --timeout=180s >/dev/null
PREVIOUS_SIDECARS=$(kubectl -n "$NS" exec deploy/sigproc-demod -- \
  sh -c 'ls -1 /recordings/sigproc-demod-*.json 2>/dev/null || true')
kubectl -n "$NS" delete job "$JOB" --ignore-not-found >/dev/null
kubectl -n "$NS" apply -f deploy/demod-gate-job.yaml >/dev/null
kubectl -n "$NS" wait --for=condition=complete "job/$JOB" --timeout=300s >/dev/null
kubectl -n "$NS" logs "job/$JOB" --tail=30

METRICS=$(kubectl -n "$NS" exec deploy/sigproc-demod -- \
  wget -qO- http://127.0.0.1:9093/metrics)
printf '%s\n' "$METRICS" | grep -E 'demod_(context_valid|samples_out_total|vita49_packets_dropped_total|vita49_seq_gaps_total) '
printf '%s\n' "$METRICS" | grep -q 'demod_context_valid 1'
printf '%s\n' "$METRICS" | grep -q 'demod_samples_out_total 100000'
printf '%s\n' "$METRICS" | grep -q 'demod_vita49_packets_dropped_total 0'
printf '%s\n' "$METRICS" | grep -q 'demod_vita49_seq_gaps_total 0'

# SIGTERM finalizes the active WAV and JSON completion marker on the PVC.
kubectl -n "$NS" delete pod -l app=sigproc-demod --wait --timeout=120s >/dev/null
kubectl -n "$NS" rollout status deploy/sigproc-demod --timeout=180s >/dev/null
POD=$(kubectl -n "$NS" get pod -l app=sigproc-demod \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')
rm -rf "$OUT"
mkdir -p "$OUT"
printf '%s\n' "$PREVIOUS_SIDECARS" > "$OUT/previous-sidecars.txt"
MSYS_NO_PATHCONV=1 kubectl -n "$NS" cp "$NS/$POD:/recordings" "$OUT/recordings"

"$PYTHON" - "$OUT/recordings" "$OUT/previous-sidecars.txt" <<'PY'
import json
import math
import pathlib
import sys
import wave
root = pathlib.Path(sys.argv[1])
baseline = {
    line.strip().rsplit("/", 1)[-1]
    for line in pathlib.Path(sys.argv[2]).read_text().splitlines()
    if line.strip()
}
expected = {
    "owner": "sigproc-demod-v1",
    "mode": "nbfm",
    "sample_rate_hz": 50_000,
    "channels": 1,
    "encoding": "PCM16LE",
    "stream_id": 915_200,
    "rf_freq_hz": 915_200_000.0,
    "first_input_sample": 0,
    "first_output_sample": 0,
    "samples": 100_000,
    "end_reason": "shutdown",
}
initial_boundary = {
    "kind": "context_change",
    "input_sample": None,
    "missing_samples": None,
    "output_sample": 0,
    "recording_sample": 0,
}
candidates = []
for path in root.glob("sigproc-demod-*.json"):
    if path.name in baseline:
        continue
    metadata = json.loads(path.read_text())
    if all(metadata.get(key) == value for key, value in expected.items()):
        if metadata.get("boundary") in (None, initial_boundary):
            candidates.append((path.stat().st_mtime_ns, path, metadata))
if not candidates:
    raise SystemExit("no new completed NBFM gate WAV with exactly 100000 samples")
_, sidecar, metadata = max(candidates)
wav_path = root / metadata["wav_file"]
with wave.open(str(wav_path), "rb") as wav:
    assert wav.getnchannels() == 1
    assert wav.getsampwidth() == 2
    assert wav.getframerate() == 50_000
    assert wav.getnframes() == metadata["samples"]
    rate = wav.getframerate()
    raw = wav.readframes(wav.getnframes())
audio = [
    int.from_bytes(raw[index:index + 2], "little", signed=True) / 32767.0
    for index in range(20_000, len(raw), 2)
]
mean = sum(audio) / len(audio)
windowed = [
    (sample - mean) * (0.5 - 0.5 * math.cos(2.0 * math.pi * n / (len(audio) - 1)))
    for n, sample in enumerate(audio)
]

def tone_power(frequency):
    omega = 2.0 * math.pi * frequency / rate
    coefficient = 2.0 * math.cos(omega)
    previous = 0.0
    current = 0.0
    for sample in windowed:
        previous, current = current, sample + coefficient * current - previous
    return current * current + previous * previous - coefficient * current * previous

coarse = max((float(frequency) for frequency in range(100, 5_001, 10)), key=tone_power)
tone = max((coarse + offset / 10.0 for offset in range(-100, 101)), key=tone_power)
rms = math.sqrt(sum(sample * sample for sample in audio) / len(audio))
assert abs(tone - 1_000.0) <= 5.0, tone
assert rms > 0.05, rms
print(json.dumps({
    "ok": True,
    "wav": str(wav_path),
    "sidecar": str(sidecar),
    "samples": metadata["samples"],
    "tone_hz": tone,
    "tone_error_hz": tone - 1_000.0,
    "rms": rms,
    "mode": metadata["mode"],
    "end_reason": metadata["end_reason"],
}))
PY
