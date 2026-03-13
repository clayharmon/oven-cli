use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{GlobalOpts, TicketArgs, TicketCommands};
use crate::issues::local::rewrite_frontmatter_labels;

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
            let content = format_ticket(
                id,
                &create_args.title,
                "open",
                &labels,
                &body,
                create_args.repo.as_deref(),
            );
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
            let filtered: Vec<_> = tickets
                .iter()
                .filter(|t| {
                    list_args.label.as_ref().is_none_or(|l| t.labels.contains(l))
                        && list_args.status.as_ref().is_none_or(|s| t.status == *s)
                })
                .collect();

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
        TicketCommands::Label(label_args) => {
            let path = issues_dir.join(format!("{}.md", label_args.id));
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("ticket #{} not found", label_args.id))?;
            let mut ticket =
                parse_ticket_frontmatter(&content).context("failed to parse ticket frontmatter")?;
            if label_args.remove {
                ticket.labels.retain(|l| l != &label_args.label);
            } else if !ticket.labels.contains(&label_args.label) {
                ticket.labels.push(label_args.label.clone());
            }
            let updated = rewrite_frontmatter_labels(&content, &ticket.labels);
            std::fs::write(&path, updated).context("updating ticket")?;
            println!("updated ticket #{}", label_args.id);
        }
        TicketCommands::Edit(edit_args) => {
            let path = issues_dir.join(format!("{}.md", edit_args.id));
            if !path.exists() {
                anyhow::bail!("ticket #{} not found", edit_args.id);
            }
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
            std::process::Command::new(&editor)
                .arg(&path)
                .status()
                .with_context(|| format!("opening {editor}"))?;
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

fn format_ticket(
    id: u32,
    title: &str,
    status: &str,
    labels: &[String],
    body: &str,
    target_repo: Option<&str>,
) -> String {
    let labels_str = if labels.is_empty() {
        "[]".to_string()
    } else {
        format!("[{}]", labels.iter().map(|l| format!("\"{l}\"")).collect::<Vec<_>>().join(", "))
    };
    let now = chrono::Utc::now().to_rfc3339();
    let target_line = target_repo.map_or_else(String::new, |r| format!("target_repo: {r}\n"));
    format!(
        "---\nid: {id}\ntitle: {title}\nstatus: {status}\nlabels: {labels_str}\n{target_line}created_at: {now}\n---\n\n{body}\n"
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
            None,
        );
        assert!(content.contains("id: 1"));
        assert!(content.contains("title: Add retry logic"));
        assert!(content.contains("status: open"));
        assert!(content.contains("\"o-ready\""));
        assert!(content.contains("Implement retry."));
        assert!(!content.contains("target_repo:"));
    }

    #[test]
    fn format_ticket_no_labels() {
        let content = format_ticket(1, "Test", "open", &[], "body", None);
        assert!(content.contains("labels: []"));
    }

    #[test]
    fn format_ticket_with_target_repo() {
        let content = format_ticket(1, "Multi", "open", &[], "body", Some("api"));
        assert!(content.contains("target_repo: api"));
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
    fn label_add_and_remove() {
        let content = "---\nid: 1\ntitle: Test\nstatus: open\nlabels: [\"o-ready\"]\n---\n\nbody";
        let mut ticket = parse_ticket_frontmatter(content).unwrap();
        assert_eq!(ticket.labels, vec!["o-ready"]);

        // Add a label
        if !ticket.labels.contains(&"o-cooking".to_string()) {
            ticket.labels.push("o-cooking".to_string());
        }
        let updated = rewrite_frontmatter_labels(content, &ticket.labels);
        assert!(updated.contains("\"o-ready\""));
        assert!(updated.contains("\"o-cooking\""));

        // Remove a label
        ticket.labels.retain(|l| l != "o-ready");
        let updated2 = rewrite_frontmatter_labels(content, &ticket.labels);
        assert!(!updated2.contains("\"o-ready\""));
        assert!(updated2.contains("\"o-cooking\""));
    }

    #[test]
    fn list_filters_by_status() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("1.md"),
            "---\nid: 1\ntitle: Open\nstatus: open\nlabels: []\n---\n\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("2.md"),
            "---\nid: 2\ntitle: Closed\nstatus: closed\nlabels: []\n---\n\n",
        )
        .unwrap();

        let tickets = read_all_tickets(&dir.path().to_path_buf()).unwrap();
        let open: Vec<_> = tickets.iter().filter(|t| t.status == "open").collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id, 1);

        let closed: Vec<_> = tickets.iter().filter(|t| t.status == "closed").collect();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].id, 2);
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
