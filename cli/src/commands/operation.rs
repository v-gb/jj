// Copyright 2020-2023 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::Write as _;
use std::slice;

use clap::Subcommand;
use itertools::Itertools as _;
use jj_lib::object_id::ObjectId;
use jj_lib::op_store::OperationId;
use jj_lib::op_walk;
use jj_lib::repo::Repo;

use crate::cli_util::{
    user_error, user_error_with_hint, CommandError, CommandHelper, LogContentFormat,
};
use crate::graphlog::{get_graphlog, Edge};
use crate::operation_templater;
use crate::templater::Template as _;
use crate::ui::Ui;

/// Commands for working with the operation log
///
/// For information about the operation log, see
/// https://github.com/martinvonz/jj/blob/main/docs/operation-log.md.
#[derive(Subcommand, Clone, Debug)]
pub enum OperationCommand {
    Abandon(OperationAbandonArgs),
    Log(OperationLogArgs),
    Undo(OperationUndoArgs),
    Restore(OperationRestoreArgs),
}

/// Show the operation log
#[derive(clap::Args, Clone, Debug)]
pub struct OperationLogArgs {
    /// Limit number of operations to show
    #[arg(long, short)]
    limit: Option<usize>,
    /// Don't show the graph, show a flat list of operations
    #[arg(long)]
    no_graph: bool,
    /// Render each operation using the given template
    ///
    /// For the syntax, see https://github.com/martinvonz/jj/blob/main/docs/templates.md
    #[arg(long, short = 'T')]
    template: Option<String>,
}

/// Create a new operation that restores the repo to an earlier state
///
/// This restores the repo to the state at the specified operation, effectively
/// undoing all later operations. It does so by creating a new operation.
#[derive(clap::Args, Clone, Debug)]
pub struct OperationRestoreArgs {
    /// The operation to restore to
    ///
    /// Use `jj op log` to find an operation to restore to. Use e.g. `jj
    /// --at-op=<operation ID> log` before restoring to an operation to see the
    /// state of the repo at that operation.
    operation: String,

    /// What portions of the local state to restore (can be repeated)
    ///
    /// This option is EXPERIMENTAL.
    #[arg(long, value_enum, default_values_t = DEFAULT_UNDO_WHAT)]
    what: Vec<UndoWhatToRestore>,
}

/// Create a new operation that undoes an earlier operation
///
/// This undoes an individual operation by applying the inverse of the
/// operation.
#[derive(clap::Args, Clone, Debug)]
pub struct OperationUndoArgs {
    /// The operation to undo
    ///
    /// Use `jj op log` to find an operation to undo.
    #[arg(default_value = "@")]
    operation: String,

    /// What portions of the local state to restore (can be repeated)
    ///
    /// This option is EXPERIMENTAL.
    #[arg(long, value_enum, default_values_t = DEFAULT_UNDO_WHAT)]
    what: Vec<UndoWhatToRestore>,
}

/// Abandon operation history
///
/// To discard old operation history, use `jj op abandon ..<operation ID>`. It
/// will abandon the specified operation and all its ancestors. The descendants
/// will be reparented onto the root operation.
///
/// To discard recent operations, use `jj op restore <operation ID>` followed
/// by `jj op abandon <operation ID>..@-`.
///
/// The abandoned operations, commits, and other unreachable objects can later
/// be garbage collected by using `jj util gc` command.
#[derive(clap::Args, Clone, Debug)]
pub struct OperationAbandonArgs {
    /// The operation or operation range to abandon
    operation: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
enum UndoWhatToRestore {
    /// The jj repo state and local branches
    Repo,
    /// The remote-tracking branches. Do not restore these if you'd like to push
    /// after the undo
    RemoteTracking,
}

const DEFAULT_UNDO_WHAT: [UndoWhatToRestore; 2] =
    [UndoWhatToRestore::Repo, UndoWhatToRestore::RemoteTracking];

fn cmd_op_log(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &OperationLogArgs,
) -> Result<(), CommandError> {
    let workspace_command = command.workspace_helper(ui)?;
    let repo = workspace_command.repo();
    let head_op = repo.operation().clone();
    let head_op_id = head_op.id().clone();

    let template_string = match &args.template {
        Some(value) => value.to_owned(),
        None => command.settings().config().get_string("templates.op_log")?,
    };
    let template = operation_templater::parse(
        repo,
        &template_string,
        workspace_command.template_aliases_map(),
    )?;
    let with_content_format = LogContentFormat::new(ui, command.settings())?;

    ui.request_pager();
    let mut formatter = ui.stdout_formatter();
    let formatter = formatter.as_mut();
    let iter = op_walk::walk_ancestors(&[head_op]).take(args.limit.unwrap_or(usize::MAX));
    if !args.no_graph {
        let mut graph = get_graphlog(command.settings(), formatter.raw());
        let default_node_symbol = graph.default_node_symbol().to_owned();
        for op in iter {
            let op = op?;
            let mut edges = vec![];
            for id in op.parent_ids() {
                edges.push(Edge::direct(id.clone()));
            }
            let is_head_op = op.id() == &head_op_id;
            let mut buffer = vec![];
            with_content_format.write_graph_text(
                ui.new_formatter(&mut buffer).as_mut(),
                |formatter| {
                    formatter.with_label("op_log", |formatter| template.format(&op, formatter))
                },
                || graph.width(op.id(), &edges),
            )?;
            if !buffer.ends_with(b"\n") {
                buffer.push(b'\n');
            }
            let node_symbol = if is_head_op {
                "@"
            } else {
                &default_node_symbol
            };
            graph.add_node(
                op.id(),
                &edges,
                node_symbol,
                &String::from_utf8_lossy(&buffer),
            )?;
        }
    } else {
        for op in iter {
            let op = op?;
            with_content_format.write(formatter, |formatter| {
                formatter.with_label("op_log", |formatter| template.format(&op, formatter))
            })?;
        }
    }

    Ok(())
}

/// Restore only the portions of the view specified by the `what` argument
fn view_with_desired_portions_restored(
    view_being_restored: &jj_lib::op_store::View,
    current_view: &jj_lib::op_store::View,
    what: &[UndoWhatToRestore],
) -> jj_lib::op_store::View {
    let repo_source = if what.contains(&UndoWhatToRestore::Repo) {
        view_being_restored
    } else {
        current_view
    };
    let remote_source = if what.contains(&UndoWhatToRestore::RemoteTracking) {
        view_being_restored
    } else {
        current_view
    };
    jj_lib::op_store::View {
        head_ids: repo_source.head_ids.clone(),
        public_head_ids: repo_source.public_head_ids.clone(),
        local_branches: repo_source.local_branches.clone(),
        tags: repo_source.tags.clone(),
        remote_views: remote_source.remote_views.clone(),
        git_refs: current_view.git_refs.clone(),
        git_head: current_view.git_head.clone(),
        wc_commit_ids: repo_source.wc_commit_ids.clone(),
    }
}

pub fn cmd_op_undo(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &OperationUndoArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let bad_op = workspace_command.resolve_single_op(&args.operation)?;
    let mut parent_ops = bad_op.parents();
    let Some(parent_op) = parent_ops.next().transpose()? else {
        return Err(user_error("Cannot undo repo initialization"));
    };
    if parent_ops.next().is_some() {
        return Err(user_error("Cannot undo a merge operation"));
    }

    let mut tx = workspace_command.start_transaction();
    let repo_loader = tx.base_repo().loader();
    let bad_repo = repo_loader.load_at(&bad_op)?;
    let parent_repo = repo_loader.load_at(&parent_op)?;
    tx.mut_repo().merge(&bad_repo, &parent_repo);
    let new_view = view_with_desired_portions_restored(
        tx.repo().view().store_view(),
        tx.base_repo().view().store_view(),
        &args.what,
    );
    tx.mut_repo().set_view(new_view);
    tx.finish(ui, format!("undo operation {}", bad_op.id().hex()))?;

    Ok(())
}

fn cmd_op_restore(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &OperationRestoreArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let target_op = workspace_command.resolve_single_op(&args.operation)?;
    let mut tx = workspace_command.start_transaction();
    let new_view = view_with_desired_portions_restored(
        target_op.view()?.store_view(),
        tx.base_repo().view().store_view(),
        &args.what,
    );
    tx.mut_repo().set_view(new_view);
    tx.finish(ui, format!("restore to operation {}", target_op.id().hex()))?;

    Ok(())
}

fn cmd_op_abandon(
    ui: &mut Ui,
    command: &CommandHelper,
    args: &OperationAbandonArgs,
) -> Result<(), CommandError> {
    let mut workspace_command = command.workspace_helper(ui)?;
    let repo = workspace_command.repo();
    let current_head_op = repo.operation();
    let (abandon_root_op, abandon_head_op) =
        if let Some((root_op_str, head_op_str)) = args.operation.split_once("..") {
            let root_op = if root_op_str.is_empty() {
                // TODO: Introduce a virtual root operation and use it instead.
                op_walk::walk_ancestors(slice::from_ref(current_head_op))
                    .last()
                    .unwrap()?
            } else {
                workspace_command.resolve_single_op(root_op_str)?
            };
            let head_op = if head_op_str.is_empty() {
                current_head_op.clone()
            } else {
                workspace_command.resolve_single_op(head_op_str)?
            };
            (root_op, head_op)
        } else {
            let op = workspace_command.resolve_single_op(&args.operation)?;
            let parent_ops: Vec<_> = op.parents().try_collect()?;
            let parent_op = match parent_ops.len() {
                0 => return Err(user_error("Cannot abandon the root operation")),
                1 => parent_ops.into_iter().next().unwrap(),
                _ => return Err(user_error("Cannot abandon a merge operation")),
            };
            (parent_op, op)
        };

    if abandon_head_op == *current_head_op {
        return Err(user_error_with_hint(
            "Cannot abandon the current operation",
            "Run `jj undo` to revert the current operation, then use `jj op abandon`",
        ));
    }

    // Reparent descendants, count the number of abandoned operations.
    let stats = op_walk::reparent_range(
        repo.op_store().as_ref(),
        slice::from_ref(&abandon_head_op),
        slice::from_ref(current_head_op),
        &abandon_root_op,
    )?;
    let [new_head_id]: [OperationId; 1] = stats.new_head_ids.try_into().unwrap();
    if current_head_op.id() == &new_head_id {
        writeln!(ui.stderr(), "Nothing changed.")?;
        return Ok(());
    }
    writeln!(
        ui.stderr(),
        "Abandoned {} operations and reparented {} descendant operations.",
        stats.unreachable_count,
        stats.rewritten_count,
    )?;
    repo.op_heads_store()
        .update_op_heads(slice::from_ref(current_head_op.id()), &new_head_id);
    // Remap the operation id of the current workspace. If there were any
    // concurrent operations, user will need to re-abandon their ancestors.
    let (locked_ws, _) = workspace_command.start_working_copy_mutation()?;
    locked_ws.finish(new_head_id)?;
    Ok(())
}

pub fn cmd_operation(
    ui: &mut Ui,
    command: &CommandHelper,
    subcommand: &OperationCommand,
) -> Result<(), CommandError> {
    match subcommand {
        OperationCommand::Abandon(args) => cmd_op_abandon(ui, command, args),
        OperationCommand::Log(args) => cmd_op_log(ui, command, args),
        OperationCommand::Restore(args) => cmd_op_restore(ui, command, args),
        OperationCommand::Undo(args) => cmd_op_undo(ui, command, args),
    }
}
