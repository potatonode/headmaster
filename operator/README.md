# operator

The `headmaster` Kubernetes operator.

## Configuration

| Variable             | Required | Description                                                        |
| -------------------- | -------- | ------------------------------------------------------------------ |
| `OPERATOR_NAMESPACE` | yes      | Namespace the operator itself is deployed into                     |
| `RUST_LOG`           | no       | Log filter (e.g. `info`, `headmaster=debug`). Defaults to `error`. |

## Health endpoints

The operator listens on **port 8080** (hard-coded). Both endpoints return
`200 OK` when the process is running.

| Path       | Probe type |
| ---------- | ---------- |
| `/healthz` | liveness   |
| `/readyz`  | readiness  |

Port 8080 is hard-coded and intentionally not configurable. It must match the
`containerPort`, `livenessProbe`, and `readinessProbe` in the Deployment
manifest, so making it a runtime variable would only push the coordination
problem elsewhere.
