# integration-tests

Functional and end-to-end test suites for the headmaster workspace. Not published
(`publish = false`).

## Test tiers

| Tier       | Command                | What it covers                                                |
| ---------- | ---------------------- | ------------------------------------------------------------- |
| Functional | `task test-functional` | Controller logic against a real API server via envtest        |
| E2E        | `task test-e2e`        | Full operator in k3d with Pocket ID OIDC and a live headscale |

Run both with `task verify`.

## Shared helpers (src/lib.rs)

- **`kube_client()`** — builds a `kube::Client` from `KUBE_CONTEXT` env var, or
  falls back to the default kubeconfig context.
- **`wait_for_namespace_ready()`** — polls all namespaced resources until every
  `Ready`/`Available` status condition reports `True`. Fails fast on
  `ImagePullBackOff`, `ErrImagePull`, `InvalidImageName`, `BackoffLimitExceeded`,
  or more than 5 `CrashLoopBackOff` restarts.

## Binaries

- **`wait_namespace_ready`** — thin CLI wrapper around `wait_for_namespace_ready`;
  used from Taskfile shell steps to block until a namespace is healthy.
