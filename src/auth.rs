//! Opaque credentials and secret-free request identity. Content policy is a
//! separate layer: possession of a context alone does not authorize an operation.
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::store::{StoreError, LOCAL_PRINCIPAL_ID};

pub const MAX_GRANTS: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid or expired credential")]
    Unauthenticated,
    #[error("credential authority does not permit this action")]
    Forbidden,
    #[error("{0}")]
    InvalidInput(&'static str),
    #[error("credential storage operation failed")]
    Storage(#[from] StoreError),
    #[error("secure credential generation failed")]
    Entropy,
}

pub type Result<T> = std::result::Result<T, AuthError>;

/// Collection capabilities, each implying the ones before it. `write` covers
/// creating records and changing your own; `admin` is owner-level control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    Read,
    Write,
    Admin,
}

impl Operation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Admin => "admin",
        }
    }
}

impl FromStr for Operation {
    type Err = AuthError;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "read" => Ok(Self::Read),
            "write" => Ok(Self::Write),
            "admin" => Ok(Self::Admin),
            _ => Err(AuthError::InvalidInput("unknown credential operation")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Grant {
    pub collection_id: String,
    pub operation: Operation,
}

impl Grant {
    pub fn new(collection_id: impl Into<String>, operation: Operation) -> Self {
        Self {
            collection_id: collection_id.into(),
            operation,
        }
    }
}

impl FromStr for Grant {
    type Err = AuthError;
    fn from_str(value: &str) -> Result<Self> {
        let (collection, operation) = value.rsplit_once(':').ok_or(AuthError::InvalidInput(
            "grant must be COLLECTION:OPERATION",
        ))?;
        if collection.is_empty() {
            return Err(AuthError::InvalidInput(
                "grant requires an explicit collection",
            ));
        }
        Ok(Self::new(collection, operation.parse()?))
    }
}

/// Public credential metadata. Neither verifier nor bearer secret is exposed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CredentialInfo {
    pub id: String,
    pub principal_id: String,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub grant_version: i64,
    pub grants: BTreeSet<Grant>,
}

/// Returned only by issuance/rotation. Deliberately not Clone or Serialize.
/// Explicit extraction consumes the wrapper; Debug never prints the secret.
pub struct IssuedCredential {
    pub info: CredentialInfo,
    pub(crate) secret: String,
}

impl IssuedCredential {
    pub fn into_secret(self) -> String {
        self.secret
    }
}

impl fmt::Debug for IssuedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedCredential")
            .field("info", &self.info)
            .field("secret", &"[REDACTED]")
            .finish()
    }
}

/// In-process identity snapshot, minted by authentication, never from JSON.
/// Must be revalidated against storage before using its grants or cache entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestContext {
    pub(crate) credential: Option<CredentialInfo>,
    pub(crate) principal_version: i64,
    pub(crate) local: bool,
}

impl RequestContext {
    /// Explicit trusted local mode. Policy confines this identity to `local`.
    pub const fn local() -> Self {
        Self {
            credential: None,
            principal_version: 0,
            local: true,
        }
    }
    /// Anonymous callers can read public collections only.
    pub const fn anonymous() -> Self {
        Self {
            credential: None,
            principal_version: 0,
            local: false,
        }
    }
    pub fn principal_id(&self) -> Option<&str> {
        self.credential
            .as_ref()
            .map(|c| c.principal_id.as_str())
            .or_else(|| self.local.then_some(LOCAL_PRINCIPAL_ID))
    }
    pub(crate) fn is_local(&self) -> bool {
        self.local
    }
    pub fn credential(&self) -> Option<&CredentialInfo> {
        self.credential.as_ref()
    }
    pub fn principal_version(&self) -> i64 {
        self.principal_version
    }
    /// Snapshot only; not a substitute for service policy or revalidation.
    pub fn has_grant(&self, collection: &str, operation: Operation) -> bool {
        self.credential
            .as_ref()
            .is_some_and(|c| c.grants.contains(&Grant::new(collection, operation)))
    }
}
