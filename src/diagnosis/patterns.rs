use crate::diagnosis::Finding;

/// Known failure signatures. Each entry is `(needle_lowercase, human_reason)`.
pub const DIAG_PATTERNS: &[(&str, &str)] = &[
    (
        "econnrefused",
        "Connection refused — is the required service running?",
    ),
    (
        "amqp: connection refused",
        "RabbitMQ is not reachable — check Docker container",
    ),
    (
        "connection refused to localhost:5672",
        "RabbitMQ port 5672 not reachable",
    ),
    (
        "connection refused to localhost:5432",
        "PostgreSQL not reachable — check Docker container",
    ),
    (
        "could not connect to server: connection refused",
        "PostgreSQL is not reachable",
    ),
    (
        "redis: could not connect",
        "Redis is not reachable — check Docker container",
    ),
    ("error connecting to redis", "Redis connection failed"),
    (
        "connection refused to localhost:6379",
        "Redis port 6379 not reachable",
    ),
    (
        "elasticsearch: no living connections",
        "Elasticsearch cluster is unreachable",
    ),
    (
        "connection refused to localhost:9200",
        "Elasticsearch port 9200 not reachable",
    ),
    ("minio: connection refused", "MinIO/S3 is not reachable"),
    (
        "connection refused to localhost:9000",
        "MinIO port 9000 not reachable",
    ),
    (
        "address already in use",
        "Port conflict — another process is using this port",
    ),
    (
        "eaddrinuse",
        "Port already in use — stop the conflicting process",
    ),
    (
        "no module named",
        "Python module missing — run pip install or recreate venv",
    ),
    (
        "modulenotfounderror",
        "Python module not found — check venv",
    ),
    (
        "cannot find module",
        "Node.js module missing — run yarn install",
    ),
    (
        "error: cannot find module",
        "Node.js module missing — run yarn install",
    ),
    (
        "changeme",
        "Placeholder credentials detected — edit .env.dev",
    ),
    (
        "invalid pem",
        "Invalid PEM certificate — check CONNECTOR_LICENCE_KEY_PEM",
    ),
    (
        "certificate verify failed",
        "TLS certificate verification failed",
    ),
    (
        "permission denied",
        "Permission denied — check file/directory ownership",
    ),
    ("killed", "Process killed — possibly out of memory (OOM)"),
    (
        "out of memory",
        "Out of memory — free RAM or increase system swap",
    ),
    (
        "no space left on device",
        "Disk full — free up space before restarting",
    ),
];

// ── Finding kinds ─────────────────────────────────────────────────────────────

pub const KIND_INFO: &str = "info/generic";
pub const KIND_INFO_LOG_TAIL: &str = "info/log-tail";
pub const KIND_INFO_LOG_PATTERNS: &str = "info/log-patterns";
pub const KIND_INFO_NO_ISSUES: &str = "info/no-issues";
pub const KIND_INFO_BOOTSTRAP_CHECK: &str = "info/bootstrap-check";

pub const KIND_PYTHON_VENV: &str = "python-venv-missing";
pub const KIND_NODE_MODULES: &str = "node-modules-missing";
pub const KIND_ENV_PLACEHOLDER: &str = "env-placeholder-credentials";
pub const KIND_BOOTSTRAP_RUN: &str = "bootstrap-command-needed";
pub const KIND_DEGRADED_UNKNOWN: &str = "service-degraded-unknown";
pub const KIND_CRASH: &str = "service-crashed";
pub const KIND_OPENCTI_ES_PARTIAL_INIT: &str = "opencti-es-partial-init";
pub const KIND_CONNECTOR_TYPE_MISSING: &str = "connector-type-missing";
pub const KIND_CONNECTOR_LICENCE_MISSING: &str = "connector-licence-missing";
pub const KIND_MINIO_DOWN: &str = "docker-service-down/minio";

pub const RECIPE_CATALOG: &[&str] = &[
    KIND_PYTHON_VENV,
    KIND_NODE_MODULES,
    KIND_ENV_PLACEHOLDER,
    KIND_BOOTSTRAP_RUN,
    KIND_OPENCTI_ES_PARTIAL_INIT,
    KIND_CONNECTOR_TYPE_MISSING,
    KIND_CONNECTOR_LICENCE_MISSING,
];

pub fn needs_recipe(f: &Finding) -> bool {
    if f.kind.starts_with("info/") {
        return false;
    }
    !RECIPE_CATALOG.contains(&f.kind)
}
