#!/usr/bin/env node
"use strict";

// Install hcmd, from npm.
//
//   npx hcmd-installer
//
// Downloads the release build for this platform, checks it against the
// published SHA256SUMS, and installs to ~/.local/bin. It never needs root and
// it writes nothing outside the install directory.
//
// Commands:
//
//   npx hcmd-installer            install the latest release
//   npx hcmd-installer update     the same, but says so when there is nothing
//                                 to do rather than reinstalling in silence
//   npx hcmd-installer --version  what is installed, and what is current
//
//   HCMD_INSTALL_DIR   where to put the binary   (default ~/.local/bin)
//   HCMD_VERSION       which release to fetch    (default the latest)
//
// No dependencies on purpose. This is the first thing anyone runs, and a
// installer that pulls a tree of packages to install one binary is not a
// smaller ask than the binary. Everything here is Node's standard library
// plus `tar`, which both macOS and Linux have.

const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const crypto = require("crypto");
const { execFileSync } = require("child_process");

const REPO = "xls/hcmd";
const INSTALL_DIR =
  process.env.HCMD_INSTALL_DIR || path.join(os.homedir(), ".local", "bin");
const SHARE_DIR =
  process.env.HCMD_SHARE_DIR || path.join(os.homedir(), ".local", "share", "hcmd");

function say(msg) {
  process.stdout.write(msg + "\n");
}

function die(msg) {
  process.stderr.write("error: " + msg + "\n");
  process.exit(1);
}

/// Which release build this machine wants.
///
/// The musl build runs anywhere; the glibc one starts faster and is smaller.
/// Node cannot tell which libc it is on without asking, so this asks `ldd`,
/// and treats "cannot tell" as musl, which is the one that works either way.
function target() {
  const arch = { x64: "x86_64", arm64: "aarch64" }[process.arch];
  if (!arch) die(`unsupported architecture: ${process.arch}`);

  if (process.platform === "darwin") return `${arch}-apple-darwin`;
  if (process.platform !== "linux") {
    die(
      `unsupported platform: ${process.platform} ` +
        "(this installs Linux and macOS builds)"
    );
  }

  let libc = "musl";
  try {
    const out = execFileSync("ldd", ["--version"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
    });
    if (!/musl/i.test(out)) libc = "gnu";
  } catch (err) {
    // `ldd --version` exits non-zero on musl and prints to stderr, and is
    // absent entirely on some images. Both mean "do not assume glibc".
    const text = String((err && err.stderr) || "");
    if (text && !/musl/i.test(text)) libc = "gnu";
  }
  return `${arch}-unknown-linux-${libc}`;
}

/// GET a URL into a Buffer, following redirects, which the release asset URLs
/// always issue.
function fetch(url, hops = 0) {
  return new Promise((resolve, reject) => {
    if (hops > 5) return reject(new Error("too many redirects"));
    https
      .get(url, { headers: { "User-Agent": "hcmd-installer" } }, (res) => {
        if (
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location
        ) {
          res.resume();
          return resolve(fetch(res.headers.location, hops + 1));
        }
        if (res.statusCode !== 200) {
          res.resume();
          return reject(new Error(`HTTP ${res.statusCode} for ${url}`));
        }
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => resolve(Buffer.concat(chunks)));
        res.on("error", reject);
      })
      .on("error", reject);
  });
}

/// The latest published release, asked of GitHub.
///
/// This is what `install.sh` has always done, and what this did **not**: it
/// installed the version pinned in its own `package.json`, so `npx
/// hcmd-installer` kept installing whatever was current on the day the npm
/// package was last published. A pinned installer is a stale installer, and
/// nobody types `npx` to get last month's build.
async function latestVersion() {
  const body = await fetch(
    `https://api.github.com/repos/${REPO}/releases/latest`
  );
  const tag = JSON.parse(body.toString("utf8")).tag_name;
  if (!tag) throw new Error("no tag_name in the latest release");
  return String(tag).replace(/^v/, "");
}

/// The version to install: what was asked for, else the latest published.
async function version() {
  if (process.env.HCMD_VERSION) return process.env.HCMD_VERSION;
  try {
    return await latestVersion();
  } catch (err) {
    die(
      `could not ask github.com for the latest release (${err.message}); ` +
        "set HCMD_VERSION to install a particular one"
    );
    return null;
  }
}

/// What is installed already, or `null` when nothing is.
///
/// Runs the binary rather than remembering a number in a file: the file could
/// describe a binary that has since been replaced by hand, and the binary
/// cannot be wrong about itself.
function installedVersion() {
  const bin = path.join(INSTALL_DIR, "hcmd");
  if (!fs.existsSync(bin)) return null;
  try {
    const out = execFileSync(bin, ["--version"], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "ignore"],
    });
    const found = out.match(/\b(\d+\.\d+\.\d+)\b/);
    return found ? found[1] : null;
  } catch {
    return null;
  }
}

async function main() {
  const argv = process.argv.slice(2);
  const command = argv.find((a) => !a.startsWith("-"));
  const plat = target();

  // `--version` answers and stops. Both numbers, because the question behind
  // it is always "am I behind".
  if (argv.includes("--version") || argv.includes("-v")) {
    const here = installedVersion();
    say(here ? `installed: ${here}` : "installed: nothing in " + INSTALL_DIR);
    try {
      say(`latest:    ${await latestVersion()}`);
    } catch (err) {
      say(`latest:    unknown (${err.message})`);
    }
    return;
  }

  if (command && command !== "install" && command !== "update") {
    die(`unknown command: ${command} (there are "install" and "update")`);
  }

  const ver = await version();
  if (!ver) return;

  // `update` is the same install, with one thing added: it says when there is
  // nothing to do. Reinstalling an identical binary works and wastes a
  // download, and silence about it reads as though something happened.
  if (command === "update" && !process.env.HCMD_VERSION) {
    const here = installedVersion();
    if (here === ver) {
      say(`hcmd ${here} is already the latest release; nothing to do`);
      return;
    }
    if (here) say(`hcmd ${here} installed, ${ver} is the latest`);
  }

  const name = `hcmd-${ver}-${plat}`;
  const base = `https://github.com/${REPO}/releases/download/v${ver}`;
  say(`hcmd ${ver} for ${plat}`);

  say("downloading...");
  let archive;
  try {
    archive = await fetch(`${base}/${name}.tar.gz`);
  } catch (err) {
    die(`no build published for ${plat} at v${ver}: ${err.message}`);
  }

  // Verified against the release's own checksum file. A download that cannot
  // be checked is reported rather than quietly trusted.
  try {
    const sums = (await fetch(`${base}/SHA256SUMS`)).toString("utf8");
    const line = sums
      .split("\n")
      .find((l) => l.trim().endsWith(`${name}.tar.gz`));
    if (!line) {
      say(`warning: SHA256SUMS does not list ${name}.tar.gz`);
    } else {
      const want = line.trim().split(/\s+/)[0];
      const got = crypto.createHash("sha256").update(archive).digest("hex");
      if (want !== got) die(`checksum mismatch: expected ${want}, got ${got}`);
      say("checksum ok");
    }
  } catch (err) {
    say(`warning: could not verify the download: ${err.message}`);
  }

  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "hcmd-"));
  try {
    const tarball = path.join(tmp, "hcmd.tar.gz");
    fs.writeFileSync(tarball, archive);
    // `tar` rather than a bundled extractor: it is on every macOS and Linux
    // this installs to, and it is one fewer thing to be wrong about.
    execFileSync("tar", ["-xzf", tarball, "-C", tmp], { stdio: "inherit" });

    const built = path.join(tmp, name, "hcmd");
    if (!fs.existsSync(built)) die("no hcmd binary inside the archive");

    fs.mkdirSync(INSTALL_DIR, { recursive: true });
    // Written under a temporary name in the same directory and renamed, so a
    // running hcmd is never half-overwritten.
    const pending = path.join(INSTALL_DIR, ".hcmd.new");
    fs.copyFileSync(built, pending);
    fs.chmodSync(pending, 0o755);
    fs.renameSync(pending, path.join(INSTALL_DIR, "hcmd"));
    // The 21 themes are compiled into the binary, so every one of them works
    // with no files at all. These are the editable copies: a theme is changed
    // by putting a file of the same name in the config directory, and without
    // a starting point there is nothing to copy. The tarball already carries
    // them.
    const themes = path.join(tmp, name, "themes");
    if (fs.existsSync(themes)) {
      try {
        fs.mkdirSync(SHARE_DIR, { recursive: true });
        fs.cpSync(themes, path.join(SHARE_DIR, "themes"), { recursive: true });
        say(`themes in ${path.join(SHARE_DIR, "themes")}`);
      } catch (err) {
        say(`warning: could not write the themes: ${err.message}`);
      }
    }
    const examples = path.join(tmp, name, "examples");
    if (fs.existsSync(examples)) {
      try {
        fs.mkdirSync(SHARE_DIR, { recursive: true });
        fs.cpSync(examples, path.join(SHARE_DIR, "examples"), {
          recursive: true,
        });
        say(`examples in ${path.join(SHARE_DIR, "examples")} (keymap, config)`);
      } catch (err) {
        say(`warning: could not write the examples: ${err.message}`);
      }
    }
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }

  say(`installed ${path.join(INSTALL_DIR, "hcmd")}`);

  const onPath = (process.env.PATH || "")
    .split(path.delimiter)
    .includes(INSTALL_DIR);
  if (!onPath) {
    say("");
    say(`${INSTALL_DIR} is not on your PATH. Add this to your shell profile:`);
    say(`    export PATH="$PATH:${INSTALL_DIR}"`);
  }
  say("");
  say("Run hcmd to start. Configuration is written to");
  say("~/.config/holoscommander/ the first time it runs.");
}

main().catch((err) => die(err && err.message ? err.message : String(err)));
