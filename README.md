# Headmaster

Headmaster is a Kubernetes operator that manages self-hosted
[headscale](https://github.com/juanfont/headscale) control-plane instances.
It provisions headscale as a StatefulSet, reconciles ACL policies, and ships a
SCIM sidecar that keeps headscale users in sync with an OIDC provider.

## Features

- **Declarative instances** — one `HeadscaleInstance` CR per control plane;
  headmaster handles the StatefulSet, Services, PVCs, and Secrets
- **ACL policy management** — the operator takes ownership of the acl policy
  which users define in the CRD.
- **OIDC integration** — link an instance to an OIDC provider via
  `scimProviderID`; users and groups flow in automatically
- **SCIM user sync** — the bundled `headmaster-scim` sidecar bridges the OIDC
  provider's SCIM endpoint to the headscale API
- **Per-Ingress access grants** — declare who can reach each app directly on
  the Ingress using headscale grants, including app-capability headers
- **Admission webhook** — validates `HeadscaleInstance` and `Ingress` specs at
  apply time

## Requirements

| Tool       | Version | Notes                    |
| ---------- | ------- | ------------------------ |
| Kubernetes | 1.32+   |                          |
| Helm       | 3.x     |                          |
| headscale  | 0.29.0+ | image set in values.yaml |

## Installation

```sh
helm upgrade --install headmaster \
  oci://ghcr.io/potatonode/charts/headmaster \
  --namespace headmaster-system --create-namespace \
  --set headscaleImage=ghcr.io/juanfont/headscale:v0.29.0-beta.2
```

## Usage

Create a `HeadscaleInstance` in the same namespace as the operator:

```yaml
apiVersion: headmaster.potatonode.github.io/v1alpha1
kind: HeadscaleInstance
metadata:
  name: main
  namespace: headmaster-system
spec:
  serverUrl: "https://headscale.example.com"
  dnsBaseDomain: "ts.example.com"
  storage:
    size: 5Gi
  policy:
    inline: |
      {
        "acls": [{ "action": "accept", "src": ["*"], "dst": ["*:*"] }]
      }
```

See [`examples/`](examples/) for a full values file including OIDC and SCIM
configuration.

### Per-Ingress access grants

The `access` field on the headmaster annotation lets you express who can reach
an app directly on the `Ingress`, instead of editing the shared inline policy
on `HeadscaleInstance`. Each grant specifies a set of source principals and an
optional map of app capabilities.

**Plain access grant** — allow a group to reach the app over any port:

```yaml
annotations:
  headmaster.potatonode.github.io/config: |
    {
      "headscale-ref": "main",
      "user": "alice",
      "access": [
        { "from": ["group:eng"] }
      ]
    }
```

**Capability grant** — attach roles that the app receives via the
`Tailscale-App-Capabilities` HTTP header:

```yaml
annotations:
  headmaster.potatonode.github.io/config: |
    {
      "headscale-ref": "main",
      "user": "alice",
      "access": [
        {
          "from": ["group:eng"],
          "capabilities": {
            "myapp/cap/admin": [{ "role": "admin" }]
          }
        },
        {
          "from": ["group:viewers"],
          "capabilities": {
            "myapp/cap/admin": [{ "role": "viewer" }]
          }
        }
      ]
    }
```

The operator assigns a synthetic tag `tag:hm-<namespace>-<name>` to the proxy
and uses it as the grant destination. If a `group:*` reference in `from` is not
yet synced (e.g. SCIM hasn't run), that grant is skipped and a `WaitingForGroup`
warning event is posted on the Ingress. Once the group appears, the next
reconcile applies the grant automatically.

The admission webhook validates that each access grant's `from` list is non-empty.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for development environment setup and
common commands.

## License

BSD-3-Clause — see [LICENSE](LICENSE).
