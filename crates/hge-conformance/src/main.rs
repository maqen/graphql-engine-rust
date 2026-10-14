//! `hge-conformance`: run the conformance suite (`conformance/suites`) against our engine
//! or against Hasura V2 itself.
//!
//! ```sh
//! # our engine (fails on regressions and on cases that started passing)
//! hge-conformance --pg-url postgres://postgres:postgres@localhost:5432/postgres \
//!     rust --bin target/debug/graphql-engine
//! # Hasura (every case must pass: proves the recorded expectations)
//! hge-conformance --pg-url ... hasura --image hasura/graphql-engine:v2.51.0-ce
//! ```

mod compare;
mod db;
mod jwt;
mod report;
mod run;
mod suite;
mod target;
mod ws;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};

use crate::report::Report;
use crate::suite::Suite;
use crate::target::{Slot, Target};

#[derive(Parser)]
#[command(
    name = "hge-conformance",
    about = "Run the conformance suite against an engine"
)]
struct Cli {
    /// Postgres superuser URL. Per-slot databases are created on this server; it must be
    /// reachable at the same address by the engine (Hasura runs with `--network host`).
    #[arg(long, env = "CONFORMANCE_PG_URL")]
    pg_url: String,

    /// Suite root.
    #[arg(long, default_value = "conformance/suites")]
    suites: PathBuf,

    /// Only run cases whose id (`area/suite/case`) contains this string.
    #[arg(long)]
    filter: Option<String>,

    /// Suites run in parallel.
    #[arg(long, short)]
    jobs: Option<usize>,

    /// Write the per-area summary as Markdown.
    #[arg(long)]
    report_md: Option<PathBuf>,

    /// Write every case result as JSON.
    #[arg(long)]
    report_json: Option<PathBuf>,

    #[command(subcommand)]
    target: TargetArg,
}

#[derive(Subcommand)]
enum TargetArg {
    /// Our `graphql-engine` binary.
    Rust {
        #[arg(long, default_value = "target/debug/graphql-engine")]
        bin: PathBuf,
        /// Known failures; cases listed here may fail, others may not.
        #[arg(long, default_value = "conformance/expected-failures.txt")]
        expected_failures: PathBuf,
        /// Rewrite the expected-failures list from this run (whole suite only).
        #[arg(long)]
        write_expected_failures: bool,
    },
    /// A Hasura V2 container; every case must pass.
    Hasura {
        #[arg(long, default_value = "hasura/graphql-engine:v2.51.0-ce")]
        image: String,
        /// Rewrite the expected responses of the selected cases from Hasura's answers
        /// (for writing new cases: give the requests, record the responses).
        #[arg(long)]
        record: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    // Signing keys for JWT cases live next to the suites.
    jwt::set_keys_dir(cli.suites.join("../keys"));
    let mut suites = suite::load_all(&cli.suites)?;
    // Suites with the same engine environment run back to back, so Hasura containers get reused.
    suites.sort_by_key(|s| (s.engine_key(), s.id.clone()));
    let filter = cli.filter.clone();
    let matches = move |id: &str| filter.as_deref().is_none_or(|f| id.contains(f));
    suites.retain(|s| s.cases.iter().any(|c| matches(&c.id)));

    if cli.filter.is_some()
        && matches!(
            cli.target,
            TargetArg::Rust {
                write_expected_failures: true,
                ..
            }
        )
    {
        bail!("--write-expected-failures needs a full run (no --filter)");
    }
    let (target, name, default_jobs) = match &cli.target {
        TargetArg::Rust { bin, .. } => {
            if !bin.exists() {
                bail!(
                    "{} not found; build it with `cargo build -p graphql-engine`",
                    bin.display()
                );
            }
            (Target::Rust { bin: bin.clone() }, "rust".to_owned(), 4)
        }
        TargetArg::Hasura { image, .. } => {
            target::remove_exited_containers().await;
            (
                Target::Hasura {
                    image: image.clone(),
                },
                image.clone(),
                2,
            )
        }
    };
    let jobs = cli
        .jobs
        .unwrap_or(default_jobs)
        .max(1)
        .min(suites.len().max(1));
    let total_cases: usize = suites.iter().map(|s| s.cases.len()).sum();
    eprintln!(
        "running {} suites ({total_cases} cases) against {name} with {jobs} slots",
        suites.len()
    );

    let record = matches!(cli.target, TargetArg::Hasura { record: true, .. });
    let queue: Arc<Mutex<VecDeque<Suite>>> = Arc::new(Mutex::new(suites.into()));
    let mut workers = vec![];
    for index in 0..jobs {
        let queue = queue.clone();
        let target = target.clone();
        let pg_url = cli.pg_url.clone();
        let matches = matches.clone();
        workers.push(tokio::spawn(async move {
            let mut slot = Slot::new(index, &pg_url, target).await?;
            let mut results = vec![];
            loop {
                let Some(suite) = queue.lock().expect("queue lock").pop_front() else {
                    break;
                };
                let r = run::run_suite(&mut slot, &suite, &matches, record).await;
                let passed = r.iter().filter(|c| c.passed).count();
                eprintln!("  {}: {passed}/{} passed", suite.id, r.len());
                results.extend(r);
            }
            slot.finish().await;
            anyhow::Ok(results)
        }));
    }
    // Wait for every worker (each cleans up its engine and databases) before reporting errors.
    let mut results = vec![];
    let mut errors = vec![];
    for w in futures::future::join_all(workers).await {
        match w {
            Ok(Ok(r)) => results.extend(r),
            Ok(Err(e)) => errors.push(format!("{e:#}")),
            Err(e) => errors.push(e.to_string()),
        }
    }
    if !errors.is_empty() {
        bail!("worker failed: {}", errors.join("; "));
    }

    let report = match &cli.target {
        TargetArg::Rust {
            expected_failures,
            write_expected_failures,
            ..
        } => {
            if !expected_failures.exists() && !write_expected_failures {
                bail!(
                    "{} not found (create it with --write-expected-failures)",
                    expected_failures.display()
                );
            }
            let listed = report::read_expected_failures(expected_failures)?;
            let report = Report::new(&name, results, Some(&listed));
            if *write_expected_failures {
                report::write_expected_failures(expected_failures, &report.failing_ids())?;
                eprintln!("wrote {}", expected_failures.display());
            }
            report
        }
        TargetArg::Hasura { .. } => Report::new(&name, results, None),
    };

    let md = report.markdown();
    println!("{md}");
    if let Some(p) = &cli.report_md {
        std::fs::write(p, &md)?;
    }
    if let Some(p) = &cli.report_json {
        std::fs::write(p, serde_json::to_string_pretty(&report)?)?;
    }
    let write_mode = record
        || matches!(
            cli.target,
            TargetArg::Rust {
                write_expected_failures: true,
                ..
            }
        );
    for c in &report.cases {
        match (c.result.passed, c.expected_failure) {
            (false, _) if record => println!(
                "NOT RECORDED {}\n    {}",
                c.result.id,
                c.result.detail.as_deref().unwrap_or("")
            ),
            (false, false) => println!(
                "FAIL {}\n    {}",
                c.result.id,
                c.result.detail.as_deref().unwrap_or("")
            ),
            (true, true) if !write_mode => println!(
                "NOW PASSING (remove from the expected-failures list) {}",
                c.result.id
            ),
            _ => {}
        }
    }
    let failed = if record {
        report.total.passed < report.total.cases
    } else {
        !write_mode && !report.ok()
    };
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
