import * as core from "@actions/core";
import * as exec from "@actions/exec";
import * as tc from "@actions/tool-cache";
import * as os from "os";
import * as path from "path";

interface Platform {
  os: string;
  arch: string;
}

function getPlatform(): Platform {
  const platform = os.platform();
  const arch = os.arch();

  let osName: string;
  if (platform === "linux") {
    osName = "linux";
  } else if (platform === "darwin") {
    osName = "darwin";
  } else {
    throw new Error(`Unsupported platform: ${platform}`);
  }

  let archName: string;
  if (arch === "x64") {
    archName = "x86_64";
  } else if (arch === "arm64") {
    archName = "aarch64";
  } else {
    throw new Error(`Unsupported architecture: ${arch}`);
  }

  return { os: osName, arch: archName };
}

const SEMVER_RE = /^\d+\.\d+\.\d+(-[\w.]+)?$/;

function validateVersion(version: string): void {
  if (version !== "latest" && !SEMVER_RE.test(version)) {
    throw new Error(
      `Invalid oven-version "${version}": must be "latest" or a valid semver (e.g. "1.2.3")`,
    );
  }
}

export async function installOven(version: string): Promise<string> {
  validateVersion(version);
  const platform = getPlatform();

  if (version === "latest") {
    core.info("Installing oven via cargo install (latest)");
    await exec.exec("cargo", ["install", "oven-cli"]);
    return "latest";
  }

  // Try to download a pre-built binary from GitHub releases
  const target = `${platform.arch}-unknown-${platform.os}-gnu`;
  const ext = "tar.gz";
  const url = `https://github.com/clayharmon/oven-cli/releases/download/v${version}/oven-${target}.${ext}`;

  core.info(`Downloading oven v${version} from ${url}`);

  try {
    const downloadPath = await tc.downloadTool(url);
    const extractedPath = await tc.extractTar(downloadPath);
    core.addPath(extractedPath);
    return version;
  } catch {
    core.warning(
      `Pre-built binary not found for v${version}, falling back to cargo install`,
    );
    await exec.exec("cargo", ["install", "oven-cli", "--version", version]);
    return version;
  }
}

function validateClaudeVersion(version: string | undefined): void {
  if (version && !SEMVER_RE.test(version)) {
    throw new Error(
      `Invalid claude-version "${version}": must be a valid semver (e.g. "1.0.5") or empty for latest`,
    );
  }
}

export async function installClaude(claudeVersion?: string): Promise<void> {
  validateClaudeVersion(claudeVersion);
  const pkg = claudeVersion
    ? `@anthropic-ai/claude-code@${claudeVersion}`
    : "@anthropic-ai/claude-code";
  core.info(`Installing Claude CLI via npm: ${pkg}`);
  await exec.exec("npm", ["install", "-g", pkg]);
}

export async function verifyInstallation(): Promise<void> {
  core.info("Verifying installation...");

  let ovenOutput = "";
  await exec.exec("oven", ["--version"], {
    listeners: {
      stdout: (data: Buffer) => {
        ovenOutput += data.toString();
      },
    },
  });
  core.info(`oven version: ${ovenOutput.trim()}`);

  let claudeOutput = "";
  await exec.exec("claude", ["--version"], {
    listeners: {
      stdout: (data: Buffer) => {
        claudeOutput += data.toString();
      },
    },
  });
  core.info(`claude version: ${claudeOutput.trim()}`);
}

export async function install(
  version: string,
  claudeVersion?: string,
): Promise<void> {
  const installedVersion = await installOven(version);
  await installClaude(claudeVersion);
  await verifyInstallation();

  const binDir = path.join(os.homedir(), ".cargo", "bin");
  core.addPath(binDir);
}
