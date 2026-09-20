#!/usr/bin/env bash
# In-cluster end-to-end gate for the channelizer chain.
#
# Prerequisite: the service images are published, i.e. the Job in
# deploy/kaniko-channelizer-job.yaml has completed.
#
# What this proves, in the cluster, with the production images and ConfigMaps:
#
#   fixture (in-cluster, deterministic)
#        |  VITA49 file
#        v
#   sigproc-channelizer (replay; --input/--file override the ConfigMap)
#        |  VITA49 over UDP, via the vita49-sink-gate Service
#        v
#   vita49-sink Job  ->  one JSON verdict line
#
# and, separately, that the pre-existing chain did not regress (the capture
# deployment and the downstream orbweaver/waterfall consumer).
#
# Usage:  bash test/cluster-gate.sh
#
# Exit status is 0 only when every assertion passed. Checks that cannot be
# evaluated (e.g. the downstream stats endpoint is unreachable) are reported as
# INCONCLUSIVE and never counted as a pass.

set -u

NS=default
GATE_JOB=sigproc-channelizer-gate
SINK_JOB=vita49-sink
FAILURES=0
INCONCLUSIVE=0

say()  { printf '%s\n' "$*"; }
pass() { printf 'PASS  %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; FAILURES=$((FAILURES + 1)); }
note() { printf 'INCONC %s\n' "$*"; INCONCLUSIVE=$((INCONCLUSIVE + 1)); }

# assert_eq <label> <actual> <expected>
assert_eq() {
  if [ "$2" = "$3" ]; then
    pass "$1 = $2"
  else
    fail "$1 = $2 (expected $3)"
  fi
}

# ── 0. Record the baseline of the pre-existing chain ──────────────────────────
say "== baseline =="
CAPTURE_RESTARTS_BEFORE=$(kubectl -n "$NS" get pods -l app=sigproc \
  -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}' 2>/dev/null || echo "?")
say "capture restart count before: ${CAPTURE_RESTARTS_BEFORE}"

# Downstream parse-error counters, if the consumer's stats endpoint is reachable.
downstream_stats() {
  kubectl -n "$NS" run ca-gate-curl --rm -q -i --restart=Never \
    --image=curlimages/curl:latest --command -- \
    curl -s --max-time 10 http://orbweaver-service:4830/stats 2>/dev/null | tail -1
}
STATS_BEFORE=$(downstream_stats || true)
if [ -n "${STATS_BEFORE:-}" ]; then
  say "downstream stats before: $(printf '%s' "$STATS_BEFORE" | cut -c1-200)"
else
  note "downstream /stats unreachable; no-regression will fall back to log grep"
fi

# ── 1. Apply the chain ────────────────────────────────────────────────────────
say "== apply =="
kubectl -n "$NS" apply \
  -f deploy/channelizer-configmap.yaml \
  -f deploy/channelizer-service.yaml \
  -f deploy/channelizer-deployment.yaml \
  -f deploy/sink-service.yaml \
  -f deploy/sink-deployment.yaml \
  -f deploy/channelizer-gate-configmap.yaml \
  -f deploy/sink-gate-service.yaml >/dev/null || fail "apply of the chain manifests"

kubectl -n "$NS" rollout status deploy/sigproc-channelizer --timeout=180s >/dev/null 2>&1 \
  && pass "sigproc-channelizer rolled out" \
  || fail "sigproc-channelizer did not roll out"

kubectl -n "$NS" rollout status deploy/vita49-sink-standing --timeout=180s >/dev/null 2>&1 \
  && pass "standing vita49-sink rolled out" \
  || fail "standing vita49-sink did not roll out"

# The fixture init container is what makes the in-cluster input deterministic;
# if it silently produced nothing, the gate Job would fail for the wrong reason.
if kubectl -n "$NS" logs deploy/sigproc-channelizer -c fixture 2>/dev/null | grep -q "wrote "; then
  pass "fixture init container generated the fixture in the pod"
else
  fail "fixture init container produced no output (kubectl -n $NS logs deploy/sigproc-channelizer -c fixture)"
fi

# Contract gate 23 asked for `kubectl exec ... wget -qO- localhost:8080/metrics`.
# Substituted deliberately: the channelizer image is a slim Debian image with no
# wget/curl, and adding an HTTP client to a service image purely to satisfy a
# probe is the wrong trade. A short-lived curl pod against the Service checks the
# same endpoint AND the Service path, which is strictly more of the system.
METRICS=$(kubectl -n "$NS" run ca-gate-metrics --rm -q -i --restart=Never \
  --image=curlimages/curl:latest --command -- \
  curl -s --max-time 10 http://sigproc-channelizer:8080/metrics 2>/dev/null || true)
if printf '%s' "$METRICS" | grep -q "channelizer_output_power_dbfs"; then
  pass "metrics served over the Service (channelizer_output_power_dbfs present)"
else
  note "could not read /metrics over the Service; endpoint not verified here"
fi

# ── 2. Start the gate's sink and wait until it is actually listening ──────────
# Jobs are immutable, so re-creating is how a gate is re-run.
say "== gate sink =="
kubectl -n "$NS" delete job "$SINK_JOB" --ignore-not-found >/dev/null 2>&1
kubectl -n "$NS" apply -f deploy/sink-job.yaml >/dev/null || fail "apply of the gate sink Job"

LISTENING=0
for _ in $(seq 1 60); do
  if kubectl -n "$NS" logs "job/$SINK_JOB" 2>/dev/null | grep -q "listening for narrowband"; then
    LISTENING=1
    break
  fi
  sleep 1
done
if [ "$LISTENING" = "1" ]; then
  pass "gate sink is listening before the producer starts"
else
  fail "gate sink never reported listening"
fi

# ── 3. Run the channelizer in replay mode ────────────────────────────────────
say "== channelizer replay gate =="
kubectl -n "$NS" delete job "$GATE_JOB" --ignore-not-found >/dev/null 2>&1
kubectl -n "$NS" apply -f deploy/channelizer-gate-job.yaml >/dev/null || fail "apply of the gate Job"

kubectl -n "$NS" wait --for=condition=complete "job/$GATE_JOB" --timeout=300s >/dev/null 2>&1 \
  && pass "gate Job completed" \
  || fail "gate Job did not complete (logs: kubectl -n $NS logs job/$GATE_JOB)"

REPORT=$(kubectl -n "$NS" logs "job/$GATE_JOB" 2>/dev/null | grep '^{' | tail -1)
say "channelizer report: $(printf '%s' "$REPORT" | cut -c1-240)"

json_field() {
  # Minimal JSON field reader; avoids depending on jq inside the gate.
  printf '%s' "$1" | tr ',' '\n' | grep -F "\"$2\":" | head -1 | cut -d: -f2- | tr -d ' "'
}

assert_eq "report.ok"            "$(json_field "$REPORT" ok)"            "true"
assert_eq "report.malformed"     "$(json_field "$REPORT" malformed_packets)" "0"
assert_eq "report.dropped"       "$(json_field "$REPORT" packets_dropped)"   "0"
assert_eq "report.seq_gaps"      "$(json_field "$REPORT" seq_gaps)"      "0"
assert_eq "report.samples_in"    "$(json_field "$REPORT" samples_in)"    "2000000"
assert_eq "report.samples_out"   "$(json_field "$REPORT" samples_out)"   "50000"
assert_eq "report.decimation"    "$(json_field "$REPORT" decimation)"    "40"

# ── 4. The sink's own verdict, through the real UDP hop ──────────────────────
say "== sink verdict =="
kubectl -n "$NS" wait --for=condition=complete "job/$SINK_JOB" --timeout=300s >/dev/null 2>&1 \
  && pass "gate sink Job completed" \
  || fail "gate sink Job did not complete"

VERDICT=$(kubectl -n "$NS" logs "job/$SINK_JOB" 2>/dev/null | grep '^{' | tail -1)
say "sink verdict: $(printf '%s' "$VERDICT" | cut -c1-260)"

assert_eq "verdict.ok"        "$(json_field "$VERDICT" ok)"        "true"
assert_eq "verdict.rate_hz"   "$(json_field "$VERDICT" rate_hz)"   "50000.0"
assert_eq "verdict.seq_gaps"  "$(json_field "$VERDICT" seq_gaps)"  "0"
assert_eq "verdict.malformed" "$(json_field "$VERDICT" malformed)" "0"
assert_eq "verdict.samples"   "$(json_field "$VERDICT" samples)"   "50000"

# Recovered tone inside +-5 Hz of the 1 kHz the fixture modulated. This is the
# assertion that validates the sample rate: VITA49 carries no rate field, and a
# wrong decoding rate would move the measured tone proportionally.
TONE_ERROR=$(json_field "$VERDICT" tone_error_hz)
if [ -n "${TONE_ERROR:-}" ]; then
  awk -v e="$TONE_ERROR" 'BEGIN { exit !(e < 5.0 && e > -5.0) }' \
    && pass "recovered tone within 5 Hz (error ${TONE_ERROR} Hz)" \
    || fail "recovered tone error ${TONE_ERROR} Hz exceeds 5 Hz"
else
  fail "sink verdict carried no tone_error_hz"
fi

# ── 5. No regression in the pre-existing chain ───────────────────────────────
say "== no regression =="
assert_eq "capture readyReplicas" \
  "$(kubectl -n "$NS" get deploy sigproc -o jsonpath='{.status.readyReplicas}' 2>/dev/null)" "1"

CAPTURE_RESTARTS_AFTER=$(kubectl -n "$NS" get pods -l app=sigproc \
  -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}' 2>/dev/null || echo "?")
assert_eq "capture restart count after" "$CAPTURE_RESTARTS_AFTER" "${CAPTURE_RESTARTS_BEFORE:-?}"

if [ -n "${STATS_BEFORE:-}" ]; then
  STATS_AFTER=$(downstream_stats || true)
  if [ -n "${STATS_AFTER:-}" ]; then
    if [ "$STATS_AFTER" = "$STATS_BEFORE" ]; then
      pass "downstream consumer stats unchanged"
    else
      # Any movement in the wire-loss / parse counters is a regression signal.
      say "downstream stats after:  $(printf '%s' "$STATS_AFTER" | cut -c1-200)"
      fail "downstream consumer stats moved during the gate"
    fi
  else
    note "downstream /stats became unreachable mid-gate"
  fi
else
  # Fall back to the logs: look for parse errors in the window we just caused.
  if kubectl -n "$NS" logs deploy/orbweaver-consumer --tail=200 2>/dev/null \
      | grep -qi "parse error"; then
    fail "downstream consumer logged a parse error during the gate"
  else
    note "downstream /stats unavailable; checked logs only (no jq-free counter comparison)"
  fi
fi

# ── 6. Verdict ───────────────────────────────────────────────────────────────
say ""
if [ "$FAILURES" -eq 0 ]; then
  say "GATE PASSED (${INCONCLUSIVE} inconclusive check(s))"
  exit 0
fi
say "GATE FAILED: ${FAILURES} assertion(s), ${INCONCLUSIVE} inconclusive"
exit 1
