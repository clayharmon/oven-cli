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
    return issueNumber > 0 ? issueNumber : null;
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

  // Mask secrets so they don't appear in logs. Do NOT use core.exportVariable
  // which persists secrets to $GITHUB_ENV, making them available to all
  // subsequent steps (including third-party actions).
  const anthropicKey = core.getInput("anthropic-api-key");
  const ghToken = core.getInput("github-token");
  core.setSecret(anthropicKey);
  core.setSecret(ghToken);

  const secretEnv = {
    ...process.env,
    ANTHROPIC_API_KEY: anthropicKey,
    GH_TOKEN: ghToken,
  };

  // Configure git identity from the authenticated token so oven can make
  // commits (e.g. the seed empty commit before PR creation). Works for both
  // GitHub App tokens (bot identity) and PATs (user identity).
  const octokit = github.getOctokit(ghToken);
  const { data: authUser } = await octokit.rest.users.getAuthenticated();
  await exec.exec("git", [
    "config",
    "user.name",
    authUser.login,
  ]);
  await exec.exec("git", [
    "config",
    "user.email",
    `${authUser.id}+${authUser.login}@users.noreply.github.com`,
  ]);

  // Run oven prep if recipe.toml doesn't exist
  try {
    await exec.exec("test", ["-f", "recipe.toml"]);
  } catch {
    core.info("No recipe.toml found, running oven prep");
    await exec.exec("oven", ["prep"], { env: secretEnv });
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
        ...secretEnv,
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
