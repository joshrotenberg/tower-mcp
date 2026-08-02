//! Background task registry and notification reconciliation.

use std::collections::HashMap;
use std::sync::Mutex;

use nu_ansi_term::Style;
use tower_mcp::protocol::{TaskObject, TaskStatus, TaskStatusParams};
use tower_mcp::tasks::TaskStatusNotificationParams;

use crate::output::AsyncOutput;
use crate::style::{paint, tag, task_status_style};

const MAX_PENDING_NOTIFICATIONS: usize = 128;

/// A task started by this REPL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Job {
    pub task_id: String,
    pub tool: String,
    pub status: TaskStatus,
    pub status_message: Option<String>,
}

#[derive(Clone)]
struct PendingStatus {
    status: TaskStatus,
    status_message: Option<String>,
}

#[derive(Default)]
struct State {
    jobs: Vec<Job>,
    pending: HashMap<String, PendingStatus>,
}

struct Transition {
    task_id: String,
    status: TaskStatus,
    status_message: Option<String>,
}

/// Shared tasks started by the REPL.
pub struct Jobs {
    state: Mutex<State>,
    output: AsyncOutput,
    announce: bool,
}

impl Jobs {
    pub fn new(output: AsyncOutput, announce: bool) -> Self {
        Self {
            state: Mutex::new(State::default()),
            output,
            announce,
        }
    }

    /// Record a task returned by a background tool call. If its notification
    /// raced ahead of the response, reconcile and announce that newer state.
    pub fn register(
        &self,
        task_id: String,
        tool: String,
        status: TaskStatus,
        status_message: Option<String>,
    ) {
        let transition = {
            let mut state = self.state.lock().unwrap();
            let pending = state.pending.remove(&task_id);
            state.jobs.push(Job {
                task_id: task_id.clone(),
                tool,
                status,
                status_message,
            });
            pending.and_then(|pending| {
                apply_status(
                    &mut state.jobs,
                    &task_id,
                    pending.status,
                    pending.status_message,
                )
            })
        };
        self.announce(transition);
    }

    /// Observe a legacy task notification.
    pub fn observe_legacy(&self, params: TaskStatusParams) {
        self.observe(params.task_id, params.status, params.status_message);
    }

    /// Observe a final-protocol task notification.
    pub fn observe_final(&self, params: TaskStatusNotificationParams) {
        self.observe(
            params.task.task_id().to_string(),
            params.task.status(),
            params.task.metadata().status_message.clone(),
        );
    }

    /// Observe a status fetched by the bounded per-task polling fallback.
    pub fn observe_task(&self, task: &TaskObject) {
        self.observe(
            task.task_id.clone(),
            task.status,
            task.status_message.clone(),
        );
    }

    /// Silently refresh a known task after an explicit `jobs`, `task`, or
    /// `wait` request. Manual commands render their own authoritative output.
    pub fn sync(&self, task_id: &str, status: TaskStatus, status_message: Option<String>) {
        let mut state = self.state.lock().unwrap();
        if let Some(job) = state.jobs.iter_mut().find(|job| job.task_id == task_id) {
            job.status = status;
            job.status_message = status_message;
        }
    }

    pub fn list(&self) -> Vec<Job> {
        self.state.lock().unwrap().jobs.clone()
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().unwrap().jobs.is_empty()
    }

    pub fn is_terminal(&self, task_id: &str) -> bool {
        self.state
            .lock()
            .unwrap()
            .jobs
            .iter()
            .find(|job| job.task_id == task_id)
            .is_some_and(|job| job.status.is_terminal())
    }

    pub fn automatic_updates_enabled(&self) -> bool {
        self.announce
    }

    fn observe(&self, task_id: String, status: TaskStatus, status_message: Option<String>) {
        let transition = {
            let mut state = self.state.lock().unwrap();
            if let Some(transition) =
                apply_status(&mut state.jobs, &task_id, status, status_message.clone())
            {
                Some(transition)
            } else if state.jobs.iter().any(|job| job.task_id == task_id) {
                None
            } else {
                if state.pending.len() >= MAX_PENDING_NOTIFICATIONS
                    && !state.pending.contains_key(&task_id)
                    && let Some(evicted) = state.pending.keys().next().cloned()
                {
                    state.pending.remove(&evicted);
                }
                state.pending.insert(
                    task_id,
                    PendingStatus {
                        status,
                        status_message,
                    },
                );
                None
            }
        };
        self.announce(transition);
    }

    fn announce(&self, transition: Option<Transition>) {
        if !self.announce {
            return;
        }
        let Some(transition) = transition else {
            return;
        };
        let status = transition.status.to_string();
        let mut line = format!(
            "{} {}",
            tag(Style::new(), &format!("task {}", transition.task_id)),
            paint(task_status_style(transition.status), &status)
        );
        if let Some(message) = transition
            .status_message
            .filter(|message| !message.is_empty())
        {
            line.push_str(&format!(" — {message}"));
        }
        if transition.status.is_terminal() || transition.status == TaskStatus::InputRequired {
            line.push_str(&format!(
                "  {}",
                paint(
                    Style::new().dimmed(),
                    &format!("run `task {}` for details", transition.task_id)
                )
            ));
        }
        self.output.line(line);
    }
}

fn apply_status(
    jobs: &mut [Job],
    task_id: &str,
    status: TaskStatus,
    status_message: Option<String>,
) -> Option<Transition> {
    let job = jobs.iter_mut().find(|job| job.task_id == task_id)?;
    if job.status == status {
        job.status_message = status_message;
        return None;
    }
    job.status = status;
    job.status_message = status_message.clone();
    Some(Transition {
        task_id: task_id.to_string(),
        status,
        status_message,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::*;

    fn fixture() -> (Jobs, reedline::ExternalPrinter<String>) {
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let printer = output.external_printer().unwrap();
        (Jobs::new(output, true), printer)
    }

    #[test]
    fn tracked_transitions_print_once() {
        let (jobs, printer) = fixture();
        jobs.register(
            "task-1".into(),
            "slow_add".into(),
            TaskStatus::Working,
            None,
        );
        assert!(printer.get_line().is_none());

        jobs.observe_legacy(TaskStatusParams {
            task_id: "task-1".into(),
            status: TaskStatus::Completed,
            status_message: Some("done".into()),
            created_at: "2026-08-02T00:00:00Z".into(),
            last_updated_at: "2026-08-02T00:00:01Z".into(),
            ttl: None,
            poll_interval: None,
            meta: None,
        });
        let line = printer.get_line().unwrap();
        assert!(line.contains("[task task-1]"));
        assert!(line.contains("completed"));
        assert!(line.contains("task task-1"));

        jobs.observe("task-1".into(), TaskStatus::Completed, None);
        assert!(printer.get_line().is_none(), "replay must be deduplicated");
    }

    #[test]
    fn notification_that_wins_the_creation_race_is_reconciled() {
        let (jobs, printer) = fixture();
        jobs.observe("task-race".into(), TaskStatus::Failed, Some("boom".into()));
        assert!(printer.get_line().is_none(), "unknown tasks stay silent");

        jobs.register("task-race".into(), "run".into(), TaskStatus::Working, None);
        let line = printer.get_line().unwrap();
        assert!(line.contains("failed"));
        assert!(line.contains("boom"));
        assert_eq!(jobs.list()[0].status, TaskStatus::Failed);
    }

    #[test]
    fn input_failed_and_cancelled_transitions_are_visible() {
        let (jobs, printer) = fixture();
        for (id, status) in [
            ("input", TaskStatus::InputRequired),
            ("failed", TaskStatus::Failed),
            ("cancelled", TaskStatus::Cancelled),
        ] {
            jobs.register(id.into(), "run".into(), TaskStatus::Working, None);
            jobs.observe(id.into(), status, None);
            let line = printer.get_line().unwrap();
            assert!(line.contains(&status.to_string()), "{line}");
            assert!(line.contains(&format!("task {id}")), "{line}");
        }
    }

    #[test]
    fn one_shot_policy_suppresses_automatic_lines() {
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let printer = output.external_printer().unwrap();
        let jobs = Jobs::new(output, false);
        jobs.register(
            "task-1".into(),
            "slow_add".into(),
            TaskStatus::Working,
            None,
        );
        jobs.observe("task-1".into(), TaskStatus::Cancelled, None);
        assert!(printer.get_line().is_none());
    }
}
