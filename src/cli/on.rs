use anyhow::Result;

use super::{GlobalOpts, OnArgs};

#[allow(clippy::unused_async)]
pub async fn run(_args: OnArgs, _global: &GlobalOpts) -> Result<()> {
    println!("not implemented");
    Ok(())
}
