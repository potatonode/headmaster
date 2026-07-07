# Examples

Static files for deploying headmaster together with
[Pocket ID](https://github.com/pocket-id/pocket-id) as the OIDC/SCIM provider.
Copy and adapt them for your own cluster.

## Prerequisites

- `kubectl` and `helm` installed
- A Kubernetes cluster with the headmaster CRDs installed
- `openssl` for generating the encryption key

## Steps

### 1. Generate the Pocket ID encryption key

```sh
kubectl create secret generic pocket-id-encryption-key \
  --namespace headmaster-system \
  --from-literal=key=$(openssl rand -base64 24)
```

### 2. Edit the values files

Replace the placeholder URLs in both values files with your real hostnames:

- `values-headmaster.yaml` — headscale `serverUrl`, `dnsBaseDomain`, and `oidcIssuer`
- `values-pocket-id.yaml` — Pocket ID `appUrl`

### 3. Install the headmaster operator

```sh
helm upgrade --install headmaster oci://ghcr.io/potatonode/charts/headmaster \
  --namespace headmaster-system --create-namespace \
  -f values-headmaster.yaml
```

### 4. Install the Pocket ID operator

```sh
helm upgrade --install pocket-id-operator oci://ghcr.io/aclerici38/charts/pocket-id-operator \
  --namespace headmaster-system \
  -f values-pocket-id.yaml
```

### 5. Apply the manifests

Edit the `<PLACEHOLDER>` values in `manifests/` to match your hostnames and
namespace, then apply:

```sh
kubectl apply -n headmaster-system -f manifests/
```
