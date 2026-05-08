"""
AutoResearch runner service.
Manages a local autoresearch repo clone and exposes an HTTP API so the XTM One
agent can read/modify training code, trigger 5-minute GPU experiments, stream
live logs, and push knowledge-base documents for retrieval during experiments.
"""
from __future__ import annotations

import asyncio
import json
import os
import subprocess
import threading
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path
from typing import AsyncIterator

from fastapi import Depends, FastAPI, HTTPException, Request, Response
from fastapi.responses import PlainTextResponse
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from pydantic import BaseModel, Field
from sse_starlette.sse import EventSourceResponse

app = FastAPI(title="AutoResearch Runner")
_bearer = HTTPBearer()

_API_TOKEN = os.environ["AUTORESEARCH_API_KEY"]
_AR_DIR    = Path(os.environ["AUTORESEARCH_DIR"])
_REPO_DIR  = _AR_DIR / "repo"
_RUNS_DIR  = _AR_DIR / "runs"
_KB_DIR    = _AR_DIR / "knowledge-bases"
_REPO_URL  = os.environ.get(
    "AUTORESEARCH_REPO_URL",
    "https://github.com/miolini/autoresearch-macos",
)

_MAX_PAYLOAD_BYTES = 50 * 1024 * 1024  # 50 MB

_WEBHOOK_QUEUE_DIR = _AR_DIR / "webhook-queue"
_LEDGER_PATH       = _AR_DIR / "runs-ledger.jsonl"

_RUNS_DIR.mkdir(parents=True, exist_ok=True)
_KB_DIR.mkdir(parents=True, exist_ok=True)
_WEBHOOK_QUEUE_DIR.mkdir(parents=True, exist_ok=True)

_prepare: dict = {"status": "idle"}
_runs: dict[str, dict] = {}

# ingestion_id → ingestion record
_ingestions: dict[str, dict] = {}
# idempotency_key → ingestion_id  (persisted across restarts via _KB_DIR/idempotency.json)
_idempotency: dict[str, str] = {}
# "{doc_id}:{content_hash}" → True  (dedup index)
_doc_index: set[str] = set()

_idempotency_lock = threading.Lock()

_EDITABLE = {"train.py", "program.md"}

_TERMINAL_STATUSES = {"done", "crashed", "failed"}


# ── Startup: reload persisted state ──────────────────────────────────────────

def _load_persisted_state() -> None:
    # Prepare status
    prepare_path = _AR_DIR / "prepare-status.json"
    if prepare_path.exists():
        try:
            _prepare.update(json.loads(prepare_path.read_text()))
        except Exception:
            pass
    # Fallback: if cache dir exists with tokenizer, data is ready regardless of stored status
    if _prepare["status"] != "done" and Path.home().joinpath(".cache/autoresearch/tokenizer").is_dir():
        _prepare["status"] = "done"
        _persist_prepare()

    # Runs
    if _RUNS_DIR.is_dir():
        for run_dir in _RUNS_DIR.iterdir():
            meta_path = run_dir / "meta.json"
            if not meta_path.exists():
                continue
            try:
                rec = json.loads(meta_path.read_text())
                run_id = rec.get("run_id", run_dir.name)
                # Runs in-progress at restart will never complete — mark as crashed
                if rec.get("status") in ("pending", "running"):
                    rec["status"] = "crashed"
                    rec["error"] = "runner restarted while run was in-progress"
                    rec["completed_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
                    meta_path.write_text(json.dumps(rec, indent=2))
                _runs[run_id] = rec
            except Exception:
                pass

    # Knowledge-base idempotency + ingestions + doc index
    idem_path = _KB_DIR / "idempotency.json"
    if idem_path.exists():
        try:
            _idempotency.update(json.loads(idem_path.read_text()))
        except Exception:
            pass

    ingestions_dir = _KB_DIR / "ingestions"
    if ingestions_dir.is_dir():
        for p in ingestions_dir.glob("*.json"):
            try:
                rec = json.loads(p.read_text())
                _ingestions[rec["ingestion_id"]] = rec
            except Exception:
                pass

    docs_dir = _KB_DIR / "documents"
    if docs_dir.is_dir():
        for p in docs_dir.rglob("*.json"):
            try:
                doc = json.loads(p.read_text())
                _doc_index.add(f"{doc['id']}:{doc['content_hash']}")
            except Exception:
                pass


_load_persisted_state()


# ── Auth ──────────────────────────────────────────────────────────────────────

def _auth(creds: HTTPAuthorizationCredentials = Depends(_bearer)) -> None:
    if creds.credentials != _API_TOKEN:
        raise HTTPException(status_code=401, detail="Invalid token")


# ── Models: training runs ─────────────────────────────────────────────────────

class RunStatus(BaseModel):
    run_id: str
    status: str                  # pending | running | done | crashed | failed
    metrics: dict | None = None  # val_bpb, peak_vram_mb, training_seconds, …
    error: str | None = None


class WebhookRegistration(BaseModel):
    url: str
    secret: str | None = None   # sent as Authorization: Bearer {secret} on fire


class WebhookInfo(BaseModel):
    url: str
    registered_at: str


# ── Models: knowledge-base ingestion ─────────────────────────────────────────

class IngestionSource(BaseModel):
    platform: str
    tenant_id: str | None = None
    workspace_id: str | None = None
    export_mode: str = "snapshot"          # snapshot | delta
    exported_at: str


class KnowledgeBaseInfo(BaseModel):
    id: str
    name: str
    description: str | None = None
    embedding_model: str | None = None
    chunk_size: int | None = None
    chunk_overlap: int | None = None
    updated_at: str


class DocumentItem(BaseModel):
    id: str
    filename: str | None = None
    source_type: str | None = None
    content: str
    content_hash: str
    status: str | None = None
    created_at: str | None = None
    extra_metadata: dict | None = None


class Paging(BaseModel):
    batch_index: int = 0
    batch_count: int = 1
    is_last_batch: bool = True


class IngestionRequest(BaseModel):
    schema_version: str
    source: IngestionSource
    knowledge_base: KnowledgeBaseInfo
    documents: list[DocumentItem] = Field(default_factory=list)
    paging: Paging = Field(default_factory=Paging)
    idempotency_key: str


class DocumentError(BaseModel):
    id: str
    reason: str


class IngestionResponse(BaseModel):
    ingestion_id: str
    accepted_documents: int
    rejected_documents: int
    duplicate_documents: int
    status: str                            # accepted | processing | completed | failed
    errors: list[DocumentError] = Field(default_factory=list)


class IngestionStatusResponse(BaseModel):
    ingestion_id: str
    status: str                            # queued | processing | completed | failed
    accepted_documents: int
    rejected_documents: int
    duplicate_documents: int
    errors: list[DocumentError] = Field(default_factory=list)
    completed_at: str | None = None


# ── Health ────────────────────────────────────────────────────────────────────

@app.get("/health")
def health() -> dict:
    return {
        "status": "ok",
        "repo_cloned": (_REPO_DIR / "train.py").exists(),
        "data_ready": Path.home().joinpath(".cache/autoresearch").is_dir(),
        "prepare_status": _prepare["status"],
        "knowledge_bases": len({r["knowledge_base_id"] for r in _ingestions.values()}),
    }


# ── Data preparation (one-time, ~2 min) ──────────────────────────────────────

@app.post("/prepare", status_code=202)
def trigger_prepare(_: None = Depends(_auth)) -> dict:
    if _prepare["status"] == "running":
        return {"status": "running"}
    if _prepare["status"] == "done":
        return {"status": "done", "message": "already prepared"}
    if not (_REPO_DIR / "prepare.py").exists():
        raise HTTPException(503, "Repo not cloned yet")
    _prepare["status"] = "running"
    _persist_prepare()
    threading.Thread(target=_run_prepare, daemon=True).start()
    return {"status": "running"}


@app.get("/prepare/status")
def prepare_status(_: None = Depends(_auth)) -> dict:
    log_path = _AR_DIR / "prepare.log"
    tail: list[str] = []
    if log_path.exists():
        tail = log_path.read_text().splitlines()[-50:]
    return {"status": _prepare["status"], "error": _prepare.get("error"), "log_tail": tail}


def _run_prepare() -> None:
    log_path = _AR_DIR / "prepare.log"
    with log_path.open("w") as f:
        try:
            proc = subprocess.Popen(
                ["uv", "run", "prepare.py"],
                cwd=_REPO_DIR, stdout=f, stderr=subprocess.STDOUT, text=True,
            )
            proc.wait()
            _prepare["status"] = "done" if proc.returncode == 0 else "failed"
            if proc.returncode != 0:
                _prepare["error"] = f"prepare.py exited {proc.returncode}"
        except Exception as exc:
            _prepare["status"] = "failed"
            _prepare["error"] = str(exc)
    _persist_prepare()


# ── File operations ───────────────────────────────────────────────────────────

@app.get("/files/{filename}", response_class=PlainTextResponse)
def read_file(filename: str, _: None = Depends(_auth)) -> str:
    _guard_filename(filename)
    path = _REPO_DIR / filename
    if not path.exists():
        raise HTTPException(404, f"{filename} not found")
    return path.read_text()


@app.put("/files/{filename}", status_code=204)
async def write_file(filename: str, request: Request, _: None = Depends(_auth)) -> Response:
    _guard_filename(filename)
    content = await request.body()
    (_REPO_DIR / filename).write_text(content.decode())
    return Response(status_code=204)


def _guard_filename(name: str) -> None:
    if name not in _EDITABLE:
        raise HTTPException(400, f"Only {sorted(_EDITABLE)} are writable")


# ── Training runs ─────────────────────────────────────────────────────────────

@app.get("/runs", response_model=list[RunStatus])
def list_runs(_: None = Depends(_auth)) -> list[RunStatus]:
    return [
        RunStatus(
            run_id=run_id,
            status=run["status"],
            metrics=run.get("metrics"),
            error=run.get("error"),
        )
        for run_id, run in _runs.items()
    ]


@app.post("/runs", status_code=202, response_model=RunStatus)
def start_run(_: None = Depends(_auth)) -> RunStatus:
    if not (_REPO_DIR / "train.py").exists():
        raise HTTPException(503, "Repo not ready")
    run_id = str(uuid.uuid4())
    run_dir = _RUNS_DIR / run_id
    run_dir.mkdir(parents=True)
    _runs[run_id] = {"run_id": run_id, "status": "pending", "dir": str(run_dir)}
    _persist_run(run_id)
    threading.Thread(target=_run_training, args=(run_id, run_dir), daemon=True).start()
    return RunStatus(run_id=run_id, status="pending")


@app.get("/runs/{run_id}", response_model=RunStatus)
def get_run(run_id: str, _: None = Depends(_auth)) -> RunStatus:
    run = _get_run_or_404(run_id)
    return RunStatus(
        run_id=run_id,
        status=run["status"],
        metrics=run.get("metrics"),
        error=run.get("error"),
    )


@app.get("/runs/{run_id}/logs/stream")
async def stream_logs(run_id: str, _: None = Depends(_auth)) -> EventSourceResponse:
    _get_run_or_404(run_id)
    log_path = _RUNS_DIR / run_id / "train.log"

    async def _generate() -> AsyncIterator[dict]:
        sent = 0
        while True:
            if log_path.exists():
                with log_path.open() as f:
                    f.seek(sent)
                    chunk = f.read()
                if chunk:
                    sent += len(chunk)
                    for line in chunk.splitlines():
                        yield {"data": line}
            status = _runs.get(run_id, {}).get("status", "")
            if status in _TERMINAL_STATUSES:
                yield {"data": f"[runner] {run_id} — {status}"}
                return
            await asyncio.sleep(0.4)

    return EventSourceResponse(_generate())


# ── Webhook registration ──────────────────────────────────────────────────────

@app.post("/runs/{run_id}/webhook", response_model=WebhookInfo)
def register_webhook(
    run_id: str,
    body: WebhookRegistration,
    _: None = Depends(_auth),
) -> WebhookInfo:
    run = _get_run_or_404(run_id)
    run["webhook_url"] = body.url
    if body.secret is not None:
        run["webhook_secret"] = body.secret
    run["webhook_registered_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _persist_run(run_id)

    # If run already finished, fire the webhook immediately
    if run.get("status") in _TERMINAL_STATUSES:
        threading.Thread(target=_fire_webhook, args=(run_id, run), daemon=True).start()

    return WebhookInfo(url=body.url, registered_at=run["webhook_registered_at"])


@app.get("/runs/{run_id}/webhook", response_model=WebhookInfo)
def get_webhook(run_id: str, _: None = Depends(_auth)) -> WebhookInfo:
    run = _get_run_or_404(run_id)
    url = run.get("webhook_url")
    if not url:
        raise HTTPException(404, "No webhook registered for this run")
    return WebhookInfo(url=url, registered_at=run.get("webhook_registered_at", ""))


# ── Webhook delivery queue (failsafe polling) ─────────────────────────────────

@app.get("/webhook-deliveries/pending")
def list_pending_deliveries(_: None = Depends(_auth)) -> list[dict]:
    """Returns webhook payloads that could not be delivered. Poll this to catch missed pushes."""
    deliveries = []
    for p in sorted(_WEBHOOK_QUEUE_DIR.glob("*.json")):
        try:
            deliveries.append(json.loads(p.read_text()))
        except Exception:
            pass
    return deliveries


@app.post("/webhook-deliveries/{delivery_id}/ack", status_code=204)
def ack_delivery(delivery_id: str, _: None = Depends(_auth)) -> Response:
    """Acknowledge (remove) a pending delivery once it has been processed."""
    path = _WEBHOOK_QUEUE_DIR / f"{delivery_id}.json"
    if not path.exists():
        raise HTTPException(404, "Delivery not found")
    path.unlink()
    return Response(status_code=204)


# ── Runs ledger (immutable local record) ──────────────────────────────────────

@app.get("/runs-ledger", response_class=PlainTextResponse)
def get_runs_ledger(_: None = Depends(_auth)) -> str:
    """JSON Lines file with one entry per completed run. Always up-to-date regardless of webhook delivery."""
    if not _LEDGER_PATH.exists():
        return ""
    return _LEDGER_PATH.read_text()


# ── Results TSV ───────────────────────────────────────────────────────────────

@app.get("/results", response_class=PlainTextResponse)
def get_results(_: None = Depends(_auth)) -> str:
    path = _REPO_DIR / "results.tsv"
    if not path.exists():
        return "commit\tval_bpb\tmemory_gb\tstatus\tdescription\n"
    return path.read_text()


@app.put("/results", status_code=204)
async def write_results(request: Request, _: None = Depends(_auth)) -> Response:
    content = await request.body()
    (_REPO_DIR / "results.tsv").write_text(content.decode())
    return Response(status_code=204)


# ── Knowledge-base ingestion ──────────────────────────────────────────────────

@app.post("/knowledge-bases/ingestions", status_code=202, response_model=IngestionResponse)
async def ingest_knowledge_base(
    request: Request,
    _: None = Depends(_auth),
) -> IngestionResponse:
    # Payload size guard
    content_length = request.headers.get("content-length")
    if content_length and int(content_length) > _MAX_PAYLOAD_BYTES:
        raise HTTPException(413, "Payload too large (max 50 MB)")

    body = await request.body()
    if len(body) > _MAX_PAYLOAD_BYTES:
        raise HTTPException(413, "Payload too large (max 50 MB)")

    try:
        payload = IngestionRequest.model_validate_json(body)
    except Exception as exc:
        raise HTTPException(422, f"Invalid payload: {exc}") from exc

    # Schema version check
    if payload.schema_version != "1.0":
        raise HTTPException(400, f"Unsupported schema_version: {payload.schema_version!r}")

    # Idempotency check
    with _idempotency_lock:
        existing_id = _idempotency.get(payload.idempotency_key)
        if existing_id and existing_id in _ingestions:
            rec = _ingestions[existing_id]
            return IngestionResponse(
                ingestion_id=existing_id,
                accepted_documents=rec["accepted"],
                rejected_documents=rec["rejected"],
                duplicate_documents=rec["duplicates"],
                status=rec["status"],
                errors=[DocumentError(**e) for e in rec.get("errors", [])],
            )

        ingestion_id = str(uuid.uuid4())
        _idempotency[payload.idempotency_key] = ingestion_id
        _persist_idempotency()

    # Process documents synchronously (classify accept/reject/dup); store async
    accepted: list[DocumentItem] = []
    rejected: list[DocumentError] = []
    duplicates = 0

    for doc in payload.documents:
        err = _validate_document(doc)
        if err:
            rejected.append(DocumentError(id=doc.id, reason=err))
            continue
        dedup_key = f"{doc.id}:{doc.content_hash}"
        if dedup_key in _doc_index:
            duplicates += 1
            continue
        _doc_index.add(dedup_key)
        accepted.append(doc)

    rec: dict = {
        "ingestion_id": ingestion_id,
        "knowledge_base_id": payload.knowledge_base.id,
        "knowledge_base_name": payload.knowledge_base.name,
        "idempotency_key": payload.idempotency_key,
        "status": "queued",
        "accepted": len(accepted),
        "rejected": len(rejected),
        "duplicates": duplicates,
        "errors": [e.model_dump() for e in rejected],
        "created_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "completed_at": None,
    }
    _ingestions[ingestion_id] = rec
    _persist_ingestion(ingestion_id, rec)

    threading.Thread(
        target=_process_ingestion,
        args=(ingestion_id, payload, accepted),
        daemon=True,
    ).start()

    return IngestionResponse(
        ingestion_id=ingestion_id,
        accepted_documents=len(accepted),
        rejected_documents=len(rejected),
        duplicate_documents=duplicates,
        status="accepted",
        errors=rejected,
    )


@app.get("/knowledge-bases/ingestions/{ingestion_id}", response_model=IngestionStatusResponse)
def get_ingestion_status(ingestion_id: str, _: None = Depends(_auth)) -> IngestionStatusResponse:
    rec = _ingestions.get(ingestion_id)
    if rec is None:
        raise HTTPException(404, "Ingestion not found")
    return IngestionStatusResponse(
        ingestion_id=ingestion_id,
        status=rec["status"],
        accepted_documents=rec["accepted"],
        rejected_documents=rec["rejected"],
        duplicate_documents=rec["duplicates"],
        errors=[DocumentError(**e) for e in rec.get("errors", [])],
        completed_at=rec.get("completed_at"),
    )


def _validate_document(doc: DocumentItem) -> str | None:
    if not doc.id:
        return "missing id"
    if not doc.content:
        return "empty content"
    if not doc.content_hash:
        return "missing content_hash"
    return None


def _process_ingestion(ingestion_id: str, payload: IngestionRequest, docs: list[DocumentItem]) -> None:
    rec = _ingestions[ingestion_id]
    rec["status"] = "processing"
    _persist_ingestion(ingestion_id, rec)

    kb_dir = _KB_DIR / "documents" / payload.knowledge_base.id
    kb_dir.mkdir(parents=True, exist_ok=True)

    # Persist knowledge-base metadata once per KB
    meta_path = _KB_DIR / "documents" / payload.knowledge_base.id / "_kb.json"
    if not meta_path.exists():
        meta_path.write_text(payload.knowledge_base.model_dump_json(indent=2))

    try:
        for doc in docs:
            doc_data = doc.model_dump()
            doc_data["kb_id"] = payload.knowledge_base.id
            doc_data["ingestion_id"] = ingestion_id
            (kb_dir / f"{doc.id}.json").write_text(json.dumps(doc_data, indent=2))

        rec["status"] = "completed"
        rec["completed_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    except Exception as exc:
        rec["status"] = "failed"
        rec["errors"].append({"id": "_batch", "reason": str(exc)})

    _persist_ingestion(ingestion_id, rec)


# ── Knowledge-base query (simple keyword search for agent retrieval) ──────────

@app.get("/knowledge-bases/{kb_id}/documents")
def list_kb_documents(kb_id: str, _: None = Depends(_auth)) -> list[dict]:
    kb_dir = _KB_DIR / "documents" / kb_id
    if not kb_dir.is_dir():
        raise HTTPException(404, "Knowledge base not found")
    docs = []
    for p in kb_dir.glob("*.json"):
        if p.name == "_kb.json":
            continue
        try:
            docs.append(json.loads(p.read_text()))
        except Exception:
            pass
    return docs


@app.get("/knowledge-bases/{kb_id}/search")
def search_kb(kb_id: str, q: str, _: None = Depends(_auth)) -> list[dict]:
    docs = list_kb_documents(kb_id, _)
    q_lower = q.lower()
    return [d for d in docs if q_lower in d.get("content", "").lower()][:20]


# ── Persistence helpers ───────────────────────────────────────────────────────

def _append_ledger(run_id: str, run: dict) -> None:
    entry = {
        "run_id": run_id,
        "status": run["status"],
        "metrics": run.get("metrics"),
        "error": run.get("error"),
        "completed_at": run.get("completed_at"),
    }
    try:
        with _LEDGER_PATH.open("a") as f:
            f.write(json.dumps(entry) + "\n")
    except Exception:
        pass


def _persist_prepare() -> None:
    try:
        (_AR_DIR / "prepare-status.json").write_text(json.dumps(_prepare, indent=2))
    except Exception:
        pass


def _persist_run(run_id: str) -> None:
    run = _runs.get(run_id)
    if run is None:
        return
    try:
        run_dir = Path(run["dir"])
        (run_dir / "meta.json").write_text(json.dumps(run, indent=2))
    except Exception:
        pass


def _persist_idempotency() -> None:
    try:
        (_KB_DIR / "idempotency.json").write_text(json.dumps(_idempotency, indent=2))
    except Exception:
        pass


def _persist_ingestion(ingestion_id: str, rec: dict) -> None:
    try:
        ingestions_dir = _KB_DIR / "ingestions"
        ingestions_dir.mkdir(parents=True, exist_ok=True)
        (ingestions_dir / f"{ingestion_id}.json").write_text(json.dumps(rec, indent=2))
    except Exception:
        pass


# ── Webhook firing ────────────────────────────────────────────────────────────

def _fire_webhook(run_id: str, run: dict) -> None:
    url = run.get("webhook_url")
    if not url:
        return
    body: dict = {
        "event": "run.completed",
        "run_id": run_id,
        "status": run["status"],
        "metrics": run.get("metrics"),
        "error": run.get("error"),
        "completed_at": run.get("completed_at"),
    }
    payload = json.dumps(body).encode()
    headers: dict[str, str] = {"Content-Type": "application/json"}
    secret = run.get("webhook_secret")
    if secret:
        headers["Authorization"] = f"Bearer {secret}"

    for attempt in range(3):
        try:
            req = urllib.request.Request(url, data=payload, headers=headers, method="POST")
            with urllib.request.urlopen(req, timeout=10) as resp:
                if resp.status < 300:
                    return
        except Exception:
            pass
        if attempt < 2:
            time.sleep(2 ** attempt)

    # All attempts failed — enqueue for polling
    delivery_id = str(uuid.uuid4())
    record = {
        "delivery_id": delivery_id,
        "target_url": url,
        "failed_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "attempts": 3,
        **body,
    }
    try:
        (_WEBHOOK_QUEUE_DIR / f"{delivery_id}.json").write_text(json.dumps(record, indent=2))
    except Exception:
        pass


# ── Background: training run ──────────────────────────────────────────────────

def _run_training(run_id: str, run_dir: Path) -> None:
    _runs[run_id]["status"] = "running"
    _persist_run(run_id)
    log_path = run_dir / "train.log"
    with log_path.open("w") as f:
        try:
            proc = subprocess.Popen(
                ["uv", "run", "train.py"],
                cwd=_REPO_DIR, stdout=f, stderr=subprocess.STDOUT, text=True,
            )
            _runs[run_id]["pid"] = proc.pid
            proc.wait()
            log_text = log_path.read_text()
            metrics = _parse_metrics(log_text)
            if proc.returncode != 0 and metrics is None:
                _runs[run_id]["status"] = "crashed"
                _runs[run_id]["error"] = f"exit {proc.returncode}"
            else:
                _runs[run_id]["status"] = "done"
                _runs[run_id]["metrics"] = metrics
        except Exception as exc:
            _runs[run_id]["status"] = "failed"
            _runs[run_id]["error"] = str(exc)

    _runs[run_id]["completed_at"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    _persist_run(run_id)
    _append_ledger(run_id, _runs[run_id])
    _fire_webhook(run_id, _runs[run_id])


# ── Helpers ───────────────────────────────────────────────────────────────────

def _get_run_or_404(run_id: str) -> dict:
    run = _runs.get(run_id)
    if run is None:
        raise HTTPException(404, "Run not found")
    return run


def _parse_metrics(log: str) -> dict | None:
    keys = {
        "val_bpb", "sigma_validity_rate",
        "training_seconds", "total_seconds", "peak_vram_mb",
        "mfu_percent", "total_tokens_M", "num_steps", "num_params_M", "depth",
    }
    metrics: dict = {}
    for line in log.splitlines():
        if ":" in line:
            k, _, v = line.partition(":")
            if k.strip() in keys:
                try:
                    metrics[k.strip()] = float(v.strip())
                except ValueError:
                    pass
    return metrics if "val_bpb" in metrics else None


# ── Entry point ───────────────────────────────────────────────────────────────

if __name__ == "__main__":
    import uvicorn
    port = int(os.environ.get("AUTORESEARCH_PORT", "8400"))
    uvicorn.run(app, host="0.0.0.0", port=port)
