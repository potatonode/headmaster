# headscale-client

A typed Rust gRPC client for the [headscale](https://github.com/juanfont/headscale) control plane API.

## What it provides

- **`HeadscaleConnector` trait** — an async factory trait that produces a
  `HeadscaleServiceClient<Channel>` given a Kubernetes namespace and service name. The production
  implementation (`LiveConnector`) dials `http://{name}.{namespace}.svc:50444`. Tests supply a
  different implementation that returns a client wired to the in-process fake server.
- **`fake` module** (feature `fake-server`) — an in-process `FakeHeadscaleServer` backed by
  `Arc<Mutex<Vec<...>>>` state, plus `spawn_fake_server` which wires a duplex channel and returns
  a `HeadscaleServiceClient<Channel>` ready to use — no network or cluster needed.

## Proto files

The proto files in `proto/` are vendored from the headscale repository. To update them, run:

```
task update-protos                          # sync from main
HEADSCALE_REF=v0.29.0-beta.2 task update-protos   # sync from a specific tag or commit
```

The task uses `git sparse-checkout` to fetch only the `proto/headscale/` subtree without
downloading the full headscale repository. New proto files added upstream are picked up
automatically; no hardcoded file list is maintained.

## Code generation

`build.rs` uses [`protox`](https://crates.io/crates/protox) (pure Rust protobuf parser, no system
`protoc` required) and [`tonic-prost-build`](https://crates.io/crates/tonic-prost-build) to
generate the client and, when the `fake-server` feature is enabled, the server stubs.
