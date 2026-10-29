//! `hasura version`.

use std::io::Write;

use crate::cmd::{ServerArgs, validate};
use crate::context::{CLI_VERSION, GlobalFlags};
use crate::logger::{Field, Level, format_entry};

/// Logs the CLI version and, when a project and server are reachable, the server's, on
/// stdout. A failing server check is not an error.
pub fn run(flags: &GlobalFlags) -> crate::error::Result<()> {
    let terminal = crate::cmd::is_terminal();
    let print = |msg: &str, fields: &[(&str, Field)]| {
        let line = format_entry(Level::Info, msg, fields, terminal, flags.no_color);
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    };
    print("hasura cli", &[("version", Field::Str(CLI_VERSION))]);
    if let Ok(ctx) = validate(flags, &ServerArgs::default()) {
        print(
            "hasura graphql engine",
            &[
                ("endpoint", Field::Str(&ctx.config.endpoint)),
                ("version", Field::Str(&ctx.server_version)),
            ],
        );
    }
    Ok(())
}
