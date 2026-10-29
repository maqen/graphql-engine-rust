//! `hasura deploy`: apply metadata, all migrations, metadata again, reload, and optionally
//! seeds, stopping at the first failure like the official CLI's state machine.

use crate::cmd::{Database, metadata, migrate, seed};
use crate::context::Ctx;
use crate::error::{Error, Result};

pub struct DeployOptions {
    pub with_seeds: bool,
    pub no_transaction: bool,
    pub per_migration_transaction: bool,
    pub disable_state_copy: bool,
}

pub fn run(ctx: &mut Ctx, opts: &DeployOptions) -> Result<()> {
    if opts.no_transaction && opts.per_migration_transaction {
        return Err(Error(
            "--no-transaction and --per-migration-transaction are mutually exclusive".into(),
        ));
    }
    ctx.config.disable_interactive = true;
    let failed = |e: Error| Error(format!("operation failed: {e}"));

    ctx.logger.debug("Applying Initial Metadata");
    if let Err(e) = metadata::apply(ctx, &metadata::ApplyOptions::default()) {
        ctx.logger.debug("Applying Initial Metadata Failed");
        ctx.logger.error("applying metadata failed");
        ctx.logger
            .info("This can happen when metadata in your project metadata directory is malformed");
        ctx.logger.debug(&e.0);
        ctx.logger.debug("Operation Failed");
        return Err(failed(e));
    }

    ctx.logger.debug("Applying Migrations");
    let all = Database {
        all: true,
        ..Default::default()
    };
    let migrate_opts = migrate::ApplyOptions {
        no_transaction: opts.no_transaction,
        per_migration_transaction: opts.per_migration_transaction,
        disable_state_copy: opts.disable_state_copy,
        migration_type: "up".into(),
        ..Default::default()
    };
    if let Err(e) = migrate::apply(ctx, &all, &migrate_opts) {
        ctx.logger.debug("Applying Migrations Failed");
        ctx.logger.error("applying migrations failed");
        ctx.logger.debug(&e.0);
        ctx.logger.debug("Operation Failed");
        return Err(failed(e));
    }

    ctx.logger.debug("Applying Metadata");
    ctx.logger.spin("Applying metadata...");
    let r = metadata::apply(ctx, &metadata::ApplyOptions::default());
    ctx.logger.stop_spinner();
    if let Err(e) = r {
        ctx.logger.debug("Applying Metadata Failed");
        ctx.logger.error("applying metadata failed");
        ctx.logger.debug(&e.0);
        ctx.logger.debug("Operation Failed");
        return Err(failed(e));
    }
    ctx.logger.info("Metadata applied");

    ctx.logger.debug("Reloading Metadata");
    if let Err(e) = metadata::reload_cmd(ctx) {
        ctx.logger.debug("Reloading Metadata Failed");
        ctx.logger.error("reloading metadata failed");
        ctx.logger.debug(&e.0);
        ctx.logger.debug("Operation Failed");
        return Err(failed(e));
    }

    if opts.with_seeds {
        ctx.logger.debug("Applying Seeds");
        if let Err(e) = seed::apply(ctx, &all, &[]) {
            ctx.logger.debug("Applying Seeds Failed");
            ctx.logger.error("applying seeds failed");
            ctx.logger.debug(&e.0);
        }
    }
    Ok(())
}
