//! CLI process entrypoint; the shared library owns parsing and runtime lifetime.
fn main() -> anyhow::Result<()> {
    shenko::run_cli()
}
