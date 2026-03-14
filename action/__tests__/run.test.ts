import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock @actions modules
vi.mock("@actions/core", () => ({
  info: vi.fn(),
  warning: vi.fn(),
  getInput: vi.fn(),
  setOutput: vi.fn(),
  setFailed: vi.fn(),
  setSecret: vi.fn(),
  exportVariable: vi.fn(),
}));

vi.mock("@actions/exec", () => ({
  exec: vi.fn().mockResolvedValue(0),
}));

vi.mock("@actions/github", () => ({
  context: {
    eventName: "issues",
    payload: {
      label: { name: "o-ready" },
      issue: { number: 42 },
    },
    repo: { owner: "test-owner", repo: "test-repo" },
  },
  getOctokit: vi.fn().mockReturnValue({
    rest: {
      users: {
        getAuthenticated: vi.fn().mockResolvedValue({
          data: { login: "test-app[bot]", id: 12345 },
        }),
      },
      issues: {
        createComment: vi.fn().mockResolvedValue({}),
      },
    },
  }),
}));

describe("run", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("skips non-o-ready label events", async () => {
    const github = await import("@actions/github");
    const core = await import("@actions/core");

    // Override context for this test
    Object.defineProperty(github, "context", {
      value: {
        eventName: "issues",
        payload: {
          label: { name: "bug" },
          issue: { number: 42 },
        },
        repo: { owner: "test", repo: "test" },
      },
      writable: true,
    });

    const { run } = await import("../src/run");
    const result = await run();
    expect(result.status).toBe("skipped");
    expect(core.info).toHaveBeenCalledWith(
      expect.stringContaining("not \"o-ready\""),
    );
  });

  it("handles workflow_dispatch with issue-number", async () => {
    const github = await import("@actions/github");
    const core = await import("@actions/core");

    Object.defineProperty(github, "context", {
      value: {
        eventName: "workflow_dispatch",
        payload: {
          inputs: { "issue-number": "99" },
        },
        repo: { owner: "test", repo: "test" },
      },
      writable: true,
    });

    vi.mocked(core.getInput).mockImplementation((name: string) => {
      const inputs: Record<string, string> = {
        "anthropic-api-key": "test-key",
        "github-token": "test-token",
        "auto-merge": "false",
        "max-parallel": "2",
        "cost-budget": "10.0",
      };
      return inputs[name] ?? "";
    });

    const exec = await import("@actions/exec");
    vi.mocked(exec.exec).mockResolvedValue(0);

    const { run } = await import("../src/run");
    const result = await run();
    expect(result.status).toBe("complete");
  });

  it("sets environment variables from inputs", async () => {
    const github = await import("@actions/github");
    const core = await import("@actions/core");

    Object.defineProperty(github, "context", {
      value: {
        eventName: "issues",
        payload: {
          label: { name: "o-ready" },
          issue: { number: 10 },
        },
        repo: { owner: "test", repo: "test" },
      },
      writable: true,
    });

    vi.mocked(core.getInput).mockImplementation((name: string) => {
      const inputs: Record<string, string> = {
        "anthropic-api-key": "sk-ant-test",
        "github-token": "ghp_test",
        "auto-merge": "false",
        "max-parallel": "2",
        "cost-budget": "10.0",
      };
      return inputs[name] ?? "";
    });

    const exec = await import("@actions/exec");
    vi.mocked(exec.exec).mockResolvedValue(0);

    const { run } = await import("../src/run");
    await run();

    expect(core.setSecret).toHaveBeenCalledWith("sk-ant-test");
    expect(core.setSecret).toHaveBeenCalledWith("ghp_test");
    // Secrets should NOT be exported globally via core.exportVariable
    expect(core.exportVariable).not.toHaveBeenCalled();
  });
});
