# Workspace Concept

This document explains how `dev-launcher` organizes feature branches into isolated workspaces.

## What is a workspace?

A workspace is a self-contained environment for a specific set of product/branch combinations. It groups together:

- Git worktrees for each product repository
- Isolated environment variable files for each product
- Dedicated Docker Compose project namespaces to avoid container collisions
- Offset ports so the workspace does not conflict with instances started manually

## Directory layout

All workspaces live under a single **workspace root** that you own (for example `~/dev/filigran/`). The workspace root holds the main clones of all product repositories as direct subdirectories:

```
{workspace_root}/
  filigran-copilot/          <- main clone (your default branch)
  opencti/                   <- main clone
  openaev/                   <- main clone
  connectors/                <- main clone
  .dev-workspaces/           <- workspace metadata for all workspaces
```

When you launch a feature, `dev-launcher` adds a worktree directory next to each main clone:

```
{workspace_root}/
  filigran-copilot/          <- main clone
  filigran-copilot-a1b2c3d4/ <- worktree for workspace a1b2c3d4
  opencti/
  opencti-a1b2c3d4/
  connectors/
  connectors-a1b2c3d4/
  .dev-workspaces/
    a1b2c3d4/
      workspace.conf
      copilot.env
      opencti.env
      connector.env
```

Worktrees share the same `.git` object store as the main clone, so no history is duplicated.

## Workspace identity

A workspace is identified by an 8-character FNV-1a hash computed from the sorted set of `repo=branch` pairs that make up the workspace. For example:

```
connectors=feat/my-feature
filigran-copilot=feat/my-feature
opencti=feat/my-feature
```

This means launching the same product/branch combination twice always opens the same workspace (idempotent). If you change even one branch, the hash changes and a new workspace is created.

## Workspace config storage

All workspace metadata lives under `{workspace_root}/.dev-workspaces/`, one subdirectory per workspace named by its hash:

```
.dev-workspaces/
  a1b2c3d4/
    workspace.conf    <- human-readable product/branch mapping
    copilot.env       <- Copilot environment variables for this workspace
    opencti.env       <- OpenCTI environment variables
    openaev.env       <- OpenAEV environment variables
    connector.env     <- Connector environment variables
```

### workspace.conf format

A simple INI-like file recording which branch each product is on:

```ini
[filigran-copilot]
branch=feat/my-feature

[opencti]
branch=feat/my-feature

[connectors]
branch=feat/my-feature
```

When a workspace is launched with `--copilot-commit <hash>`, the worktree is checked out in detached HEAD mode and the entry is recorded as:

```ini
branch=commit:<hash>
```

The workspace selector shows the short hash as the label.

## Environment isolation

Each workspace receives its own copy of the `.env` files for every product. These files are initialized from the `.env.sample` or `.env.example` template in the repository the first time the workspace is created. After that they are never overwritten, so you can freely edit them without affecting other workspaces or the main clone.

Practical consequence: changing `ADMIN_PASSWORD` in one workspace does not touch any other workspace.

## Port isolation

The launcher runs Copilot on offset ports to avoid colliding with a manually started `./dev.sh` instance:

| Service | Default (`./dev.sh`) | Workspace (`dev-launcher`) |
|---|---|---|
| Copilot backend | 8000 | 8100 |
| Copilot frontend | 3000 | 3100 |

Each product also gets its own Docker Compose project suffix derived from the workspace hash, preventing container name collisions when multiple workspaces are running simultaneously.

## Workspace lifecycle

**Created**: the first time a unique product/branch combination is launched. The worktrees are checked out and the env files are initialized from templates.

**Listed**: on subsequent runs without explicit flags, `dev-launcher` shows an interactive selector of existing workspaces, sorted most-recent-first. Each row shows product labels, branch names, and the last-used date.

**Resumed**: pressing Enter in the selector starts the stack for that workspace.

**New**: pressing N in the selector exits the list and prompts for branch names to create a fresh workspace.

**Deleted**: pressing D in the selector opens a deletion confirmation flow that:

1. Lists all worktrees that will be removed.
2. Checks for **Git blockers** -- uncommitted changes or branches not yet pushed to `origin`. If blockers are found, you must type `YES` twice to force removal. This is a safety gate against losing work.
3. If no blockers exist but the worktree has staged or unstaged changes, a single `YES` confirmation is required.
4. Removes the worktrees, Docker volumes, and env files for that workspace.

The `workspace.conf` entry is **tombstoned** rather than deleted: a `deleted=<date>` line is appended to the file, and the workspace is hidden from the selector. The `.dev-workspaces/<hash>/` directory itself can be removed manually afterward if desired.

## Workspace selector keys

| Key | Action |
|---|---|
| `↑` / `k` | Move cursor up |
| `↓` / `j` | Move cursor down |
| `Enter` | Resume the selected workspace (or create new if `[+]` is selected) |
| `d` / `D` | Delete the selected workspace |
| `q` / `Esc` | Quit without launching |

## Full directory layout reference

```
{workspace_root}/
  filigran-copilot/          <- main clone
  filigran-copilot-a1b2c3d4/ <- worktree (workspace a1b2c3d4)
  opencti/
  opencti-a1b2c3d4/
  connectors/
  connectors-a1b2c3d4/
  .dev-workspaces/
    a1b2c3d4/
      workspace.conf
      copilot.env
      opencti.env
      connector.env
```
