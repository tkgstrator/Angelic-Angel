//! Bulk "enable device notifications for everyone I follow" job.
//!
//! One owned async task at a time. It first loads the complete following list,
//! then updates accounts strictly one after another. It never touches the saved
//! configuration or the push registration, and it only reports fixed messages.

use super::error::ApiError;
use super::service::Backend;
use crate::config::TwitterConfig;
use crate::twitter::XError;
use serde::Serialize;
use std::collections::HashSet;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const MAX_DURATION: Duration = Duration::from_secs(24 * 60 * 60);
const SHUTDOWN_JOIN: Duration = Duration::from_secs(5);
const MAX_PAGES: usize = 200;
const FIRST_CURSOR: i64 = -1;
const LAST_CURSOR: i64 = 0;

#[derive(Debug, Clone, Copy)]
pub struct Timings {
    pub request_timeout: Duration,
    pub interval: Duration,
    pub max_duration: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            request_timeout: REQUEST_TIMEOUT,
            interval: REQUEST_INTERVAL,
            max_duration: MAX_DURATION,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Idle,
    Loading,
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Loading | Self::Running | Self::Cancelling)
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FollowingNotificationsDto {
    pub state: JobState,
    pub total: Option<u64>,
    pub success_count: u64,
    pub failed_count: u64,
    pub remaining_count: Option<u64>,
    pub last_error: Option<String>,
}

struct Progress {
    /// Identifies the job these numbers belong to; a stale task can never write into a newer one.
    generation: u64,
    state: JobState,
    total: Option<u64>,
    success: u64,
    failed: u64,
    last_error: Option<String>,
}

impl Progress {
    fn with_state(state: JobState, generation: u64) -> Self {
        Self {
            generation,
            state,
            total: None,
            success: 0,
            failed: 0,
            last_error: None,
        }
    }
}

enum Outcome {
    Completed,
    Cancelled,
    Failed(String),
}

struct Shared {
    progress: Mutex<Progress>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            progress: Mutex::new(Progress::with_state(JobState::Idle, 0)),
        }
    }
}

impl Shared {
    fn with<R>(&self, f: impl FnOnce(&mut Progress) -> R) -> R {
        f(&mut self.progress.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn snapshot(&self) -> FollowingNotificationsDto {
        self.with(|p| FollowingNotificationsDto {
            state: p.state,
            total: p.total,
            success_count: p.success,
            failed_count: p.failed,
            remaining_count: p.total.map(|t| t.saturating_sub(p.success + p.failed)),
            last_error: p.last_error.clone(),
        })
    }

    fn begin(&self, generation: u64, total: u64) {
        self.with(|p| {
            if p.generation != generation {
                return;
            }
            p.total = Some(total);
            if p.state == JobState::Loading {
                p.state = JobState::Running;
            }
        });
    }

    fn record_success(&self, generation: u64) {
        self.with(|p| {
            if p.generation == generation {
                p.success += 1;
            }
        });
    }

    fn record_failure(&self, generation: u64, error: &str) {
        self.with(|p| {
            if p.generation == generation {
                p.failed += 1;
                p.last_error = Some(error.to_string());
            }
        });
    }

    fn finish(&self, generation: u64, outcome: Outcome) {
        self.with(|p| {
            if p.generation != generation || !p.state.is_active() {
                return;
            }
            let cancel_requested = p.state == JobState::Cancelling;
            match outcome {
                Outcome::Completed => p.state = JobState::Completed,
                Outcome::Cancelled => p.state = JobState::Cancelled,
                Outcome::Failed(error) => {
                    p.state = if cancel_requested {
                        JobState::Cancelled
                    } else {
                        JobState::Failed
                    };
                    p.last_error = Some(error);
                },
            }
        });
    }
}

/// Settles the visible state if the task ends without reporting (panic or abort).
struct FinalizeGuard {
    shared: Arc<Shared>,
    generation: u64,
}

impl Drop for FinalizeGuard {
    fn drop(&mut self) {
        self.shared.finish(
            self.generation,
            Outcome::Failed("The job stopped unexpectedly.".into()),
        );
    }
}

struct Task {
    cancel: watch::Sender<bool>,
    handle: JoinHandle<()>,
}

pub struct FollowingJobs {
    shared: Arc<Shared>,
    task: Mutex<Option<Task>>,
    timings: Timings,
}

impl Default for FollowingJobs {
    fn default() -> Self {
        Self::new(Timings::default())
    }
}

impl FollowingJobs {
    pub fn new(timings: Timings) -> Self {
        Self {
            shared: Arc::new(Shared::default()),
            task: Mutex::new(None),
            timings,
        }
    }

    pub fn snapshot(&self) -> FollowingNotificationsDto {
        self.shared.snapshot()
    }

    pub fn is_active(&self) -> bool {
        self.shared.with(|p| p.state.is_active())
    }

    pub fn start<B: Backend>(
        &self,
        backend: Arc<B>,
        twitter: TwitterConfig,
    ) -> Result<FollowingNotificationsDto, ApiError> {
        let mut task = self.task.lock().unwrap_or_else(|e| e.into_inner());
        let generation = self.shared.with(|p| {
            if p.state.is_active() {
                return Err(ApiError::conflict(
                    "following_job_active",
                    "A bulk notification job is already in progress.",
                ));
            }
            let generation = p.generation + 1;
            *p = Progress::with_state(JobState::Loading, generation);
            Ok(generation)
        })?;

        let (cancel, cancelled) = watch::channel(false);
        let guard = FinalizeGuard {
            shared: Arc::clone(&self.shared),
            generation,
        };
        let shared = Arc::clone(&self.shared);
        let timings = self.timings;
        let handle = tokio::spawn(async move {
            let _guard = guard;
            let job = Job {
                generation,
                shared: &shared,
                timings,
            };
            let outcome = job.execute(&*backend, &twitter, cancelled).await;
            shared.finish(generation, outcome);
        });
        *task = Some(Task { cancel, handle });
        Ok(self.shared.snapshot())
    }

    /// Requests cancellation of an active job; a settled job is left as it is.
    pub fn cancel(&self) -> (bool, FollowingNotificationsDto) {
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner());
        let requested = self.shared.with(|p| {
            let active = p.state.is_active();
            if active {
                p.state = JobState::Cancelling;
            }
            active
        });
        if requested && let Some(task) = task.as_ref() {
            let _ = task.cancel.send(true);
        }
        (requested, self.shared.snapshot())
    }

    /// Cancels the job and waits for the task to end.
    pub async fn shutdown(&self) {
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        let Some(Task { cancel, mut handle }) = task else {
            return;
        };
        self.shared.with(|p| {
            if p.state.is_active() {
                p.state = JobState::Cancelling;
            }
        });
        let _ = cancel.send(true);
        if tokio::time::timeout(SHUTDOWN_JOIN, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
        }
    }
}

// ---- job body ---------------------------------------------------------------

enum Interrupt {
    Cancelled,
    Timeout,
    Api(XError),
}

impl Interrupt {
    fn into_outcome(self) -> Outcome {
        match self {
            Self::Cancelled => Outcome::Cancelled,
            Self::Timeout => Outcome::Failed("X did not respond in time.".into()),
            Self::Api(error) => Outcome::Failed(error.to_string()),
        }
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    // A dropped sender also means nobody wants the result any more.
    let _ = cancel.wait_for(|c| *c).await.map(|_| ());
}

/// Runs one remote call, bounded by the per-request timeout and cancellation.
async fn guarded<T>(
    cancel: &mut watch::Receiver<bool>,
    limit: Duration,
    call: impl Future<Output = Result<T, XError>>,
) -> Result<T, Interrupt> {
    tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(Interrupt::Cancelled),
        result = tokio::time::timeout(limit, call) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(Interrupt::Api(error)),
            Err(_) => Err(Interrupt::Timeout),
        },
    }
}

async fn pause(cancel: &mut watch::Receiver<bool>, duration: Duration) -> Result<(), Interrupt> {
    tokio::select! {
        biased;
        _ = cancelled(cancel) => Err(Interrupt::Cancelled),
        _ = tokio::time::sleep(duration) => Ok(()),
    }
}

fn time_limit() -> Outcome {
    Outcome::Failed("The time limit was reached before every account was updated.".into())
}

struct Job<'a> {
    generation: u64,
    shared: &'a Shared,
    timings: Timings,
}

impl Job<'_> {
    async fn load_following<B: Backend>(
        &self,
        backend: &B,
        twitter: &TwitterConfig,
        deadline: Instant,
        cancel: &mut watch::Receiver<bool>,
    ) -> Result<Vec<String>, Outcome> {
        let timings = self.timings;
        let screen_name = guarded(
            cancel,
            timings.request_timeout,
            backend.screen_name(twitter),
        )
        .await
        .map_err(Interrupt::into_outcome)?;

        let mut ids = Vec::new();
        let mut seen_ids = HashSet::new();
        let mut seen_cursors = HashSet::new();
        let mut cursor = FIRST_CURSOR;
        for page_number in 0..MAX_PAGES {
            if Instant::now() >= deadline {
                return Err(time_limit());
            }
            if !seen_cursors.insert(cursor) {
                return Err(Outcome::Failed(
                    "X returned an inconsistent following list.".into(),
                ));
            }
            if page_number > 0 {
                pause(cancel, timings.interval)
                    .await
                    .map_err(Interrupt::into_outcome)?;
            }
            let page = guarded(
                cancel,
                timings.request_timeout,
                backend.following_page(twitter, &screen_name, cursor),
            )
            .await
            .map_err(Interrupt::into_outcome)?;
            for id in page.ids {
                if seen_ids.insert(id.clone()) {
                    ids.push(id);
                }
            }
            if page.next_cursor == LAST_CURSOR {
                return Ok(ids);
            }
            cursor = page.next_cursor;
        }
        Err(Outcome::Failed("The following list is too large.".into()))
    }

    async fn execute<B: Backend>(
        &self,
        backend: &B,
        twitter: &TwitterConfig,
        mut cancel: watch::Receiver<bool>,
    ) -> Outcome {
        let timings = self.timings;
        let deadline = Instant::now() + timings.max_duration;
        let ids = match self
            .load_following(backend, twitter, deadline, &mut cancel)
            .await
        {
            Ok(ids) => ids,
            Err(outcome) => return outcome,
        };
        self.shared.begin(self.generation, ids.len() as u64);

        for id in &ids {
            if let Err(interrupt) = pause(&mut cancel, timings.interval).await {
                return interrupt.into_outcome();
            }
            if Instant::now() >= deadline {
                return time_limit();
            }
            let result = guarded(
                &mut cancel,
                timings.request_timeout,
                backend.enable_device_notifications(twitter, id),
            )
            .await;
            match result {
                Ok(()) => self.shared.record_success(self.generation),
                Err(Interrupt::Cancelled) => return Outcome::Cancelled,
                Err(Interrupt::Api(error)) if error.is_fatal() => {
                    return Interrupt::Api(error).into_outcome();
                },
                Err(Interrupt::Api(_)) => self
                    .shared
                    .record_failure(self.generation, "Some accounts could not be updated."),
                Err(Interrupt::Timeout) => self
                    .shared
                    .record_failure(self.generation, "A request to X timed out."),
            }
        }
        Outcome::Completed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::twitter::FollowingPage;
    use crate::web::service::fake::FakeBackend;
    use std::sync::atomic::Ordering;

    fn timings() -> Timings {
        Timings {
            request_timeout: Duration::from_millis(150),
            interval: Duration::from_millis(1),
            max_duration: Duration::from_secs(30),
        }
    }

    fn twitter() -> TwitterConfig {
        TwitterConfig {
            auth_token: "AUTH-SECRET".into(),
            ct0: "CT0-SECRET".into(),
        }
    }

    fn page(ids: &[&str], next: i64) -> Result<FollowingPage, XError> {
        Ok(FollowingPage {
            ids: ids.iter().map(|s| s.to_string()).collect(),
            next_cursor: next,
        })
    }

    // `Backend` is implemented for `Arc<FakeBackend>`, and `start` takes `Arc<B>`.
    #[allow(clippy::redundant_allocation)]
    fn fake_with(pages: Vec<(i64, Result<FollowingPage, XError>)>) -> Arc<Arc<FakeBackend>> {
        let fake = Arc::new(FakeBackend::default());
        fake.pages.lock().unwrap().extend(pages);
        Arc::new(fake)
    }

    #[allow(clippy::redundant_allocation)]
    fn three_accounts() -> Arc<Arc<FakeBackend>> {
        fake_with(vec![(-1, page(&["1", "2", "3"], 0))])
    }

    async fn until(what: &str, mut done: impl FnMut() -> bool) {
        for _ in 0..500 {
            if done() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("timed out waiting for {what}");
    }

    async fn settle(jobs: &FollowingJobs) -> FollowingNotificationsDto {
        until("the job to settle", || !jobs.is_active()).await;
        jobs.snapshot()
    }

    #[test]
    fn default_limits_allow_a_full_5000_account_run() {
        let t = Timings::default();
        assert_eq!(t.request_timeout, Duration::from_secs(30));
        assert_eq!(t.interval, Duration::from_secs(1));
        assert_eq!(t.max_duration, Duration::from_secs(24 * 60 * 60));
        let worst_case_5000 = t.interval * 5000 + t.request_timeout;
        assert!(worst_case_5000 < t.max_duration);
    }

    #[tokio::test]
    async fn idle_snapshot_has_exact_shape() {
        let jobs = FollowingJobs::new(timings());
        assert_eq!(
            serde_json::to_value(jobs.snapshot()).unwrap(),
            serde_json::json!({
                "state": "idle",
                "total": null,
                "success_count": 0,
                "failed_count": 0,
                "remaining_count": null,
                "last_error": null,
            })
        );
        let (requested, snapshot) = jobs.cancel();
        assert!(!requested);
        assert_eq!(snapshot.state, JobState::Idle);
    }

    #[tokio::test]
    async fn pages_are_merged_deduplicated_and_applied_in_order() {
        let fake = fake_with(vec![
            (-1, page(&["1", "2", "2"], 10)),
            (10, page(&["2", "3", "1"], 20)),
            (20, page(&["4"], 0)),
        ]);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;

        assert_eq!(done.state, JobState::Completed);
        assert_eq!(done.total, Some(4));
        assert_eq!(done.success_count, 4);
        assert_eq!(done.failed_count, 0);
        assert_eq!(done.remaining_count, Some(0));
        assert_eq!(done.last_error, None);
        assert_eq!(*fake.page_calls.lock().unwrap(), [-1, 10, 20]);
        assert_eq!(*fake.updated.lock().unwrap(), ["1", "2", "3", "4"]);
        assert_eq!(fake.max_updates_in_flight.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn nothing_is_changed_until_every_page_is_loaded() {
        let fake = fake_with(vec![
            (-1, page(&["1", "2"], 10)),
            (10, Err(XError::Status(500))),
        ]);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;

        assert_eq!(done.state, JobState::Failed);
        assert_eq!(done.total, None);
        assert_eq!(done.remaining_count, None);
        assert_eq!(done.success_count, 0);
        assert!(done.last_error.unwrap().contains("500"));
        assert!(fake.updated.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeating_cursor_fails_without_changing_anything() {
        let fake = fake_with(vec![
            (-1, page(&["1"], 5)),
            (5, page(&["2"], 7)),
            (7, page(&["3"], 5)),
        ]);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;

        assert_eq!(done.state, JobState::Failed);
        assert_eq!(done.total, None);
        assert!(fake.updated.lock().unwrap().is_empty());
        assert_eq!(*fake.page_calls.lock().unwrap(), [-1, 5, 7]);
    }

    #[tokio::test]
    async fn endless_pagination_is_bounded() {
        let fake = Arc::new(FakeBackend::default());
        {
            let mut pages = fake.pages.lock().unwrap();
            pages.insert(-1, page(&["1"], 1));
            for cursor in 1..=(MAX_PAGES as i64 + 5) {
                pages.insert(cursor, page(&["1"], cursor + 1));
            }
        }
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::new(fake), twitter()).unwrap();
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(done.total, None);
    }

    #[tokio::test]
    async fn loading_reports_unknown_total_and_cancel_is_honest() {
        let fake = three_accounts();
        fake.hang_pages.store(true, Ordering::SeqCst);
        let jobs = FollowingJobs::new(timings());
        let started = jobs.start(Arc::clone(&fake), twitter()).unwrap();
        assert_eq!(started.state, JobState::Loading);
        until("the first page request", || {
            !fake.page_calls.lock().unwrap().is_empty()
        })
        .await;
        let loading = jobs.snapshot();
        assert_eq!(loading.state, JobState::Loading);
        assert_eq!((loading.total, loading.remaining_count), (None, None));

        let (requested, cancelling) = jobs.cancel();
        assert!(requested);
        assert_eq!(cancelling.state, JobState::Cancelling);
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Cancelled);
        assert_eq!((done.total, done.remaining_count), (None, None));
        assert!(fake.updated.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn page_request_timeout_fails_the_job_before_any_change() {
        let fake = three_accounts();
        fake.hang_pages.store(true, Ordering::SeqCst);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Failed);
        assert!(done.last_error.unwrap().contains("did not respond"));
        assert!(fake.updated.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn screen_name_failure_stops_before_listing() {
        let fake = three_accounts();
        *fake.screen_error.lock().unwrap() = Some(XError::Unauthorized);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Failed);
        assert!(done.last_error.unwrap().contains("401"));
        assert!(fake.page_calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn auth_and_rate_limit_responses_stop_the_job() {
        for (error, code) in [
            (XError::Unauthorized, "401"),
            (XError::Forbidden, "403"),
            (XError::RateLimited, "429"),
        ] {
            let fake = three_accounts();
            fake.update_results
                .lock()
                .unwrap()
                .insert("2".to_string(), error);
            let jobs = FollowingJobs::new(timings());
            jobs.start(Arc::clone(&fake), twitter()).unwrap();
            let done = settle(&jobs).await;

            assert_eq!(done.state, JobState::Failed, "{code}");
            assert_eq!(done.total, Some(3));
            assert_eq!(done.success_count, 1);
            assert_eq!(done.failed_count, 0);
            assert_eq!(done.remaining_count, Some(2), "{code}");
            assert!(done.last_error.unwrap().contains(code));
            assert_eq!(*fake.updated.lock().unwrap(), ["1", "2"], "{code}");
        }
    }

    #[tokio::test]
    async fn isolated_failures_are_counted_and_the_job_continues() {
        let fake = three_accounts();
        fake.update_results
            .lock()
            .unwrap()
            .insert("2".to_string(), XError::Status(500));
        fake.hang_updates.lock().unwrap().push("3".to_string());
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;

        assert_eq!(done.state, JobState::Completed);
        assert_eq!(done.success_count, 1);
        assert_eq!(done.failed_count, 2);
        assert_eq!(done.remaining_count, Some(0));
        assert!(done.last_error.is_some());
        assert_eq!(*fake.updated.lock().unwrap(), ["1", "2", "3"]);
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cancel_stops_after_the_current_request_and_keeps_counts_honest() {
        let fake = three_accounts();
        fake.hang_updates.lock().unwrap().push("2".to_string());
        let slow = Timings {
            request_timeout: Duration::from_secs(30),
            ..timings()
        };
        let jobs = FollowingJobs::new(slow);
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        until("the second account", || {
            fake.updated.lock().unwrap().len() == 2
        })
        .await;
        assert_eq!(jobs.snapshot().state, JobState::Running);

        let (requested, cancelling) = jobs.cancel();
        assert!(requested);
        assert_eq!(cancelling.state, JobState::Cancelling);
        let done = settle(&jobs).await;

        assert_eq!(done.state, JobState::Cancelled);
        assert_eq!(done.total, Some(3));
        assert_eq!(done.success_count, 1);
        assert_eq!(done.failed_count, 0);
        assert_eq!(done.remaining_count, Some(2));
        assert_eq!(*fake.updated.lock().unwrap(), ["1", "2"]);
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 0);

        let (requested_again, snapshot) = jobs.cancel();
        assert!(!requested_again);
        assert_eq!(snapshot, done);
    }

    #[tokio::test]
    async fn a_second_job_is_rejected_while_active_and_allowed_afterwards() {
        let fake = three_accounts();
        fake.hang_pages.store(true, Ordering::SeqCst);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let err = jobs.start(Arc::clone(&fake), twitter()).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::CONFLICT);
        assert_eq!(err.code, "following_job_active");

        jobs.cancel();
        settle(&jobs).await;
        fake.hang_pages.store(false, Ordering::SeqCst);
        let restarted = jobs.start(Arc::clone(&fake), twitter()).unwrap();
        assert_eq!(restarted.state, JobState::Loading);
        assert_eq!(restarted.success_count, 0);
        assert_eq!(restarted.last_error, None);
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Completed);
        assert_eq!(done.success_count, 3);
    }

    #[tokio::test]
    async fn shutdown_cancels_and_awaits_the_task() {
        let fake = three_accounts();
        fake.hang_updates.lock().unwrap().push("1".to_string());
        let slow = Timings {
            request_timeout: Duration::from_secs(30),
            ..timings()
        };
        let jobs = FollowingJobs::new(slow);
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        until("the first account", || {
            fake.updates_in_flight.load(Ordering::SeqCst) == 1
        })
        .await;

        jobs.shutdown().await;
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 0);
        assert_eq!(jobs.snapshot().state, JobState::Cancelled);
        jobs.shutdown().await;
    }

    #[tokio::test]
    async fn the_overall_time_limit_ends_the_job_with_an_honest_remainder() {
        let fake = three_accounts();
        let limited = Timings {
            request_timeout: Duration::from_millis(150),
            interval: Duration::from_millis(40),
            max_duration: Duration::from_millis(60),
        };
        let jobs = FollowingJobs::new(limited);
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Failed);
        assert!(done.last_error.unwrap().contains("time limit"));
        assert!(done.remaining_count.unwrap() >= 1);
        assert_eq!(
            done.success_count + done.remaining_count.unwrap(),
            done.total.unwrap()
        );
    }

    #[tokio::test]
    async fn malformed_page_stops_the_job_with_zero_updates() {
        let fake = fake_with(vec![
            (-1, page(&["1", "2"], 10)),
            (10, Err(XError::Malformed)),
        ]);
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let done = settle(&jobs).await;
        assert_eq!(done.state, JobState::Failed);
        assert_eq!(
            (done.total, done.success_count, done.failed_count),
            (None, 0, 0)
        );
        assert!(fake.updated.lock().unwrap().is_empty());
        assert!(!done.last_error.unwrap().contains("SECRET"));
    }

    #[tokio::test]
    async fn a_new_job_is_refused_until_the_cancelled_one_has_ended() {
        let fake = three_accounts();
        fake.hang_updates.lock().unwrap().push("1".to_string());
        let slow = Timings {
            request_timeout: Duration::from_secs(30),
            ..timings()
        };
        let jobs = FollowingJobs::new(slow);
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        until("the first account", || {
            fake.updates_in_flight.load(Ordering::SeqCst) == 1
        })
        .await;

        let (requested, cancelling) = jobs.cancel();
        assert!(requested);
        assert_eq!(cancelling.state, JobState::Cancelling);
        let err = jobs.start(Arc::clone(&fake), twitter()).unwrap_err();
        assert_eq!(err.code, "following_job_active");

        assert_eq!(settle(&jobs).await.state, JobState::Cancelled);
        assert_eq!(fake.updates_in_flight.load(Ordering::SeqCst), 0);
        fake.hang_updates.lock().unwrap().clear();
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        assert_eq!(settle(&jobs).await.state, JobState::Completed);
    }

    #[test]
    fn a_stale_task_cannot_write_into_a_newer_job() {
        let shared = Shared::default();
        shared.with(|p| *p = Progress::with_state(JobState::Loading, 2));

        shared.begin(1, 99);
        shared.record_success(1);
        shared.record_failure(1, "stale");
        shared.finish(1, Outcome::Failed("The job stopped unexpectedly.".into()));
        let snapshot = shared.snapshot();
        assert_eq!(snapshot.state, JobState::Loading);
        assert_eq!(snapshot.total, None);
        assert_eq!(snapshot.success_count + snapshot.failed_count, 0);
        assert_eq!(snapshot.last_error, None);

        shared.finish(2, Outcome::Completed);
        assert_eq!(shared.snapshot().state, JobState::Completed);
    }

    #[tokio::test]
    async fn snapshots_never_contain_credentials() {
        let fake = three_accounts();
        fake.update_results
            .lock()
            .unwrap()
            .insert("2".to_string(), XError::Status(500));
        let jobs = FollowingJobs::new(timings());
        jobs.start(Arc::clone(&fake), twitter()).unwrap();
        let text = serde_json::to_string(&settle(&jobs).await).unwrap();
        assert!(!text.contains("SECRET"));
    }
}
