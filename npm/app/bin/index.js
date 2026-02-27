#!/usr/bin/env node

const { spawnSync } = require("node:child_process");

function packageOs() {
  return process.platform === "win32" ? "windows" : process.platform;
}

function extension() {
  return process.platform === "win32" ? ".exe" : "";
}

function resolveBinary() {
  const os = packageOs();
  const arch = process.arch;
  const pkg = `@bradsjm/mail-imap-mcp-rs-${os}-${arch}`;
  const exe = `mail-imap-mcp-rs${extension()}`;
  try {
    return require.resolve(`${pkg}/bin/${exe}`);
  } catch {
    throw new Error(
      `Unsupported platform or missing package: ${os}-${arch}. Expected optional dependency ${pkg}.`
    );
  }
}

const result = spawnSync(resolveBinary(), process.argv.slice(2), {
  stdio: "inherit",
});

process.exit(result.status ?? 1);
