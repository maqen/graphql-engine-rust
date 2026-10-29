//! `hasura metadata diff`: the per-file report between the project and the server (or two
//! directories), and the `--type` text diffs.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use crate::context::{Ctx, MetadataMode};
use crate::error::{Error, Result};
use crate::metadata::{build, dyff};

const ZERO_DIFFERENCES: &str = "zero differences found";

/// Metadata objects in the order the CLI diffs them, with their root file, the directory
/// paths are reported relative to, and the include files the diff skips.
struct Object {
    root: &'static str,
    extra: Option<&'static str>,
    exclude: &'static [&'static str],
}

const OBJECTS: &[Object] = &[
    Object {
        root: "version.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "databases/databases.yaml",
        extra: None,
        exclude: &["tables/tables.yaml", "functions/functions.yaml"],
    },
    Object {
        root: "remote_schemas.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "query_collections.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "allow_list.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "actions.yaml",
        extra: Some("actions.graphql"),
        exclude: &[],
    },
    Object {
        root: "cron_triggers.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "network.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "rest_endpoints.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "inherited_roles.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "opentelemetry.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "backend_configs.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "api_limits.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "graphql_schema_introspection.yaml",
        extra: None,
        exclude: &[],
    },
    Object {
        root: "metrics_config.yaml",
        extra: None,
        exclude: &[],
    },
];

pub fn run(ctx: &Ctx, args: &[String], diff_type: &str, out: &mut dyn Write) -> Result<()> {
    if ctx.config.version < 2 {
        return Err(Error(format!(
            "metadata diff for config {} not supported",
            ctx.config.version
        )));
    }
    match ctx.metadata_mode {
        MetadataMode::Directory => directory_diff(ctx, args, diff_type, out),
        MetadataMode::Json => file_diff(ctx, args, false, out),
        MetadataMode::Yaml => file_diff(ctx, args, true, out),
    }
}

fn check_dir(path: &str) -> Result<()> {
    let meta = std::fs::metadata(path)
        .map_err(|e| Error(format!("stat {path}: {}", crate::context::io_message(&e))))?;
    if !meta.is_dir() {
        return Err(Error(format!(
            "metadata diff only works with folder but got file {path}"
        )));
    }
    Ok(())
}

fn directory_diff(ctx: &Ctx, args: &[String], diff_type: &str, out: &mut dyn Write) -> Result<()> {
    let (mut from_name, mut to_name) = ("project".to_string(), "server".to_string());
    let mut from_dir = ctx.metadata_dir.clone();
    let mut to_dir: Option<PathBuf> = None;
    if let Some(a) = args.first() {
        check_dir(a)?;
        from_name = a.clone();
        from_dir = PathBuf::from(a);
    }
    if let Some(b) = args.get(1) {
        check_dir(b)?;
        to_name = b.clone();
        to_dir = Some(PathBuf::from(b));
    }
    ctx.logger.info(&format!(
        "Showing diff between {from_name} and {to_name}..."
    ));

    let tmp;
    let to_dir = match to_dir {
        Some(d) => d,
        None => {
            tmp = TempDir::new()?;
            let files = crate::cmd::metadata::export_files(ctx)?;
            crate::cmd::metadata::write_files(&tmp.0, &files)?;
            tmp.0.clone()
        }
    };

    if !diff_type.is_empty() {
        let new = build::build(&to_dir, &ctx.logger)?;
        let old = build::build(&from_dir, &ctx.logger)?;
        let text = |v: &serde_json::Value, json: bool| -> Result<String> {
            if json {
                let compact = serde_json::to_string(v).map_err(|e| Error(e.to_string()))?;
                crate::gojson::indent(&compact, "  ")
                    .ok_or_else(|| Error("cannot indent json".into()))
            } else {
                Ok(crate::yaml::Node::from_json(&crate::json::Json::from_serde(v)).to_yaml())
            }
        };
        let w = |out: &mut dyn Write, s: &str| out.write_all(s.as_bytes()).map_err(Error::from);
        match diff_type {
            "json" | "yaml" => {
                let json = diff_type == "json";
                print_myers_diff(
                    &text(&new, json)?,
                    &text(&old, json)?,
                    &to_name,
                    &from_name,
                    ctx.flags.no_color,
                    out,
                )?;
            }
            _ => {
                let json = diff_type == "unified-json";
                w(out, &unified_diff(&text(&new, json)?, &text(&old, json)?))?;
            }
        }
        return Ok(());
    }

    // The report reads from the server's files to the project's.
    for object in OBJECTS {
        if let Err(e) = write_diff(object, &to_dir, &from_dir, ctx.flags.no_color, out) {
            ctx.logger.error(&e.0);
        }
    }
    Ok(())
}

/// `printMyersDiff`.
fn print_myers_diff(
    before: &str,
    after: &str,
    from: &str,
    to: &str,
    no_color: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let mut text = format!(
        "- {}\n+ {}\n",
        color(from, "31", no_color),
        color(to, "32", no_color)
    );
    let (count, body) = myers_diff(before, after, no_color);
    text.push_str(&body);
    if count == 0 {
        text.push_str(ZERO_DIFFERENCES);
        text.push('\n');
    }
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// `metadata diff` in JSON or YAML metadata mode: the server's metadata against the file.
fn file_diff(ctx: &Ctx, args: &[String], yaml: bool, out: &mut dyn Write) -> Result<()> {
    if !args.is_empty() {
        return Err(Error(format!(
            "expected 0 arguments, found: [{}]",
            args.join(" ")
        )));
    }
    let server = crate::cmd::metadata::server_metadata_text(ctx)
        .map_err(|e| Error(format!("exporting metadata from server: {e}")))?;
    let server = if yaml {
        let j = crate::json::Json::parse(&server)
            .map_err(|e| Error(format!("parsing server metadata as yaml: {e}")))?;
        crate::metadata::export::metadata_node(&j).to_yaml()
    } else {
        server
    };
    let file = ctx.metadata_file.clone().unwrap_or_default();
    let local = std::fs::read_to_string(&file).map_err(|e| {
        Error(format!(
            "reading local metadata: open {}: {}",
            file.display(),
            crate::context::io_message(&e)
        ))
    })?;
    print_myers_diff(
        &server,
        &local,
        "server",
        "project",
        ctx.flags.no_color,
        out,
    )
}

fn color(text: &str, code: &str, no_color: bool) -> String {
    if no_color {
        text.to_string()
    } else {
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}

// --- one metadata object

/// The object's files: the root file and everything it includes (`DefaultGetFiles`).
fn object_files(dir: &Path, object: &Object) -> Result<Vec<PathBuf>> {
    let root = dir.join(object.root);
    let mut files = Vec::new();
    match std::fs::read_to_string(&root) {
        Ok(text) => {
            files.push(clean(&root));
            let y = build::parse_yaml(&text)
                .map_err(|e| Error(format!("finding child files in failed: {e}")))?;
            let base = root.parent().unwrap_or(dir).to_path_buf();
            let mut included = Vec::new();
            build::resolve_includes_tracking(&y, &base, &mut included).map_err(Error)?;
            files.extend(included.iter().map(|p| clean(p)));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(Error(format!(
                "open {}: {}",
                root.display(),
                crate::context::io_message(&e)
            )));
        }
    }
    if let Some(extra) = object.extra {
        files.push(dir.join(extra));
    }
    Ok(files
        .into_iter()
        .filter(|f| {
            !object
                .exclude
                .iter()
                .any(|p| f.to_string_lossy().contains(p))
        })
        .collect())
}

/// `filepath.Clean`.
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn relative(base: &Path, file: &Path) -> String {
    let base = clean(base);
    file.strip_prefix(&base)
        .unwrap_or(file)
        .to_string_lossy()
        .into_owned()
}

fn is_yaml(name: &str) -> bool {
    name.ends_with(".yaml") || name.ends_with(".yml")
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| {
        Error(format!(
            "open {}: {}",
            path.display(),
            crate::context::io_message(&e)
        ))
    })
}

fn load_yaml(path: &Path) -> Result<Vec<dyff::Node>> {
    dyff::load_documents(&read(path)?).map_err(|e| {
        Error(format!(
            "failed to load input file from {}: {e}",
            path.display()
        ))
    })
}

/// `DefaultWriteDiff`.
fn write_diff(
    object: &Object,
    from_dir: &Path,
    to_dir: &Path,
    no_color: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let from_files = object_files(from_dir, object)?;
    let to_files = object_files(to_dir, object)?;
    let to_rel: Vec<String> = to_files.iter().map(|f| relative(to_dir, f)).collect();
    let mut processed = vec![false; to_files.len()];
    let mut text = String::new();

    for file in &from_files {
        let rel = relative(from_dir, file);
        match to_rel.iter().position(|r| *r == rel) {
            Some(idx) => {
                processed[idx] = true;
                if is_yaml(&rel) {
                    let mut a = load_yaml(file)?;
                    let mut b = load_yaml(&to_files[idx])?;
                    text.push_str(&dyff::yaml_diff(&mut a, &mut b, &rel).map_err(Error)?);
                } else {
                    let (count, body) = myers_diff(&read(file)?, &read(&to_files[idx])?, no_color);
                    if count > 0 {
                        text.push_str(&format!("{rel}\n{body}\n"));
                    }
                }
            }
            None => text.push_str(&diff_with_empty(file, &rel, true, no_color)?),
        }
    }
    for (i, file) in to_files.iter().enumerate() {
        if !processed[i] {
            text.push_str(&diff_with_empty(file, &to_rel[i], false, no_color)?);
        }
    }
    out.write_all(text.as_bytes())?;
    Ok(())
}

/// A file that exists on one side only, against an empty file of the same shape.
fn diff_with_empty(file: &Path, rel: &str, is_from: bool, no_color: bool) -> Result<String> {
    if is_yaml(rel) {
        let mut docs = load_yaml(file)?;
        let mut empty = dyff::empty_like(&docs);
        let r = if is_from {
            dyff::yaml_diff(&mut docs, &mut empty, rel)
        } else {
            dyff::yaml_diff(&mut empty, &mut docs, rel)
        };
        return r.map_err(Error);
    }
    let content = read(file)?;
    let (count, body) = if is_from {
        myers_diff(&content, "", no_color)
    } else {
        myers_diff("", &content, no_color)
    };
    Ok(if count > 0 {
        format!("{rel}\n{body}\n")
    } else {
        String::new()
    })
}

// --- text diffs

/// `diff.MyersDiff`: the `-`, `+` and `@@` lines of a unified diff (gotextdiff), and the
/// number of lines of the full unified text (0 when the texts are equal).
pub fn myers_diff(before: &str, after: &str, no_color: bool) -> (usize, String) {
    let unified = unified_text(before, after);
    let lines: Vec<&str> = unified.split('\n').collect();
    if lines.len() <= 2 {
        return (0, String::new());
    }
    let mut out = String::new();
    for line in &lines[2..] {
        if line.is_empty() {
            break;
        }
        let code = match line.as_bytes()[0] {
            b'-' => "31",
            b'+' => "32",
            b'@' => "36",
            _ => continue,
        };
        out.push_str(&color(line, code, no_color));
        out.push('\n');
    }
    (lines.len(), out)
}

fn split_lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// The edit operations of gotextdiff's Myers implementation, line by line: `-` (from
/// `a`), `+` (from `b`) and `=`.
pub fn myers_ops<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(char, &'a str)> {
    let mut out = Vec::new();
    let mut ai = 0;
    for op in operations(a, b) {
        while ai < op.i1 {
            out.push(('=', a[ai]));
            ai += 1;
        }
        match op.kind {
            OpKind::Delete => {
                for line in &a[op.i1..op.i2] {
                    out.push(('-', *line));
                }
                ai = op.i2;
            }
            OpKind::Insert => {
                for line in &b[op.j1..op.j1 + op.len] {
                    out.push(('+', *line));
                }
            }
        }
    }
    while ai < a.len() {
        out.push(('=', a[ai]));
        ai += 1;
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum OpKind {
    Delete,
    Insert,
}

struct Operation {
    kind: OpKind,
    i1: usize,
    i2: usize,
    j1: usize,
    len: usize,
}

fn operations(a: &[&str], b: &[&str]) -> Vec<Operation> {
    if a.is_empty() && b.is_empty() {
        return Vec::new();
    }
    let (trace, offset) = shortest_edit_sequence(a, b);
    let snakes = backtrack(&trace, a.len() as i64, b.len() as i64, offset);
    let (m, n) = (a.len() as i64, b.len() as i64);
    let mut solution = Vec::new();
    let (mut x, mut y) = (0i64, 0i64);
    for snake in snakes.iter().flatten() {
        let (sx, sy) = *snake;
        let mut op: Option<Operation> = None;
        while sx - sy > x - y {
            if op.is_none() {
                op = Some(Operation {
                    kind: OpKind::Delete,
                    i1: x as usize,
                    i2: 0,
                    j1: y as usize,
                    len: 0,
                });
            }
            x += 1;
            if x == m {
                break;
            }
        }
        if let Some(mut o) = op.take() {
            o.i2 = x as usize;
            solution.push(o);
        }
        while sx - sy < x - y {
            if op.is_none() {
                op = Some(Operation {
                    kind: OpKind::Insert,
                    i1: x as usize,
                    i2: 0,
                    j1: y as usize,
                    len: 0,
                });
            }
            y += 1;
        }
        if let Some(mut o) = op.take() {
            o.i2 = x as usize;
            o.len = y as usize - o.j1;
            solution.push(o);
        }
        while x < sx {
            x += 1;
            y += 1;
        }
        if x >= m && y >= n {
            break;
        }
    }
    solution
}

fn backtrack(trace: &[Vec<i64>], mut x: i64, mut y: i64, offset: i64) -> Vec<Option<(i64, i64)>> {
    let mut snakes = vec![None; trace.len()];
    let mut d = trace.len() as i64 - 1;
    while x > 0 && y > 0 && d > 0 {
        let v = &trace[d as usize];
        if v.is_empty() {
            d -= 1;
            continue;
        }
        snakes[d as usize] = Some((x, y));
        let k = x - y;
        let at = |i: i64| v[(i + offset) as usize];
        let k_prev = if k == -d || (k != d && at(k - 1) < at(k + 1)) {
            k + 1
        } else {
            k - 1
        };
        x = at(k_prev);
        y = x - k_prev;
        d -= 1;
    }
    if x < 0 || y < 0 {
        return snakes;
    }
    if d >= 0 {
        snakes[d as usize] = Some((x, y));
    }
    snakes
}

fn shortest_edit_sequence(a: &[&str], b: &[&str]) -> (Vec<Vec<i64>>, i64) {
    let (m, n) = (a.len() as i64, b.len() as i64);
    let offset = n + m;
    let mut v = vec![0i64; (2 * (n + m) + 1) as usize];
    let mut trace: Vec<Vec<i64>> = vec![Vec::new(); (n + m + 1) as usize];
    for d in 0..=(n + m) {
        let mut k = -d;
        while k <= d {
            let at = |v: &[i64], i: i64| v[(i + offset) as usize];
            let mut x = if k == -d || (k != d && at(&v, k - 1) < at(&v, k + 1)) {
                at(&v, k + 1)
            } else {
                at(&v, k - 1) + 1
            };
            let mut y = x - k;
            while x < m && y < n && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            v[(k + offset) as usize] = x;
            if x == m && y == n {
                trace[d as usize] = v.clone();
                return (trace, offset);
            }
            k += 2;
        }
        trace[d as usize] = v.clone();
    }
    (Vec::new(), 0)
}

/// gotextdiff's `ToUnified` formatted as text, with three lines of context.
fn unified_text(before: &str, after: &str) -> String {
    const EDGE: i64 = 3;
    const GAP: i64 = EDGE * 2;
    let a = split_lines(before);
    let b = split_lines(after);
    // Edits as (start line, end line, inserted lines), lines counted from 0 in `a`.
    let mut edits: Vec<(i64, i64, Vec<&str>)> = Vec::new();
    for op in operations(&a, &b) {
        match op.kind {
            OpKind::Delete => edits.push((op.i1 as i64, op.i2 as i64, Vec::new())),
            OpKind::Insert => {
                let lines = b[op.j1..op.j1 + op.len].to_vec();
                if !lines.concat().is_empty() {
                    edits.push((op.i1 as i64, op.i1 as i64, lines));
                }
            }
        }
    }
    edits.sort_by_key(|(s, e, _)| (*s, *e));
    if edits.is_empty() {
        return String::new();
    }

    struct Hunk<'a> {
        from_line: i64,
        to_line: i64,
        lines: Vec<(char, &'a str)>,
    }
    fn add_equal<'a>(a: &[&'a str], h: &mut Hunk<'a>, start: i64, end: i64) -> i64 {
        let mut delta = 0;
        for i in start..end {
            if i < 0 {
                continue;
            }
            if i >= a.len() as i64 {
                return delta;
            }
            h.lines.push((' ', a[i as usize]));
            delta += 1;
        }
        delta
    }
    let mut hunks: Vec<Hunk<'_>> = Vec::new();
    let mut h: Option<Hunk<'_>> = None;
    let (mut last, mut to_line) = (0i64, 0i64);
    for (start, end, new) in &edits {
        let (start, end) = (*start, *end);
        match h.as_mut() {
            Some(_) if start == last => {}
            Some(hh) if start <= last + GAP => {
                add_equal(&a, hh, last, start);
            }
            _ => {
                if let Some(mut old) = h.take() {
                    add_equal(&a, &mut old, last, last + EDGE);
                    hunks.push(old);
                }
                to_line += start - last;
                let mut nh = Hunk {
                    from_line: start + 1,
                    to_line: to_line + 1,
                    lines: Vec::new(),
                };
                let delta = add_equal(&a, &mut nh, start - EDGE, start);
                nh.from_line -= delta;
                nh.to_line -= delta;
                h = Some(nh);
            }
        }
        let hh = h.as_mut().expect("hunk");
        last = start;
        for i in start..end {
            hh.lines.push(('-', a[i as usize]));
            last += 1;
        }
        for line in new {
            hh.lines.push(('+', line));
            to_line += 1;
        }
    }
    if let Some(mut old) = h.take() {
        add_equal(&a, &mut old, last, last + EDGE);
        hunks.push(old);
    }

    let mut out = String::from("--- a\n+++ b\n");
    for hunk in &hunks {
        let from_count = hunk.lines.iter().filter(|(k, _)| *k != '+').count();
        let to_count = hunk.lines.iter().filter(|(k, _)| *k != '-').count();
        out.push_str("@@");
        if from_count > 1 {
            out.push_str(&format!(" -{},{}", hunk.from_line, from_count));
        } else {
            out.push_str(&format!(" -{}", hunk.from_line));
        }
        if to_count > 1 {
            out.push_str(&format!(" +{},{}", hunk.to_line, to_count));
        } else {
            out.push_str(&format!(" +{}", hunk.to_line));
        }
        out.push_str(" @@\n");
        for (kind, content) in &hunk.lines {
            out.push(*kind);
            out.push_str(content);
            if !content.ends_with('\n') {
                out.push_str("\n\\ No newline at end of file\n");
            }
        }
    }
    out
}

/// `printUnifiedDiff` (aryann/difflib, always coloured).
fn unified_diff(before: &str, after: &str) -> String {
    let a: Vec<&str> = before.split('\n').collect();
    let b: Vec<&str> = after.split('\n').collect();
    let mut start = 0;
    while start < a.len() && start < b.len() && a[start] == b[start] {
        start += 1;
    }
    let mut end = 0;
    let (mut i, mut j) = (a.len() as i64 - 1, b.len() as i64 - 1);
    while i > start as i64 && j > start as i64 && a[i as usize] == b[j as usize] {
        i -= 1;
        j -= 1;
        end += 1;
    }
    let mut out = String::new();
    for line in &a[..start] {
        out.push_str(line);
        out.push('\n');
    }
    let s1 = &a[start..a.len() - end];
    let s2 = &b[start..b.len() - end];
    let (r, c) = (s1.len() + 1, s2.len() + 1);
    let mut m = vec![0u32; r * c];
    for i in 1..r {
        for j in 1..c {
            m[i * c + j] = if s1[s1.len() - i] == s2[s2.len() - j] {
                m[(i - 1) * c + j - 1] + 1
            } else {
                m[(i - 1) * c + j].max(m[i * c + j - 1])
            };
        }
    }
    let (mut i, mut j) = (s1.len(), s2.len());
    while i > 0 || j > 0 {
        if i > 0 && m[i * c + j] == m[(i - 1) * c + j] {
            out.push_str(&color(s1[s1.len() - i], "31", false));
            i -= 1;
        } else if j > 0 && m[i * c + j] == m[i * c + j - 1] {
            out.push_str(&color(s2[s2.len() - j], "32", false));
            j -= 1;
        } else {
            out.push_str(s1[s1.len() - i]);
            i -= 1;
            j -= 1;
        }
        out.push('\n');
    }
    for line in &a[a.len() - end..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A temporary directory removed on drop (`os.MkdirTemp("", "*")`).
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir> {
        let base = std::env::temp_dir();
        for attempt in 0..100u32 {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let dir = base.join(format!(
                "{}{}",
                (nanos as u64).wrapping_add(attempt as u64 * 7919) % 10_000_000_000,
                std::process::id()
            ));
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            if builder.create(&dir).is_ok() {
                return Ok(TempDir(dir));
            }
        }
        Err(Error("cannot create temporary directory".into()))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn myers_insert_into_one_line() {
        let to = "type Query {\n  a: Int\n}\n";
        let (n, body) = myers_diff("\n", to, true);
        assert_eq!(n, 8);
        assert_eq!(body, "@@ -1 +1,3 @@\n-\n+type Query {\n+  a: Int\n+}\n");
    }

    #[test]
    fn myers_equal_is_zero() {
        assert_eq!(myers_diff("a\nb\n", "a\nb\n", true).0, 0);
    }

    #[test]
    fn myers_change_middle() {
        let (_, body) = myers_diff("a\nb\nc\n", "a\nx\nc\n", true);
        assert_eq!(body, "@@ -1,3 +1,3 @@\n-b\n+x\n");
    }

    #[test]
    fn unified_marks_lines() {
        let out = unified_diff("a\nb", "a\nc");
        assert_eq!(out, "a\n\x1b[31mb\x1b[0m\n\x1b[32mc\x1b[0m\n");
    }
}
