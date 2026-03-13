use anyhow::Result;

use super::{GlobalOpts, PrepArgs};

#[allow(clippy::unused_async)]
pub async fn run(_args: PrepArgs, _global: &GlobalOpts) -> Result<()> {
    println!("not implemented");
    Ok(())
}
