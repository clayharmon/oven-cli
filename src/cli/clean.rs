use anyhow::Result;

use super::{CleanArgs, GlobalOpts};

#[allow(clippy::unused_async)]
pub async fn run(_args: CleanArgs, _global: &GlobalOpts) -> Result<()> {
    println!("not implemented");
    Ok(())
}
