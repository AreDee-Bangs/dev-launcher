# Release Notes

## v0.1.3 -- 2026-04-25

### New: observability stack

Grafana and Langfuse are now selectable products in the workspace picker, right alongside Copilot, OpenCTI, and OpenAEV.

**Grafana + Loki + Promtail** (`Space` to toggle in the selector):
- Grafana 11.6.0 on port 3200 (anonymous admin by default -- no login required for dev)
- Loki 3.4.2 on port 3101 as the default log datasource, pre-provisioned automatically
- Promtail 3.4.2 tails all Docker container logs via the Docker socket and forwards them to Loki
- All ports and credentials are overridable via a `.env` file in the infra directory

**Langfuse v2** (`Space` to toggle in the selector):
- Langfuse v2 on port 3201 backed by a dedicated Postgres instance on port 5433
- Pre-seeded admin account, project, and API keys (all overridable via `.env`)
- Telemetry disabled out of the box

Both products are "infra products" -- they have no git repository, require no branch selection, and are bootstrapped from embedded templates on first launch. The infra directory is created under your workspace root and is never overwritten on subsequent launches.

### New: version + commit SHA in TUI header

The header bar now displays `dev-launcher v<version>-<sha>` (e.g. `dev-launcher v0.1.3-17987a0`) so it is always clear which binary and which commit is running.

### Fix: `q` returns to workspace selector instead of exiting

Pressing `q` in any TUI mode now stops all services, tears down Docker, and returns to the product/branch selector. Use `Ctrl+C` to exit the process entirely. This makes it faster to switch between feature stacks.

### Fix: Docker Compose teardown is now reboot-safe

`docker compose down` previously passed the `/tmp/dev-launcher-override-*.yml` file as a `-f` argument. That file is lost on reboot, causing containers to be left running after a shutdown triggered post-reboot.

The teardown now uses only `-p <project>` and relies on the `com.docker.compose.project` label that Docker sets on every container. This works regardless of whether the override file still exists.

### Fix: compose override file moved out of `/tmp`

The workspace-scoped container name override file (which appends the workspace hash to explicit `container_name:` entries to avoid conflicts between parallel workspaces) is now written next to the compose file in the workspace directory as `docker-compose.override-<hash>.yml` instead of `/tmp/dev-launcher-override-<hash>.yml`. It survives reboots and is easy to inspect or delete alongside the workspace.

---

## v0.1.2

- Workspace delete gate: refuse deletion when any worktree in the workspace is in a dirty git state.

## v0.1.1

- Pull from origin when creating a worktree on an existing local branch to ensure the worktree starts from the latest remote state.
- CI and formatting fixes.
