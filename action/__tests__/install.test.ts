import { describe, it, expect, vi, beforeEach } from "vitest";

// Mock @actions modules before importing
vi.mock("@actions/core", () => ({
  info: vi.fn(),
  warning: vi.fn(),
  addPath: vi.fn(),
}));

vi.mock("@actions/exec", () => ({
  exec: vi.fn().mockResolvedValue(0),
}));

vi.mock("@actions/tool-cache", () => ({
  downloadTool: vi.fn(),
  extractTar: vi.fn(),
}));

describe("install", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it("installOven uses cargo install for latest version", async () => {
    const exec = await import("@actions/exec");
    const { installOven } = await import("../src/install");

    const result = await installOven("latest");
    expect(result).toBe("latest");
    expect(exec.exec).toHaveBeenCalledWith("cargo", ["install", "oven-cli"]);
  });

  it("installClaude installs via npm with default version", async () => {
    const exec = await import("@actions/exec");
    const { installClaude } = await import("../src/install");

    await installClaude();
    expect(exec.exec).toHaveBeenCalledWith("npm", [
      "install",
      "-g",
      "@anthropic-ai/claude-code",
    ]);
  });

  it("installClaude installs pinned version when specified", async () => {
    const exec = await import("@actions/exec");
    const { installClaude } = await import("../src/install");

    await installClaude("1.0.5");
    expect(exec.exec).toHaveBeenCalledWith("npm", [
      "install",
      "-g",
      "@anthropic-ai/claude-code@1.0.5",
    ]);
  });

  it("installOven rejects path traversal in version", async () => {
    const { installOven } = await import("../src/install");
    await expect(installOven("../../evil")).rejects.toThrow("Invalid oven-version");
  });

  it("installOven rejects shell metacharacters in version", async () => {
    const { installOven } = await import("../src/install");
    await expect(installOven("1.0.0; rm -rf /")).rejects.toThrow(
      "Invalid oven-version",
    );
  });

  it("installClaude rejects registry redirection in version", async () => {
    const { installClaude } = await import("../src/install");
    await expect(
      installClaude("1.0.0 --registry https://evil.com"),
    ).rejects.toThrow("Invalid claude-version");
  });

  it("installClaude rejects path traversal in version", async () => {
    const { installClaude } = await import("../src/install");
    await expect(installClaude("../../evil")).rejects.toThrow(
      "Invalid claude-version",
    );
  });

  it("installClaude rejects shell metacharacters in version", async () => {
    const { installClaude } = await import("../src/install");
    await expect(installClaude("; rm -rf /")).rejects.toThrow(
      "Invalid claude-version",
    );
  });

  it("installClaude accepts valid prerelease version", async () => {
    const exec = await import("@actions/exec");
    const { installClaude } = await import("../src/install");

    await installClaude("1.0.0-beta.1");
    expect(exec.exec).toHaveBeenCalledWith("npm", [
      "install",
      "-g",
      "@anthropic-ai/claude-code@1.0.0-beta.1",
    ]);
  });

  it("verifyInstallation calls both --version commands", async () => {
    const exec = await import("@actions/exec");
    const { verifyInstallation } = await import("../src/install");

    await verifyInstallation();
    expect(exec.exec).toHaveBeenCalledWith(
      "oven",
      ["--version"],
      expect.any(Object),
    );
    expect(exec.exec).toHaveBeenCalledWith(
      "claude",
      ["--version"],
      expect.any(Object),
    );
  });
});
