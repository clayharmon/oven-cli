import * as core from "@actions/core";
import * as exec from "@actions/exec";
import * as github from "@actions/github";

interface RunResult {
  runId: string;
  status: string;
  cost: string;
  prNumber: string;
}

function getIssueNumber(): number | null {
  const context = github.context;

  // workflow_dispatch with issue-number input
  if (context.eventName === "workflow_dispatch") {
    const issueNumber = parseInt(
      (context.payload.inputs as Record<string, string>)?.["issue-number"] ??
        "",
      10,
    );
    return isNaN(issueNumber) ? null : issueNumber;
  }

  // issues event (labeled)
  if (context.eventName === "issues") {
    const label = context.payload.label?.name;
    if (label !== "o-ready") {
      core.info(`Skipping: label "${label}" is not "o-ready"`);
      return null;
    }
    return context.payload.issue?.number ?? null;
  }

  core.warning(`Unexpected event: ${context.eventName}`);
  return null;
}

function parseOvenReport(output: string): Partial<RunResult> {
  const result: Partial<RunResult> = {};

  // Parse run ID from oven output (format: "run <id>")
  const runIdMatch = output.match(/run\s+([a-f0-9]{8})/i);
  if (runIdMatch) {
    result.runId = runIdMatch[1];
  }

  // Parse cost (format: "$X.XX")
  const costMatch = output.match(/\$(\d+\.\d+)/);
  if (costMatch) {
    result.cost = costMatch[1];
  }

  // Parse PR number (format: "#N" or "PR #N" or "pull/N")
  const prMatch = output.match(/(?:PR\s*#|pull\/)(\d+)/i);
  if (prMatch) {
    result.prNumber = prMatch[1];
  }

  return result;
}

export async function run(): Promise<RunResult> {
  const issueNumber = getIssueNumber();
  if (issueNumber === null) {
    core.info("No issue to process, exiting");
    return { runId: "", status: "skipped", cost: "0", prNumber: "" };
  }

  const autoMerge = core.getInput("auto-merge") === "true";
  const maxParallel = core.getInput("max-parallel");
  const costBudget = core.getInput("cost-budget");

  // Set up environment -- mask secrets so they don't appear in logs
  const anthropicKey = core.getInput("anthropic-api-key");
  const ghToken = core.getInput("github-token");
  core.setSecret(anthropicKey);
  core.setSecret(ghToken);
  core.exportVariable("ANTHROPIC_API_KEY", anthropicKey);
  core.exportVariable("GH_TOKEN", ghToken);

  // Run oven prep if recipe.toml doesn't exist
  try {
    await exec.exec("test", ["-f", "recipe.toml"]);
  } catch {
    core.info("No recipe.toml found, running oven prep");
    await exec.exec("oven", ["prep"]);
  }

  // Build oven on command
  const args = ["on", issueNumber.toString()];
  if (autoMerge) {
    args.push("-m");
  }

  core.info(`Running: oven ${args.join(" ")}`);

  let stdout = "";
  let stderr = "";
  let exitCode = 0;

  try {
    exitCode = await exec.exec("oven", args, {
      env: {
        ...process.env,
        OVEN_MAX_PARALLEL: maxParallel,
        OVEN_COST_BUDGET: costBudget,
      },
      listeners: {
        stdout: (data: Buffer) => {
          stdout += data.toString();
        },
        stderr: (data: Buffer) => {
          stderr += data.toString();
        },
      },
    });
  } catch (error) {
    exitCode = 1;
    if (error instanceof Error) {
      stderr += error.message;
    }
  }

  const status = exitCode === 0 ? "complete" : "failed";
  const parsed = parseOvenReport(stdout + stderr);

  const result: RunResult = {
    runId: parsed.runId ?? "",
    status,
    cost: parsed.cost ?? "0",
    prNumber: parsed.prNumber ?? "",
  };

  // Post summary comment on the issue
  const token = core.getInput("github-token");
  const octokit = github.getOctokit(token);
  const { owner, repo } = github.context.repo;

  const body =
    status === "complete"
      ? `Oven pipeline completed successfully.\n\n- Run ID: \`${result.runId}\`\n- Cost: $${result.cost}\n- PR: #${result.prNumber}`
      : `Oven pipeline failed.\n\n- Run ID: \`${result.runId}\`\n- Cost: $${result.cost}\n\nCheck the [workflow run logs](${process.env.GITHUB_SERVER_URL}/${owner}/${repo}/actions/runs/${process.env.GITHUB_RUN_ID}) for error details.`;

  await octokit.rest.issues.createComment({
    owner,
    repo,
    issue_number: issueNumber,
    body,
  });

  return result;
}
