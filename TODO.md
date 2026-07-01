- e2e pocket_id tests: replace `headscale_exec` (kube-rs exec into the headscale pod running
  the CLI) with direct gRPC calls using `headscale_client::LiveConnector`. Currently blocked
  on exposing the headscale gRPC port outside the k3d cluster; options include a NodePort
  service, `kubectl port-forward` managed by the Taskfile, or a Traefik TCP IngressRoute.
- transaction guard for cleanup
- organize random files in the toplevel directory
- add Release Please for automated versioning: bumps Cargo.toml + Chart.yaml via a release PR
  on every conventional-commit merge to main, auto-creates the git tag on merge
- run connectivity test from a different container (or host) for direct connect test
- docs
- ingress: rename managed key tags
- auth_key rename pre_auth_key to create_pre_auth_key
- names.rs need comments for the fields
- HeadscaleInstance reconcile requeue interval (currently 60s): consider making this
  configurable via the CR spec or a controller flag so operators can tune the SCIM
  convergence latency vs. API-server load trade-off
- use new cel support in kube-rs 4 and check if there are other modernizations we need
- update AGENTS.md
- add README to all crates (k8s-ext, integration-tests, etc)
