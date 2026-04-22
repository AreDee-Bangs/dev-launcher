# TUI Guide

After `dev-launcher` launches the stack, it enters an interactive terminal dashboard. The dashboard has four modes: Overview (default), Log View, Diagnose, and Credentials. You can switch between them with the keybindings described below.

---

## Overview mode

The default mode. Shows a table of all services with the following columns:

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
| `pending` | Waiting for a prerequisite service to become healthy before starting |
| `launching` | Process spawned, no health URL configured -- treated as running |
| `health probe #N` | Health URL is being polled (N = attempt number, polls every ~2s) |
| `up` | Health URL returned 2xx -- service is fully ready |
| `running` | Process is running (no health URL to poll) |
| `degraded (reason)` | Pre-spawn check failed (port conflict, missing venv, Docker down) |
| `crashed (N)` | Process exited with exit code N |

### Keybindings

| Key | Action |
|---|---|
| `j` / `Down` | Move cursor down |
| `k` / `Up` | Move cursor up |
| `Enter` / `l` | Open Log View for the selected service |
| `d` | Open Diagnose view for the selected service |
| `c` | Open Credentials view |
| `R` | Restart the selected service (kills process, re-spawns with same args) |
| `q` / `Ctrl+C` | Graceful shutdown (kills all services, tears down Docker Compose, exits) |

---

## Log View mode

Full-screen scrollable log output for the selected service.

### Keybindings

| Key | Action |
|---|---|
| `j` / `Down` | Scroll down one line |
| `k` / `Up` | Scroll up one line |
| `Page Down` | Scroll down half a screen |
| `Page Up` | Scroll up half a screen |
| `f` | Toggle follow mode (auto-scroll to newest line) |
| `d` | Open Diagnose view for this service |
| `q` / `Esc` / `Backspace` | Return to Overview |

---

## Diagnose mode

Shows a list of findings for a crashed or degraded service. Findings come from two sources:

1. **Known failure pattern matching**: the last 200 lines of the service log are scanned against a catalog of known error signatures. Each match produces a human-readable finding with a severity and, where available, an automated fix recipe.
2. **LLM analysis (optional)**: if `FILIGRAN_LLM_KEY` is set in the environment, the log tail is sent to the configured LLM for additional analysis. The LLM summary appears as an additional finding.

### Finding types

| Type | Description |
|---|---|
| Recipe available | The launcher knows how to fix this. Press `Enter` or `r` to apply the fix. |
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
| `j` / `Down` | Move cursor down through findings |
| `k` / `Up` | Move cursor up |
| `Enter` / `r` | Apply the selected fix recipe |
| `i` | Open a GitHub issue for the selected finding (opens in browser) |
| `l` | Open Log View for this service |
| `q` / `Esc` | Return to Overview |

---

## Credentials view

Shows all configured API credentials for the current workspace:

- Copilot admin email and password
- Copilot base URL and frontend URL
- OpenCTI admin email, password, and API token
- Connector OpenCTI token and licence status

### Keybindings

| Key | Action |
|---|---|
| `q` / `Esc` | Return to Overview |

---

## Service startup ordering

Services that depend on others show `pending` in the health column until their prerequisites are healthy. The dependency is also noted inline -- for example:

```
degraded (Waiting for copilot-backend...)
```

Once the prerequisite reaches `up` or `running`, the dependent service spawns automatically without any user interaction.

---

## Shutdown

Pressing `q` or `Ctrl+C` in any mode triggers a graceful shutdown sequence:

1. All spawned processes receive `SIGTERM`.
2. A 3-second grace period allows in-flight requests to complete.
3. Docker Compose projects are stopped (`docker compose down`).
4. The TUI exits cleanly.

Pressing `Ctrl+C` twice (or sending `SIGKILL`) bypasses the grace period and terminates processes immediately. On the next launch, `dev-launcher` detects orphaned PIDs from the previous session and kills them before starting new processes.
