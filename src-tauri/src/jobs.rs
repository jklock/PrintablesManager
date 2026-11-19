//! Background job registry.
//!
//! 1.x spawned a detached thread per clone and threw the id away, so a job
//! could not be listed, paused, or cancelled. Every long-running operation now
//! registers here, which gives the UI a single source of truth.

use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobInfo {
    pub id: String,
    pub kind: String,
    pub label: String,
    pub model_id: Option<String>,
    pub state: JobState,
    pub percent: f32,
    pub message: String,
    pub error: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<String, JobInfo>,
    order: Vec<String>,
    cancelled: HashSet<String>,
}

#[derive(Default)]
pub struct JobRegistry {
    inner: Mutex<Inner>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

impl JobRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn create(&self, kind: &str, label: &str, model_id: Option<&str>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let info = JobInfo {
            id: id.clone(),
            kind: kind.to_string(),
            label: label.to_string(),
            model_id: model_id.map(String::from),
            state: JobState::Queued,
            percent: 0.0,
            message: "Queued".to_string(),
            error: None,
            started_at: now(),
            finished_at: None,
        };
        if let Ok(mut inner) = self.inner.lock() {
            inner.order.push(id.clone());
            inner.jobs.insert(id.clone(), info);
        }
        id
    }

    pub fn start(&self, id: &str) -> Option<JobInfo> {
        self.mutate(id, |job| {
            job.state = JobState::Running;
            job.message = "Starting…".to_string();
        })
    }

    pub fn update(&self, id: &str, percent: f32, message: impl Into<String>) -> Option<JobInfo> {
        let message = message.into();
        self.mutate(id, |job| {
            job.state = JobState::Running;
            job.percent = percent.clamp(0.0, 100.0);
            job.message = message;
        })
    }

    pub fn finish(&self, id: &str, message: impl Into<String>) -> Option<JobInfo> {
        let message = message.into();
        self.mutate(id, |job| {
            job.state = JobState::Done;
            job.percent = 100.0;
            job.message = message;
            job.finished_at = Some(now());
        })
    }

    pub fn fail(&self, id: &str, error: impl Into<String>) -> Option<JobInfo> {
        let error = error.into();
        self.mutate(id, |job| {
            job.state = JobState::Failed;
            job.message = error.clone();
            job.error = Some(error);
            job.finished_at = Some(now());
        })
    }

    pub fn cancel(&self, id: &str) -> bool {
        if let Ok(mut inner) = self.inner.lock() {
            inner.cancelled.insert(id.to_string());
            if let Some(job) = inner.jobs.get_mut(id) {
                if matches!(job.state, JobState::Queued | JobState::Running) {
                    job.state = JobState::Cancelled;
                    job.message = "Cancelled".to_string();
                    job.finished_at = Some(now());
                }
                return true;
            }
        }
        false
    }

    pub fn is_cancelled(&self, id: &str) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.cancelled.contains(id))
            .unwrap_or(false)
    }

    /// Test-only accessor; production code reads `list()`.
    #[cfg(test)]
    pub fn get(&self, id: &str) -> Option<JobInfo> {
        self.inner.lock().ok()?.jobs.get(id).cloned()
    }

    pub fn list(&self) -> Vec<JobInfo> {
        let Ok(inner) = self.inner.lock() else {
            return Vec::new();
        };
        inner
            .order
            .iter()
            .rev()
            .filter_map(|id| inner.jobs.get(id).cloned())
            .collect()
    }

    /// Drop finished jobs so the list does not grow without bound.
    pub fn clear_finished(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            let keep: Vec<String> = inner
                .order
                .iter()
                .filter(|id| {
                    inner
                        .jobs
                        .get(*id)
                        .map(|j| matches!(j.state, JobState::Queued | JobState::Running))
                        .unwrap_or(false)
                })
                .cloned()
                .collect();
            inner.jobs.retain(|id, _| keep.contains(id));
            inner.order = keep;
        }
    }

    fn mutate(&self, id: &str, f: impl FnOnce(&mut JobInfo)) -> Option<JobInfo> {
        let mut inner = self.inner.lock().ok()?;
        let job = inner.jobs.get_mut(id)?;
        f(job);
        Some(job.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_and_cancellation() {
        let registry = JobRegistry::new();
        let id = registry.create("pull", "Model 1", Some("1"));
        assert_eq!(registry.get(&id).unwrap().state, JobState::Queued);

        registry.start(&id);
        registry.update(&id, 42.0, "Downloading");
        let job = registry.get(&id).unwrap();
        assert_eq!(job.state, JobState::Running);
        assert_eq!(job.percent, 42.0);

        assert!(registry.cancel(&id));
        assert!(registry.is_cancelled(&id));
        assert_eq!(registry.get(&id).unwrap().state, JobState::Cancelled);

        registry.clear_finished();
        assert!(registry.get(&id).is_none());
    }

    #[test]
    fn percent_is_clamped() {
        let registry = JobRegistry::new();
        let id = registry.create("pull", "x", None);
        registry.update(&id, 1000.0, "over");
        assert_eq!(registry.get(&id).unwrap().percent, 100.0);
        registry.update(&id, -5.0, "under");
        assert_eq!(registry.get(&id).unwrap().percent, 0.0);
    }
}
