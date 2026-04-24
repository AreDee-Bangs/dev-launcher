# Configuration Reference

This document covers all configuration files and environment variables used by `dev-launcher`.

---

## Global config file: `~/.dev-launcher/config`

A plain `key=value` file that holds machine-wide defaults. It is created automatically on first run when the setup wizard prompts for the workspace root.

### Keys

| Key | Required | Description |
|---|---|---|
| `workspace_root` | Yes | Absolute path to the workspace root directory |
| `llm_api_key` | No | API key for LLM-assisted crash diagnosis |
| `llm_url` | No | Base URL of the LLM provider |
| `llm_provider` | No | `anthropic` or `openai` (auto-inferred from URL or key prefix when omitted) |
| `llm_model` | No | Model name override (defaults: `claude-haiku-4-5-20251001` for Anthropic, `gpt-4o-mini` for OpenAI-compatible) |

### Example

```
workspace_root=/home/alice/dev/filigran
llm_api_key=sk-ant-api03-...
llm_model=claude-haiku-4-5-20251001
```

---

## Environment variables

Environment variables take precedence over the values set in `~/.dev-launcher/config`.

| Variable | Description | Overrides |
|---|---|---|
| `FILIGRAN_WORKSPACE_ROOT` | Workspace root path | `~/.dev-launcher/config` `workspace_root` |
| `FILIGRAN_LLM_KEY` | LLM API key for crash diagnosis | `~/.dev-launcher/config` `llm_api_key` |
| `FILIGRAN_LLM_URL` | LLM provider base URL | `~/.dev-launcher/config` `llm_url` |
| `FILIGRAN_LLM_MODEL` | LLM model name override | `~/.dev-launcher/config` `llm_model` |

---

## Repository registry: `repos.conf`

The tool ships with a built-in `repos.conf` that lists all Filigran product repositories. You can override it by placing a custom file at `~/.dev-launcher/repos.conf`.

The user override at `~/.dev-launcher/repos.conf` **replaces** the built-in registry entirely.

### Format

```ini
# Each [section-name] is the local directory name under the workspace root.

[filigran-copilot]
label = Filigran Copilot
url   = https://github.com/FiligranHQ/filigran-copilot.git
group = Filigran

[opencti]
label = OpenCTI
url   = https://github.com/OpenCTI-Platform/opencti.git
group = OpenCTI
```

### Fields

| Field | Description |
|---|---|
| `label` | Human-readable name shown in the clone selector and TUI |
| `url` | Git clone URL (SSH or HTTPS) |
| `group` | Optional header used to group repos in the clone selector |

---

## Per-workspace environment files

Each workspace stores its environment files under `{workspace_root}/.dev-workspaces/{hash}/`. These files are initialized from `.env.sample` or `.env.example` templates found in each repository on first use. The env wizard prompts for required values (credentials, tokens) that carry placeholder values such as `ChangeMe`.

| File | Contents |
|---|---|
| `copilot.env` | Filigran Copilot settings: DB password, admin credentials, S3 config, `PLATFORM_MODE`, etc. |
| `opencti.env` | OpenCTI GraphQL settings: admin email, password, token, encryption key |
| `openaev.env` | OpenAEV settings |
| `connector.env` | ImportDoc connector settings: OpenCTI token, licence key |

### PLATFORM_MODE

The `PLATFORM_MODE` key in `copilot.env` controls which UI and feature set Copilot exposes. It can be changed at any time via the interactive selector that appears before the dashboard when Copilot runs standalone (see [TUI Guide: Platform mode selector](tui-guide.md#platform-mode-selector)).

| Value | Description |
|---|---|
| `xtm_one` | XTM One open platform (default) |
| `copilot` | Filigran Copilot enterprise UI |
| `dev` | Copilot UI with XTM One seeding, for testing |

### Port pre-flight corrections

On every launch, `dev-launcher` checks and auto-corrects port mismatches between the env file and `docker-compose.dev.yml`. Corrections are logged to the console before services start.

| Service | Container port | Compose-mapped port |
|---|---|---|
| Redis | 6379 | 6380 |
| MinIO S3 | 9000 | 9002 |
| Copilot backend | 8000 (template default) | 8100 (dev-launcher default) |
| Copilot frontend | 3000 (template default) | 3100 (dev-launcher default) |

A correction line looks like:

```
  REDIS_URL: redis://localhost:6379 -> redis://localhost:6380  (compose maps :6379 -> :6380)
  BASE_URL: http://localhost:8000 -> http://localhost:8100  (dev-launcher port)
```

Custom port values already present in the env file are never overwritten.

---

## Global preferences file: `~/.config/dev-launcher/defaults.env`

Auto-generated values (passwords, UUIDs, tokens) that the env wizard creates are persisted here so the same values are reused across all workspaces on the same machine.

This means:

- You log into Copilot with the same admin password regardless of which workspace you open.
- Auto-generated OpenCTI tokens are consistent across workspaces.

You can edit this file directly to override any auto-generated default.

---

## Per-repo launcher config: `.dev-launcher.conf`

Each product repository can contain a `.dev-launcher.conf` file at its root. This file describes how to start the repository's services. `dev-launcher` auto-generates it on first launch if the file is not present. You can commit it to the repository so that team members get a pre-configured launcher experience without the auto-generation step.

### Format

```ini
[docker]
compose_dev=docker-compose.dev.yml
project=copilot-dev

[service.backend]
args=.venv/bin/python -m uvicorn app.main:application --reload --host 0.0.0.0 --port 8100 --timeout-graceful-shutdown 3
cwd=backend
health=http://localhost:8100/api/health
timeout_secs=120
requires_docker=true

[service.worker]
args=.venv/bin/python -m saq app.worker.settings
cwd=backend
timeout_secs=10
requires_docker=true
log_name=copilot-worker.log

[service.frontend]
args=yarn dev
cwd=frontend
health=http://localhost:3100
timeout_secs=90
```

### Sections and fields

#### `[docker]`

| Field | Description |
|---|---|
| `compose_dev` | Path to the Docker Compose file, relative to the repository root |
| `project` | Docker Compose project name passed via `-p` |

#### `[service.<name>]`

One section per process that `dev-launcher` will start. The section name becomes the display label in the TUI.

| Field | Required | Description |
|---|---|---|
| `args` | Yes | Full command line to run, relative to `cwd` |
| `cwd` | No | Working directory for the process, relative to the repository root |
| `health` | No | URL polled to determine when the service is ready |
| `timeout_secs` | No | Seconds to wait for the health check before reporting a timeout |
| `requires_docker` | No | When `true`, Docker containers must be running before this service starts |
| `log_name` | No | Override for the log file name written under the workspace log directory |
