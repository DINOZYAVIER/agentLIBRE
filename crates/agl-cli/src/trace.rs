use std::fs::{self, File};
use std::io::{BufReader, Write};

use agl_events::{
    SemanticContentRef, SemanticTraceIdentity, export_semantic_trace, replay_semantic_trace,
};
use anyhow::{Context, Result, bail};

use crate::args::{TraceCommand, TraceExportOptions, TraceReplayOptions};

pub(crate) fn run_trace(command: TraceCommand) -> Result<()> {
    match command {
        TraceCommand::Export(options) => export(options),
        TraceCommand::Replay(options) => replay(options),
    }
}

fn export(options: TraceExportOptions) -> Result<()> {
    let identity: SemanticTraceIdentity = read_json(&options.identity, "trace identity")?;
    let content_refs = options
        .content_refs
        .as_deref()
        .map(|path| read_json::<Vec<SemanticContentRef>>(path, "content references"))
        .transpose()?
        .unwrap_or_default();
    let events = File::open(&options.events).with_context(|| {
        format!(
            "failed to open canonical runtime event log {}",
            options.events.display()
        )
    })?;
    let trace = export_semantic_trace(BufReader::new(events), identity, content_refs)
        .context("failed to export canonical semantic trace")?;
    let encoded = trace.render()?;
    write_atomic(&options.out, encoded.as_bytes())?;
    println!(
        "exported {} canonical events to {} ({})",
        trace.events.len(),
        options.out.display(),
        trace.trace_digest
    );
    Ok(())
}

fn replay(options: TraceReplayOptions) -> Result<()> {
    let identity: SemanticTraceIdentity = read_json(&options.identity, "trace identity")?;
    let encoded = fs::read_to_string(&options.trace)
        .with_context(|| format!("failed to read semantic trace {}", options.trace.display()))?;
    let report = replay_semantic_trace(&encoded, &identity)
        .context("failed to replay canonical semantic trace")?;
    if options.json {
        crate::print_json(&report)?;
    } else if report.matches() {
        println!(
            "semantic replay matched {} events without executing effects",
            report.event_count
        );
    } else {
        for drift in &report.drifts {
            eprintln!(
                "drift {}: expected {}, trace {}",
                drift.field, drift.expected, drift.actual
            );
        }
    }
    if !report.matches() {
        bail!(
            "semantic replay detected {} identity drift(s)",
            report.drifts.len()
        );
    }
    Ok(())
}

fn read_json<T: serde::de::DeserializeOwned>(
    path: &std::path::Path,
    description: &str,
) -> Result<T> {
    let encoded = fs::read_to_string(path)
        .with_context(|| format!("failed to read {description} {}", path.display()))?;
    serde_json::from_str(&encoded)
        .with_context(|| format!("failed to decode {description} {}", path.display()))
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create trace directory {}", parent.display()))?;
    }
    let file_name = path
        .file_name()
        .context("semantic trace output path must have a file name")?
        .to_string_lossy();
    let temporary = path.with_file_name(format!(".{file_name}.tmp-{}", std::process::id()));
    let write_result = (|| -> Result<()> {
        let mut file = File::create(&temporary).with_context(|| {
            format!(
                "failed to create temporary semantic trace {}",
                temporary.display()
            )
        })?;
        file.write_all(bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to commit semantic trace {} to {}",
                temporary.display(),
                path.display()
            )
        })?;
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result
}
