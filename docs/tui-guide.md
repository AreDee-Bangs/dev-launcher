# TUI Guide

After `dev-launcher` launches the stack, it enters an interactive terminal dashboard. The dashboard has four modes: Overview (default), Log View, Diagnose, and Credentials. You can switch between them with the keybindings described below.

---

## Overview mode

The default mode. Shows a table of all running and ready services. Services in `pending` state (waiting for a prerequisite) are hidden until they start.

| Column | Description |
|---|---|
| Name | Service identifier (e.g. `copilot-backend`, `opencti`) |
| Health | Current health state (see below) |
| PID | OS process ID, or blank if not yet spawned |
| Uptime | Time since the process was last started |
| URL | Primary endpoint for the service, if applicable |

### Health states

| State | Meaning |
|---|---|
| `pending` | Waiting for a prerequisite service to become healthy before starting (hidden from the table) |
| `launching` | Process spawned, no health URL configured -- treated as running |
| `health probe #N` | Health URL is being polled (N = attempt number, polls every ~2s) |
| `up` | Health URL returned 2xx -- service is fully ready |
| `running` | Process is running (no health URL to poll) |
| `degraded (reason)` | Pre-spawn check failed: port conflict (shows conflicting process name and PID), missing venv, or Docker down |
| `crashed (N)` | Process exited with exit code N |

### Keybindings

| Key | Action |
|---|---|
| `j` / `↓` | Move cursor down |
| `k` / `↑` | Move cursor up |
| `Enter` / `l` / `→` | Open Log View for the selected service |
| `d` | Open Diagnose view for the selected service |
| `e` | Open Credentials view |
| `p` / `P` | Toggle full worktree paths on/off in the service table |
| `r` | Generate a report for the selected service |
| `R` (Shift+r) | Restart the selected service (kills process, re-spawns with same args) |
| `q` / `Esc` | Return to the workspace/product selector |
| `Ctrl+C` | Graceful shutdown (kills all services, tears down Docker Compose, exits) |

---

## Log View mode

Full-screen scrollable log output for the selected service.

### Keybindings

| Key | Action |
|---|---|
| `j` / `↓` | Scroll down one line |
| `k` / `↑` | Scroll up one line |
| `Page Down` | Scroll down half a screen |
| `Page Up` | Scroll up half a screen |
| `f` | Toggle follow mode (auto-scroll to newest line) |
| `d` | Open Diagnose view for this service |
| `q` / `Esc` / `←` | Return to Overview |

---

## Diagnose mode

Shows a list of findings for a crashed or degraded service. Findings come from two sources:

1. **Known failure pattern matching**: the last 200 lines of the service log are scanned against a catalog of known error signatures. Each match produces a human-readable finding with a severity and, where available, an automated fix recipe.
2. **LLM analysis (optional)**: if `FILIGRAN_LLM_KEY` is set in the environment, the log tail is sent to the configured LLM for additional analysis. The LLM summary appears as an additional finding.

### Finding types

| Type | Description |
|---|---|
| Recipe available | The launcher knows how to fix this. Press `Enter` to apply the fix. |
| Info only | Informational finding; no action needed. |
| No recipe | The failure pattern is recognized but no automated fix exists. Press `i` to open a pre-filled GitHub issue requesting the recipe. |

### Fix recipes

Fix recipes can perform the following actions automatically:

- Restart a Docker container
- Run `docker compose up -d` for a missing service
- Patch a value in the workspace `.env` file
- Re-run the env wizard for missing credentials
- Start an interactive editor for a multi-line field (e.g. a licence PEM block)

### Keybindings

| Key | Action |
|---|---|
| `j` / `↓` | Move cursor down through findings |
| `k` / `↑` | Move cursor up |
| `Enter` | Apply the selected fix recipe |
| `i` | Open a GitHub issue for the selected finding (opens in browser) |
| `l` | Open Log View for this service |
| `q` / `Esc` | Return to Overview |

---

## Credentials view

Shows all configured credentials for the current workspace, read directly from the workspace `.env` files. Content is workspace-specific -- values reflect what is actually set for the active workspace.

Typical entries include:

- Copilot admin email and password
- Copilot base URL and frontend URL
- OpenCTI admin email, password, and API token
- Connector OpenCTI token and licence status

### Keybindings

| Key | Action |
|---|---|
| `q` / `Esc` | Return to Overview |

---

## Platform mode selector

When Copilot runs standalone (without OpenCTI), a platform mode selector appears before the dashboard starts. It lets you set the `PLATFORM_MODE` environment variable for the workspace without editing the `.env` file directly.

The selection is saved to the workspace `copilot.env` and persists across restarts.

| Mode | Label | Description |
|---|---|---|
| `xtm_one` | XTM One | Open platform -- XTM One UI, EE features via license (default) |
| `copilot` | Filigran Copilot | Enterprise -- Copilot UI, license required |
| `dev` | Dev | Copilot UI + XTM One seeding (for testing) |

### Keybindings

| Key | Action |
|---|---|
| `j` / `↓` | Move cursor down |
| `k` / `↑` | Move cursor up |
| `Enter` | Confirm selection |
| `Esc` | Cancel (keep current value) |

---

## Service startup ordering

Services that depend on others show `pending` in the health column until their prerequisites are healthy, and are not shown in the Overview table. Once the prerequisite reaches `up` or `running`, the dependent service spawns automatically without any user interaction.

---

## Shutdown

Pressing `q` in any mode stops all services and returns to the workspace/product selector so you can switch products or branches without relaunching the binary.

Pressing `Ctrl+C` performs the same graceful shutdown and then exits the process entirely.

The shutdown sequence is:

1. All spawned processes receive `SIGTERM`.
2. A grace period allows in-flight requests to complete (5 s by default; 180 s for `opencti-graphql`).
3. Any process that has not exited by the deadline receives `SIGKILL`.
4. Docker Compose projects are stopped via `docker compose -p <project> down` (project-name lookup -- no dependency on temporary override files).
5. The TUI exits or returns to the selector.

On the next launch, `dev-launcher` detects orphaned PIDs from the previous session and kills them before starting new processes.
