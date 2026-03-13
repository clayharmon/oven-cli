use anyhow::Result;

use super::{GlobalOpts, TicketArgs, TicketCommands};

#[allow(clippy::unused_async)]
pub async fn run(args: TicketArgs, _global: &GlobalOpts) -> Result<()> {
    match args.command {
        TicketCommands::Create(_)
        | TicketCommands::List(_)
        | TicketCommands::View(_)
        | TicketCommands::Close(_) => {
            println!("not implemented");
        }
    }
    Ok(())
}
