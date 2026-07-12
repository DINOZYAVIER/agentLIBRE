use std::io::Write;
use std::path::PathBuf;

use agl_functions::{
    FUNCTION_FILE_NAME, FUNCTION_SYSTEM_PROMPT_FILE_NAME, FunctionListEntry, FunctionSource,
    FunctionStatusReport, FunctionToolPolicy, LoadedFunction, function_status,
    global_functions_root, list_functions, load_function, resolve_function_reference,
    workspace_functions_root,
};
use agl_runtime::AgentLibreRuntimeConfig;
use anyhow::{Context, Result, bail};
use serde::Serialize;

use crate::args::{
    FunctionCommand, FunctionDoctorOptions, FunctionInitOptions, FunctionListOptions,
    FunctionShowOptions, FunctionStatusOptions,
};

pub(crate) fn run_function(
    command: FunctionCommand,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    match command {
        FunctionCommand::List(options) => run_function_list(options, runtime),
        FunctionCommand::Show(options) => run_function_show(options, runtime),
        FunctionCommand::Status(options) => run_function_status(options, runtime),
        FunctionCommand::Init(options) => run_function_init(options, runtime),
        FunctionCommand::Doctor(options) => run_function_doctor(options, runtime),
    }
}

fn run_function_list(
    options: FunctionListOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let report = FunctionListReport {
        workspace_root: workspace_root.clone(),
        workspace_functions_root: workspace_functions_root(&workspace_root),
        global_functions_root: global_functions_root(&runtime.paths.config_dir),
        functions: list_functions(&workspace_root, &runtime.paths.config_dir)?,
    };

    crate::print_json_or(options.json, &report, || {
        println!("state=ok");
        println!("workspace_root={}", report.workspace_root.display());
        println!(
            "workspace_functions_root={}",
            report.workspace_functions_root.display()
        );
        println!(
            "global_functions_root={}",
            report.global_functions_root.display()
        );
        for function in &report.functions {
            print_function_list_entry(function);
        }
        if report.functions.is_empty() {
            println!("next_step=agl function init coding --workspace");
        }
    })
}

fn run_function_show(
    options: FunctionShowOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let function = load_function(resolve_function_reference(
        &options.reference,
        &workspace_root,
        &runtime.paths.config_dir,
    )?)?;

    crate::print_json_or(options.json, &function, || print_loaded_function(&function))
}

fn run_function_status(
    options: FunctionStatusOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let report = function_status(
        &options.reference,
        &workspace_root,
        &runtime.paths.config_dir,
    );
    print_status_report(options.json, &report)?;
    if !report.errors.is_empty() {
        bail!("function status failed");
    }
    if options.strict && !report.warnings.is_empty() {
        bail!("function status has warnings");
    }
    Ok(())
}

fn run_function_init(
    options: FunctionInitOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let (source, root) = if options.workspace {
        let workspace_root = runtime.resolve_workspace_root(None)?;
        (
            FunctionSource::Workspace,
            workspace_functions_root(&workspace_root),
        )
    } else {
        (
            FunctionSource::Global,
            global_functions_root(&runtime.paths.config_dir),
        )
    };
    let function_dir = root.join(&options.id);
    let path = function_dir.join(FUNCTION_FILE_NAME);
    let system_prompt_path = function_dir.join(FUNCTION_SYSTEM_PROMPT_FILE_NAME);
    let subagents_dir = function_dir.join("subagents");
    std::fs::create_dir_all(&subagents_dir).with_context(|| {
        format!(
            "failed to create function directory {}",
            subagents_dir.display()
        )
    })?;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("failed to create function {}", path.display()))?;
    file.write_all(function_template(&options.id, options.model_profile.as_deref()).as_bytes())
        .with_context(|| format!("failed to write function {}", path.display()))?;
    let mut system_prompt_file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&system_prompt_path)
        .with_context(|| {
            format!(
                "failed to create function system prompt {}",
                system_prompt_path.display()
            )
        })?;
    system_prompt_file
        .write_all(function_system_prompt_template(&options.id).as_bytes())
        .with_context(|| {
            format!(
                "failed to write function system prompt {}",
                system_prompt_path.display()
            )
        })?;

    let report = FunctionInitReport {
        id: options.id,
        source: source.as_str().to_string(),
        path,
        system_prompt_path,
        subagents_dir,
        wrote: true,
        next_steps: vec![
            "agl function status <id>".to_string(),
            "agl chat --function <id>".to_string(),
        ],
    };
    crate::print_json_or(options.json, &report, || {
        println!("state=ok");
        println!("function.id={}", report.id);
        println!("function.source={}", report.source);
        println!("function.path={}", report.path.display());
        println!(
            "function.system_path={}",
            report.system_prompt_path.display()
        );
        println!("function.subagents_dir={}", report.subagents_dir.display());
        println!("wrote={}", report.wrote);
        for next_step in &report.next_steps {
            println!("next_step={next_step}");
        }
    })
}

fn run_function_doctor(
    options: FunctionDoctorOptions,
    runtime: &AgentLibreRuntimeConfig,
) -> Result<()> {
    let workspace_root = runtime.resolve_workspace_root(None)?;
    let report = function_status(
        &options.reference,
        &workspace_root,
        &runtime.paths.config_dir,
    );
    let smoke_prompt = if report.errors.is_empty() {
        load_function(resolve_function_reference(
            &options.reference,
            &workspace_root,
            &runtime.paths.config_dir,
        )?)
        .ok()
        .and_then(|function| {
            function
                .front_matter
                .doctor
                .and_then(|doctor| doctor.smoke_prompt)
        })
    } else {
        None
    };
    let doctor = FunctionDoctorReport {
        status: report,
        smoke_prompt,
    };

    crate::print_json_or(options.json, &doctor, || {
        print_function_status_report(&doctor.status);
        if let Some(prompt) = &doctor.smoke_prompt {
            println!("doctor.smoke_prompt={prompt}");
            println!(
                "next_step=agl run --function {} --prompt {:?}",
                doctor.status.reference, prompt
            );
        } else if doctor.status.errors.is_empty() {
            println!("warning=doctor.smoke_prompt not configured");
        }
    })?;

    if !doctor.status.errors.is_empty() {
        bail!("function doctor failed");
    }
    Ok(())
}

fn print_status_report(json: bool, report: &FunctionStatusReport) -> Result<()> {
    crate::print_json_or(json, report, || print_function_status_report(report))
}

fn print_function_list_entry(function: &FunctionListEntry) {
    println!(
        "function id={} source={} path={} valid={}",
        function.id,
        function.source.as_str(),
        function.path.display(),
        function.valid
    );
    if let Some(title) = &function.title {
        println!("function.{}.title={title}", function.id);
    }
    if let Some(error) = &function.error {
        println!("function.{}.error={error}", function.id);
    }
}

fn print_loaded_function(function: &LoadedFunction) {
    println!("function.id={}", function.front_matter.id);
    println!("function.title={}", function.front_matter.title);
    println!("function.source={}", function.locator.source.as_str());
    println!("function.path={}", function.locator.path.display());
    if let Some(description) = &function.front_matter.description {
        println!("function.description={description}");
    }
    if let Some(profile) = function.front_matter.model_profile() {
        println!("function.model.profile={profile}");
    }
    if let Some(path) = &function.inference_config_path {
        println!("function.model.config_path={}", path.display());
    }
    if let Some(tool_mode) = function.front_matter.runtime_tool_mode() {
        println!("function.runtime.tool_mode={}", tool_mode.as_str());
    }
    if let Some(max_output_tokens) = function.front_matter.runtime_max_output_tokens() {
        println!("function.runtime.max_output_tokens={max_output_tokens}");
    }
    let tool_policy = function.front_matter.tool_policy();
    print_function_tool_policy(tool_policy.as_ref());
    println!(
        "function.system_path={}",
        function.system_prompt_path.display()
    );
    for skill in function.front_matter.selected_skills() {
        println!("function.skill={skill}");
    }
    for subagent in &function.subagents {
        println!(
            "function.subagent id={} title={} path={}",
            subagent.front_matter.id,
            subagent.front_matter.title,
            subagent.path.display()
        );
    }
    println!("--- {} ---", FUNCTION_SYSTEM_PROMPT_FILE_NAME);
    println!("{}", function.system_prompt.trim());
    if let Some(config) = function
        .inference_config_toml
        .as_deref()
        .filter(|config| !config.trim().is_empty())
    {
        println!("--- inference.toml ---");
        println!("{}", config.trim());
    }
}

fn print_function_status_report(report: &FunctionStatusReport) {
    println!("state={}", report.state);
    println!("function.reference={}", report.reference);
    if let Some(source) = &report.source {
        println!("function.source={source}");
    }
    if let Some(path) = &report.path {
        println!("function.path={}", path.display());
    }
    if let Some(path) = &report.system_prompt_path {
        println!("function.system_path={}", path.display());
    }
    if let Some(id) = &report.id {
        println!("function.id={id}");
    }
    if let Some(title) = &report.title {
        println!("function.title={title}");
    }
    if let Some(profile) = &report.profile {
        println!("function.model.profile={profile}");
    }
    if let Some(profile_path) = &report.profile_path {
        println!("function.model.profile_path={}", profile_path.display());
    }
    if let Some(config_path) = &report.inference_config_path {
        println!("function.model.config_path={}", config_path.display());
        println!(
            "function.model.config_embedded={}",
            report.inference_config_embedded
        );
    }
    if let Some(model_path) = &report.inference_model_path {
        println!("function.model.path={}", model_path.display());
    }
    if let Some(model_exists) = report.inference_model_exists {
        println!("function.model.exists={model_exists}");
    }
    if report.id.is_some() {
        print_function_tool_policy(report.tool_policy.as_ref());
    }
    for skill in &report.skills {
        println!("function.skill={skill}");
    }
    for subagent in &report.subagents {
        println!(
            "function.subagent id={} title={} description={}",
            subagent.id, subagent.title, subagent.description
        );
    }
    for warning in &report.warnings {
        println!("warning={warning}");
    }
    for error in &report.errors {
        println!("error={error}");
    }
    for next_step in &report.next_steps {
        println!("next_step={next_step}");
    }
}

fn print_function_tool_policy(policy: Option<&FunctionToolPolicy>) {
    let Some(policy) = policy else {
        println!("function.tools.policy=inherit");
        return;
    };
    println!("function.tools.policy=explicit");
    println!(
        "function.tools.allow={}",
        policy
            .allow
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
    println!(
        "function.tools.deny={}",
        policy
            .deny
            .iter()
            .map(|id| id.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );
}

fn function_template(id: &str, model_profile: Option<&str>) -> String {
    let title = title_from_id(id);
    let model_profile = model_profile.unwrap_or("local");
    format!(
        r#"---
schema: agentfunction/v1
id: {id}
title: {title}
model:
  profile: {model_profile}
runtime:
  tool_mode: read-only
skills:
  use: []
subagents:
  use: []
doctor:
  smoke_prompt: "Summarize the current workspace and report visible tools."
---
"#
    )
}

fn function_system_prompt_template(id: &str) -> String {
    format!(
        r#"You are the `{id}` agentFUNCTION.

Inspect available agentLIBRE context before acting.
Keep changes small and explain repair steps when something is missing.
Use declared skills and subagents only when they are visible in the function context.
"#
    )
}

fn title_from_id(id: &str) -> String {
    let mut title = id.replace(['-', '_', '.'], " ");
    if let Some(first) = title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    title
}

#[derive(Serialize)]
struct FunctionListReport {
    workspace_root: PathBuf,
    workspace_functions_root: PathBuf,
    global_functions_root: PathBuf,
    functions: Vec<FunctionListEntry>,
}

#[derive(Serialize)]
struct FunctionInitReport {
    id: String,
    source: String,
    path: PathBuf,
    system_prompt_path: PathBuf,
    subagents_dir: PathBuf,
    wrote: bool,
    next_steps: Vec<String>,
}

#[derive(Serialize)]
struct FunctionDoctorReport {
    status: FunctionStatusReport,
    smoke_prompt: Option<String>,
}
