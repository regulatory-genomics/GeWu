use crate::{Attempt, Lab, Task, TaskLoop, TaskStatus};
use anyhow::{Result, anyhow, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct Project {
    name: String,
    goal: String,
    tasks: BTreeMap<String, Entry>,
}

#[derive(Clone, Debug)]
struct Entry {
    task: Task,
    dependencies: BTreeSet<String>,
    history: Vec<Attempt>,
    /// An armed run owns this task between `begin_attempt` and its finish.
    running: bool,
}

pub enum ReviewDecision {
    Approve,
    Reject { feedback: String },
}

impl Project {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_goal(name: impl Into<String>, goal: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            goal: goal.into(),
            ..Self::default()
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn goal(&self) -> &str {
        &self.goal
    }

    pub fn is_complete(&self) -> bool {
        self.tasks.values().all(|entry| {
            matches!(
                entry.task.status,
                TaskStatus::Completed | TaskStatus::Cancelled
            )
        })
    }

    pub fn task(&self, id: &str) -> Option<&Task> {
        self.tasks.get(id).map(|entry| &entry.task)
    }

    pub fn task_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn task_board(&self) -> impl Iterator<Item = (&Task, &BTreeSet<String>)> {
        self.tasks
            .values()
            .map(|entry| (&entry.task, &entry.dependencies))
    }

    pub fn attempts(&self, id: &str) -> Option<&[Attempt]> {
        self.tasks.get(id).map(|entry| entry.history.as_slice())
    }

    pub fn add_task(&mut self, task: Task, dependencies: BTreeSet<String>) -> Result<()> {
        ensure!(
            !self.tasks.contains_key(&task.id),
            "duplicate task: {}",
            task.id
        );
        ensure!(
            task.status == TaskStatus::Pending
                && task.output.is_none()
                && task.pending_review.is_none(),
            "new tasks must be pending with no submission"
        );
        self.validate_dependencies(&task.id, &dependencies)?;
        self.tasks.insert(
            task.id.clone(),
            Entry {
                task,
                dependencies,
                history: Vec::new(),
                running: false,
            },
        );
        Ok(())
    }

    pub fn revise_task(
        &mut self,
        id: &str,
        name: impl Into<String>,
        deliverable: impl Into<String>,
        dependencies: BTreeSet<String>,
    ) -> Result<()> {
        self.validate_dependencies(id, &dependencies)?;
        let entry = self.entry_mut(id, TaskStatus::Pending)?;
        entry.task.name = name.into();
        entry.task.deliverable = deliverable.into();
        entry.dependencies = dependencies;
        Ok(())
    }

    pub fn cancel_task(&mut self, id: &str) -> Result<()> {
        self.entry_mut(id, TaskStatus::Pending)?.task.status = TaskStatus::Cancelled;
        Ok(())
    }

    pub fn ready_tasks(&self) -> impl Iterator<Item = &Task> {
        self.tasks.values().filter_map(|entry| {
            (entry.task.status == TaskStatus::Pending
                && entry
                    .dependencies
                    .iter()
                    .all(|id| self.tasks[id].task.status == TaskStatus::Completed))
            .then_some(&entry.task)
        })
    }

    pub async fn run_next(
        &mut self,
        make_loop: impl FnOnce(&Task) -> Result<TaskLoop>,
    ) -> Result<Option<Attempt>> {
        let Some(task) = self.ready_tasks().next() else {
            return Ok(None);
        };
        let task_id = task.id.clone();
        let task_loop = make_loop(task)?;
        self.run_attempt(
            &task_id,
            |prompt| async move { task_loop.run(&prompt).await },
        )
        .await
        .map(Some)
    }

    pub fn review(&mut self, id: &str, attempt_id: &str, decision: ReviewDecision) -> Result<()> {
        let entry = self.entry_mut(id, TaskStatus::AwaitingReview)?;
        ensure!(
            entry
                .task
                .pending_review
                .as_ref()
                .is_some_and(|attempt| attempt.id == attempt_id),
            "attempt {attempt_id} is not awaiting review for task {id}"
        );
        let mut attempt = entry.task.pending_review.take().expect("checked above");
        entry.task.status = match decision {
            ReviewDecision::Approve => TaskStatus::Completed,
            ReviewDecision::Reject { feedback } => {
                attempt.feedback = Some(feedback);
                TaskStatus::Blocked
            }
        };
        entry.history.push(attempt);
        Ok(())
    }

    pub fn retry(&mut self, id: &str) -> Result<()> {
        let entry = self.entry_mut(id, TaskStatus::Blocked)?;
        entry.task.status = TaskStatus::Pending;
        entry.task.output = None;
        Ok(())
    }

    async fn run_attempt<F: Future<Output = Result<String>>>(
        &mut self,
        id: &str,
        run: impl FnOnce(String) -> F,
    ) -> Result<Attempt> {
        let (attempt, prompt) = self.begin_attempt(id)?;
        let outcome = {
            let entry = self.tasks.get_mut(id).expect("task begun above");
            let _guard = RunningAttempt(entry);
            run(prompt).await
        };
        let failure = outcome.as_ref().err().map(|error| format!("{error:#}"));
        let attempt = self.finish_attempt(id, attempt, outcome)?;
        if let Some(message) = failure {
            return Err(anyhow!(message));
        }
        Ok(attempt)
    }

    /// Claims a pending task for execution: fails unless the task is
    /// `Pending` and no run owns it. Returns the attempt and its prompt.
    pub(crate) fn begin_attempt(&mut self, id: &str) -> Result<(Attempt, String)> {
        let entry = self.entry_mut(id, TaskStatus::Pending)?;
        ensure!(!entry.running, "task {id} is already running");
        let attempt = Attempt {
            id: format!("{id}:{}", entry.history.len() + 1),
            task_id: id.into(),
            summary: String::new(),
            uncertainties: String::new(),
            feedback: None,
        };
        entry.task.status = TaskStatus::InProgress;
        entry.running = true;
        let prompt = Self::build_prompt(entry);
        Ok((attempt, prompt))
    }

    /// Records the finished run: success submits the attempt for review,
    /// failure blocks the task with feedback.
    pub(crate) fn finish_attempt(
        &mut self,
        id: &str,
        mut attempt: Attempt,
        outcome: Result<String>,
    ) -> Result<Attempt> {
        let entry = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("unknown task: {id}"))?;
        entry.running = false;
        match outcome {
            Ok(summary) => {
                attempt.summary = summary;
                entry.task.pending_review = Some(attempt.clone());
                entry.task.status = TaskStatus::AwaitingReview;
            }
            Err(error) => {
                attempt.feedback = Some(format!("Execution failed: {error:#}"));
                entry.history.push(attempt.clone());
                entry.task.status = TaskStatus::Blocked;
            }
        }
        Ok(attempt)
    }

    /// Marks a run that never finished (its future was dropped) as blocked.
    pub(crate) fn abandon_attempt(&mut self, id: &str) {
        if let Some(entry) = self.tasks.get_mut(id)
            && entry.running
        {
            entry.running = false;
            if entry.task.status == TaskStatus::InProgress {
                entry.task.status = TaskStatus::Blocked;
            }
        }
    }

    fn build_prompt(entry: &Entry) -> String {
        let mut prompt = format!(
            "Task: {}\nDeliverable: {}",
            entry.task.name, entry.task.deliverable
        );
        if let Some(feedback) = entry
            .history
            .last()
            .and_then(|attempt| attempt.feedback.as_deref())
        {
            prompt.push_str("\nPrevious attempt feedback:\n");
            prompt.push_str(feedback);
        }
        prompt
    }

    fn entry_mut(&mut self, id: &str, expected: TaskStatus) -> Result<&mut Entry> {
        let entry = self
            .tasks
            .get_mut(id)
            .ok_or_else(|| anyhow!("unknown task: {id}"))?;
        ensure!(
            entry.task.status == expected,
            "task {id} must be {expected:?}, is {:?}",
            entry.task.status
        );
        Ok(entry)
    }

    fn validate_dependencies(&self, id: &str, dependencies: &BTreeSet<String>) -> Result<()> {
        let mut pending: Vec<&str> = dependencies.iter().map(String::as_str).collect();
        let mut visited = BTreeSet::new();
        while let Some(dependency) = pending.pop() {
            ensure!(dependency != id, "dependency cycle at task {id}");
            if visited.insert(dependency) {
                let entry = self
                    .tasks
                    .get(dependency)
                    .ok_or_else(|| anyhow!("unknown dependency: {dependency}"))?;
                pending.extend(entry.dependencies.iter().map(String::as_str));
            }
        }
        Ok(())
    }
}

struct RunningAttempt<'a>(&'a mut Entry);

impl Drop for RunningAttempt<'_> {
    fn drop(&mut self) {
        self.0.running = false;
        if self.0.task.status == TaskStatus::InProgress {
            self.0.task.status = TaskStatus::Blocked;
        }
    }
}

/// Heals a task whose run future was dropped without finishing: the drop
/// guard records the abandonment on the shared lab state.
pub struct RunningTask {
    lab: Arc<Mutex<Lab>>,
    project_id: String,
    task_id: String,
    armed: bool,
}

impl RunningTask {
    pub fn new(lab: Arc<Mutex<Lab>>, project_id: &str, task_id: &str) -> Self {
        Self {
            lab,
            project_id: project_id.into(),
            task_id: task_id.into(),
            armed: true,
        }
    }

    pub fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RunningTask {
    fn drop(&mut self) {
        if self.armed
            && let Ok(mut lab) = self.lab.lock()
            && let Some(project) = lab.project_mut(&self.project_id)
        {
            project.abandon_attempt(&self.task_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_loop::tests::coordinator;

    fn board() -> Project {
        let mut project = Project::new();
        project
            .add_task(Task::new("z", "Collect", "Notes"), BTreeSet::new())
            .unwrap();
        project
            .add_task(
                Task::new("a", "Summarize", "Report"),
                BTreeSet::from(["z".into()]),
            )
            .unwrap();
        project
    }

    #[test]
    fn invalid_edits_do_not_change_the_board() {
        let mut project = board();
        for dependency in ["a", "z", "missing"] {
            assert!(
                project
                    .revise_task(
                        "z",
                        "Changed",
                        "Changed",
                        BTreeSet::from([dependency.into()])
                    )
                    .is_err()
            );
            assert_eq!(project.task("z").unwrap().name, "Collect");
            assert_eq!(project.ready_tasks().next().unwrap().id, "z");
        }
        project.cancel_task("z").unwrap();
        assert!(project.ready_tasks().next().is_none());
    }

    #[tokio::test]
    async fn only_approved_attempts_unlock_dependents() {
        let mut project = board();
        let first = project
            .run_next(|task| {
                assert_eq!(task.id, "z");
                Ok(coordinator(vec!["draft"]))
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.summary, "draft");
        assert!(
            project
                .run_next(|_| panic!("no task is ready"))
                .await
                .unwrap()
                .is_none()
        );
        project
            .review(
                "z",
                &first.id,
                ReviewDecision::Reject {
                    feedback: "needs evidence".into(),
                },
            )
            .unwrap();
        assert!(project.ready_tasks().next().is_none());
        project.retry("z").unwrap();
        let second = project
            .run_attempt("z", |prompt| async move {
                assert!(prompt.contains("needs evidence"));
                Ok("revised".into())
            })
            .await
            .unwrap();
        assert!(
            project
                .review("z", &first.id, ReviewDecision::Approve)
                .is_err()
        );
        project
            .review("z", &second.id, ReviewDecision::Approve)
            .unwrap();
        assert_eq!(project.ready_tasks().next().unwrap().id, "a");
    }

    #[tokio::test]
    async fn failure_and_dropped_runner_leave_task_retryable() {
        let mut project = board();
        assert!(project.run_next(|_| Ok(coordinator(vec![]))).await.is_err());
        assert_eq!(project.task("z").unwrap().status, TaskStatus::Blocked);
        assert!(project.attempts("z").unwrap()[0].feedback.is_some());
        project.retry("z").unwrap();
        let mut run = Box::pin(project.run_attempt("z", |_| futures::future::pending()));
        assert!(futures::poll!(&mut run).is_pending());
        drop(run);
        assert_eq!(project.task("z").unwrap().status, TaskStatus::Blocked);
    }
}
