use std::collections::HashMap;
use std::fs::read_to_string;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{bail, Context, Result};
use assert_cmd::prelude::*;

use chrono::Local;
use handlebars::Handlebars;
use pueue_lib::settings::*;
use pueue_lib::task::TaskStatus;

use super::get_state;

/// Spawn a client command that connects to a specific daemon.
pub fn run_client_command(shared: &Shared, args: &[&str]) -> Result<Output> {
    // Inject an environment variable into the pueue command.
    // This is used to ensure that the environment is properly captured and forwarded.
    let mut envs = HashMap::new();
    envs.insert("PUEUED_TEST_ENV_VARIABLE", "Test");

    run_client_command_with_env(shared, args, envs)
}

/// Run the status command without the path being included in the output.
pub async fn run_status_without_path(shared: &Shared, args: &[&str]) -> Result<Output> {
    // Inject an environment variable into the pueue command.
    // This is used to ensure that the environment is properly captured and forwarded.
    let mut envs = HashMap::new();
    envs.insert("PUEUED_TEST_ENV_VARIABLE", "Test");

    let state = get_state(shared).await?;
    println!("{state:?}");
    let mut base_args = vec!["status"];

    // Since we want to exclude the path, we have to manually assemble the
    // list of columns that should be displayed.
    // We start with the base columns, check which optional columns should be
    // included based on the current task list and add any of those columns at
    // the correct position.
    let mut columns = vec!["id,status"];

    // Add the enqueue_at column if necessary.
    if state.tasks.iter().any(|(_, task)| {
        if let TaskStatus::Stashed { enqueue_at } = task.status {
            return enqueue_at.is_some();
        }
        false
    }) {
        columns.push("enqueue_at");
    }

    // Add the `deps` column if necessary.
    if state
        .tasks
        .iter()
        .any(|(_, task)| !task.dependencies.is_empty())
    {
        columns.push("dependencies");
    }

    // Add the `label` column if necessary.
    if state.tasks.iter().any(|(_, task)| task.label.is_some()) {
        columns.push("label");
    }

    // Add the remaining base columns.
    columns.extend_from_slice(&["command", "start", "end"]);

    let column_filter = format!("columns={}", columns.join(","));
    base_args.push(&column_filter);

    base_args.extend_from_slice(args);
    run_client_command_with_env(shared, &base_args, envs)
}

/// Spawn a client command that connects to a specific daemon.
/// Accepts a list of environment variables that'll be injected into the client's env.
pub fn run_client_command_with_env(
    shared: &Shared,
    args: &[&str],
    envs: HashMap<&str, &str>,
) -> Result<Output> {
    let output = Command::cargo_bin("pueue")?
        .arg("--config")
        .arg(shared.pueue_directory().join("pueue.yml").to_str().unwrap())
        .args(args)
        .envs(envs)
        .current_dir(shared.pueue_directory())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context(format!("Failed to execute pueue with {:?}", args))?;

    if !output.status.success() {
        bail!(
            "Command failed to run.\nCommand: {args:?}\n\nstdout:\n{}\n\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }

    Ok(output)
}

/// Read the current state and extract the tasks' info into a context.
pub async fn get_task_context(settings: &Settings) -> Result<HashMap<String, String>> {
    // Get the current state
    let state = get_state(&settings.shared).await?;

    let mut context = HashMap::new();

    // Get the current daemon cwd.
    context.insert(
        "cwd".to_string(),
        settings
            .shared
            .pueue_directory()
            .to_string_lossy()
            .to_string(),
    );

    for (id, task) in state.tasks {
        let task_name = format!("task_{}", id);

        if let Some(start) = task.start {
            // Use datetime format for datetimes that aren't today.
            let format = if start.date_naive() == Local::now().date_naive() {
                &settings.client.status_time_format
            } else {
                &settings.client.status_datetime_format
            };

            let formatted = start.format(format).to_string();
            context.insert(format!("{task_name}_start"), formatted);
            context.insert(format!("{task_name}_start_long"), start.to_rfc2822());
        }
        if let Some(end) = task.end {
            // Use datetime format for datetimes that aren't today.
            let format = if end.date_naive() == Local::now().date_naive() {
                &settings.client.status_time_format
            } else {
                &settings.client.status_datetime_format
            };

            let formatted = end.format(format).to_string();
            context.insert(format!("{task_name}_end"), formatted);
            context.insert(format!("{task_name}_end_long"), end.to_rfc2822());
        }
        if let Some(label) = &task.label {
            context.insert(format!("{task_name}_label"), label.to_string());
        }

        if let TaskStatus::Stashed {
            enqueue_at: Some(enqueue_at),
        } = task.status
        {
            // Use datetime format for datetimes that aren't today.
            let format = if enqueue_at.date_naive() == Local::now().date_naive() {
                &settings.client.status_time_format
            } else {
                &settings.client.status_datetime_format
            };

            let enqueue_at = enqueue_at.format(format);
            context.insert(format!("{task_name}_enqueue_at"), enqueue_at.to_string());
        }
    }

    Ok(context)
}

/// This function takes the name of a snapshot template, applies a given context to the template
/// and compares it with a given `stdout`.
pub fn assert_stdout_matches(
    name: &str,
    stdout: Vec<u8>,
    context: HashMap<String, String>,
) -> Result<()> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("client")
        .join("snapshots")
        .join(name);

    let actual = String::from_utf8(stdout).context("Got invalid utf8 as stdout!")?;

    let template = read_to_string(&path);
    let template = match template {
        Ok(template) => template,
        Err(_) => {
            println!("Actual output:\n{actual}");
            bail!("Failed to read template file {path:?}")
        }
    };

    // Init Handlebars. We set to strict, as we want to show an error on missing variables.
    let mut handlebars = Handlebars::new();
    handlebars.set_strict_mode(true);

    let expected = handlebars
        .render_template(&template, &context)
        .context(format!(
            "Failed to render template for file: {name} with context {context:?}"
        ))?;

    let expected = expected
        .lines()
        .map(|line| line.trim().to_owned())
        .collect::<Vec<String>>()
        .join("\n");

    let actual = actual
        .lines()
        .map(|line| line.trim().to_owned())
        .collect::<Vec<String>>()
        .join("\n");

    if expected != actual {
        println!("Expected output:\n-----\n{expected}\n-----");
        println!("\nGot output:\n-----\n{actual}\n-----");
        println!(
            "\n{}",
            similar_asserts::SimpleDiff::from_str(&expected, &actual, "expected", "actual")
        );
        bail!("The stdout of the command doesn't match the expected string");
    }

    Ok(())
}

fn is_table(line: &str) -> bool {
    line.chars().all(|c| c == '\u{2500}')
}