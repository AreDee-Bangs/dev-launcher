# Getting Started with dev-launcher

`dev-launcher` is a Rust TUI launcher that orchestrates the Filigran multi-product development stack. It creates Git worktrees for each product, starts Docker-backed services, and provides a unified terminal interface for logs, health checks, and crash diagnosis.

---

## Requirements

Before installing, make sure the following are present on your machine:

- **Git 2.5+** (worktree support is required)
- **Docker Desktop** running and responsive
- **Node.js 20+** and Yarn (`corepack enable` to activate Yarn via Corepack)
- **Python 3.13+** with `venv` available
- **16 GB RAM** recommended when running all products simultaneously

> **Windows users** — see the [Windows (WSL2)](#windows-wsl2) section below before continuing.

---

## Installation

### Download

Grab the latest release from GitHub:

**<https://github.com/AreDee-Bangs/dev-launcher/releases/latest>**

Choose the binary that matches your platform:

| Platform | Filename |
|---|---|
| macOS (universal) | `dev-launcher-macos` |
| Linux x86\_64 | `dev-launcher-linux-x86_64` |
| Linux arm64 | `dev-launcher-linux-arm64` |
| Windows (WSL2 wrapper) | `dev-launcher.ps1` |

### macOS / Linux

```sh
# Download
curl -Lo dev-launcher https://github.com/AreDee-Bangs/dev-launcher/releases/latest/download/dev-launcher-macos

chmod +x dev-launcher
sudo mv dev-launcher /usr/local/bin/
```

Replace `dev-launcher-macos` with the correct filename for your platform if you are on Linux.

### Windows (WSL2)

`dev-launcher` uses Unix process groups and signals and must run inside WSL2.
The PowerShell wrapper (`dev-launcher.ps1`) handles this transparently.

**One-time setup:**

1. **Install WSL2** (skip if already done):

   ```powershell
   wsl --install
   # Reboot when prompted, then open a WSL2 terminal to complete Linux setup
   ```

2. **Install Docker Desktop** with the WSL2 backend enabled (Settings > Resources > WSL Integration).

3. **Install the Linux binary inside WSL2** — open your WSL2 terminal and run:

   ```sh
   curl -Lo dev-launcher https://github.com/AreDee-Bangs/dev-launcher/releases/latest/download/dev-launcher-linux-x86_64
   chmod +x dev-launcher
   sudo mv dev-launcher /usr/local/bin/
   ```

4. **Download the PowerShell wrapper** from the same release page and place it somewhere on your `PATH` (e.g. `C:\Users\<you>\bin\dev-launcher.ps1`), or run it directly from any PowerShell terminal:

   ```powershell
   # Run from the directory where you saved dev-launcher.ps1
   .\dev-launcher.ps1 --help
   ```

5. **Set your workspace root** in your PowerShell profile (`$PROFILE`):

   ```powershell
   $env:FILIGRAN_WORKSPACE_ROOT = "/home/<wsl-user>/dev/filigran"
   ```

   Use the Linux path (starting with `/home/...`), not a Windows path. The wrapper forwards `FILIGRAN_WORKSPACE_ROOT` and `FILIGRAN_LLM_KEY` into WSL2 automatically.

**Usage** is then identical to macOS/Linux — all flags, workspace management, and TUI features work the same way.

### Verify the installation

```sh
dev-launcher --version
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

`dev-launcher` needs to know where this directory lives. There are three ways to provide it, listed in priority order:

1. **`--workspace-root <path>` flag** - passed directly on each invocation
2. **`FILIGRAN_WORKSPACE_ROOT` environment variable** (recommended) - set once in your shell profile
3. **First-run wizard** - interactive prompt that saves the value to `~/.dev-launcher/config`

### Recommended: environment variable

Add the following line to your `~/.zshrc` or `~/.bashrc`:

```sh
export FILIGRAN_WORKSPACE_ROOT="$HOME/dev/filigran"
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
dev-launcher

# Launch Copilot only on a specific branch (non-interactive)
dev-launcher --copilot-branch feat/my-feature

# Launch full stack for a cross-product feature
dev-launcher --copilot-branch my-feat --opencti-branch my-feat --connector-branch my-feat

# Resume an existing workspace by its hash
dev-launcher --workspace a1b2c3d4
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
