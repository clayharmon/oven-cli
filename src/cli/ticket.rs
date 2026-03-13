use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{GlobalOpts, TicketArgs, TicketCommands};

#[allow(clippy::unused_async)]
pub async fn run(args: TicketArgs, _global: &GlobalOpts) -> Result<()> {
    let project_dir = std::env::current_dir().context("getting current directory")?;
    let issues_dir = project_dir.join(".oven").join("issues");

    match args.command {
        TicketCommands::Create(create_args) => {
            std::fs::create_dir_all(&issues_dir).context("creating issues directory")?;
            let id = next_ticket_id(&issues_dir)?;
            let labels = if create_args.ready { vec!["o-ready".to_string()] } else { Vec::new() };
            let body = create_args.body.unwrap_or_default();
            let content = format_ticket(id, &create_args.title, "open", &labels, &body);
            let path = issues_dir.join(format!("{id}.md"));
            std::fs::write(&path, content).context("writing ticket")?;
            println!("created ticket #{id}: {}", create_args.title);
        }
        TicketCommands::List(list_args) => {
            if !issues_dir.exists() {
                println!("no tickets found");
                return Ok(());
            }
            let tickets = read_all_tickets(&issues_dir)?;
            let filtered: Vec<_> = list_args.label.as_ref().map_or_else(
                || tickets.iter().collect(),
                |label| tickets.iter().filter(|t| t.labels.contains(label)).collect(),
            );

            if filtered.is_empty() {
                println!("no tickets found");
            } else {
                println!("{:<5} {:<8} {:<40} Labels", "ID", "Status", "Title");
                println!("{}", "-".repeat(70));
                for t in &filtered {
                    println!(
                        "{:<5} {:<8} {:<40} {}",
                        t.id,
                        t.status,
                        truncate(&t.title, 38),
                        t.labels.join(", ")
                    );
                }
            }
        }
        TicketCommands::View(view_args) => {
            let path = issues_dir.join(format!("{}.md", view_args.id));
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("ticket #{} not found", view_args.id))?;
            println!("{content}");
        }
        TicketCommands::Close(close_args) => {
            let path = issues_dir.join(format!("{}.md", close_args.id));
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("ticket #{} not found", close_args.id))?;
            let updated = content.replace("status: open", "status: closed");
            std::fs::write(&path, updated).context("updating ticket")?;
            println!("closed ticket #{}", close_args.id);
        }
    }

    Ok(())
}

struct Ticket {
    id: u32,
    title: String,
    status: String,
    labels: Vec<String>,
}

fn format_ticket(id: u32, title: &str, status: &str, labels: &[String], body: &str) -> String {
    let labels_str = if labels.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", labels.iter().map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(", "))
    };
    let now = chrono::Utc::now().to_rfc3339();
    format!(
        "---\nid: {id}\ntitle: {title}\nstatus: {status}\nlabels: {labels_str}\ncreated_at: {now}\n---\n\n{body}\n"
    )
}

fn next_ticket_id(issues_dir: &Path) -> Result<u32> {
    let mut max_id = 0u32;
    if issues_dir.exists() {
        for entry in std::fs::read_dir(issues_dir).context("reading issues directory")? {
            let entry = entry?;
            if let Some(stem) = entry.path().file_stem().and_then(|s| s.to_str()) {
                if let Ok(id) = stem.parse::<u32>() {
                    max_id = max_id.max(id);
                }
            }
        }
    }
    Ok(max_id + 1)
}

fn read_all_tickets(issues_dir: &PathBuf) -> Result<Vec<Ticket>> {
    let mut tickets = Vec::new();

    for entry in std::fs::read_dir(issues_dir).context("reading issues directory")? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let content = std::fs::read_to_string(&path)?;
        if let Some(ticket) = parse_ticket_frontmatter(&content) {
            tickets.push(ticket);
        }
    }

    tickets.sort_by_key(|t| t.id);
    Ok(tickets)
}

fn parse_ticket_frontmatter(content: &str) -> Option<Ticket> {
    let content = content.strip_prefix("---\n")?;
    let end = content.find("---")?;
    let frontmatter = &content[..end];

    let mut id = 0u32;
    let mut title = String::new();
    let mut status = String::new();
    let mut labels = Vec::new();

    for line in frontmatter.lines() {
        if let Some(val) = line.strip_prefix("id: ") {
            id = val.trim().parse().unwrap_or(0);
        } else if let Some(val) = line.strip_prefix("title: ") {
            title = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("status: ") {
            status = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("labels: ") {
            let val = val.trim();
            if val.starts_with('[') && val.ends_with(']') {
                let inner = &val[1..val.len() - 1];
                labels = inner
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }

    if id > 0 { Some(Ticket { id, title, status, labels }) } else { None }
}

fn truncate(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_ticket_with_labels() {
        let content = format_ticket(
            1,
            "Add retry logic",
            "open",
            &["o-ready".to_string()],
            "Implement retry.",
        );
        assert!(content.contains("id: 1"));
        assert!(content.contains("title: Add retry logic"));
        assert!(content.contains("status: open"));
        assert!(content.contains("\"o-ready\""));
        assert!(content.contains("Implement retry."));
    }

    #[test]
    fn format_ticket_no_labels() {
        let content = format_ticket(1, "Test", "open", &[], "body");
        assert!(content.contains("labels: []"));
    }

    #[test]
    fn next_ticket_id_starts_at_1() {
        let dir = tempfile::tempdir().unwrap();
        let id = next_ticket_id(dir.path()).unwrap();
        assert_eq!(id, 1);
    }

    #[test]
    fn next_ticket_id_increments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.md"), "---\nid: 1\n---").unwrap();
        std::fs::write(dir.path().join("3.md"), "---\nid: 3\n---").unwrap();
        let id = next_ticket_id(dir.path()).unwrap();
        assert_eq!(id, 4);
    }

    #[test]
    fn parse_ticket_frontmatter_valid() {
        let content =
            "---\nid: 42\ntitle: Fix bug\nstatus: open\nlabels: [\"o-ready\"]\n---\n\nbody";
        let ticket = parse_ticket_frontmatter(content).unwrap();
        assert_eq!(ticket.id, 42);
        assert_eq!(ticket.title, "Fix bug");
        assert_eq!(ticket.status, "open");
        assert_eq!(ticket.labels, vec!["o-ready"]);
    }

    #[test]
    fn parse_ticket_frontmatter_no_labels() {
        let content = "---\nid: 1\ntitle: Test\nstatus: open\nlabels: []\n---\n\n";
        let ticket = parse_ticket_frontmatter(content).unwrap();
        assert_eq!(ticket.id, 1);
        assert!(ticket.labels.is_empty());
    }

    #[test]
    fn parse_ticket_frontmatter_invalid() {
        assert!(parse_ticket_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn close_ticket_updates_status() {
        let content = "---\nid: 1\ntitle: Test\nstatus: open\nlabels: []\n---\n\nbody\n";
        let updated = content.replace("status: open", "status: closed");
        assert!(updated.contains("status: closed"));
        assert!(!updated.contains("status: open"));
    }

    #[test]
    fn truncate_short_string() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_long_string() {
        assert_eq!(truncate("this is a long string", 10), "this is...");
    }

    #[test]
    fn read_all_tickets_sorts_by_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("3.md"),
            "---\nid: 3\ntitle: Third\nstatus: open\nlabels: []\n---\n\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("1.md"),
            "---\nid: 1\ntitle: First\nstatus: open\nlabels: []\n---\n\n",
        )
        .unwrap();

        let tickets = read_all_tickets(&dir.path().to_path_buf()).unwrap();
        assert_eq!(tickets.len(), 2);
        assert_eq!(tickets[0].id, 1);
        assert_eq!(tickets[1].id, 3);
    }
}
