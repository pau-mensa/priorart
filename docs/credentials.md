# Credential administration

A principal owns collections, authors records, and holds keys. Ownership and
authorship belong to the principal, not the key: rotating or revoking every key
leaves them intact, and a new key for the same principal regains access.

Principals, collections, and keys are provisioned by the operator, locally with
`priorart admin` or remotely under `/v1/admin`. Grant rules are in
[authorization](policy.md).

## Local administration

`priorart admin` opens the database in `PRIORART_DATA_DIR` directly. A fresh
database has `local-principal` and the restricted collection `local` it owns; no
key is issued automatically.

```sh
priorart admin create-principal                                # prints the new principal ID
priorart admin create-collection --owner PRINCIPAL_ID          # restricted
priorart admin create-collection --owner PRINCIPAL_ID --visibility public
priorart admin issue --principal PRINCIPAL_ID --grant COLLECTION:write
priorart admin list --principal PRINCIPAL_ID
priorart admin rotate --credential-id LOOKUP_ID
priorart admin revoke --credential-id LOOKUP_ID
priorart admin set-grants --credential-id LOOKUP_ID --grant local:read
```

- `issue` prints `credential` metadata and the `secret` once; it cannot be
  retrieved later. `--principal` defaults to `local-principal`, at least one
  `--grant` is required, and `--expires-at UNIX_SECONDS` sets an expiry.
- `list` shows grants, expiry, and revocation, never secrets.
- `rotate` issues a new key with the same principal, grants, and expiry and
  revokes the old one. Expired or revoked keys cannot be rotated.
- `revoke` is permanent.
- `set-grants` replaces the whole grant set; no `--grant` removes all access.

## Remote administration

Setting `PRIORART_ADMIN_TOKEN` (32–256 visible ASCII characters) enables operator
endpoints under `/v1/admin`; without it they return `404`. They accept only
`Authorization: Bearer <admin token>`; collection keys never open them, and the
token opens nothing else. Send it only over HTTPS or loopback.

| Request | Body | Response |
|---|---|---|
| `POST /v1/admin/principals` | none | `201 {"id"}` |
| `POST /v1/admin/collections` | `{"owner", "visibility"?}` (default `restricted`) | `201 {"id","visibility","created_at","owner_principal_id"}` |
| `POST /v1/admin/principals/{principal}/credentials` | `{"grants":[{"collection_id","operation"}], "expires_at"?}` | `201 {"credential","secret"}` |
| `GET /v1/admin/principals/{principal}/credentials` | none | `{"credentials":[…]}`, metadata only |
| `POST /v1/admin/credentials/{id}/rotate` | none | `201 {"credential","secret"}` |
| `PUT /v1/admin/credentials/{id}/grants` | `{"grants":[…]}`; empty removes all | `204` |
| `DELETE /v1/admin/credentials/{id}` | none | `204` |

Responses with a secret carry `Cache-Control: no-store`. Unknown principals,
unknown or revoked credentials, and grants the principal may not hold return
`404`.

Every change applies from the next request.

## Key format

`pa1_<32-hex lookup ID>_<64-hex secret>`. The lookup ID is not secret; the secret
is 256 random bits. The database stores only a SHA-256 verifier of the whole key.
