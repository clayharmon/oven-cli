use anyhow::Result;

use super::{GlobalOpts, ReportArgs};

#[allow(clippy::unused_async)]
pub async fn run(_args: ReportArgs, _global: &GlobalOpts) -> Result<()> {
    println!("not implemented");
    Ok(())
}
