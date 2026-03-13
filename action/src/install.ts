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

export async function installOven(version: string): Promise<string> {
  const platform = getPlatform();

  if (version === "latest") {
    core.info("Installing oven via cargo install (latest)");
    await exec.exec("cargo", ["install", "oven-cli"]);
    return "latest";
  }

  // Try to download a pre-built binary from GitHub releases
  const target = `${platform.arch}-unknown-${platform.os}-gnu`;
  const ext = platform.os === "linux" ? "tar.gz" : "tar.gz";
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

export async function installClaude(): Promise<void> {
  const platform = getPlatform();

  // Claude CLI is installed via npm
  core.info("Installing Claude CLI via npm");
  await exec.exec("npm", ["install", "-g", "@anthropic-ai/claude-code"]);
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

export async function install(version: string): Promise<void> {
  const installedVersion = await installOven(version);
  await installClaude();
  await verifyInstallation();

  const binDir = path.join(os.homedir(), ".cargo", "bin");
  core.addPath(binDir);
}
