#!/usr/bin/env node

const { spawnSync } = require("node:child_process");
const path = require("node:path");

function extension() {
  return process.platform === "win32" ? ".exe" : "";
}

function resolveBinary() {
  const os = process.platform === "win32" ? "windows" : process.platform;
  const arch = process.arch;
  const platformKey = `${os}-${arch}`;
  const exe = `mail-imap-mcp-rs${extension()}`;

  switch (platformKey) {
    case "linux-x64":
    case "darwin-x64":
    case "darwin-arm64":
    case "windows-x64":
      return path.join(__dirname, platformKey, exe);
    default:
      throw new Error(
        `Unsupported platform: ${platformKey}. Supported platforms are linux-x64, darwin-x64, darwin-arm64, windows-x64.`
      );
  }
}

const result = spawnSync(resolveBinary(), process.argv.slice(2), {
  stdio: "inherit",
});

process.exit(result.status ?? 1);
