# operator

The `headmaster` Kubernetes operator. It manages two custom resources:
`HeadscaleInstance` and `Ingress` (via `ingressClassName: headmaster`).

## How it works

### HeadscaleInstance

A `HeadscaleInstance` CR represents a headscale control-plane deployment. The
operator reconciles it into:

- A **StatefulSet** and **Service** running the headscale server.
- A **ConfigMap** holding the headscale config file (merged from spec defaults
  and any user-supplied overrides).
- An **API key Secret** bootstrapped by exec-ing into the headscale pod and
  running `headscale apikeys create --expiration 876000h` (~100 years). The key
  is only created once; subsequent reconciles reuse it.
- An optional **SCIM sidecar** StatefulSet and bearer-token Secret, provisioned
  when `.spec.scim` is configured.

All communication with the headscale server (policy sync, pre-auth key
management, node expiry) is done over **gRPC** using the API key.

All resources are created in the same namespace as the `HeadscaleInstance` CR.

### Ingress

The operator watches `Ingress` objects with `ingressClassName: headmaster`. For
each matching Ingress it provisions a Tailscale proxy in the **operator
namespace** (`OPERATOR_NAMESPACE`), consisting of:

- A **StatefulSet** running the `tailscale` sidecar container.
- A **WireGuard NodePort Service** for the Tailscale/WireGuard UDP port.
- A **pre-auth key Secret** used for the proxy's initial headscale registration.
  The key is rotated on each reconcile before the proxy pod starts.
- A **serve ConfigMap** (`serve.json`) that maps Ingress HTTP path rules to
  cluster-internal backend URLs.
- **RBAC** (ServiceAccount, Role, RoleBinding) scoped to the state Secret.

The proxy uses Tailscale Serve to forward traffic from the tailnet to the
cluster-internal backend services defined in the Ingress rules.

Namespacing: proxy resources live in `OPERATOR_NAMESPACE`; the Ingress and its
backend Services live in any namespace.

### Policy management

The operator maintains the `groups` section of the live headscale ACL policy for
each `HeadscaleInstance`. On every reconcile it:

1. Fetches the current policy over gRPC.
2. Computes a new policy by merging Ingress grants and SCIM group membership into
   the `groups` key, using `PolicyEditor` to parse and edit the HuJSON document.
3. Prunes deleted groups from all `grants` entries; a grant is removed entirely
   only when its `src` or `dst` becomes empty after pruning.
4. Skips the `SetPolicy` gRPC call when the resulting policy is semantically
   identical to the live one (same JSON values, ignoring whitespace/comments).

All other policy keys (`acls`, `hosts`, `tagOwners`, etc.) are preserved.

## Configuration

| Variable                   | Required | Default | Description                                                                                         |
| -------------------------- | -------- | ------- | --------------------------------------------------------------------------------------------------- |
| `OPERATOR_NAMESPACE`       | yes      | —       | Namespace the operator itself is deployed into; proxy resources are created here                    |
| `HEADSCALE_IMAGE`          | yes      | —       | Container image for the headscale server (e.g. `headscale/headscale:0.23`)                          |
| `PROXY_IMAGE`              | yes      | —       | Container image for the Tailscale proxy sidecar (e.g. `tailscale/tailscale:latest`)                 |
| `OPERATOR_IMAGE`           | yes      | —       | Container image for the SCIM sidecar (the operator image is reused for the SCIM binary)             |
| `INGRESS_ENABLED`          | no       | `true`  | Set to `false` or `0` to disable the Ingress controller entirely                                    |
| `INGRESS_WATCH_NAMESPACES` | no       | all     | Comma-separated list of namespaces to watch for Ingress objects; empty means all namespaces         |
| `WEBHOOK_TLS_DIR`          | no       | —       | Directory containing `tls.crt` and `tls.key` for the admission webhook; webhook disabled when unset |
| `POD_NAME`                 | no       | —       | Kubernetes pod name, injected via the downward API; used to tag events with the controller instance |
| `RUST_LOG`                 | no       | `error` | Log filter (e.g. `info`, `headmaster=debug`)                                                        |

## Health endpoints

The operator listens on **port 8080** (hard-coded). Both endpoints return
`200 OK` when the process is running.

| Path       | Probe type |
| ---------- | ---------- |
| `/healthz` | liveness   |
| `/readyz`  | readiness  |
