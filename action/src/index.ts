import * as core from "@actions/core";
import { install } from "./install";
import { run } from "./run";

async function main(): Promise<void> {
  try {
    // Install dependencies
    const version = core.getInput("oven-version");
    const claudeVersion = core.getInput("claude-version") || undefined;
    await install(version, claudeVersion);

    // Run the pipeline
    const result = await run();

    // Set outputs
    core.setOutput("run-id", result.runId);
    core.setOutput("status", result.status);
    core.setOutput("cost", result.cost);
    core.setOutput("pr-number", result.prNumber);

    if (result.status === "failed") {
      core.setFailed(`Oven pipeline failed (run: ${result.runId})`);
    }
  } catch (error) {
    if (error instanceof Error) {
      core.setFailed(error.message);
    } else {
      core.setFailed("An unexpected error occurred");
    }
  }
}

// Register SIGTERM handler for graceful shutdown
process.on("SIGTERM", () => {
  core.setFailed("Pipeline cancelled (SIGTERM)");
  process.exit(1);
});

main();
