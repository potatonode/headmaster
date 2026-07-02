# Contributing

## Development environment

| Tool          | Purpose                       | Install                              |
| ------------- | ----------------------------- | ------------------------------------ |
| Rust (stable) | Build, test, lint             | `brew install rustup && rustup-init` |
| task          | Wraps all dev commands        | `brew install go-task`               |
| git           | Version control               | preinstalled or `brew install git`   |
| Node.js (npx) | Prettier for markdown         | `brew install node`                  |
| Go 1.25+      | Required for functional tests | `brew install go`                    |
| libclang      | Required for functional tests | `brew install llvm`                  |
| Docker        | Required for e2e tests        | `brew install --cask docker`         |
| k3d           | Required for e2e tests        | `brew install k3d`                   |
| Helm          | Required for e2e tests        | `brew install helm`                  |
| kubectl       | Required for e2e tests        | `brew install kubectl`               |

Go and libclang are only needed to compile the `functional` feature used by `task test-functional`.
Docker, k3d, kubectl, and Helm are only needed for `task test-e2e` and related cluster tasks.
`task test-unit` and `task build` work without any of these.

On first run, `task test-functional` downloads `kube-apiserver` and `etcd` binaries and caches them
in `~/Library/Application Support/io.kubebuilder.envtest` (macOS).

## Common commands

- `task verify` — run all checks (fmt, clippy, unit tests, functional tests, e2e). The canonical "is everything OK?" command. Run this before every commit. Requires Go 1.25+, libclang, Docker, k3d, and Helm.
- `task build` — compile the workspace
- `task lint` — run formatting check, clippy, CRD validation, and Helm lint (no cluster needed)
- `task test-unit` — run unit tests only (no extra tools needed)
- `task test-functional` — run functional tests against an in-process API server (requires Go + libclang)
- `task test-e2e` — run e2e tests; always recreates the k3d cluster for a clean slate (requires Docker, k3d, Helm)
- `task fmt` — format Rust code and markdown in place
- `task generate` — regenerate `chart/crds/`, `chart/values.schema.json`, and `chart/README.md`; run and commit after any CRD schema change
- `task cluster-create` / `task cluster-delete` — manually manage the headmaster-test k3d cluster
- `task diagnose` — dump operator logs and cluster state; useful after an e2e failure
- `task update-protos` — re-vendor headscale proto files from the upstream repo

To keep the k3d cluster alive after `task verify` (useful when iterating locally), create a `.env` file in the repo root:

```sh
KEEP_CLUSTER=true
```

The Taskfile loads `.env` automatically. Without this, the cluster is deleted at the end of every `test-e2e` run.

After editing any `*.md` file, run `npx prettier --write <file>`.
