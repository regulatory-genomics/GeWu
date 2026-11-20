use crate::{Attempt, Project, ReviewDecision, Task, TaskLoop};
use anyhow::{Result, anyhow, ensure};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Default)]
pub struct Lab {
    projects: BTreeMap<String, Project>,
    messages: BTreeMap<String, VecDeque<String>>,
    paused: bool,
}

pub enum ProjectUpdate {
    Add {
        task: Task,
        dependencies: BTreeSet<String>,
    },
    Revise {
        task_id: String,
        name: String,
        deliverable: String,
        dependencies: BTreeSet<String>,
    },
    Cancel {
        task_id: String,
    },
    Retry {
        task_id: String,
    },
    Review {
        task_id: String,
        attempt_id: String,
        decision: ReviewDecision,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub enum LabStep {
    Submitted {
        project_id: String,
        attempt: Attempt,
    },
    Paused,
    Idle,
    AllProjectsComplete,
}

impl Lab {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_project(&mut self, id: impl Into<String>, project: Project) -> Result<()> {
        let id = id.into();
        ensure!(!self.projects.contains_key(&id), "duplicate project: {id}");
        self.projects.insert(id, project);
        Ok(())
    }

    /// Creates a named, goal-driven project with a unique generated id.
    pub fn create_project(&mut self, name: &str, goal: &str) -> Result<String> {
        ensure!(
            !name.trim().is_empty() || !goal.trim().is_empty(),
            "project needs a name or a goal"
        );
        let id = {
            let mut counter = 0;
            loop {
                let candidate = if counter == 0 {
                    name.trim().to_owned()
                } else {
                    format!("{}-{counter}", name.trim())
                };
                if candidate.is_empty() {
                    break format!("project-{}", self.projects.len() + 1);
                }
                if !self.projects.contains_key(&candidate) {
                    break candidate;
                }
                counter += 1;
            }
        };
        self.projects
            .insert(id.clone(), Project::with_goal(name, goal));
        Ok(id)
    }

    pub fn project(&self, id: &str) -> Option<&Project> {
        self.projects.get(id)
    }

    pub fn project_mut(&mut self, id: &str) -> Option<&mut Project> {
        self.projects.get_mut(id)
    }

    pub fn projects(&self) -> impl Iterator<Item = (&str, &Project)> {
        self.projects
            .iter()
            .map(|(id, project)| (id.as_str(), project))
    }

    pub fn send_project_message(&mut self, id: &str, message: String) -> Result<()> {
        ensure!(self.projects.contains_key(id), "unknown project: {id}");
        ensure!(
            !message.trim().is_empty(),
            "project message must not be empty"
        );
        self.messages
            .entry(id.into())
            .or_default()
            .push_back(message);
        Ok(())
    }

    pub fn take_project_message(&mut self, id: &str) -> Result<Option<String>> {
        ensure!(self.projects.contains_key(id), "unknown project: {id}");
        Ok(self.messages.get_mut(id).and_then(VecDeque::pop_front))
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    pub fn apply_update(&mut self, project_id: &str, update: ProjectUpdate) -> Result<()> {
        let project = self
            .projects
            .get_mut(project_id)
            .ok_or_else(|| anyhow!("unknown project: {project_id}"))?;
        match update {
            ProjectUpdate::Add { task, dependencies } => project.add_task(task, dependencies),
            ProjectUpdate::Revise {
                task_id,
                name,
                deliverable,
                dependencies,
            } => project.revise_task(&task_id, name, deliverable, dependencies),
            ProjectUpdate::Cancel { task_id } => project.cancel_task(&task_id),
            ProjectUpdate::Retry { task_id } => project.retry(&task_id),
            ProjectUpdate::Review {
                task_id,
                attempt_id,
                decision,
            } => project.review(&task_id, &attempt_id, decision),
        }
    }

    pub async fn run_next(
        &mut self,
        make_loop: impl FnOnce(&str, &Task) -> Result<TaskLoop>,
    ) -> Result<LabStep> {
        if self.paused {
            return Ok(LabStep::Paused);
        }
        if let Some((project_id, project)) = self
            .projects
            .iter_mut()
            .find(|(_, project)| project.ready_tasks().next().is_some())
        {
            return project
                .run_next(|task| make_loop(project_id, task))
                .await
                .map_err(|error| error.context(format!("project {project_id}")))
                .map(|attempt| match attempt {
                    Some(attempt) => LabStep::Submitted {
                        project_id: project_id.clone(),
                        attempt,
                    },
                    None => LabStep::Idle,
                });
        }
        Ok(if self.projects.values().all(Project::is_complete) {
            LabStep::AllProjectsComplete
        } else {
            LabStep::Idle
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskStatus, task_loop::tests::coordinator};

    #[tokio::test]
    async fn dispatches_across_projects_and_waits_for_explicit_review() {
        let mut lab = Lab::new();
        for id in ["b", "a"] {
            lab.add_project(id, Project::new()).unwrap();
            lab.apply_update(
                id,
                ProjectUpdate::Add {
                    task: Task::new("task", "Collect", "Notes"),
                    dependencies: BTreeSet::new(),
                },
            )
            .unwrap();
        }
        lab.set_paused(true);
        assert_eq!(
            lab.run_next(|_, _| panic!("paused")).await.unwrap(),
            LabStep::Paused
        );
        lab.set_paused(false);

        let mut submissions = Vec::new();
        for expected in ["a", "b"] {
            let step = lab
                .run_next(|id, task| {
                    assert_eq!(id, expected);
                    assert_eq!(task.id, "task");
                    Ok(coordinator(vec!["draft"]))
                })
                .await
                .unwrap();
            let LabStep::Submitted {
                project_id,
                attempt,
            } = step
            else {
                panic!("expected submission")
            };
            assert_eq!(project_id, expected);
            submissions.push((project_id, attempt));
        }
        assert_eq!(
            lab.run_next(|_, _| panic!("awaiting review"))
                .await
                .unwrap(),
            LabStep::Idle
        );
        for (id, attempt) in submissions {
            lab.apply_update(
                &id,
                ProjectUpdate::Review {
                    task_id: attempt.task_id,
                    attempt_id: attempt.id,
                    decision: ReviewDecision::Approve,
                },
            )
            .unwrap();
        }
        assert_eq!(
            lab.run_next(|_, _| panic!("complete")).await.unwrap(),
            LabStep::AllProjectsComplete
        );
    }

    #[test]
    fn updates_are_scoped_to_the_addressed_project() {
        let mut lab = Lab::new();
        for id in ["a", "b"] {
            let mut project = Project::new();
            project
                .add_task(Task::new("task", "Collect", "Notes"), BTreeSet::new())
                .unwrap();
            lab.add_project(id, project).unwrap();
        }
        assert!(lab.add_project("a", Project::new()).is_err());
        let cancel = || ProjectUpdate::Cancel {
            task_id: "task".into(),
        };
        assert!(lab.apply_update("missing", cancel()).is_err());
        lab.apply_update("a", cancel()).unwrap();
        assert_eq!(
            lab.project("a").unwrap().task("task").unwrap().status,
            TaskStatus::Cancelled
        );
        assert_eq!(
            lab.project("b").unwrap().task("task").unwrap().status,
            TaskStatus::Pending
        );
    }
}
