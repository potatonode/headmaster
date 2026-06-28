# AGENTS.md

Guidance for AI coding agents working on this repo.

---

## What this project is

Headmaster is a Rust Kubernetes operator that manages self-hosted
[headscale](https://github.com/juanfont/headscale) control-plane instances and
the Tailscale proxy pods that expose Kubernetes Ingresses onto a tailnet.

Two binaries ship in a single Docker image:

- **`operator`** — the kube-rs reconciler. Manages `HeadscaleInstance` CRDs and
  `Ingress` objects annotated with `ingressClassName: headmaster`.
- **`headmaster-scim`** — a sidecar HTTP server (one per headscale instance)
  that accepts SCIM push from an OIDC provider and syncs users/groups into
  headscale.

## Workspace layout

| Crate               | Purpose                                                                                                |
| ------------------- | ------------------------------------------------------------------------------------------------------ |
| `operator`          | Main binary. CRD types, controllers, webhook, health server.                                           |
| `headscale-client`  | Typed gRPC client for the headscale API (vendored protos). Has a `fake-server` feature for unit tests. |
| `scim`              | The `headmaster-scim` binary. Standalone HTTP server; shares `headscale-client`.                       |
| `integration-tests` | Functional and e2e test suites. Not published (`publish = false`).                                     |

## Controllers

**`HeadscaleInstance` controller** (`operator/src/controllers/headscale_instance.rs`):
manages headscale StatefulSets, Services, PVCs, ConfigMaps, and SCIM
sidecar resources, all in the operator namespace.

**Ingress controller** (`operator/src/controllers/ingress.rs`): for each
`Ingress` annotated `ingressClassName: headmaster`, provisions a Tailscale proxy
StatefulSet (plus WireGuard NodePort Service, auth-key Secret, serve ConfigMap,
RBAC) in the **operator namespace**, regardless of which namespace the Ingress
lives in. The `ChildApplier` struct handles SSA-applying all child resources
with consistent owner references and labels.

## Key patterns

**Server-side apply** — all child resources are applied via
`Patch::Apply` with field manager `"headmaster"` (constant `FIELD_MANAGER`).
Use `ChildApplier::apply` for namespaced resources; never patch ownership or
labels by hand.

**Status conditions** — defined once in `operator/src/condition.rs`. Every CRD
status has at minimum `observed_generation: Option<i64>` and
`conditions: Vec<Condition>`. Fields follow the Kubernetes convention: `type`,
`status` (`"True"`/`"False"`/`"Unknown"`), `reason`, `message`,
`last_transition_time`. A bare `ready: bool` is not a substitute.

**Finalizers** — always use `kube::runtime::finalizer::finalizer`. Never
manipulate the finalizer array directly.

**Events** — emit on state transitions only (not every reconcile). Use
`ctx.recorder().publish_warning(...)` or `publish_normal(...)`.

**Proxy state secret** — each proxy has a `proxy-state-<base>` Secret in the
operator namespace. The operator creates it empty; the tailscale container in
the proxy pod writes `device_id` and `device_ips` into it at runtime. The
operator also writes `headscale_ref` into it during `apply()` so `cleanup()` can
find the right headscale instance even if the Ingress annotation is later
removed.

## RBAC surface

**ClusterRole** (cluster-scoped reads):

- `networking.k8s.io`: `ingressclasses` (get/create/patch/update),
  `ingresses` (get/list/watch/patch/delete), `ingresses/status` (get/patch)
- `""`: `namespaces` (get)

**Role** (operator namespace only):

- `headmaster.potatonode.github.io`: `headscaleinstances`, `headscaleinstances/status`
- `""`: `configmaps`, `secrets`, `serviceaccounts`, `services`, `pods` (get/list), `pods/exec`, `events`
- `apps`: `statefulsets`
- `rbac.authorization.k8s.io`: `roles`, `rolebindings`
- webhook only: `batch/jobs`, `pods/log`

Any new resource the operator creates or patches needs a matching rule added to
the appropriate template in `chart/templates/`.

## CRD conventions

Apply all four rules whenever adding or changing a CRD type or field:

1. **Doc comments** — every `pub` struct and every field must have a `///`
   comment (appears in `kubectl explain`).
2. **`Vec` fields** — add `#[schemars(length(min = 1))]` unless an empty list
   is explicitly valid.
3. **`Option<T>` fields** — add `#[serde(default)]` so the field can be omitted
   in YAML without a deserialization error.
4. **Round-trip tests** — when adding any `Option<T>` field, add a test that
   omits it entirely (not just sets it to `null`) to verify `#[serde(default)]`.

After any CRD schema change, run `task crdgen` (syncs `chart/crds/`) and commit
the updated files. `task verify` will fail if the committed CRDs are out of date.

## Code ordering within files

1. Public entrypoints in lifecycle order (`new` → `apply` → `cleanup`).
2. Helpers in depth-first call order — each helper just after its first caller.
3. Shared helpers (called by more than one of the above) after all specific ones.
4. Tests at the bottom in `#[cfg(test)] mod tests { ... }`.

This applies to Rust source files and Taskfile tasks.

## Test tiers

| Tier       | Command                | What it covers                                            | Extra requirements |
| ---------- | ---------------------- | --------------------------------------------------------- | ------------------ |
| Unit       | `task test-unit`       | Pure logic, fake gRPC server                              | None               |
| Functional | `task test-functional` | CRD schema, controller logic vs real API server (envtest) | Go 1.25+, libclang |
| E2E        | `task test-e2e`        | Full operator in k3d + Pocket ID OIDC + headscale         | Docker, k3d, Helm  |

`task verify` runs all three tiers. It creates and tears down a real k3d cluster
for e2e. Set `KEEP_CLUSTER=true` in `.env` to leave the cluster running between
runs for local iteration.

## Taskfile wrappers

Always use Taskfile targets, not the underlying tools directly:

- `task build` not `cargo build`
- `task verify` not `cargo clippy && cargo test && …`
- `task fmt` not `cargo fmt && npx prettier`
- `task crdgen` not `cargo run --bin crdgen`

## Markdown formatting

After writing or editing any `*.md` file, run `npx prettier --write <file>`
immediately. `task fmt` covers everything (Rust + markdown).

## Stop and ask before

- Adding a new top-level dependency to any `Cargo.toml`.
- Changing the public shape of a CRD (fields, validation, group, version).
- Introducing a new top-level module under `operator/src/`.
- Touching `.github/workflows/`.
- A fix requires changing every caller — that signals a wrong abstraction.
- You find yourself applying a second patch to the same failure.
