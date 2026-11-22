#!/usr/bin/env bash
# Trigger a release-candidate build of all five sigproc images via the cluster's
# kaniko infrastructure. This is the manual/LAN-local equivalent of the GitHub
# Actions workflow (.github/workflows/rc-image-build.yml) — use it when you
# don't have a self-hosted runner set up yet.
#
# It templates deploy/gha/*.yaml with the current HEAD SHA and applies them,
# then waits for the kaniko Jobs to complete. Requires:
#   * kubectl configured for the cluster (KUBECONFIG env or default ~/.kube/config)
#   * deploy/kaniko-scratch-pvc.yaml applied in namespace build
#   * ghcr-push secret present in namespace build
#
# Usage:
#   ./scripts/trigger-rc-build.sh [git-ref]
#
# With no arg, builds HEAD. With a ref, builds that commit/branch/tag.
set -euo pipefail

REF="${1:-HEAD}"
KUBECONFIG="${KUBECONFIG:-$HOME/.kube/config}"
export KUBECONFIG

# Resolve the ref to a full SHA for the immutable tag.
FULL_SHA="$(git rev-parse "$REF")"
SHORT_SHA="${FULL_SHA:0:7}"

echo "==> Triggering RC build for ${REF} (${SHORT_SHA})"

# Ensure prerequisites.
echo "==> Checking prerequisites"
kubectl -n build get pvc kaniko-scratch >/dev/null 2>&1 || {
  echo "ERROR: kaniko-scratch PVC not found. Apply it first:"
  echo "  kubectl -n build apply -f deploy/kaniko-scratch-pvc.yaml"
  exit 1
}
kubectl -n build get secret ghcr-push >/dev/null 2>&1 || {
  echo "ERROR: ghcr-push secret not found in namespace build."
  echo "  kubectl -n build create secret generic ghcr-push \\"
  echo "    --from-file=.dockerconfigjson=<dockerconfigjson>"
  exit 1
}

# Template and apply the capture RC Job.
echo "==> Applying capture RC Job"
env SHA_SHORT="$SHORT_SHA" GIT_REF="$FULL_SHA" envsubst \
  < deploy/gha/kaniko-job-rc.yaml > /tmp/kaniko-rc-sigproc.yaml
kubectl apply -f /tmp/kaniko-rc-sigproc.yaml

# Template and apply the chain RC Job.
echo "==> Applying chain RC Job"
env SHA_SHORT="$SHORT_SHA" GIT_REF="$FULL_SHA" envsubst \
  < deploy/gha/kaniko-chain-rc.yaml > /tmp/kaniko-rc-chain.yaml
kubectl apply -f /tmp/kaniko-rc-chain.yaml

# Wait for completion.
echo "==> Waiting for capture RC Job (up to 90m)"
if kubectl -n build wait --for=condition=complete \
    "job/kaniko-rc-sigproc-${SHORT_SHA}" --timeout=5400s; then
  echo "==> Capture RC Job complete"
else
  echo "==> Capture RC Job FAILED — last logs:"
  kubectl -n build logs "job/kaniko-rc-sigproc-${SHORT_SHA}" --tail=100 || true
  exit 1
fi

echo "==> Waiting for chain RC Job (up to 2h)"
if kubectl -n build wait --for=condition=complete \
    "job/kaniko-rc-sigproc-chain-${SHORT_SHA}" --timeout=7200s; then
  echo "==> Chain RC Job complete"
else
  echo "==> Chain RC Job FAILED — last logs:"
  kubectl -n build logs "job/kaniko-rc-sigproc-chain-${SHORT_SHA}" --tail=100 || true
  exit 1
fi

echo ""
echo "==> RC images pushed to ghcr.io/rodoyle/ tagged :${SHORT_SHA} and :latest"
echo "    sigproc, sigproc-channelizer, sigproc-fixture, sigproc-sink, sigproc-demod"
