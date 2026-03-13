use anyhow::Result;

use super::{GlobalOpts, LookArgs};

#[allow(clippy::unused_async)]
pub async fn run(_args: LookArgs, _global: &GlobalOpts) -> Result<()> {
    println!("not implemented");
    Ok(())
}
