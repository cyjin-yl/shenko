//! Public metadata probe. This does not negotiate UCP or access an account.
use anyhow::{Context, Result, ensure};

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let origin = args.next().context("usage: discover https://company.example")?;
    ensure!(args.next().is_none(), "expected exactly one company origin");
    let report = shenko_compat_poppy::discovery::discover(&origin).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
