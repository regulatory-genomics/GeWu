use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    AwaitingReview,
    Completed,
    Blocked,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub status: TaskStatus,
    pub deliverable: String,
    pub output: Option<TaskOutput>,
    pub pending_review: Option<Attempt>,
}

impl Task {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        deliverable: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            status: TaskStatus::Pending,
            deliverable: deliverable.into(),
            output: None,
            pending_review: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskOutput(PathBuf);

impl TaskOutput {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    pub id: String,
    pub task_id: String,
    pub summary: String,
    pub uncertainties: String,
    pub feedback: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_task_starts_pending_without_output_or_review() {
        let task = Task::new("task-1", "Draft report", "report.md");

        assert_eq!(task.id, "task-1");
        assert_eq!(task.name, "Draft report");
        assert_eq!(task.deliverable, "report.md");
        assert_eq!(task.status, TaskStatus::Pending);
        assert!(task.output.is_none());
        assert!(task.pending_review.is_none());
    }

    #[test]
    fn task_output_exposes_its_directory() {
        let path = Path::new("project/outputs/task-1/attempt-1");

        assert_eq!(TaskOutput::new(path).path(), path);
    }
}
