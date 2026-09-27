# Local credential administration

Credential lifecycle, [service policy](policy.md), and [HTTP header authentication](protocol.md)
are implemented. `PRIORART_MODE=authenticated` enables credential-based access with
anonymous public reads; default `local` mode uses the local principal when no key
is supplied. Invalid supplied keys always fail. Both modes are loopback-only;
hosted operation is disabled. MCP currently supports the local workflow only.

## Bootstrap and administration

`priorart admin` opens the SQLite database selected by `PRIORART_DATA_DIR` directly.
It requires host filesystem access and starts neither an encoder nor a network
listener. A fresh database creates the existing `local-principal`, `local-account`,
and restricted `local` collection. No credential is issued automatically.

Issue an agent key with explicit collection permissions:

```sh
priorart admin issue --grant local:read --grant local:contribute --grant local:report
```

The command writes one JSON object with `credential` metadata and a `secret` to
stdout. Store that secret securely; it cannot be retrieved again. The Rust issuance
wrapper is neither serializable nor cloneable, redacts `Debug`, and requires an
explicit consuming `into_secret()` call. If output is lost, rotate using the lookup
ID from `admin list`. Never put the bearer secret in command-line arguments.

Use `--principal PRINCIPAL_ID` to select another existing owner principal and
`--expires-at UNIX_SECONDS` for an exclusive UTC expiry. Omitting expiry creates a
non-expiring key. Issuance accepts only collections owned by the selected principal;
unknown collections and another principal's collections fail with the same error.
There are no wildcard or implicit grants. `read` covers record reads and search in
the service policy. The CLI requires at least one explicit grant at issuance.

```sh
priorart admin list
priorart admin rotate --credential-id LOOKUP_ID
priorart admin revoke --credential-id LOOKUP_ID
priorart admin set-grants --credential-id LOOKUP_ID --grant local:read
```

`list` returns metadata, including grants, expiry, parent ID, revocation timestamp,
and grant version. It never returns secrets or verifiers. Rotation atomically
issues a new ID/secret with the same principal, grants, parent, and expiry and
revokes the old key and its descendants. Revocation is permanent and idempotent.
`set-grants` replaces the full grant set and permanently revokes descendants;
omitting `--grant` removes all permissions. Restoring a parent's grants does not
restore revoked children. Local administration cannot rotate an expired or revoked
key; issue a new root key instead.

Operations are independent: `read`, `contribute`, `update`, `delete`, `report`,
`feedback_read`, `feedback_delete`, `report_publish`, `moderate`, `export`, `admin`,
and `delegate`. `admin` grants no implicit read, write, or delegation permission.
Routine keys should carry only their required operations; administrative and
delegation capabilities are explicit. None of these grants authorizes billing,
refills, or account-level collection creation.

## Delegation and contexts

The Rust `Store::delegate_credential` API requires an authenticated, revalidated
`RequestContext`. Agent keys retain the issuer's principal by default;
`Store::delegate_credential_to` selects another existing principal. A delegated grant must
be present on the issuer along with `delegate` on that same collection. The child
cannot outlive its parent; a finite parent expiry forbids an unbounded child expiry.
Credentials have at most 64 grants and at most eight levels including the root.
Issuer-chain checks apply across principals. Remote grant-management endpoints are
not implemented.

Authentication and revalidation read one SQLite snapshot and check every ancestor's
current grants, expiry, revocation, and root ownership. Credential mutations run
in immediate transactions, including version changes. Each credential has a grant
version; every issuance, rotation, grant replacement, or effective revocation also
increments the principal's authentication version. Thus saved contexts for sibling
keys become stale after administration, but the independent sibling bearer keys
remain valid and can authenticate again. There is no authentication cache.

`Service::authenticate` mints a secret-free context with private fields and no
serialization/deserialization support. `Service::validate_context` rejects a stale
snapshot. `has_grant` inspects only the snapshot; it is not an authorization check.
The service policy validates within every content operation and again after lengthy
work. See [service revocation boundaries](policy.md#revocation-indexes-and-concurrency)
for the final-check/commit boundary. The explicit local context is confined to
`local` and cannot be passed as an authenticated credential to delegation APIs.

## Secret storage and schema

The format is `pa1_<32-hex lookup ID>_<64-hex random secret>`. The lookup ID is
non-secret. The secret contains 256 random bits from the OS using
[`getrandom::fill`](https://docs.rs/getrandom/0.4.3/getrandom/fn.fill.html).
SQLite stores only a domain-separated SHA-256 verifier over the complete bearer
value. This is a random token, not a human password requiring a password KDF.
Verification compares fixed-size digests using
[`subtle::ConstantTimeEq`](https://docs.rs/subtle/2.6.1/subtle/trait.ConstantTimeEq.html),
including a dummy digest for unknown lookup IDs. This does not promise identical
total request timing for every invalid credential. Malformed, unknown, wrong,
expired, and revoked credentials have the same safe authentication error.

Schema `v0.3.0` adds credentials, collection/operation grants, and principal version
state through a transactional migration from `v0.2.0`. Existing records, revisions,
collections, and index mirrors remain intact. Revoked credential verifiers and
metadata remain stored; retention/purge work is deferred. Plaintext content storage
and the trusted-operator boundary are unchanged. Bearer secrets can still exist in
the issuing caller's memory and explicit stdout output; they are not persisted in
SQLite, index files, context objects, or application logs.
