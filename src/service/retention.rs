use super::*;
use crate::store::{FeedbackKind, RetentionJob, RetentionKind};

impl Service {
    pub fn delete_feedback(
        &self,
        context: &RequestContext,
        collection: &str,
        id: &str,
        kind: FeedbackKind,
    ) -> Result<()> {
        let handle = self.state(context, collection, Operation::FeedbackDelete)?;
        let state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let store = &state.store;
        policy::collection(store, context, collection, Operation::FeedbackDelete)?;
        let principal = context.principal_id().ok_or(PolicyError::Unavailable)?;
        let moderate = context.is_local() || context.has_grant(collection, Operation::Moderate);
        if !store.delete_feedback(collection, id, principal, kind, moderate)? {
            return Err(PolicyError::Unavailable.into());
        }
        policy::validate(store, context)?;
        Ok(())
    }

    pub fn create_retention_job(
        &self,
        context: &RequestContext,
        collection: &str,
        id: &str,
        kind: RetentionKind,
        before_unix: i64,
    ) -> Result<RetentionJob> {
        let handle = self.state(context, collection, Operation::Admin)?;
        let state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        policy::collection(&state.store, context, collection, Operation::Admin)?;
        if !is_record_id(id)
            || before_unix < 0
            || before_unix > time::OffsetDateTime::now_utc().unix_timestamp()
        {
            return invalid("job requires a valid id and a cutoff between the Unix epoch and now");
        }
        let mut job = state
            .store
            .create_retention_job(collection, id, kind, before_unix)?;
        if job.kind == RetentionKind::Revisions && state.store.needs_purge(collection)? {
            job.complete = false;
        }
        policy::validate(&state.store, context)?;
        Ok(job)
    }

    pub fn retention_job(
        &self,
        context: &RequestContext,
        collection: &str,
        id: &str,
    ) -> Result<RetentionJob> {
        let store = self.connect()?;
        policy::collection(&store, context, collection, Operation::Admin)?;
        let mut job = store
            .retention_job(collection, id)?
            .ok_or(PolicyError::Unavailable)?;
        if job.kind == RetentionKind::Revisions && store.needs_purge(collection)? {
            job.complete = false;
        }
        policy::validate(&store, context)?;
        Ok(job)
    }

    pub fn run_retention_batch(
        &self,
        context: &RequestContext,
        collection: &str,
        id: &str,
    ) -> Result<RetentionJob> {
        let handle = self.state(context, collection, Operation::Admin)?;
        let mut state = handle.lock().map_err(|_| ServiceError::Poisoned)?;
        let State { store, indexes } = &mut *state;
        policy::collection(store, context, collection, Operation::Admin)?;
        let job = store
            .retention_job(collection, id)?
            .ok_or(PolicyError::Unavailable)?;
        if matches!(
            job.kind,
            RetentionKind::Revisions | RetentionKind::Mutations
        ) && store.has_pending_mutations(collection)?
        {
            indexes.get(store, collection)?;
            store.complete_mutations(collection)?;
        }
        policy::collection(store, context, collection, Operation::Admin)?;
        let job = store.run_retention_batch(collection, id)?;
        indexes.finish_purge(store, collection)?;
        policy::validate(store, context)?;
        Ok(job)
    }
}
