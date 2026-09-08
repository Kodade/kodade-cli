//! The bundled guide is available before config, inherited context or transport.

use std::io::{self, Write};

pub fn print() -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    writeln!(
        out,
        "Ködade CLI {} · automation guide 1 · protocol {}\n",
        env!("CARGO_PKG_VERSION"),
        kodade_cli_proto::PROTOCOL_VERSION
    )?;
    out.write_all(include_bytes!("../../../docs/AGENT-GUIDE.md"))?;
    Ok(())
}
