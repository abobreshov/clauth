//! Portable task records and opt-in local execution observations. Neither
//! declared nor host-observed identity alone transfers task ownership.

#[cfg(target_os = "linux")]
mod agy_session;
#[cfg(target_os = "linux")]
mod codex_session;
#[cfg(target_os = "linux")]
mod execution;
#[cfg(not(target_os = "linux"))]
#[path = "execution_unsupported.rs"]
mod execution;
#[cfg(target_os = "linux")]
mod grok_session;
#[cfg(target_os = "linux")]
mod native_session;
mod resources;
mod store;
mod workspace;

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use clap::{Args, Subcommand, ValueEnum};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::out::{errln, outln};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionMode {
    #[default]
    Direct,
    GrokSessionBinding,
    AgySessionBinding,
    CodexSessionBinding,
}

#[derive(Debug, Args)]
pub(crate) struct TaskArgs {
    #[command(subcommand)]
    command: TaskCommand,
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Register a task and its source session; emits a durable task ID as JSON.
    Register {
        /// JSON input; no credentials or environment dumps.
        #[arg(long = "from")]
        input: PathBuf,
    },
    /// Read the current task, checkpoint, declared resources and limitations.
    Show { task: String },
    /// Save a compacted brief that covers every constraint and declared resource.
    Checkpoint {
        task: String,
        /// Bind the brief to a bounded, observed-stable fingerprint of workspace content.
        #[arg(long)]
        capture_workspace: bool,
        #[command(flatten)]
        actor: MutationArgs,
    },
    /// Register a resource declaration; does not adopt, close or stop it.
    Resource {
        task: String,
        #[command(flatten)]
        actor: MutationArgs,
    },
    /// Read a Linux process identity for a resource declaration; does not register it.
    ProcessIdentity { pid: u32 },
    /// Read a local Herdr pane identity using an explicit socket; does not register it.
    HerdrIdentity {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long)]
        pane: String,
    },
    /// Re-observe one declared resource. Does not adopt, stop or grant control of it.
    InspectResource {
        task: String,
        #[arg(long)]
        resource: String,
    },
    /// Record explicit destination/model and data-sharing rules. Does not send any data.
    Policy {
        task: String,
        #[command(flatten)]
        actor: MutationArgs,
    },
    /// Re-observe workspace and check sharing policy; does not launch or transfer ownership.
    CheckHandoff {
        task: String,
        #[arg(long = "to")]
        tool: String,
        #[arg(long)]
        model: String,
    },
    /// Persist a checkpoint-bound transfer proposal; does not launch or grant ownership.
    ProposeHandoff {
        task: String,
        /// Stable caller-generated key; reuse it only for an exact retry.
        #[arg(long)]
        request_id: String,
        #[arg(long = "to")]
        tool: String,
        #[arg(long)]
        model: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Cancel an exact active proposal; no native process has been launched by it.
    CancelHandoff {
        task: String,
        #[arg(long)]
        handoff_id: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Classify declared resources against a source scope. Does not stop or close them.
    ClassifyResources {
        task: String,
        #[arg(long)]
        handoff_id: String,
        /// Cgroup v2 path of the source scope, such as `/user.slice/.../scope`.
        #[arg(long)]
        source_cgroup: String,
        /// Digest of the immutable source execution binding.
        #[arg(long)]
        binding_digest: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Re-probe resources after the source scope is empty. Does not execute cleanup text.
    ReconcileResources {
        task: String,
        #[arg(long)]
        handoff_id: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Launch the successor, deliver the checkpoint, and commit ownership after receipts read back.
    ContinueHandoff {
        task: String,
        #[arg(long)]
        handoff_id: String,
        #[arg(long)]
        target: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Attach to a saved successor or refuse to start another. Does not send a second prompt.
    RecoverHandoff {
        task: String,
        #[arg(long)]
        handoff_id: String,
    },
    /// Stop-first source-scope release from an external coordinator. Does not launch a successor.
    ReleaseHandoffSource {
        task: String,
        #[arg(long)]
        handoff_id: String,
        #[arg(long)]
        execution_id: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Run the source tool in a recorded Linux user scope; not a completed handoff.
    Run {
        task: String,
        #[arg(long)]
        target: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
        /// Native arguments after --. Native terminal streams remain attached.
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Inspect the task's recorded local process scope without stopping it.
    Execution { task: String },
    /// Observe a registered Codex, Grok or agy session; sends no prompt or handoff.
    BindSession {
        task: String,
        #[arg(long)]
        target: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    /// Stop only the task's recorded scope, including local background writers.
    StopExecution {
        task: String,
        /// Exact run observed before requesting shutdown; reject a replaced run.
        #[arg(long)]
        execution_id: String,
        #[command(flatten)]
        actor: ExecutionActorArgs,
    },
    #[command(name = "__scope-worker", hide = true)]
    ScopeWorker { task: String, execution_id: String },
    #[command(name = "__successor-worker", hide = true)]
    SuccessorWorker { task: String, handoff_id: String },
    /// Print an input template. These commands do not transfer task ownership.
    Example { kind: Template },
}

#[derive(Debug, Args)]
struct ExecutionActorArgs {
    /// Registered source session ID; consistency check, not authentication.
    #[arg(long)]
    session: String,
    /// Reject a request based on an outdated task record.
    #[arg(long)]
    expected_generation: u64,
}

#[derive(Debug, Args)]
struct MutationArgs {
    /// Recorded native source session ID. This is a consistency check, not authentication.
    #[arg(long)]
    session: String,
    /// Reject updates if another writer has changed the task since it was read.
    #[arg(long)]
    expected_generation: u64,
    /// Explicit checkpoint/resource JSON input file.
    #[arg(long = "from")]
    input: PathBuf,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Template {
    Task,
    Checkpoint,
    Resource,
    Policy,
}

fn read_input<T: DeserializeOwned>(path: &Path) -> Result<T> {
    const LIMIT: u64 = 128 * 1024;
    let file =
        std::fs::File::open(path).map_err(|_| anyhow::anyhow!("cannot read task input file"))?;
    let mut bytes = Vec::new();
    file.take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| anyhow::anyhow!("cannot read task input file"))?;
    if bytes.len() as u64 > LIMIT {
        bail!("task input exceeds 128 KiB");
    }
    // serde parser messages may include values accidentally pasted from an auth
    // file. The caller gets schema guidance, never raw input or parser excerpts.
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid task input JSON; see `clauth tasks example`"))
}

pub(crate) fn run(args: TaskArgs) -> Result<()> {
    let value = match args.command {
        TaskCommand::Register { input } => {
            serde_json::to_value(store::register(read_input(&input)?)?)?
        }
        TaskCommand::Show { task } => serde_json::to_value(store::show(&task)?)?,
        TaskCommand::Checkpoint {
            task,
            actor,
            capture_workspace,
        } => {
            let save = if capture_workspace {
                store::checkpoint_capturing_workspace
            } else {
                store::checkpoint
            };
            serde_json::to_value(save(
                &task,
                &actor.session,
                actor.expected_generation,
                read_input(&actor.input)?,
            )?)?
        }
        TaskCommand::Resource { task, actor } => serde_json::to_value(store::register_resource(
            &task,
            &actor.session,
            actor.expected_generation,
            read_input(&actor.input)?,
        )?)?,
        TaskCommand::ProcessIdentity { pid } => {
            serde_json::to_value(resources::process_identity(pid)?)?
        }
        TaskCommand::HerdrIdentity { socket, pane } => {
            serde_json::to_value(resources::herdr_identity(&socket, &pane)?)?
        }
        TaskCommand::InspectResource { task, resource } => {
            serde_json::to_value(resources::inspect(&task, &resource)?)?
        }
        TaskCommand::Policy { task, actor } => serde_json::to_value(store::set_policy(
            &task,
            &actor.session,
            actor.expected_generation,
            read_input(&actor.input)?,
        )?)?,
        TaskCommand::CheckHandoff { task, tool, model } => {
            serde_json::to_value(store::check_handoff(&task, &tool, &model)?)?
        }
        TaskCommand::ProposeHandoff {
            task,
            request_id,
            tool,
            model,
            actor,
        } => serde_json::to_value(store::propose_handoff(
            &task,
            &actor.session,
            actor.expected_generation,
            &request_id,
            &tool,
            &model,
        )?)?,
        TaskCommand::CancelHandoff {
            task,
            handoff_id,
            actor,
        } => serde_json::to_value(store::cancel_handoff(
            &task,
            &actor.session,
            actor.expected_generation,
            &handoff_id,
        )?)?,
        TaskCommand::Run {
            task,
            target,
            actor,
            args,
        } => {
            let record =
                store::check_current_actor(&task, &actor.session, actor.expected_generation)?;
            if !store::source_launch_allowed(&record) {
                bail!("managed source launch is frozen for this handoff");
            }
            let (program, args) =
                execution_command(&record, &target, &args, ExecutionMode::Direct)?;
            let report = execution::run(
                &task,
                &actor.session,
                actor.expected_generation,
                &program,
                &args,
                ExecutionMode::Direct,
            )?;
            // Native stdout may be an interactive terminal or structured model
            // output. Do not mix the supervisor's JSON into that stream.
            errln!(
                "clauth local execution: {}",
                serde_json::to_string(&report)?
            );
            if report.exit_code != Some(0) {
                bail!(
                    "native tool did not exit successfully; inspect `clauth tasks execution` for local process state"
                );
            }
            return Ok(());
        }
        TaskCommand::ClassifyResources {
            task,
            handoff_id,
            source_cgroup,
            binding_digest,
            actor,
        } => serde_json::to_value(store::classify_resources(
            &task,
            &actor.session,
            actor.expected_generation,
            &handoff_id,
            &source_cgroup,
            &binding_digest,
        )?)?,
        TaskCommand::ReconcileResources {
            task,
            handoff_id,
            actor,
        } => serde_json::to_value(store::reconcile_resources(
            &task,
            &actor.session,
            actor.expected_generation,
            &handoff_id,
        )?)?,
        TaskCommand::ContinueHandoff {
            task,
            handoff_id,
            target,
            actor,
        } => {
            let record = store::show(&task)?;
            let (tool, model) = store::handoff_destination(&record, &handoff_id)
                .ok_or_else(|| anyhow::anyhow!("continue requires the active handoff"))?;
            let (program, _args) = successor_program(&record, &target, tool, model)?;
            let value = store::continue_handoff(
                &task,
                &actor.session,
                actor.expected_generation,
                &handoff_id,
                &program,
            )?;
            serde_json::to_value(value)?
        }
        TaskCommand::RecoverHandoff { task, handoff_id } => {
            serde_json::to_value(store::recover_handoff(&task, &handoff_id)?)?
        }
        TaskCommand::ReleaseHandoffSource {
            task,
            handoff_id,
            execution_id,
            actor,
        } => {
            let record = execution::release_handoff_source(
                &task,
                &actor.session,
                actor.expected_generation,
                &handoff_id,
                &execution_id,
            )?;
            if !record.source_scope_released(&handoff_id) {
                outln!("{}", serde_json::to_string_pretty(&record)?);
                bail!(
                    "source release parked; inspect the recorded handoff and reconcile the exact intent; no successor was launched"
                );
            }
            serde_json::to_value(record)?
        }
        TaskCommand::Execution { task } => serde_json::to_value(execution::inspect(&task)?)?,
        TaskCommand::BindSession {
            task,
            target,
            actor,
        } => {
            let record =
                store::check_current_actor(&task, &actor.session, actor.expected_generation)?;
            if !store::source_launch_allowed(&record) {
                bail!("managed source launch is frozen for this handoff");
            }
            let mode = match record.owner.tool.as_str() {
                "grok" => ExecutionMode::GrokSessionBinding,
                "agy" => ExecutionMode::AgySessionBinding,
                "codex" => ExecutionMode::CodexSessionBinding,
                _ => bail!("native session binding currently supports only Codex, Grok and agy"),
            };
            let (program, args) = execution_command(&record, &target, &[], mode)?;
            let report = execution::run(
                &task,
                &actor.session,
                actor.expected_generation,
                &program,
                &args,
                mode,
            )?;
            if report.exit_code != Some(0) {
                bail!(
                    "native session binding failed; inspect `clauth tasks execution` for retained execution evidence"
                );
            }
            serde_json::to_value(report)?
        }
        TaskCommand::StopExecution {
            task,
            execution_id,
            actor,
        } => {
            store::check_current_actor(&task, &actor.session, actor.expected_generation)?;
            serde_json::to_value(execution::stop(
                &task,
                &actor.session,
                actor.expected_generation,
                &execution_id,
            )?)?
        }
        TaskCommand::ScopeWorker { task, execution_id } => {
            return execution::worker(&task, &execution_id);
        }
        TaskCommand::SuccessorWorker { task, handoff_id } => {
            return store::successor_worker(&task, &handoff_id);
        }
        TaskCommand::Example { kind } => example(kind),
    };
    outln!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn execution_command(
    record: &store::TaskRecord,
    id: &str,
    extra: &[String],
    mode: ExecutionMode,
) -> Result<(PathBuf, Vec<String>)> {
    let config = crate::provider_monitor::config::load()?;
    let target = config
        .targets
        .iter()
        .find(|target| target.id == id)
        .ok_or_else(|| anyhow::anyhow!("provider target not found in providers.toml"))?;
    if !target.enabled {
        bail!("provider target is disabled");
    }
    if target.provider.tool() != record.owner.tool {
        bail!(
            "source execution target must match the registered source tool; this command does not transfer ownership"
        );
    }
    if target.auth_file.is_some() || target.auth_entry.is_some() {
        bail!(
            "custom auth_file/auth_entry targets are monitoring-only; native account selection is not verified"
        );
    }
    if mode != ExecutionMode::Direct && (!target.args.is_empty() || !extra.is_empty()) {
        bail!(
            "native session binding does not accept configured arguments or extra native arguments"
        );
    }
    let program = match &target.command {
        Some(command) => crate::provider_monitor::config::expand(command)?,
        None => PathBuf::from(target.provider.tool()),
    };
    // Bind the executable path before persisting the gated launch. Resolve
    // relative PATH entries against the same cwd the native tool will use.
    let program = which::which_in(&program, std::env::var_os("PATH"), &record.workspace)
        .map_err(|_| anyhow::anyhow!("configured native executable is unavailable"))?;
    let mut args = match mode {
        ExecutionMode::GrokSessionBinding => vec!["agent".into(), "--no-leader".into()],
        ExecutionMode::AgySessionBinding => vec![
            "--input-format=stream-json".into(),
            "--output-format=stream-json".into(),
            "--conversation".into(),
            record.owner.native_session_id.clone(),
        ],
        ExecutionMode::Direct => target.args.clone(),
        ExecutionMode::CodexSessionBinding => {
            vec!["app-server".into(), "--listen".into(), "stdio://".into()]
        }
    };
    if let Some(model) = &target.model {
        if mode == ExecutionMode::CodexSessionBinding {
            args.extend([
                "--config".into(),
                format!("model={}", serde_json::to_string(model)?),
            ]);
        } else {
            args.extend(["--model".into(), model.clone()]);
        }
    }
    args.extend_from_slice(extra);
    if mode == ExecutionMode::GrokSessionBinding {
        args.push("stdio".into());
    }
    Ok((program, args))
}

fn successor_program(
    record: &store::TaskRecord,
    id: &str,
    tool: &str,
    model: &str,
) -> Result<(PathBuf, Vec<String>)> {
    let config = crate::provider_monitor::config::load()?;
    let target = config
        .targets
        .iter()
        .find(|target| target.id == id)
        .ok_or_else(|| anyhow::anyhow!("provider target not found in providers.toml"))?;
    if !target.enabled {
        bail!("provider target is disabled");
    }
    if target.provider.tool() != tool {
        bail!("successor target does not match the handoff destination");
    }
    if target.auth_file.is_some() || target.auth_entry.is_some() || !target.args.is_empty() {
        bail!("successor target cannot add arguments or select a custom auth store");
    }
    if target
        .model
        .as_ref()
        .is_some_and(|configured| configured != model)
    {
        bail!("successor target model does not match the handoff");
    }
    let program = match &target.command {
        Some(command) => crate::provider_monitor::config::expand(command)?,
        None => PathBuf::from(tool),
    };
    let program = which::which_in(&program, std::env::var_os("PATH"), &record.workspace)
        .map_err(|_| anyhow::anyhow!("configured native executable is unavailable"))?;
    Ok((program, Vec::new()))
}

fn example(kind: Template) -> Value {
    match kind {
        Template::Task => json!({
            "objective":"Describe the original task and its acceptance criteria",
            "workspace":"/absolute/path/to/project",
            "constraints":["Preserve unrelated user changes"],
            "source": {
                "tool":"codex", "model":null,
                "native_session_id":"replace-with-actual-source-session-id", "account_ref":null
            }
        }),
        Template::Checkpoint => json!({
            "brief":"A compact, provider-neutral account of the current state",
            "completed":[], "remaining_plan":["Describe the remaining acceptance check"],
            "decisions":[], "uncertainties":[],
            "next_action":"Describe the exact next safe action",
            "constraints":["Preserve unrelated user changes"],
            "resource_ids":[]
        }),
        Template::Resource => json!({
            "id":"tests", "kind":"herdr_pane",
            "native_identity": {
                "instance":"replace-with-Herdr-instance",
                "session":"replace-with-Herdr-session",
                "workspace_id":"replace-with-workspace-id",
                "tab_id":"replace-with-tab-id", "pane_id":"replace-with-pane-id"
            },
            "purpose":"Existing test process and its output",
            "ownership":"task", "disposition":"adopt", "may_write":false,
            "reconnect":"Verify the native pane identity before reading its output",
            "cleanup":null
        }),
        Template::Policy => json!({
            "destinations":[],
            "share_checkpoint":false,
            "share_workspace":false,
            "share_resource_metadata":false
        }),
    }
}
