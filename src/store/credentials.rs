//! Credential transactions share the store connection and its caller's lock.
use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use time::OffsetDateTime;

use super::{new_id, Store, StoreError};
use crate::auth::{
    AuthError, CredentialInfo, Grant, IssuedCredential, Operation, RequestContext, Result,
    MAX_CREDENTIAL_DEPTH, MAX_GRANTS,
};

impl From<rusqlite::Error> for AuthError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(StoreError::Sqlite(error))
    }
}

fn now() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

impl Store {
    /// Host-operator bootstrap only. Not a remote issuance API. Root grants must
    /// name collections owned by this principal; billing authority is separate.
    pub fn issue_local_credential(
        &self,
        principal: &str,
        grants: &[Grant],
        expires_at: Option<i64>,
    ) -> Result<IssuedCredential> {
        let transaction = self.write()?;
        let grants = validate_grants(grants)?;
        require_owned(&transaction, principal, &grants)?;
        let issued = insert(&transaction, principal, None, grants, expires_at, now())?;
        transaction.commit()?;
        Ok(issued)
    }

    /// A secret-free context. Malformed, unknown, incorrect, expired, and revoked
    /// credentials use the same error. No bearer value is retained or logged.
    pub fn authenticate(&self, bearer: &str) -> Result<RequestContext> {
        let id = parse_bearer(bearer)?;
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let stored: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT verifier FROM credentials WHERE id = ?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let expected: [u8; 32] = stored
            .as_deref()
            .and_then(|v| v.try_into().ok())
            .unwrap_or([0; 32]);
        let actual = verifier(bearer);
        let matches = bool::from(actual.ct_eq(&expected));
        if !matches || stored.is_none() {
            return Err(AuthError::Unauthenticated);
        }
        let info = active(&transaction, id, now())?.0;
        let context = context(&transaction, info)?;
        transaction.commit()?;
        Ok(context)
    }

    /// Recheck current state (including ancestors) in a single read snapshot.
    /// Stale contexts fail; callers must authenticate again to obtain new grants.
    pub fn validate_context(&self, request: &RequestContext) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        validate_context(&transaction, request, now())?;
        transaction.commit()?;
        Ok(())
    }

    /// Agent keys share the issuer's principal. Cross-principal grant management
    /// and account/billing delegation are deliberately outside this step.
    pub fn delegate_credential(
        &self,
        issuer: &RequestContext,
        grants: &[Grant],
        expires_at: Option<i64>,
    ) -> Result<IssuedCredential> {
        let transaction = self.write()?;
        let timestamp = now();
        let (parent, depth) = validate_context(&transaction, issuer, timestamp)?;
        let grants = validate_grants(grants)?;
        if grants.is_empty() || depth >= MAX_CREDENTIAL_DEPTH {
            return Err(AuthError::Forbidden);
        }
        require_subset(&parent, &grants, expires_at)?;
        let issued = insert(
            &transaction,
            &parent.principal_id,
            Some(&parent.id),
            grants,
            expires_at,
            timestamp,
        )?;
        transaction.commit()?;
        Ok(issued)
    }

    /// Local inspection exposes no verifier or secret. Revoked records remain
    /// visible here for administration; this is not a remote listing endpoint.
    pub fn local_credentials(&self, principal: &str) -> Result<Vec<CredentialInfo>> {
        let transaction =
            Transaction::new_unchecked(&self.connection, TransactionBehavior::Deferred)?;
        let ids = transaction
            .prepare("SELECT id FROM credentials WHERE principal_id = ?1 ORDER BY created_at, id")?
            .query_map([principal], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let result = ids
            .iter()
            .map(|id| info(&transaction, id))
            .collect::<Result<Vec<_>>>()?;
        transaction.commit()?;
        Ok(result)
    }

    /// Immediately and permanently revoke the credential and its descendants.
    pub fn revoke_local_credential(&self, id: &str) -> Result<()> {
        let transaction = self.write()?;
        let credential = info(&transaction, id)?;
        if credential.revoked_at.is_none() {
            revoke_tree(&transaction, id, now())?;
            bump_principal(&transaction, &credential.principal_id)?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Rotation issues a fresh lookup ID and secret with exactly the same scope,
    /// parent, and expiry, and revokes the old credential subtree atomically.
    pub fn rotate_local_credential(&self, id: &str) -> Result<IssuedCredential> {
        let transaction = self.write()?;
        let timestamp = now();
        let old = active(&transaction, id, timestamp)?.0;
        let issued = insert(
            &transaction,
            &old.principal_id,
            old.parent_id.as_deref(),
            old.grants,
            old.expires_at,
            timestamp,
        )?;
        revoke_tree(&transaction, id, timestamp)?;
        transaction.commit()?;
        Ok(issued)
    }

    /// Explicit local grant replacement increments versions and revokes existing
    /// descendants. Empty grants disable all operations; there is no wildcard.
    pub fn replace_local_credential_grants(&self, id: &str, grants: &[Grant]) -> Result<()> {
        let transaction = self.write()?;
        let timestamp = now();
        let old = active(&transaction, id, timestamp)?.0;
        let grants = validate_grants(grants)?;
        if let Some(parent) = &old.parent_id {
            require_subset(
                &active(&transaction, parent, timestamp)?.0,
                &grants,
                old.expires_at,
            )?;
        } else {
            require_owned(&transaction, &old.principal_id, &grants)?;
        }
        transaction.execute(
            "DELETE FROM credential_grants WHERE credential_id = ?1",
            [id],
        )?;
        write_grants(&transaction, id, &grants)?;
        transaction.execute(
            "UPDATE credentials SET grant_version = grant_version + 1 WHERE id = ?1",
            [id],
        )?;
        // The parent remains usable but delegated authority cannot reappear if
        // its grants are subsequently expanded again.
        transaction.execute(
            "WITH RECURSIVE descendants(id) AS (
                SELECT id FROM credentials WHERE parent_id = ?1
                UNION ALL SELECT c.id FROM credentials c JOIN descendants d ON c.parent_id = d.id
             ) UPDATE credentials SET revoked_at = ?2, grant_version = grant_version + 1
             WHERE id IN (SELECT id FROM descendants) AND revoked_at IS NULL",
            (id, timestamp),
        )?;
        bump_principal(&transaction, &old.principal_id)?;
        transaction.commit()?;
        Ok(())
    }
}

fn validate_grants(grants: &[Grant]) -> Result<BTreeSet<Grant>> {
    if grants.len() > MAX_GRANTS || grants.iter().any(|g| g.collection_id.is_empty()) {
        return Err(AuthError::InvalidInput(
            "credentials require explicit scopes and at most 64 grants",
        ));
    }
    Ok(grants.iter().cloned().collect())
}

fn require_owned(connection: &Connection, principal: &str, grants: &BTreeSet<Grant>) -> Result<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM principals WHERE id = ?1)",
        [principal],
        |r| r.get(0),
    )?;
    if !exists {
        return Err(AuthError::Forbidden);
    }
    for grant in grants {
        let owns: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM collections c JOIN accounts a ON a.id = c.owner_account_id
             WHERE c.id = ?1 AND a.owner_principal_id = ?2)",
            (&grant.collection_id, principal),
            |r| r.get(0),
        )?;
        if !owns {
            return Err(AuthError::Forbidden);
        }
    }
    Ok(())
}

fn require_subset(
    parent: &CredentialInfo,
    grants: &BTreeSet<Grant>,
    expiry: Option<i64>,
) -> Result<()> {
    if parent
        .expires_at
        .is_some_and(|end| expiry.is_none_or(|child| child > end))
        || grants.iter().any(|g| {
            !parent.grants.contains(g)
                || !parent
                    .grants
                    .contains(&Grant::new(&g.collection_id, Operation::Delegate))
        })
    {
        return Err(AuthError::Forbidden);
    }
    Ok(())
}

fn info(connection: &Connection, id: &str) -> Result<CredentialInfo> {
    let mut info = connection.query_row(
        "SELECT id, principal_id, parent_id, created_at, expires_at, revoked_at, grant_version FROM credentials WHERE id = ?1", [id],
        |r| Ok(CredentialInfo {
            id: r.get(0)?, principal_id: r.get(1)?, parent_id: r.get(2)?, created_at: r.get(3)?,
            expires_at: r.get(4)?, revoked_at: r.get(5)?, grant_version: r.get(6)?, grants: BTreeSet::new(),
        }),
    ).optional()?.ok_or(AuthError::Unauthenticated)?;
    let mut statement = connection.prepare("SELECT collection_id, operation FROM credential_grants WHERE credential_id = ?1 ORDER BY collection_id, operation")?;
    let rows = statement.query_map([id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (collection, operation) = row?;
        info.grants
            .insert(Grant::new(collection, operation.parse()?));
    }
    Ok(info)
}

/// All ancestors must still authorize the complete child scope. Bounded traversal
/// also fails closed on cycles or corrupted/deeper-than-supported hierarchies.
fn active(connection: &Connection, id: &str, timestamp: i64) -> Result<(CredentialInfo, usize)> {
    let leaf = info(connection, id)?;
    let mut current = leaf.clone();
    for depth in 1..=MAX_CREDENTIAL_DEPTH {
        if current.revoked_at.is_some() || current.expires_at.is_some_and(|end| end <= timestamp) {
            return Err(AuthError::Unauthenticated);
        }
        match current.parent_id.as_deref() {
            None => {
                require_owned(connection, &current.principal_id, &current.grants).map_err(
                    |error| match error {
                        AuthError::Forbidden => AuthError::Unauthenticated,
                        other => other,
                    },
                )?;
                return Ok((leaf, depth));
            }
            Some(parent_id) => {
                let parent = info(connection, parent_id)?;
                if parent.principal_id != current.principal_id {
                    return Err(AuthError::Unauthenticated);
                }
                require_subset(&parent, &current.grants, current.expires_at)
                    .map_err(|_| AuthError::Unauthenticated)?;
                current = parent;
            }
        }
    }
    Err(AuthError::Unauthenticated)
}

fn context(connection: &Connection, credential: CredentialInfo) -> Result<RequestContext> {
    let version = connection.query_row(
        "SELECT version FROM principal_auth_state WHERE principal_id = ?1",
        [&credential.principal_id],
        |r| r.get(0),
    )?;
    Ok(RequestContext {
        credential: Some(credential),
        principal_version: version,
    })
}

fn validate_context(
    connection: &Connection,
    request: &RequestContext,
    timestamp: i64,
) -> Result<(CredentialInfo, usize)> {
    let id = &request
        .credential
        .as_ref()
        .ok_or(AuthError::Unauthenticated)?
        .id;
    let (credential, depth) = active(connection, id, timestamp)?;
    if context(connection, credential.clone())? != *request {
        return Err(AuthError::Unauthenticated);
    }
    Ok((credential, depth))
}

fn insert(
    transaction: &Transaction<'_>,
    principal: &str,
    parent: Option<&str>,
    grants: BTreeSet<Grant>,
    expires_at: Option<i64>,
    timestamp: i64,
) -> Result<IssuedCredential> {
    if expires_at.is_some_and(|end| end <= timestamp) {
        return Err(AuthError::InvalidInput(
            "credential expiry must be in the future",
        ));
    }
    let id = new_id();
    let mut random = [0u8; 32];
    getrandom::fill(&mut random).map_err(|_| AuthError::Entropy)?;
    let secret = format!(
        "pa1_{id}_{}",
        random
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    );
    transaction.execute(
        "INSERT INTO credentials (id, principal_id, parent_id, verifier, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        (
            &id,
            principal,
            parent,
            verifier(&secret).as_slice(),
            timestamp,
            expires_at,
        ),
    )?;
    write_grants(transaction, &id, &grants)?;
    bump_principal(transaction, principal)?;
    Ok(IssuedCredential {
        info: info(transaction, &id)?,
        secret,
    })
}

fn write_grants(connection: &Connection, id: &str, grants: &BTreeSet<Grant>) -> Result<()> {
    let mut statement = connection.prepare("INSERT INTO credential_grants VALUES (?1, ?2, ?3)")?;
    for grant in grants {
        statement.execute((id, &grant.collection_id, grant.operation.as_str()))?;
    }
    Ok(())
}

fn bump_principal(connection: &Connection, principal: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO principal_auth_state VALUES (?1, 1)
        ON CONFLICT(principal_id) DO UPDATE SET version = version + 1",
        [principal],
    )?;
    Ok(())
}

fn revoke_tree(connection: &Connection, id: &str, timestamp: i64) -> Result<()> {
    connection.execute(
        "WITH RECURSIVE descendants(id) AS (
            SELECT id FROM credentials WHERE id = ?1
            UNION ALL SELECT c.id FROM credentials c JOIN descendants d ON c.parent_id = d.id
         ) UPDATE credentials SET revoked_at = ?2, grant_version = grant_version + 1
         WHERE id IN (SELECT id FROM descendants) AND revoked_at IS NULL",
        (id, timestamp),
    )?;
    Ok(())
}

fn verifier(bearer: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"priorart-credential-v1\0");
    hash.update(bearer.as_bytes());
    hash.finalize().into()
}

fn parse_bearer(bearer: &str) -> Result<&str> {
    // ASCII, exact length, no whitespace, URL, or header interpretation.
    let Some(value) = bearer.strip_prefix("pa1_").filter(|s| s.len() == 97) else {
        return Err(AuthError::Unauthenticated);
    };
    let Some((id, secret)) = value.split_once('_') else {
        return Err(AuthError::Unauthenticated);
    };
    let hex = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if id.len() != 32 || secret.len() != 64 || !hex(id) || !hex(secret) {
        return Err(AuthError::Unauthenticated);
    }
    Ok(id)
}
