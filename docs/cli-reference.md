# dev-launcher CLI Reference

`dev-launcher` is a multi-product stack launcher that creates and manages git worktrees for
cross-product feature development (Filigran Copilot, OpenCTI, OpenAEV, and the ImportDoc
connector). It can be run interactively or driven entirely by flags.

## Synopsis

```
dev-launcher [OPTIONS]
```

## Workspace Shortcuts

| Flag | Argument | Description |
|------|----------|-------------|
| `--workspace` | `<HASH>` | Open an existing workspace by its 8-character hash ID (shown in the workspace list). Skips the product/branch selector and goes directly to the environment step. |

## Branch Flags

Each flag checks out the named branch as a git worktree. If a workspace already exists whose
saved branches match all the supplied values, that workspace is resumed instead of creating a
new one.

| Flag | Argument | Product |
|------|----------|---------|
| `--copilot-branch` | `<BRANCH>` | Filigran Copilot |
| `--opencti-branch` | `<BRANCH>` | OpenCTI |
| `--openaev-branch` | `<BRANCH>` | OpenAEV |
| `--connector-branch` | `<BRANCH>` | ImportDoc connector |

## Worktree Path Overrides

These flags point a product slot at an existing local directory instead of managing a worktree.
They are runtime-only and are not saved to the workspace config.

| Flag | Argument | Product |
|------|----------|---------|
| `--copilot-worktree` | `<PATH>` | Filigran Copilot |
| `--opencti-worktree` | `<PATH>` | OpenCTI |
| `--openaev-worktree` | `<PATH>` | OpenAEV |
| `--connector-worktree` | `<PATH>` | ImportDoc connector |

## Commit Pinning

Pin a product to a specific commit. A detached worktree is created at that commit. The commit
hash is saved in the workspace config so that resuming the workspace reuses the same commit.

| Flag | Argument | Product |
|------|----------|---------|
| `--copilot-commit` | `<HASH>` | Filigran Copilot |
| `--opencti-commit` | `<HASH>` | OpenCTI |
| `--openaev-commit` | `<HASH>` | OpenAEV |
| `--connector-commit` | `<HASH>` | ImportDoc connector |

## Runtime-Only Flags

These flags affect the current run only and are not persisted to the workspace config.

| Flag | Description |
|------|-------------|
| `--no-opencti-front` | Skip the OpenCTI React frontend. The Node.js GraphQL API still starts. Useful when only backend changes are being tested. |
| `--no-openaev-front` | Skip the OpenAEV React frontend. Only has effect when OpenAEV is included in the workspace. |
| `--logs-dir <PATH>` | Override the log directory. Each service writes a `.log` file there. Default: `/tmp/dev-launcher-logs/{workspace-hash}/`. |

## Root Configuration

| Flag | Argument | Description |
|------|----------|-------------|
| `--workspace-root` | `<PATH>` | Path to the directory containing all product repositories. Overrides the `FILIGRAN_WORKSPACE_ROOT` environment variable and the config file. Equivalent to setting `FILIGRAN_WORKSPACE_ROOT`. |

## Standard Flags

| Flag | Description |
|------|-------------|
| `--version` | Print version and build timestamp, then exit. |
| `--help` | Print usage summary, then exit. |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `FILIGRAN_WORKSPACE_ROOT` | Path to the directory containing all product repositories. Overridden by `--workspace-root`. |

## Usage Patterns

```sh
# Interactive mode (shows product/branch selector)
dev-launcher

# Single product, specific branch
dev-launcher --copilot-branch fix/auth-bug

# Cross-product feature (all three repos on matching branches)
dev-launcher \
  --copilot-branch feat/hf-import \
  --opencti-branch feat/hf-import \
  --connector-branch feat/hf-import

# Resume workspace by hash (skips all selectors)
dev-launcher --workspace 4d448a3f

# Use a local directory you already have checked out
dev-launcher --copilot-worktree ~/dev/filigran/filigran-copilot

# Pin Copilot to a specific commit for regression testing
dev-launcher --copilot-commit abc1234def5678

# Full stack minus both frontends (faster boot, API-only testing)
dev-launcher --no-opencti-front --no-openaev-front

# Custom log directory
dev-launcher --logs-dir /var/log/dev-launcher
```

## Exit Codes

| Code | Meaning |
|------|---------|
| `0` | User quit cleanly (`q` key). |
| `1` | Fatal error (workspace root not found, etc.). |
