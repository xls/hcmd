# holos-installer

Installs [hcmd](https://github.com/xls/holos), a Total Commander alternative
for the terminal.

```sh
npx holos-installer            # install the latest release
npx holos-installer update     # the same, and says so when already current
npx holos-installer --version  # what is installed, and what is current
```

It downloads the release build for your platform, checks it against the
published `SHA256SUMS`, and installs to `~/.local/bin`. It never needs root.

**It always installs the latest release**, not the version of this package.
This package's version says when the installer itself last changed; what it
installs is whatever `xls/holos` has published, asked for at the moment you run
it.

| Variable | Meaning |
| --- | --- |
| `HCMD_INSTALL_DIR` | where to put the binary (default `~/.local/bin`) |
| `HCMD_VERSION` | which release to fetch (default the latest) |

This package is the installer, not the program: it has no dependencies and
contains one Node script. If you would rather not run an installer at all,
download the tarball for your platform from
[Releases](https://github.com/xls/holos/releases), or use the shell equivalent:

```sh
curl -fsSL https://raw.githubusercontent.com/xls/holos/main/install.sh | sh
```
