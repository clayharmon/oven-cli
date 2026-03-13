use anyhow::{Context, Result};

use super::GhClient;
use crate::process::CommandRunner;

/// Label colors for oven labels.
const LABEL_COLORS: &[(&str, &str, &str)] = &[
    ("o-ready", "0E8A16", "Ready for oven pipeline pickup"),
    ("o-cooking", "FBCA04", "Oven pipeline is working on this"),
    ("o-complete", "1D76DB", "Oven pipeline completed successfully"),
    ("o-failed", "D93F0B", "Oven pipeline failed"),
];

impl<R: CommandRunner> GhClient<R> {
    /// Add a label to an issue.
    pub async fn add_label(&self, issue_number: u32, label: &str) -> Result<()> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&["issue", "edit", &issue_number.to_string(), "--add-label", label]),
                &self.repo_dir,
            )
            .await
            .context("adding label")?;
        Self::check_output(&output, "add label")?;
        Ok(())
    }

    /// Remove a label from an issue.
    pub async fn remove_label(&self, issue_number: u32, label: &str) -> Result<()> {
        let output = self
            .runner
            .run_gh(
                &Self::s(&["issue", "edit", &issue_number.to_string(), "--remove-label", label]),
                &self.repo_dir,
            )
            .await
            .context("removing label")?;
        // Removing a label that doesn't exist is not an error
        if !output.success && !output.stderr.contains("not found") {
            anyhow::bail!("remove label failed: {}", output.stderr.trim());
        }
        Ok(())
    }

    /// Ensure all oven labels exist in the repository.
    pub async fn ensure_labels_exist(&self) -> Result<()> {
        for (name, color, description) in LABEL_COLORS {
            let output = self
                .runner
                .run_gh(
                    &Self::s(&[
                        "label",
                        "create",
                        name,
                        "--color",
                        color,
                        "--description",
                        description,
                        "--force",
                    ]),
                    &self.repo_dir,
                )
                .await
                .context("creating label")?;
            Self::check_output(&output, &format!("create label {name}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::{github::GhClient, process::CommandOutput};

    fn mock_runner(success: bool) -> crate::process::MockCommandRunner {
        let mut mock = crate::process::MockCommandRunner::new();
        mock.expect_run_gh().returning(move |_, _| {
            Box::pin(async move {
                Ok(CommandOutput { stdout: String::new(), stderr: String::new(), success })
            })
        });
        mock
    }

    #[tokio::test]
    async fn add_label_succeeds() {
        let client = GhClient::new(mock_runner(true), Path::new("/tmp"));
        let result = client.add_label(42, "o-cooking").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_label_failure_propagates() {
        let mut mock = crate::process::MockCommandRunner::new();
        mock.expect_run_gh().returning(|_, _| {
            Box::pin(async {
                Ok(CommandOutput {
                    stdout: String::new(),
                    stderr: "not authorized".to_string(),
                    success: false,
                })
            })
        });
        let client = GhClient::new(mock, Path::new("/tmp"));
        let result = client.add_label(42, "o-cooking").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ensure_labels_exist_succeeds() {
        let client = GhClient::new(mock_runner(true), Path::new("/tmp"));
        let result = client.ensure_labels_exist().await;
        assert!(result.is_ok());
    }
}
