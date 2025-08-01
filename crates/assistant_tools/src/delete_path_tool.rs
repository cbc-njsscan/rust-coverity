use crate::schema::json_schema_for;
use anyhow::{Context as _, Result, anyhow};
use assistant_tool::{ActionLog, Tool, ToolResult};
use futures::{SinkExt, StreamExt, channel::mpsc};
use gpui::{AnyWindowHandle, App, AppContext, Entity, Task};
use language_model::{LanguageModel, LanguageModelRequest, LanguageModelToolSchemaFormat};
use project::{Project, ProjectPath};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use ui::IconName;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DeletePathToolInput {
    /// The path of the file or directory to delete.
    ///
    /// <example>
    /// If the project has the following files:
    ///
    /// - directory1/a/something.txt
    /// - directory2/a/things.txt
    /// - directory3/a/other.txt
    ///
    /// You can delete the first file by providing a path of "directory1/a/something.txt"
    /// </example>
    pub path: String,
}

pub struct DeletePathTool;

impl Tool for DeletePathTool {
    fn name(&self) -> String {
        "delete_path".into()
    }

    fn needs_confirmation(&self, _: &serde_json::Value, _: &Entity<Project>, _: &App) -> bool {
        false
    }

    fn may_perform_edits(&self) -> bool {
        true
    }

    fn description(&self) -> String {
        include_str!("./delete_path_tool/description.md").into()
    }

    fn icon(&self) -> IconName {
        IconName::ToolDeleteFile
    }

    fn input_schema(&self, format: LanguageModelToolSchemaFormat) -> Result<serde_json::Value> {
        json_schema_for::<DeletePathToolInput>(format)
    }

    fn ui_text(&self, input: &serde_json::Value) -> String {
        match serde_json::from_value::<DeletePathToolInput>(input.clone()) {
            Ok(input) => format!("Delete “`{}`”", input.path),
            Err(_) => "Delete path".to_string(),
        }
    }

    fn run(
        self: Arc<Self>,
        input: serde_json::Value,
        _request: Arc<LanguageModelRequest>,
        project: Entity<Project>,
        action_log: Entity<ActionLog>,
        _model: Arc<dyn LanguageModel>,
        _window: Option<AnyWindowHandle>,
        cx: &mut App,
    ) -> ToolResult {
        let path_str = match serde_json::from_value::<DeletePathToolInput>(input) {
            Ok(input) => input.path,
            Err(err) => return Task::ready(Err(anyhow!(err))).into(),
        };
        let Some(project_path) = project.read(cx).find_project_path(&path_str, cx) else {
            return Task::ready(Err(anyhow!(
                "Couldn't delete {path_str} because that path isn't in this project."
            )))
            .into();
        };

        let Some(worktree) = project
            .read(cx)
            .worktree_for_id(project_path.worktree_id, cx)
        else {
            return Task::ready(Err(anyhow!(
                "Couldn't delete {path_str} because that path isn't in this project."
            )))
            .into();
        };

        let worktree_snapshot = worktree.read(cx).snapshot();
        let (mut paths_tx, mut paths_rx) = mpsc::channel(256);
        cx.background_spawn({
            let project_path = project_path.clone();
            async move {
                for entry in
                    worktree_snapshot.traverse_from_path(true, false, false, &project_path.path)
                {
                    if !entry.path.starts_with(&project_path.path) {
                        break;
                    }
                    paths_tx
                        .send(ProjectPath {
                            worktree_id: project_path.worktree_id,
                            path: entry.path.clone(),
                        })
                        .await?;
                }
                anyhow::Ok(())
            }
        })
        .detach();

        cx.spawn(async move |cx| {
            while let Some(path) = paths_rx.next().await {
                if let Ok(buffer) = project
                    .update(cx, |project, cx| project.open_buffer(path, cx))?
                    .await
                {
                    action_log.update(cx, |action_log, cx| {
                        action_log.will_delete_buffer(buffer.clone(), cx)
                    })?;
                }
            }

            let deletion_task = project
                .update(cx, |project, cx| {
                    project.delete_file(project_path, false, cx)
                })?
                .with_context(|| {
                    format!("Couldn't delete {path_str} because that path isn't in this project.")
                })?;
            deletion_task
                .await
                .with_context(|| format!("Deleting {path_str}"))?;
            Ok(format!("Deleted {path_str}").into())
        })
        .into()
    }
}
