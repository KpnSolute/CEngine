//! Durable import jobs: types, the state machine and the `JobStore` contract (CEngine #103).
//!
//! The server owns a long, multi-step import as one job row with a checked state machine, so a
//! lost response, a closed tab or a restarted process no longer decides whether an import
//! finished. This module is storage-neutral; the Postgres implementation lives in `ce-store-pg`.

use crate::StoreError;
use serde_json::Value;

pub const STATES: &[&str] = &[
    "uploaded",
    "parsing",
    "needs_review",
    "validated",
    "queued",
    "processing",
    "committing",
    "completed",
    "retryable",
    "failed",
    "cancelled",
];

/// Where a job is in its life. `Completed`, `Failed` and `Cancelled` are final.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Uploaded,
    Parsing,
    NeedsReview,
    Validated,
    Queued,
    Processing,
    Committing,
    Completed,
    /// Stopped by a transient problem; may be queued again.
    Retryable,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Uploaded => "uploaded",
            JobState::Parsing => "parsing",
            JobState::NeedsReview => "needs_review",
            JobState::Validated => "validated",
            JobState::Queued => "queued",
            JobState::Processing => "processing",
            JobState::Committing => "committing",
            JobState::Completed => "completed",
            JobState::Retryable => "retryable",
            JobState::Failed => "failed",
            JobState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<JobState> {
        STATES.iter().position(|x| *x == s).map(|i| ALL[i])
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            JobState::Completed | JobState::Failed | JobState::Cancelled
        )
    }

    /// Whether a job may move from `self` to `to`. A job that is committing cannot be cancelled:
    /// its final post is one transaction that either lands or does not.
    pub fn can_move_to(self, to: JobState) -> bool {
        use JobState::*;
        matches!(
            (self, to),
            (Uploaded, Parsing | Cancelled)
                | (
                    Parsing,
                    NeedsReview | Validated | Retryable | Failed | Cancelled
                )
                | (NeedsReview, Validated | Cancelled)
                | (Validated, Queued | Cancelled)
                | (Queued, Processing | Cancelled)
                | (Processing, Committing | Retryable | Failed | Cancelled)
                | (Committing, Completed | Retryable | Failed)
                | (Retryable, Queued | Failed | Cancelled)
        )
    }
}

pub const ALL: [JobState; 11] = [
    JobState::Uploaded,
    JobState::Parsing,
    JobState::NeedsReview,
    JobState::Validated,
    JobState::Queued,
    JobState::Processing,
    JobState::Committing,
    JobState::Completed,
    JobState::Retryable,
    JobState::Failed,
    JobState::Cancelled,
];

#[derive(Debug, Clone, PartialEq)]
pub enum JobError {
    Store(StoreError),
    /// The idempotency key was used before for a different request.
    IdempotencyKeyReused,
    /// The scope already has a live job.
    ScopeBusy {
        job_id: String,
    },
    BadTransition {
        from: JobState,
        to: JobState,
    },
    /// The job is not in the state the caller expected.
    StateChanged {
        actual: JobState,
    },
    NotFound,
    /// The caller does not hold a live lease on the job.
    LeaseLost,
    /// A row was already staged at this position with different content.
    StagedMismatch {
        seq: i32,
    },
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobError::Store(e) => write!(f, "store error: {e:?}"),
            JobError::IdempotencyKeyReused => {
                write!(
                    f,
                    "this idempotency key was already used for a different job"
                )
            }
            JobError::ScopeBusy { job_id } => {
                write!(
                    f,
                    "another import for this scope is still live (job {job_id})"
                )
            }
            JobError::BadTransition { from, to } => {
                write!(
                    f,
                    "a job cannot move from {} to {}",
                    from.as_str(),
                    to.as_str()
                )
            }
            JobError::StateChanged { actual } => {
                write!(f, "the job is now {}", actual.as_str())
            }
            JobError::NotFound => write!(f, "no such job"),
            JobError::LeaseLost => write!(f, "the worker no longer holds this job's lease"),
            JobError::StagedMismatch { seq } => {
                write!(f, "row {seq} was already staged with different content")
            }
        }
    }
}

impl std::error::Error for JobError {}

impl From<StoreError> for JobError {
    fn from(e: StoreError) -> Self {
        JobError::Store(e)
    }
}

/// A job as stored. Times are milliseconds since the Unix epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    pub id: String,
    pub tenant_id: String,
    pub kind: String,
    pub scope_key: String,
    pub generation: i32,
    pub idempotency_key: String,
    pub fingerprint: String,
    pub file_sha256: Option<String>,
    pub upload_ref: Option<String>,
    pub actor: String,
    /// The role the actor held when the job was made; the final post runs with it.
    pub actor_role: Option<String>,
    pub state: JobState,
    pub total: i64,
    pub processed: i64,
    pub attempt: i32,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub result: Option<Value>,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
}

/// What the caller supplies to start a job.
#[derive(Debug, Clone)]
pub struct NewJob<'a> {
    pub tenant: &'a str,
    pub kind: &'a str,
    /// Names the thing being imported (for example `vendor/usfoods/invoice/511063`). At most one
    /// live job exists per scope.
    pub scope_key: &'a str,
    pub idempotency_key: &'a str,
    /// A hash of the request that matters (file hash, period, options). The same key with a
    /// different fingerprint is refused.
    pub fingerprint: &'a str,
    pub file_sha256: Option<&'a str>,
    pub upload_ref: Option<&'a str>,
    pub actor: &'a str,
    pub actor_role: Option<&'a str>,
    pub total: i64,
}

/// One entry of a job's transition log.
#[derive(Debug, Clone, PartialEq)]
pub struct JobEvent {
    pub from_state: Option<String>,
    pub to_state: String,
    pub detail: Value,
}

/// What a store must offer to run import jobs. Every method except `claim` takes the tenant id of
/// the caller's verified scope and treats another tenant's job as `NotFound`.
pub trait JobStore: Send + Sync {
    /// Starts a job in `uploaded`, or returns the one this key already started. The `bool` is
    /// true when a new job was made.
    fn create(&self, n: &NewJob<'_>) -> Result<(Job, bool), JobError>;
    fn get(&self, tenant: &str, id: &str) -> Result<Job, JobError>;
    /// The job's transition log, oldest first.
    fn events(&self, tenant: &str, id: &str) -> Result<Vec<JobEvent>, JobError>;
    /// Compare-and-set move; `error` is a code and message kept on the job.
    fn transition(
        &self,
        tenant: &str,
        id: &str,
        from: JobState,
        to: JobState,
        error: Option<(&str, &str)>,
    ) -> Result<Job, JobError>;
    /// Finishes a committing job and records its result.
    fn complete(&self, tenant: &str, id: &str, result: &Value) -> Result<Job, JobError>;
    /// Hands the oldest runnable job to `worker`. Serves every tenant.
    fn claim(
        &self,
        worker: &str,
        lease_secs: i64,
        max_attempts: i32,
    ) -> Result<Option<Job>, JobError>;
    fn heartbeat(
        &self,
        tenant: &str,
        id: &str,
        worker: &str,
        lease_secs: i64,
        processed: Option<i64>,
    ) -> Result<Job, JobError>;
    /// Stores rows against a job still being uploaded or parsed; a repeat adds nothing.
    fn stage_rows(&self, tenant: &str, id: &str, rows: &[(i32, Value)]) -> Result<u64, JobError>;
    /// Staged rows with `seq` above `after`, in order, at most `limit`.
    fn staged(
        &self,
        tenant: &str,
        id: &str,
        after: i32,
        limit: i64,
    ) -> Result<Vec<(i32, Value)>, JobError>;
    fn staged_count(&self, tenant: &str, id: &str) -> Result<i64, JobError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_round_trip_and_final_states_have_no_exits() {
        for s in ALL {
            assert_eq!(JobState::parse(s.as_str()), Some(s));
            if s.is_terminal() {
                assert!(
                    ALL.iter().all(|t| !s.can_move_to(*t)),
                    "{s:?} must be final"
                );
            }
        }
        assert_eq!(STATES.len(), ALL.len());
    }

    #[test]
    fn the_happy_path_is_allowed_and_shortcuts_are_not() {
        use JobState::*;
        let path = [
            Uploaded, Parsing, Validated, Queued, Processing, Committing, Completed,
        ];
        for w in path.windows(2) {
            assert!(w[0].can_move_to(w[1]), "{:?} -> {:?}", w[0], w[1]);
        }
        assert!(!Uploaded.can_move_to(Completed));
        assert!(!Validated.can_move_to(Processing));
        assert!(
            !Committing.can_move_to(Cancelled),
            "a commit in flight cannot be cancelled"
        );
        assert!(Retryable.can_move_to(Queued));
    }
}
