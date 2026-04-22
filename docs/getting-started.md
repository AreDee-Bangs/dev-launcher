# Getting Started with dev-feature

`dev-feature` is a Rust TUI launcher that orchestrates the Filigran multi-product development stack. It creates Git worktrees for each product, starts Docker-backed services, and provides a unified terminal interface for logs, health checks, and crash diagnosis.

---

## Requirements

Before installing, make sure the following are present on your machine:

- **Git 2.5+** (worktree support is required)
- **Docker Desktop** running and responsive
- **Node.js 20+** and Yarn (`corepack enable` to activate Yarn via Corepack)
- **Python 3.13+** with `venv` available
- **16 GB RAM** recommended when running all products simultaneously

---

## Installation

### Download

Grab the latest release from GitHub:

**<https://github.com/AreDee-Bangs/dev-launcher/releases/latest>**

Choose the binary that matches your platform:

| Platform | Filename |
|---|---|
| macOS (universal) | `dev-feature-macos` |
| Linux x86\_64 | `dev-feature-linux-x86_64` |
| Linux arm64 | `dev-feature-linux-arm64` |
| Windows x86\_64 | `dev-feature-windows-x86_64.exe` |
| Windows arm64 | `dev-feature-windows-arm64.exe` |

### macOS / Linux

```sh
# Download
curl -Lo dev-feature https://github.com/AreDee-Bangs/dev-launcher/releases/latest/download/dev-feature-macos

chmod +x dev-feature
sudo mv dev-feature /usr/local/bin/
```

Replace `dev-feature-macos` with the correct filename for your platform if you are on Linux.

### Windows

Copy the `.exe` to a directory that is already on your `PATH` (for example `C:\Users\<you>\bin\`), or add the directory containing the binary to `PATH` via **System Settings > Advanced system settings > Environment Variables**.

### Verify the installation

```sh
dev-feature --version
```

---

## Setting the workspace root

The workspace root is the single directory that contains (or will contain) all product repositories as subdirectories:

```
~/dev/filigran/
  filigran-copilot/
  opencti/
  openaev/
  connectors/
```

`dev-feature` needs to know where this directory lives. There are three ways to provide it, listed in priority order:

1. **`--workspace-root <path>` flag** - passed directly on each invocation
2. **`FILIGRAN_WORKSPACE_ROOT` environment variable** (recommended) - set once in your shell profile
3. **First-run wizard** - interactive prompt that saves the value to `~/.dev-launcher/config`

### Recommended: environment variable

Add the following line to your `~/.zshrc` or `~/.bashrc`:

```sh
export FILIGRAN_WORKSPACE_ROOT="$HOME/dev/filigran"
```

On Windows, add this to your PowerShell profile (`$PROFILE`):

```powershell
$env:FILIGRAN_WORKSPACE_ROOT = "C:\dev\filigran"
```

Reload your shell (or open a new terminal) for the change to take effect.

---

## First run

If `FILIGRAN_WORKSPACE_ROOT` is not set and no saved config exists at `~/.dev-launcher/config`, the tool launches an interactive setup wizard:

1. **Prompts for the workspace root path.** The directory will be created if it does not already exist.
2. **Detects missing repositories** and offers to clone them automatically.
3. **Saves the configuration** to `~/.dev-launcher/config` so the wizard does not run again.

After the wizard completes, the launcher proceeds normally.

---

## Quickstart: launching the stack

```sh
# Launch with interactive product/branch selector
dev-feature

# Launch Copilot only on a specific branch (non-interactive)
dev-feature --copilot-branch feat/my-feature

# Launch full stack for a cross-product feature
dev-feature --copilot-branch my-feat --opencti-branch my-feat --connector-branch my-feat

# Resume an existing workspace by its hash
dev-feature --workspace a1b2c3d4
```

---

## Adding LLM-assisted diagnosis (optional)

The crash-diagnosis engine can optionally send unknown failure output to an LLM for analysis. To enable it, add your API key to `~/.dev-launcher/config`:

```
llm_api_key=sk-ant-...
```

Alternatively, set the `FILIGRAN_LLM_KEY` environment variable in your shell profile:

```sh
export FILIGRAN_LLM_KEY="sk-ant-..."
```

For the full list of configuration options, see [`docs/configuration.md`](configuration.md).
