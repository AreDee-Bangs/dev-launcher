# dev-launcher

[![CI](https://github.com/AreDee-Bangs/dev-launcher/actions/workflows/ci.yml/badge.svg)](https://github.com/AreDee-Bangs/dev-launcher/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/AreDee-Bangs/dev-launcher)](https://github.com/AreDee-Bangs/dev-launcher/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows%20WSL2-lightgrey)](#installation)

A Rust TUI launcher that spins up the full Filigran multi-product development stack (Filigran Copilot, OpenCTI, OpenAEV, ImportDoc connector) from git feature branches in a single command, replacing fragile shell scripts with process-group lifecycle management, live health monitoring, and automatic crash diagnosis.

<!-- screenshot -->

## Features

- **Multi-product stack** - starts Copilot (Python FastAPI + React), OpenCTI (Node.js GraphQL + React), OpenAEV, and the ImportDoc connector together or in any combination
- **Workspace isolation** - git worktrees keep each feature branch sandboxed; workspaces are hashed and re-used across sessions
- **Environment wizard with auto-generation** - detects missing `.env` values, prompts once, and auto-generates secrets (UUID tokens, base64 keys) where applicable
- **Port pre-flight** - checks that required ports are free before spawning any process
- **Live TUI dashboard** - redraws every 500 ms with per-service status, PID, health endpoint, and uptime
- **Crash diagnosis with fix recipes** - 25 built-in failure patterns matched instantly against logs; unknown errors escalate to LLM analysis
- **LLM-assisted analysis** - optional integration with any OpenAI-compatible API (Anthropic, OpenAI, Ollama, LiteLLM, etc.) for one-sentence diagnoses of unrecognised errors
- **Git worktree management** - automatically creates worktrees for missing branches, fetching from origin when needed

## Installation

Download the binary for your platform from [GitHub Releases](https://github.com/AreDee-Bangs/dev-launcher/releases/latest):

| Platform | File |
|----------|------|
| macOS (Apple Silicon) | `dev-launcher-macos-arm64` |
| macOS (Intel) | `dev-launcher-macos-x86_64` |
| macOS (universal) | `dev-launcher-macos` |
| Linux x86_64 | `dev-launcher-linux-x86_64` |
| Linux arm64 | `dev-launcher-linux-arm64` |
| Windows (via WSL2) | `dev-launcher.ps1` + Linux binary |

**macOS / Linux - manual install:**

```bash
chmod +x dev-launcher-macos-arm64
mv dev-launcher-macos-arm64 /usr/local/bin/dev-launcher
```

**macOS - one-liner (Apple Silicon):**

```bash
curl -fsSL https://github.com/AreDee-Bangs/dev-launcher/releases/latest/download/dev-launcher-macos-arm64 \
  -o /usr/local/bin/dev-launcher && chmod +x /usr/local/bin/dev-launcher
```

**Windows:** `dev-launcher` runs inside WSL2. Install the Linux binary inside your WSL2 distro, then use the `dev-launcher.ps1` PowerShell wrapper from Windows. See [Windows setup](docs/getting-started.md#windows-wsl2) for the full walkthrough.

The binary has no runtime dependencies - no Node.js, Python, or Rust toolchain required to run it.

## Quick Start

1. **Set your workspace root** (the directory containing `filigran-copilot/`, `opencti/`, etc.):

   ```bash
   export FILIGRAN_WORKSPACE_ROOT=~/Development/filigran
   ```

2. **Run `dev-launcher`** - on first launch the setup wizard records your workspace root and offers to clone any missing repositories:

   ```bash
   dev-launcher
   ```

3. **Pick products and branches** in the interactive selector - toggle products on/off with `Space`, set a branch with `b`, then press `Enter` to confirm.

4. **Watch the TUI dashboard** - services appear as they start, health probes run automatically, and any crash is diagnosed inline.

Press `q` or `Ctrl+C` to shut down. All process groups are terminated cleanly within 5 seconds.

## Documentation

| Document | Contents |
|----------|----------|
| [Getting Started](docs/getting-started.md) | Installation, first-run setup, quickstart |
| [CLI Reference](docs/cli-reference.md) | All flags and arguments with examples |
| [Workspace Concept](docs/workspace-concept.md) | How workspaces, worktrees, and environment isolation work |
| [Configuration](docs/configuration.md) | Config files, environment variables, repos registry |
| [TUI Guide](docs/tui-guide.md) | Dashboard modes, keybindings, health states, diagnosis |

## Requirements

| Tool | Minimum version |
|------|----------------|
| Git | 2.5 (worktree support) |
| Docker Desktop | any recent version |
| Node.js | 20 |
| Python | 3.13 |
| Yarn | 1.x or 4.x |

## License

MIT
