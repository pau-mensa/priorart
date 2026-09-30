# Credential administration

Credential lifecycle, [service policy](policy.md), and [HTTP header authentication](protocol.md)
are implemented. `PRIORART_MODE=authenticated` enables credential-based access with
anonymous public reads; default `local` mode uses the local principal when no key
is supplied. Invalid supplied keys always fail. Only authenticated mode can be
[served beyond loopback](protocol.md#network-hosting). MCP currently supports the
local workflow only.

## Principals, collections, and keys

A principal is the identity that owns collections, authors records, and holds keys.
Ownership and authorship belong to the principal, not to a key: rotating or
revoking every key leaves them intact, and a new key for the same principal
regains access to its records.

`priorart admin` opens the SQLite database selected by `PRIORART_DATA_DIR` directly.
It requires host filesystem access and starts no network
listener. A fresh database creates `local-principal` and the restricted `local`
collection it owns. No credential is issued automatically.

```sh
priorart admin create-principal                                # prints the new principal ID
priorart admin create-collection --owner PRINCIPAL_ID          # restricted
priorart admin create-collection --owner PRINCIPAL_ID --visibility public
priorart admin issue --principal PRINCIPAL_ID --grant COLLECTION:write
```

`issue` writes one JSON object with `credential` metadata and a `secret` to stdout.
Store that secret securely; it cannot be retrieved again. The Rust issuance wrapper
is neither serializable nor cloneable, redacts `Debug`, and requires an explicit
consuming `into_secret()` call. If output is lost, rotate using the lookup ID from
`admin list`. Never put the bearer secret in command-line arguments.

`--principal` defaults to `local-principal`. `--expires-at UNIX_SECONDS` sets an
exclusive UTC expiry; omitting it creates a non-expiring key. The CLI requires at
least one explicit grant at issuance. There are no wildcard or implicit grants.

A key may hold:

- any grant on collections its principal owns;
- `read` or `write` on any public collection. Changing a record still requires
  its authorship, or `admin`, which only the owner can hold.

Any other grant fails with the same error as an unknown collection.

```sh
priorart admin list --principal PRINCIPAL_ID
priorart admin rotate --credential-id LOOKUP_ID
priorart admin revoke --credential-id LOOKUP_ID
priorart admin set-grants --credential-id LOOKUP_ID --grant local:read
```

`list` returns metadata, including grants, expiry, revocation timestamp, and grant
version. It never returns secrets or verifiers. Rotation atomically issues a new
ID/secret with the same principal, grants, and expiry and revokes the old key.
Revocation is permanent and idempotent. `set-grants` replaces the full grant set;
omitting `--grant` removes all permissions. Administration cannot rotate an
expired or revoked key; issue a new key instead.

Grants are `read`, `write`, and `admin`, each implying the ones before it. `write`
creates records and changes your own; `admin` is owner-level control: changing
anyone's records, export, collection deletion, and diagnostics.

## Remote administration

Setting `PRIORART_ADMIN_TOKEN` (32–256 visible ASCII characters) enables operator
endpoints under `/v1/admin`; without it they return `404`. They accept only
`Authorization: Bearer <admin token>`, compared in constant time. Collection keys
never open them, and the admin token is not a credential anywhere else. Secrets
are returned once, with `Cache-Control: no-store`.

| Request | Body | Response |
|---|---|---|
| `POST /v1/admin/principals` | none | `201 {"id"}` |
| `POST /v1/admin/collections` | `{"owner", "visibility"?}` (default `restricted`) | `201 {"id","visibility","created_at","owner_principal_id"}` |
| `POST /v1/admin/principals/{principal}/credentials` | `{"grants":[{"collection_id","operation"}], "expires_at"?}` | `201 {"credential","secret"}` |
| `GET /v1/admin/principals/{principal}/credentials` | none | `{"credentials":[…]}`, metadata only |
| `POST /v1/admin/credentials/{id}/rotate` | none | `201 {"credential","secret"}` |
| `PUT /v1/admin/credentials/{id}/grants` | `{"grants":[…]}`; empty removes all | `204` |
| `DELETE /v1/admin/credentials/{id}` | none | `204` |

The same grant rules apply as for the CLI. Unknown principals, unknown or revoked
credentials, and grants the principal may not hold return `404`. Grant replacement
bumps the key's grant version, so it applies to the next request. The operator
token is a bearer secret like any key: send it only over HTTPS or loopback.

## Contexts

Authentication and revalidation read one SQLite snapshot and check the key's
current grants, expiry, and revocation. Credential mutations run in immediate
transactions. Each credential has a grant version; every issuance, rotation, grant
replacement, or revocation also increments the principal's authentication version.
Saved contexts for sibling keys therefore become stale after administration, but
the sibling bearer keys remain valid and can authenticate again. There is no
authentication cache.

`Service::authenticate` mints a secret-free context with private fields and no
serialization support. `Service::validate_context` rejects a stale snapshot.
`has_grant` inspects only the snapshot; it is not an authorization check. The
service policy validates within every content operation and again after lengthy
work. See [service revocation boundaries](policy.md#revocation-indexes-and-concurrency).
The explicit local context is confined to `local`.

## Secret storage

The format is `pa1_<32-hex lookup ID>_<64-hex random secret>`. The lookup ID is
non-secret. The secret contains 256 random bits from the OS using
[`getrandom::fill`](https://docs.rs/getrandom/0.4.3/getrandom/fn.fill.html).
SQLite stores only a domain-separated SHA-256 verifier over the complete bearer
value. This is a random token, not a human password requiring a password KDF.
Verification compares fixed-size digests using
[`subtle::ConstantTimeEq`](https://docs.rs/subtle/2.6.1/subtle/trait.ConstantTimeEq.html),
including a dummy digest for unknown lookup IDs. Malformed, unknown, wrong,
expired, and revoked credentials have the same authentication error.

Revoked credential verifiers and metadata remain stored. Bearer secrets can exist
in the issuing caller's memory and explicit stdout output; they are not persisted
in SQLite, index files, context objects, or application logs.
