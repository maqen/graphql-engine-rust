//! `hasura metadata …`: apply, export, clear, reload, diff and inconsistency.

use std::io::Write;
use std::path::Path;

use serde_json::{Value, json};

use crate::context::{Ctx, MetadataMode};
use crate::error::{Error, Result};
use crate::json::Json;
use crate::metadata::{build, diff, export};

fn stdout(text: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// The server's metadata as the raw JSON text of `export_metadata`.
pub fn server_metadata_text(ctx: &Ctx) -> Result<String> {
    ctx.client
        .metadata_ok(&json!({"type": "export_metadata", "args": {}}))
}

/// `writeByOutputFormat`.
fn write_by_format(text: &str, format: &str) -> Result<()> {
    match format {
        "json" => {
            let out = crate::gojson::indent(text, "  ")
                .ok_or_else(|| Error("invalid character in JSON".into()))?;
            stdout(&out);
        }
        "yaml" => {
            let j = Json::parse(text).map_err(Error)?;
            stdout(&export::metadata_node(&j).to_yaml());
        }
        f => {
            return Err(Error(format!(
                "output format '{f}' is not supported. supported formats: json, yaml"
            )));
        }
    }
    Ok(())
}

fn print_server_metadata(ctx: &Ctx, format: &str) -> Result<()> {
    let text =
        server_metadata_text(ctx).map_err(|e| Error(format!("failed to export metadata: {e}")))?;
    write_by_format(&text, format)
}

// --- apply

#[derive(Default)]
pub struct ApplyOptions {
    pub dry_run: bool,
    pub output: String,
    pub disallow_inconsistent: bool,
}

/// `metadata apply` (the command, with its log lines).
pub fn apply_cmd(ctx: &Ctx, opts: &ApplyOptions) -> Result<()> {
    if !opts.dry_run && opts.output.is_empty() {
        ctx.logger.spin("Applying metadata...");
    }
    let r = apply(ctx, opts);
    ctx.logger.stop_spinner();
    r?;
    if opts.output.is_empty() && !opts.dry_run {
        ctx.logger.info("Metadata applied");
    }
    Ok(())
}

fn applying_error(e: Error) -> Error {
    Error(format!("error applying metadata \n{e}"))
}

/// Apply the project's metadata (`MetadataApplyOptions.Run`).
pub fn apply(ctx: &Ctx, opts: &ApplyOptions) -> Result<()> {
    let metadata: Value = match ctx.metadata_mode {
        MetadataMode::Directory => {
            if opts.dry_run {
                let built = build::build(&ctx.metadata_dir, &ctx.logger)
                    .map_err(|e| Error(format!("error building project metadata: {e}")))?;
                let mut format = opts.output.clone();
                if format.is_empty() {
                    format = "json".into();
                    ctx.logger.warn("behaviour of --dry-run flag has changed from v2.0.0. It used to show a diff between metadata on server and local project");
                    ctx.logger.warn("new behaviour is to output local project metadata as JSON by default. The output format is configurable by -o flag eg: `hasura metadata apply --dry-run -o yaml`");
                    ctx.logger.warn(
                        "the old behaviour can be achieved using `hasura metadata diff` command",
                    );
                }
                return write_by_format(
                    &serde_json::to_string(&built).unwrap_or_default(),
                    &format,
                )
                .map_err(|e| Error(format!("displaying metadata failed: {e}")));
            }
            build::build(&ctx.metadata_dir, &ctx.logger).map_err(applying_error)?
        }
        mode => {
            let file = ctx.metadata_file.as_deref().unwrap_or(Path::new(""));
            let text = std::fs::read_to_string(file).map_err(|e| {
                Error(format!(
                    "reading metadata file: {}",
                    crate::context::io_message(&e)
                ))
            })?;
            if opts.dry_run {
                let format = if opts.output.is_empty() {
                    "json"
                } else {
                    &opts.output
                };
                return write_by_format(&text, format)
                    .map_err(|e| Error(format!("displaying metadata failed: {e}")));
            }
            if mode == MetadataMode::Yaml {
                let y = build::parse_yaml(&text)
                    .map_err(|e| Error(format!("parsing yaml metadata to json: {e}")))?;
                crate::config::yaml_to_json(&y)
            } else {
                serde_json::from_str(&text)
                    .map_err(|e| Error(format!("parsing metadata as json: {e}")))?
            }
        }
    };
    let body = json!({
        "type": "replace_metadata",
        "version": 2,
        "args": {"allow_inconsistent_metadata": !opts.disallow_inconsistent, "metadata": metadata},
    });
    let response = ctx.client.post_raw(
        &ctx.config.v1_metadata_endpoint(),
        crate::gojson::marshal(&body, None),
    )?;
    if !response.ok() {
        return Err(applying_error(Error(response.body)));
    }
    let consistent = response
        .json()
        .ok()
        .and_then(|v| v["is_consistent"].as_bool())
        .unwrap_or(false);
    if !consistent {
        ctx.logger.warn("Metadata is inconsistent");
        ctx.logger
            .warn("Use 'hasura metadata ic list' command to list inconsistent objects");
    }
    ctx.logger
        .debug("metadata applied using v2 replace_metadata");
    if !opts.output.is_empty() {
        print_server_metadata(ctx, &opts.output)?;
    }
    Ok(())
}

// --- export

pub fn export_cmd(ctx: &Ctx, output: &str) -> Result<()> {
    if output.is_empty() {
        ctx.logger.spin("Exporting metadata...");
    }
    let r = export_run(ctx, output);
    ctx.logger.stop_spinner();
    r?;
    if output.is_empty() {
        ctx.logger.info("Metadata exported");
    }
    Ok(())
}

fn export_run(ctx: &Ctx, output: &str) -> Result<()> {
    if !output.is_empty() {
        return print_server_metadata(ctx, output);
    }
    match ctx.metadata_mode {
        MetadataMode::Directory => {
            let files =
                export_files(ctx).map_err(|e| Error(format!("failed to export metadata: {e}")))?;
            ctx.logger.stop_spinner();
            write_files(&ctx.metadata_dir, &files)
                .map_err(|e| Error(format!("cannot write metadata to project: {e}")))
        }
        mode => {
            let text = server_metadata_text(ctx)
                .map_err(|e| Error(format!("exporting metadata from server: {e}")))?;
            let bytes = if mode == MetadataMode::Yaml {
                let j = Json::parse(&text)
                    .map_err(|e| Error(format!("parsing metadata to yaml: {e}")))?;
                export::metadata_node(&j).to_yaml()
            } else {
                text
            };
            let file = ctx.metadata_file.as_deref().unwrap_or(Path::new(""));
            std::fs::write(file, bytes).map_err(|e| Error(format!("writing metadata to file: {e}")))
        }
    }
}

/// The server's metadata as project files.
pub fn export_files(ctx: &Ctx) -> Result<export::Files> {
    let text = server_metadata_text(ctx)?;
    let j = Json::parse(&text).map_err(Error)?;
    export::export(&j)
}

/// Write exported files under `dir` (existing files that are not exported stay).
pub fn write_files(dir: &Path, files: &export::Files) -> Result<()> {
    for (name, content) in files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content).map_err(|e| {
            Error(format!(
                "creating metadata file {} failed: {e}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

// --- clear, reload

pub fn clear_cmd(ctx: &Ctx, called_as_reset: bool) -> Result<()> {
    if called_as_reset {
        ctx.logger
            .warn("metadata reset command is deprecated, use metadata clear instead");
    }
    ctx.logger.spin("Clearing metadata...");
    let r = ctx
        .client
        .metadata_ok(&json!({"type": "clear_metadata", "args": {}}));
    ctx.logger.stop_spinner();
    r.map_err(|e| {
        Error(format!(
            "failed to clear metadata: cannot clear Metadata: {e}"
        ))
    })?;
    ctx.logger.info("Metadata cleared");
    Ok(())
}

/// `metadata reload` and the reload step of `deploy`.
pub fn reload_cmd(ctx: &Ctx) -> Result<()> {
    ctx.logger.spin("Reloading metadata...");
    let r = ctx
        .client
        .metadata_ok(&json!({"type": "reload_metadata", "args": {}}));
    ctx.logger.stop_spinner();
    r.map_err(|e| {
        Error(format!(
            "failed to reload metadata: cannot reload metadata: {e}"
        ))
    })?;
    ctx.logger.info("Metadata reloaded");
    let (consistent, _) =
        inconsistent(ctx).map_err(|e| Error(format!("failed to read metadata status: {e}")))?;
    if consistent {
        ctx.logger.info("Metadata is consistent");
    } else {
        ctx.logger.warn(
            "Metadata is inconsistent, use 'hasura metadata ic list' command to see the inconsistent objects",
        );
    }
    Ok(())
}

// --- inconsistency

/// `get_inconsistent_metadata`: whether the metadata is consistent, and the objects.
pub fn inconsistent(ctx: &Ctx) -> Result<(bool, Vec<Value>)> {
    let body = ctx
        .client
        .metadata_ok(&json!({"type": "get_inconsistent_metadata", "args": {}}))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| Error(e.to_string()))?;
    let objects = v["inconsistent_objects"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    Ok((v["is_consistent"].as_bool().unwrap_or(false), objects))
}

fn object_name(o: &Value) -> String {
    match o
        .get("definition")
        .and_then(|d| d.as_object())
        .and_then(|d| d.get("name"))
    {
        Some(n) => crate::gojson::go_value(n),
        None => "N/A".into(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn ic_list_cmd(ctx: &Ctx, output: &str) -> Result<()> {
    ctx.logger.spin("Getting inconsistent metadata...");
    let r = ic_list(ctx, output);
    ctx.logger.stop_spinner();
    let consistent = r.map_err(|e| Error(format!("failed to list inconsistent metadata: {e}")))?;
    if consistent {
        ctx.logger.info("metadata is consistent");
    }
    Ok(())
}

fn ic_list(ctx: &Ctx, output: &str) -> Result<bool> {
    let (consistent, objects) = inconsistent(ctx)?;
    if consistent {
        return Ok(true);
    }
    // The CLI decodes each object into {definition, reason, type}.
    let objects: Vec<Value> = objects
        .into_iter()
        .map(|o| {
            json!({
                "definition": o.get("definition").cloned().unwrap_or(Value::Null),
                "reason": o.get("reason").cloned().unwrap_or(Value::Null),
                "type": o.get("type").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    if output == "json" {
        ctx.logger.stop_spinner();
        stdout(&format!("{}\n", go_marshal_struct_list(&objects)));
        return Ok(false);
    }
    let rows: Vec<Vec<String>> = objects
        .iter()
        .map(|o| {
            let ty = o["type"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| "N/A".into());
            let description = format!(
                "{}...",
                truncate(&crate::gojson::marshal(&o["definition"], None), 50)
            );
            let reason = match &o["reason"] {
                Value::String(s) => s.clone(),
                r => format!("{}...", truncate(&crate::gojson::marshal(r, None), 80)),
            };
            vec![object_name(o), ty, description, reason]
        })
        .collect();
    ctx.logger.stop_spinner();
    stdout(&crate::table::render_table(
        &["NAME", "TYPE", "DESCRIPTION", "REASON"],
        &rows,
    ));
    Ok(false)
}

/// `json.MarshalIndent` of `[]InconsistentMetadataObject`: struct fields in declaration
/// order (definition, reason, type), values as Go re-encodes them.
fn go_marshal_struct_list(objects: &[Value]) -> String {
    if objects.is_empty() {
        return "null".into();
    }
    let mut out = String::from("[");
    for (i, o) in objects.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("\n  {");
        for (j, key) in ["definition", "reason", "type"].iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            let v = crate::gojson::marshal(&o[*key], Some("  "));
            let v = v.replace('\n', "\n    ");
            out.push_str(&format!("\n    \"{key}\": {v}"));
        }
        out.push_str("\n  }");
    }
    out.push_str("\n]");
    out
}

pub fn ic_status_cmd(ctx: &Ctx) -> Result<()> {
    ctx.logger.spin("reading metadata status...");
    let r = inconsistent(ctx);
    ctx.logger.stop_spinner();
    let (consistent, _) = r.map_err(|e| Error(format!("failed to read metadata status: {e}")))?;
    if consistent {
        ctx.logger.info("metadata is consistent");
        Ok(())
    } else {
        Err(Error(
            "metadata is inconsistent, use 'hasura metadata ic list' command to see the inconsistent objects".into(),
        ))
    }
}

pub fn ic_drop_cmd(ctx: &Ctx) -> Result<()> {
    ctx.logger.spin("Dropping inconsistent metadata...");
    let r = ctx
        .client
        .metadata_ok(&json!({"type": "drop_inconsistent_metadata", "args": {}}));
    ctx.logger.stop_spinner();
    r.map_err(|e| Error(format!("failed to drop inconsistent metadata: {e}")))?;
    ctx.logger
        .info("all inconsistent objects removed from metadata");
    Ok(())
}

// --- diff

pub fn diff_cmd(ctx: &Ctx, args: &[String], diff_type: &str) -> Result<()> {
    if !diff_type.is_empty()
        && !["json", "yaml", "unified-json", "unified-yaml"].contains(&diff_type)
    {
        return Err(Error(format!(
            "metadata diff doesn't support difftype {diff_type}"
        )));
    }
    let mut out = std::io::stdout().lock();
    diff::run(ctx, args, diff_type, &mut out)
}
