//! Service policy, independent of HTTP. Unknown and forbidden collections share
//! one error, and no index may be loaded before this layer authorizes its scope.
use crate::auth::{AuthError, Operation, RequestContext};
use crate::store::{Collection, Store, StoreError, Visibility, LOCAL_COLLECTION_ID};

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("requested resource is unavailable")]
    Unavailable,
    #[error(transparent)]
    Authentication(#[from] AuthError),
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub type Result<T> = std::result::Result<T, PolicyError>;

pub(crate) fn validate(store: &Store, context: &RequestContext) -> Result<()> {
    if context.credential().is_some() {
        store.validate_context(context)?;
    }
    Ok(())
}

/// Even local authority is confined to its explicit collection, never all data.
pub(crate) fn collection(
    store: &Store,
    context: &RequestContext,
    id: &str,
    operation: Operation,
) -> Result<Collection> {
    validate(store, context)?;
    if context.is_local() && id != LOCAL_COLLECTION_ID {
        return Err(PolicyError::Unavailable);
    }
    let collection = match store.get_collection(id) {
        Ok(collection) => collection,
        Err(StoreError::CollectionNotFound(_)) => return Err(PolicyError::Unavailable),
        Err(error) => return Err(error.into()),
    };
    if permitted(context, &collection, operation) {
        Ok(collection)
    } else {
        Err(PolicyError::Unavailable)
    }
}

fn permitted(context: &RequestContext, collection: &Collection, operation: Operation) -> bool {
    if context.is_local() {
        return collection.id == LOCAL_COLLECTION_ID;
    }
    (operation == Operation::Read && collection.visibility == Visibility::Public)
        || context.has_grant(&collection.id, operation)
}

/// Updates/deletes require their explicit operation and authorship, or that
/// operation plus moderate. Ownership alone grants no credential permissions.
pub(crate) fn mutation(
    store: &Store,
    context: &RequestContext,
    id: &str,
    record: &str,
    operation: Operation,
) -> Result<()> {
    let collection = collection(store, context, id, operation)?;
    let author = store.record_author(id, record)?;
    if context.is_local()
        || permitted(context, &collection, Operation::Moderate)
        || author
            .as_ref()
            .is_some_and(|author| author.as_deref() == context.principal_id())
    {
        Ok(())
    } else {
        Err(PolicyError::Unavailable)
    }
}
