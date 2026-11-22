# Run Book: ghcr-push Secret Regeneration

Securely regenerate the GHCR push secret for the `build` namespace kaniko Jobs,
using minimal privileges and encrypted-at-rest storage with `rage` + `sops`.

## What this secret is

The `ghcr-push` secret holds a Docker `config.json` that kaniko uses to push
images to `ghcr.io/rodoyle/*`. It is consumed by the kaniko Jobs in namespace
`build` (`deploy/kaniko-job.yaml`, `deploy/kaniko-channelizer-job.yaml`,
`deploy/gha/kaniko-*.yaml`) via a read-only secret volume.

## Security goals

1. **Minimal privilege** — the GHCR PAT only has `write:packages` for the
   `rodoyle` packages; no `read:packages`, no `delete:packages`, no repo admin.
2. **Encrypted at rest** — the plaintext `config.json` never lives in git.
   It is stored in the repo as a `sops`-encrypted file, decryptable only by
   holders of the `rage` identity private key.
3. **Namespace-scoped** — the secret lives only in `build`, not cluster-wide.
4. **Revocable** — a PAT can be rotated/deleted in GHCR without touching the
   cluster; regenerate the secret and re-apply.

## Prerequisites

Install once:

```bash
# age/rage key management
cargo install rage age-keygen          # or: winget install age

# sops (Mozilla)
winget install Mozilla.sops            # or: scoop install sops
```

Verify:

```bash
rage --version
sops --version
age-keygen --version
```

## Step 1 — Create the GHCR PAT with minimal scope

1. In GitHub, go to **Settings → Developer settings → Personal access tokens**.
   Use a **fine-grained** token (preferred) or classic token.
2. **Fine-grained token** (recommended):
   - Resource owner: `rodoyle` (user or org)
   - Repository access: **only select repositories** → choose `sigproc` (or leave
     empty if you want packages-only scope)
   - Package permissions: **Write** (this grants `write:packages`)
   - Repository permissions: **None** (packages do not need repo read/write)
   - Expiration: set a reasonable rotation window (e.g. 90 days)
3. **Classic token** (fallback):
   - Scopes: check only **`write:packages`**
   - Do **not** check `repo`, `delete:packages`, or any admin scope.
4. Generate and copy the token (`ghp_...` or `github_pat_...`). Treat it as a
   secret — it will be encrypted immediately.

> Minimal privilege note: `write:packages` lets kaniko create/overwrite image
> tags under `ghcr.io/rodoyle/*`. It cannot read other packages, delete
> packages, or touch repositories. This is the least scope that satisfies the
> build push.

## Step 2 — Create the rage identity (encryption key)

Generate a `rage` keypair that will encrypt/decrypt the secret file. Store the
private key outside the repo (e.g. a secrets vault or an offline location).

```bash
# Generate an age/rage identity keypair
age-keygen -o ~/.config/sops/age/keys.txt
chmod 600 ~/.config/sops/age/keys.txt

# Show the public key (this goes into .sops.yaml)
rage --encrypt --recipient "$(age-keygen -y ~/.config/sops/age/keys.txt)" </dev/null >/dev/null
AGE_PUB=$(age-keygen -y ~/.config/sops/age/keys.txt)
echo "Public key: $AGE_PUB"
```

Record the public key (`AGE_PUB`) — it is safe to commit in `.sops.yaml`.
Keep `~/.config/sops/age/keys.txt` private and backed up.

## Step 3 — Configure sops

Create `.sops.yaml` at the repo root so sops knows which key to use for the
secret file:

```yaml
creation_rules:
  - path_regex: secrets/.*\.yaml$
    age: <AGE_PUB>
```

Replace `<AGE_PUB>` with the public key from Step 2. Commit `.sops.yaml` — it
only contains the public key.

## Step 4 — Build the plaintext docker config.json

GHCR uses a docker config where the auth field is `base64(USERNAME:TOKEN)`.

```bash
GHCR_USER="rodoyle"
GHCR_TOKEN="<the PAT from Step 1>"

# Compute the base64 auth blob
AUTH_BLOB=$(printf '%s:%s' "$GHCR_USER" "$GHCR_TOKEN" | base64 -w0)

# Write the docker config.json (plaintext, temporary)
mkdir -p secrets
cat > secrets/ghcr-push.dockerconfigjson <<EOF
{
  "auths": {
    "ghcr.io": {
      "auth": "${AUTH_BLOB}"
    }
  }
}
EOF

# Sanity check: this config should authenticate to ghcr.io
# (optional, requires docker/podman)
# echo "$GHCR_TOKEN" | docker login ghcr.io -u "$GHCR_USER" --password-stdin
```

Do **not** commit `secrets/ghcr-push.dockerconfigjson` in plaintext.

## Step 5 — Encrypt with sops

```bash
sops --encrypt --in-place secrets/ghcr-push.dockerconfigjson
```

The file is now encrypted. Verify it is encrypted:

```bash
head -c 200 secrets/ghcr-push.dockerconfigjson
# Should show: sops: version, age recipients, encrypted data keys — NOT plaintext JSON
```

Add a `.gitignore` rule to prevent accidental plaintext commits if you ever
write a new plaintext file:

```
# secrets/*.plaintext
```

But keep the encrypted `secrets/ghcr-push.dockerconfigjson` tracked.

## Step 6 — Create the Kubernetes secret in the build namespace

Two options. **Option A** encrypts the whole Kubernetes secret manifest (better
for storing the manifest in git). **Option B** creates the secret directly from
the encrypted dockerconfig without storing the manifest.

### Option A — Encrypted secret manifest in git

Create the plaintext secret manifest:

```bash
cat > secrets/ghcr-push-secret.yaml <<'EOF'
apiVersion: v1
kind: Secret
metadata:
  name: ghcr-push
  namespace: build
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: PLACEHOLDER
EOF
```

Populate the `.dockerconfigjson` data field from the plaintext config, then
encrypt the whole manifest:

```bash
# Encode the plaintext config into the manifest's data field
PLAIN_B64=$(base64 -w0 < secrets/ghcr-push.dockerconfigjson)
sed -i "s|PLACEHOLDER|${PLAIN_B64}|" secrets/ghcr-push-secret.yaml

# Encrypt the manifest with sops
sops --encrypt --in-place secrets/ghcr-push-secret.yaml

# Commit the encrypted manifest
git add .sops.yaml secrets/ghcr-push-secret.yaml
git commit -m "secrets: add encrypted ghcr-push secret manifest"
```

Apply it (sops decrypts on the fly):

```bash
sops --decrypt secrets/ghcr-push-secret.yaml | kubectl apply -f -
```

### Option B — One-shot create from encrypted dockerconfig (no manifest in git)

If you prefer not to store a manifest:

```bash
# Decrypt and pipe straight into kubectl
sops --decrypt secrets/ghcr-push.dockerconfigjson \
  | kubectl create secret generic ghcr-push \
      --namespace build \
      --from-file=.dockerconfigjson=/dev/stdin \
      --type=kubernetes.io/dockerconfigjson \
      --dry-run=client -o yaml \
  | kubectl apply -f -
```

This creates/updates the secret in `build` without persisting a manifest.

## Step 7 — Verify

```bash
# Secret exists in build namespace
kubectl -n build get secret ghcr-push -o jsonpath='{.type}'
# Should print: kubernetes.io/dockerconfigjson

# kaniko can authenticate (spot check the registry)
# Using crane if available:
crane auth login ghcr.io -u rodoyle -p "$(sops --decrypt secrets/ghcr-push.dockerconfigjson | jq -r '.auths."ghcr.io".auth' | base64 -d)"
crane ls ghcr.io/rodoyle/sigproc
```

Or simply run a build trigger:

```bash
./scripts/trigger-rc-build.sh
```

If the kaniko Job pushes without `DENIED`, the secret is valid.

## Rotation

To rotate the PAT:

1. Revoke the old PAT in GitHub.
2. Create a new minimal-privilege PAT (Step 1).
3. Regenerate `secrets/ghcr-push.dockerconfigjson` (Step 4) and re-encrypt
   (Step 5).
4. Re-apply the secret (Step 6).
5. No cluster or Job changes needed — kaniko reads the secret at Job start.

## Cleanup

- Never commit a plaintext `*.dockerconfigjson` or `*-secret.yaml` without
  `sops` encryption.
- Delete any temporary plaintext files:
  ```bash
  shred -u secrets/ghcr-push.dockerconfigjson 2>/dev/null || rm -f secrets/ghcr-push.dockerconfigjson
  ```
- Back up the rage private key (`~/.config/sops/age/keys.txt`) to your secrets
  vault. Without it, the encrypted files cannot be decrypted.

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| kaniko Job fails with `DENIED` | PAT lacks `write:packages` or is expired | Regenerate PAT with correct scope (Step 1) |
| `sops: error: no matching data keys` | Wrong rage private key or corrupted file | Ensure `~/.config/sops/age/keys.txt` is present and matches `.sops.yaml` |
| `kubectl` says secret already exists | Use `apply` not `create`, or delete first | `kubectl -n build delete secret ghcr-push` then re-create |
| base64 auth fails | Token contains special chars not handled by shell | Use a fresh token and verify `printf '%s:%s'` output length |
