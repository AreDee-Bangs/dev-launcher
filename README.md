# dev-launcher

[![Release](https://img.shields.io/github/v/release/AreDee-Bangs/dev-launcher)](https://github.com/AreDee-Bangs/dev-launcher/releases/latest)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Platforms](https://img.shields.io/badge/platform-macOS%20%7C%20Linux%20%7C%20Windows%20WSL2-lightgrey)](#installation)

`dev-launcher` is a single-binary TUI tool that spins up the full Filigran multi-product development stack -- Filigran Copilot, OpenCTI, OpenAEV, the ImportDoc connector, and the observability stack (Grafana + Loki + Langfuse) -- from any set of feature branches, in one command.

<!-- screenshot -->

## Features

- **One command for the whole stack** -- starts Copilot (Python FastAPI + React), OpenCTI (Node.js GraphQL + React), OpenAEV, and the ImportDoc connector together or in any combination
- **Observability stack** -- optional Grafana 11 + Loki + Promtail and Langfuse v2 selectable from the product picker; bootstrapped automatically on first launch, no manual setup required
- **Git worktree isolation** -- each feature branch runs in its own worktree; workspaces are hashed and re-used across sessions so you can switch between features without re-cloning
- **Environment wizard** -- detects missing `.env` values on first launch, prompts once, and auto-generates secrets (UUID tokens, base64 keys, random passwords) where applicable
- **Port pre-flight** -- checks that required ports are free before spawning any process, reports the conflicting process by name and PID
- **Live TUI dashboard** -- per-service status, PID, health endpoint, and uptime, redrawn every 500 ms; header shows the exact binary version and commit SHA
- **Crash diagnosis** -- 25 built-in failure patterns matched instantly against logs, with automated fix recipes; unknown errors escalate to optional LLM analysis
- **LLM-assisted analysis** -- one-sentence diagnosis of unrecognised errors via any OpenAI-compatible API (Anthropic, OpenAI, Ollama, LiteLLM, etc.)
- **Clean shutdown** -- `q` returns to the workspace selector; `Ctrl+C` exits; both tear down Docker Compose reliably via project-name lookup (no `/tmp` dependency)

## Installation

Download the binary for your platform from the [latest release](https://github.com/AreDee-Bangs/dev-launcher/releases/latest):

| Platform | File |
|---|---|
| macOS (Apple Silicon) | `dev-launcher-macos-arm64` |
| macOS (Intel) | `dev-launcher-macos-x86_64` |
| macOS (universal) | `dev-launcher-macos` |
| Linux x86\_64 | `dev-launcher-linux-x86_64` |
| Linux arm64 | `dev-launcher-linux-arm64` |
| Windows (via WSL2) | `dev-launcher.ps1` + Linux binary |

```bash
# macOS (Apple Silicon) one-liner
curl -fsSL https://github.com/AreDee-Bangs/dev-launcher/releases/latest/download/dev-launcher-macos-arm64 \
  -o /usr/local/bin/dev-launcher && chmod +x /usr/local/bin/dev-launcher
```

The binary has no runtime dependencies -- no Node.js, Python, or Rust toolchain required to run it.

For Linux, Windows WSL2, and build-from-source instructions, see [Getting Started](docs/getting-started.md).

## Quick Start

1. **Set your workspace root** -- the directory that contains (or will contain) your product repos:

   ```bash
   export FILIGRAN_WORKSPACE_ROOT=~/dev/filigran
   ```

2. **Run `dev-launcher`**:

   ```bash
   dev-launcher
   ```

   On first launch, the setup wizard records your workspace root and offers to clone any missing repositories.

3. **Pick products and branches** in the interactive selector -- `Space` to toggle products, `b` to set a branch, `Enter` to confirm.

4. **Watch the TUI dashboard** -- services appear as they start, health probes run automatically, and any crash is diagnosed inline.

Press `q` or `Ctrl+C` to shut down cleanly.

### Common invocations

```bash
# Copilot only, on a specific branch
dev-launcher --copilot-branch feat/my-feature

# Full cross-product stack
dev-launcher \
  --copilot-branch feat/my-feature \
  --opencti-branch feat/my-feature \
  --connector-branch feat/my-feature

# Resume a previous workspace by hash
dev-launcher --workspace a1b2c3d4
```

## Requirements

| Tool | Minimum version |
|---|---|
| Git | 2.5 (worktree support) |
| Docker Desktop | any recent version |
| Node.js | 20 |
| Python | 3.13 |
| Yarn | 1.x or 4.x |

## Documentation

| Document | Contents |
|---|---|
| [Getting Started](docs/getting-started.md) | Installation, first-run setup, quickstart |
| [CLI Reference](docs/cli-reference.md) | All flags and arguments with examples |
| [Workspace Concept](docs/workspace-concept.md) | How workspaces, worktrees, and environment isolation work |
| [Configuration](docs/configuration.md) | Config files, environment variables, repos registry |
| [TUI Guide](docs/tui-guide.md) | Dashboard modes, keybindings, health states, diagnosis |

## License

MIT
