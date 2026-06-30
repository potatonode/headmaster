# headmaster-scim

A SCIM 2.0 server that bridges an IdP (Okta, Entra ID, Authentik, Pocket ID,
etc.) to headscale's ACL policy. It manages **groups only** — user accounts are
created by headscale's OIDC login flow, not by SCIM.

## What it does

- Exposes a SCIM 2.0 endpoint (`/scim/v2/...`) protected by a static bearer token.
- Accepts `Users` and `Groups` provisioning requests from the IdP.
- Stores a local mapping of SCIM external IDs → headscale usernames in a JSON file
  (see `SCIM_EXTERNAL_ID_FILE`).
- On every group change, rewrites the `groups` section of the live headscale ACL
  policy over gRPC, leaving all other policy keys (acls, hosts, tagOwners, etc.)
  untouched.
- Optionally expires all headscale nodes belonging to a user when their policy
  identifier changes, forcing re-authentication (see `EXPIRE_NODES_ON_CHANGE`).

## Configuration

All configuration is via environment variables.

| Variable                 | Required                         | Default                      | Description                                                                              |
| ------------------------ | -------------------------------- | ---------------------------- | ---------------------------------------------------------------------------------------- |
| `HEADSCALE_URL`          | yes                              | —                            | gRPC endpoint of the headscale server (e.g. `http://headscale:50443`)                    |
| `HEADSCALE_API_KEY`      | yes                              | —                            | headscale API key used to authenticate gRPC calls                                        |
| `SCIM_BEARER_TOKEN`      | yes                              | —                            | Static bearer token the IdP must send in `Authorization: Bearer <token>`                 |
| `SCIM_EXTERNAL_ID_FILE`  | no                               | `/data/external-id-map.json` | Path to the JSON file that persists the SCIM externalId → headscale username mapping     |
| `SCIM_LISTEN_ADDR`       | no                               | `0.0.0.0:8081`               | Address and port to listen on                                                            |
| `POLICY_USER_KEY`        | no                               | `email`                      | Which identifier to write into headscale policy group entries. See below.                |
| `OIDC_ISSUER`            | if `POLICY_USER_KEY=external_id` | —                            | OIDC issuer URL (trailing slash is stripped). Used to construct the ProviderIdentifier.  |
| `EXPIRE_NODES_ON_CHANGE` | no                               | `false`                      | When `true`, expire all of a user's headscale nodes when their policy identifier changes |

### `POLICY_USER_KEY`

Controls the format of the member strings written into headscale policy groups:

- `email` (default) — `alice@example.com`. Works with any IdP that always provides
  email. If the user's email changes, the policy member string changes too; enable
  `EXPIRE_NODES_ON_CHANGE` to force re-authentication so headscale picks up the
  new identifier.
- `username` — `alice@`. Works with any IdP. Same caveat as `email` if the
  username changes.
- `external_id` — `https://idp.example.com/<uuid>@`. Uses the OIDC
  ProviderIdentifier, which is stable across email and username changes. The IdP
  must send the same value as `externalId` in SCIM that it presents as the subject
  (`sub`) in OIDC tokens — the ProviderIdentifier headscale stores is constructed
  as `<OIDC_ISSUER>/<sub>`. Requires `OIDC_ISSUER` and a compatible IdP (Pocket
  ID, Authentik, Okta with default config). Recommended when available.

## Data / persistence model

The only persistent state is `SCIM_EXTERNAL_ID_FILE`, a JSON object mapping
SCIM `externalId` values to headscale usernames. This file is read on startup
and written on every user create/update. If the file does not exist the server
starts with an empty mapping.

The headscale ACL policy itself is the source of truth for group membership;
no separate group state is persisted locally.

## Endpoints

SCIM routes are served under `/scim/v2/` and require the bearer token.
Health/readiness probes are available unauthenticated at `/livez` and `/readyz`.
