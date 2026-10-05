"""temba-mcp — agrégateur MCP : un seul point d'entrée MCP (tool_search + call_tool)
qui agrège des MCP distants (streamable HTTP) enregistrés dans un registry YAML.

Recherche de tools : sémantique (embeddings via endpoint OpenAI-compatible si
EMBEDDINGS_BASE_URL est défini) sinon fallback local BM25.

SDK mcp >= 2.x (MCPServer, snake_case, streamable_http_client).
"""
from __future__ import annotations

import asyncio
import ipaddress
import hashlib
import html
import json
import logging
import math
import os
import re
import secrets
import socket
import time
from contextlib import asynccontextmanager
from copy import deepcopy
from typing import Any
from urllib.parse import urlparse, parse_qs

import httpx2
import yaml
from mcp import ClientSession
from mcp.client.auth import OAuthClientProvider
from mcp.server.auth.settings import AuthSettings
from mcp.server.auth.middleware.auth_context import get_access_token
from security import BrowserLogin, KeycloakTokenVerifier, identity_key, require_https
from mcp.client.streamable_http import streamable_http_client
from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.context import Context
from mcp.shared.auth import (
    AuthorizationCodeResult,
    OAuthClientInformationFull,
    OAuthClientMetadata,
    OAuthToken,
)
from mcp.types import ToolListChangedNotification
from rank_bm25 import BM25Plus
import snowballstemmer
from starlette.requests import Request
from starlette.responses import HTMLResponse, JSONResponse, RedirectResponse

log = logging.getLogger("temba-mcp")


class StatelessSessionLogFilter(logging.Filter):
    def filter(self, record: logging.LogRecord) -> bool:
        # The SDK logs this for every stateless request; there is no session to identify.
        return record.getMessage() != "Terminating session: None"


logging.getLogger("mcp.server.streamable_http").addFilter(StatelessSessionLogFilter())

TOOL_SEARCH_LIMIT = int(os.getenv("TOOL_SEARCH_LIMIT", "5"))
CALL_TIMEOUT = float(os.getenv("CALL_TIMEOUT", "60"))
LIST_TIMEOUT = float(os.getenv("LIST_TIMEOUT", "30"))
HEALTH_INTERVAL = float(os.getenv("HEALTH_INTERVAL", "60"))
RECONNECT_DELAY = float(os.getenv("RECONNECT_DELAY", "5"))
CONNECTION_PROGRESS_INTERVAL = float(os.getenv("CONNECTION_PROGRESS_INTERVAL", "10"))
if CONNECTION_PROGRESS_INTERVAL <= 0:
    raise ValueError("CONNECTION_PROGRESS_INTERVAL must be positive")
PORT = int(os.getenv("PORT", "8080"))

# OAuth dynamique (MCP auth spec) : DCR + PKCE, login humain via /auth
PUBLIC_BASE_URL = os.getenv("PUBLIC_BASE_URL", f"http://localhost:{PORT}").rstrip("/")
AUTH_LOGIN_TIMEOUT = float(os.getenv("AUTH_LOGIN_TIMEOUT", "600"))

# Tickets de login à usage limité : l'URL renvoyée au LLM contient un jeton
# volant qui ne permet d'authentifier QUE le serveur concerné. Pas de page
# d'admin partagée : chaque demande produit son propre lien.
LOGIN_TICKET_TTL = float(os.getenv("LOGIN_TICKET_TTL", "900"))

# Rétention des tokens OAuth en mémoire, renouvelée à chaque écriture/refresh.
TOKEN_RETENTION = float(os.getenv("TOKEN_RETENTION", "2592000"))

_VAR_RE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")


def _env_bool(name: str, default: bool = False) -> bool:
    """Env booléen tolérant : 1/true/yes/on (casse indifférente)."""
    raw = os.getenv(name)
    if raw is None or raw.strip() == "":
        return default
    return raw.strip().lower() in ("1", "true", "yes", "on")


# API admin (/servers) : activation/désactivation de MCP à chaud, partagés à tous.
# Vide = API désactivée. Token en bearer, distribué hors LLM.
ADMIN_API_TOKEN = os.getenv("ADMIN_API_TOKEN", "")
# MCP ajoutés par un LLM (add_mcp) : privés (id secret) + durée de vie limitée.
PRIVATE_SERVER_TTL = float(os.getenv("PRIVATE_SERVER_TTL", "86400"))
# Tool add_mcp : désactivé par défaut (sécurité) ; ADD_MCP_ENABLED=true pour l'activer.
ADD_MCP_ENABLED = _env_bool("ADD_MCP_ENABLED")
LABELS_HEADER = "x-temba-labels"

AUTH_ENABLED = _env_bool("AUTH_ENABLED", False)
ALLOW_INSECURE_HTTP = _env_bool("ALLOW_INSECURE_HTTP", False)
AUTH_COOKIE_SECURE = urlparse(PUBLIC_BASE_URL).scheme == "https"
AUTH_ISSUER = os.getenv("AUTH_ISSUER", "")
AUTH_AUDIENCE = os.getenv("AUTH_AUDIENCE", "")
AUTH_BROWSER_CLIENT_ID = os.getenv("AUTH_BROWSER_CLIENT_ID", "temba-login")
AUTH_SCOPES = os.getenv("AUTH_SCOPES", "temba:use").split()
if ADD_MCP_ENABLED and not AUTH_ENABLED:
    raise ValueError("add_mcp requires Temba authentication")
USER_IDLE_TTL = float(os.getenv("USER_IDLE_TTL", "3600"))
MAX_USER_GATEWAYS = int(os.getenv("MAX_USER_GATEWAYS", "128"))
_TOKEN_MEMORY: dict[tuple[str, str, str], dict] = {}
_TOKEN_MEMORY_EXPIRY: dict[tuple[str, str, str], float] = {}
_DYN_MEMORY: dict[str, dict] = {}
# Index sémantique optionnel (format OpenAI /v1/embeddings)
EMBEDDINGS_BASE_URL = os.getenv("EMBEDDINGS_BASE_URL", "").rstrip("/")
EMBEDDINGS_API_KEY = os.getenv("EMBEDDINGS_API_KEY", "")
EMBEDDINGS_MODEL = os.getenv("EMBEDDINGS_MODEL", "text-embedding-3-small")
EMBED_BATCH = int(os.getenv("EMBED_BATCH", "64"))
EMBED_MIN_SCORE = float(os.getenv("EMBED_MIN_SCORE", "0.25"))


def _expand(value: Any) -> Any:
    if isinstance(value, str):
        def sub(match: re.Match) -> str:
            var = match.group(1)
            if var not in os.environ:
                raise ValueError(f"variable d'environnement manquante: {var}")
            return os.environ[var]
        return _VAR_RE.sub(sub, value)
    if isinstance(value, dict):
        return {k: _expand(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_expand(v) for v in value]
    return value


_STOPWORDS = frozenset("""
au aux ce dans de des du elle en et il je la le les ma mon ne nos notre nous
on ou par pas pour qu que qui sa se ses son sur ta te tes ton tu un une vos
votre vous the and for with from this that are was will can what how where
""".split())


_STEMMERS = [snowballstemmer.stemmer("french"), snowballstemmer.stemmer("english")]


def _tokenize(text: str) -> list[str]:
    stems: set[str] = set()
    for raw in re.findall(r"[a-z0-9]+", text.lower()):
        if len(raw) < 3 or raw in _STOPWORDS:
            continue
        stems.update(s.stemWord(raw) for s in _STEMMERS)
    return sorted(stems)


def _tool_text(entry: dict) -> str:
    """Représentation textuelle d'un tool, à indexer."""
    return (
        f"{entry['name']} {entry['name']}\n"
        f"{entry['description']}\n"
        f"{json.dumps(entry['input_schema'], default=str)}"
    )


class BM25Index:
    """Index lexical pondéré (BM25Plus, IDF toujours positif), 100% local."""

    name = "bm25"

    def __init__(self, entries: list[dict]):
        corpus = [self._doc(e) for e in entries]
        self.entries = entries
        self.bm25 = BM25Plus(corpus) if corpus else None

    @staticmethod
    def _doc(entry: dict) -> list[str]:
        toks = list(_tokenize(entry["name"])) * 3          # poids du nom
        toks += list(_tokenize(entry["description"])) * 2  # poids de la description
        toks += list(_tokenize(json.dumps(entry["input_schema"], default=str)))
        return toks

    async def search(self, query: str, limit: int) -> list[tuple[float, dict]]:
        if self.bm25 is None:
            return []
        scores = self.bm25.get_scores(_tokenize(query))
        ranked = sorted(zip(scores, self.entries), key=lambda x: x[0], reverse=True)
        return [(round(float(s), 2), e) for s, e in ranked[:limit] if s > 0]


class EmbeddingIndex:
    """Index vectoriel via un endpoint OpenAI-compatible /v1/embeddings."""

    def __init__(self, entries: list[dict], client: httpx2.AsyncClient):
        self.name = f"embeddings:{EMBEDDINGS_MODEL}"
        self.entries = entries
        self.client = client
        self.vectors: list[list[float]] = []

    async def build(self) -> None:
        texts = [_tool_text(e) for e in self.entries]
        vectors = await self._embed(texts)
        self.vectors = [self._normalize(v) for v in vectors]

    async def _embed(self, texts: list[str]) -> list[list[float]]:
        out: list[list[float]] = []
        for i in range(0, len(texts), EMBED_BATCH):
            body: dict[str, Any] = {"model": EMBEDDINGS_MODEL, "input": texts[i : i + EMBED_BATCH]}
            headers = {"Authorization": f"Bearer {EMBEDDINGS_API_KEY}"} if EMBEDDINGS_API_KEY else None
            r = await self.client.post(f"{EMBEDDINGS_BASE_URL}/embeddings", json=body, headers=headers)
            r.raise_for_status()
            data = sorted(r.json()["data"], key=lambda d: d["index"])
            out.extend(d["embedding"] for d in data)
        return out

    @staticmethod
    def _normalize(v: list[float]) -> list[float]:
        norm = math.sqrt(sum(x * x for x in v)) or 1.0
        return [x / norm for x in v]

    async def search(self, query: str, limit: int) -> list[tuple[float, dict]]:
        if not self.vectors:
            return []
        qvec = self._normalize((await self._embed([query]))[0])
        scored = [
            (sum(a * b for a, b in zip(qvec, vec)), e)
            for vec, e in zip(self.vectors, self.entries)
        ]
        scored.sort(key=lambda x: x[0], reverse=True)
        return [(round(s, 3), e) for s, e in scored[:limit] if s >= EMBED_MIN_SCORE]


def _token_key(server_id: str, owner: str, url: str) -> tuple[str, str, str]:
    return server_id, owner, url


def purge_tokens(*, server_id: str | None = None, owner: str | None = None,
                 expired_only: bool = False) -> None:
    now = time.time()
    for key in list(_TOKEN_MEMORY):
        if (server_id is not None and key[0] != server_id) or (owner is not None and key[1] != owner):
            continue
        if expired_only and _TOKEN_MEMORY_EXPIRY.get(key, 0) > now:
            continue
        _TOKEN_MEMORY.pop(key, None)
        _TOKEN_MEMORY_EXPIRY.pop(key, None)


class TokenStore:
    """Process-local OAuth state, isolated by validated user and MCP endpoint."""

    def __init__(self, server_id: str, owner: str, url: str):
        if not owner:
            raise ValueError("OAuth token storage requires an authenticated owner")
        self.key = _token_key(server_id, owner, url)
        self.lock = asyncio.Lock()

    async def _get(self) -> dict:
        if _TOKEN_MEMORY_EXPIRY.get(self.key, 0) <= time.time():
            _TOKEN_MEMORY.pop(self.key, None)
            _TOKEN_MEMORY_EXPIRY.pop(self.key, None)
        return deepcopy(_TOKEN_MEMORY.get(self.key, {}))

    async def _set(self, data: dict) -> None:
        _TOKEN_MEMORY[self.key] = deepcopy(data)
        _TOKEN_MEMORY_EXPIRY[self.key] = time.time() + TOKEN_RETENTION

    async def get_tokens(self) -> OAuthToken | None:
        data = await self._get()
        return OAuthToken.model_validate(data["tokens"]) if data.get("tokens") else None

    async def set_tokens(self, tokens: OAuthToken) -> None:
        async with self.lock:
            data = await self._get()
            data["tokens"] = tokens.model_dump(mode="json", exclude_none=True)
            await self._set(data)

    async def get_client_info(self) -> OAuthClientInformationFull | None:
        data = await self._get()
        return OAuthClientInformationFull.model_validate(data["client_info"]) if data.get("client_info") else None

    async def set_client_info(self, info: OAuthClientInformationFull) -> None:
        async with self.lock:
            data = await self._get()
            data["client_info"] = info.model_dump(mode="json", exclude_none=True)
            await self._set(data)


class AuthCoordinator:
    """Suit les flows OAuth en attente d'un login humain.

    Chaque flow en attente émet un ticket volant (URL /auth?t=...) transmis via
    le hint des tools : il ne permet d'authentifier que le serveur concerné,
    expire, et meurt dès que le login aboutit ou que le flow est relancé.
    """

    def __init__(self, gateway):
        self.gateway = gateway
        self.by_state: dict[str, ServerState] = {}
        self.futures: dict[str, asyncio.Future] = {}
        self.tickets: dict[str, dict] = {}
        self.browser_flows: dict[str, dict] = {}

    def _prune(self) -> None:
        now = time.time()
        for t in [k for k, v in self.tickets.items() if v["exp"] < now]:
            del self.tickets[t]
        for state in [k for k, v in self.browser_flows.items() if v["exp"] < now]:
            entry = self.browser_flows.pop(state)
            self.abort(entry["server_id"])

    def issue_ticket(self, server_id: str) -> str:
        self._prune()
        code = secrets.token_urlsafe(24)
        self.tickets[code] = {"server_id": server_id, "exp": time.time() + LOGIN_TICKET_TTL}
        return code

    def ticket_ok(self, ticket: str, server_id: str | None = None) -> bool:
        self._prune()
        entry = self.tickets.get(ticket)
        return entry is not None and (server_id is None or entry["server_id"] == server_id)

    def consume(self, server_id: str) -> None:
        for t in [k for k, v in self.tickets.items() if v["server_id"] == server_id]:
            del self.tickets[t]

    def server_for_ticket(self, ticket: str) -> ServerState | None:
        if not self.ticket_ok(ticket):
            return None
        return next((s for s in self.gateway.states if s.id == self.tickets[ticket]["server_id"]), None)

    def start(self, st: ServerState, authorize_url: str) -> asyncio.Future:
        old = self.futures.get(st.id)
        if old is not None and not old.done():
            if st.auth_url == authorize_url:
                return old
        self.abort(st.id)
        fut = asyncio.get_running_loop().create_future()
        self.futures[st.id] = fut
        st.auth_url = authorize_url
        st.status = "auth_required"
        st.error = None
        self.consume(st.id)
        st.login_ticket = self.issue_ticket(st.id)
        require_https(authorize_url.split("?", 1)[0])
        query = parse_qs(urlparse(authorize_url).query)
        state = query.get("state", [""])[0]
        if not state or state in self.by_state or query.get("code_challenge_method") != ["S256"]:
            self.abort(st.id)
            raise ValueError("OAuth requires a unique state and S256 PKCE")
        self.by_state[state] = st
        self.browser_flows[state] = {"server_id": st.id, "exp": time.time() + min(LOGIN_TICKET_TTL, AUTH_LOGIN_TIMEOUT),
                                     "browser": None}
        log.warning("AUTH REQUIS pour '%s' — ticket de login émis (%s/auth?t=...)", st.id, PUBLIC_BASE_URL)
        return fut

    def begin_login(self, ticket: str) -> tuple[ServerState, str, str] | None:
        st = self.server_for_ticket(ticket)
        if st is None:
            return None
        state = next((k for k, v in self.by_state.items() if v is st), None)
        if state is None or state not in self.browser_flows:
            return None
        browser = secrets.token_urlsafe(32)
        self.browser_flows[state]["browser"] = hashlib.sha256(browser.encode()).hexdigest()
        self.consume(st.id)
        st.login_ticket = None
        return st, state, browser

    def complete(self, code: str, state: str, browser: str, issuer: str | None = None) -> bool:
        self._prune()
        entry = self.browser_flows.get(state)
        st = self.by_state.get(state)
        if not entry or not st or not entry["browser"] or not browser:
            return False
        if not secrets.compare_digest(entry["browser"], hashlib.sha256(browser.encode()).hexdigest()):
            return False
        fut = self.futures.get(st.id)
        if fut is None or fut.done():
            return False
        self.by_state.pop(state)
        self.browser_flows.pop(state)
        fut.set_result(AuthorizationCodeResult(code=code, state=state, iss=issuer))
        self.consume(st.id)
        return True

    def abort(self, server_id: str) -> None:
        fut = self.futures.pop(server_id, None)
        if fut is not None and not fut.done():
            fut.cancel()
        self.consume(server_id)
        for state in [k for k, st in self.by_state.items() if st.id == server_id]:
            self.by_state.pop(state)
            self.browser_flows.pop(state, None)



def public_server(cfg: dict) -> bool:
    """Anonymous access never lends configured credentials or private resources."""
    url = urlparse(cfg.get("url", ""))
    return (cfg.get("oauth") is None and not cfg.get("headers") and not cfg.get("private")
            and not url.username and not url.password and cfg.get("access") != "authenticated")


class ServerState:
    def __init__(self, cfg: dict):
        self.cfg = cfg
        self.id = cfg["id"]
        self.timeout: float | None = cfg.get("timeout")
        self.labels: frozenset[str] = frozenset(str(l).lower() for l in (cfg.get("labels") or []))
        self.private: bool = bool(cfg.get("private"))
        self.expires_at: float | None = cfg.get("expires_at")
        self.status = "starting"
        self.error: str | None = None
        self.tools: dict[str, dict] = {}
        self.session: ClientSession | None = None
        self.wake = asyncio.Event()
        self.call_lock = asyncio.Lock()
        self.auth_url: str | None = None
        self.login_ticket: str | None = None


class Gateway:
    def __init__(self, registry_path: str, owner: str | None = None, configs: list[dict] | None = None):
        self.owner = owner
        self.log_catalog = "public" if owner is None else "user-" + secrets.token_hex(4)
        self.last_used = time.monotonic()
        self.active_calls = 0
        self.ready = asyncio.Event()
        self.states: list[ServerState] = [ServerState(c) for c in (self._load_registry(registry_path) if configs is None else configs)]
        self._stop = asyncio.Event()
        self._tasks: list[asyncio.Task] = []
        self._index: BM25Index | EmbeddingIndex | None = None
        self._index_fp: str | None = None
        self._index_lock = asyncio.Lock()
        self._mutation_lock = asyncio.Lock()
        self._embed_client = httpx2.AsyncClient(timeout=30)
        self.auth = AuthCoordinator(self)

    def _connection_states(self) -> list[ServerState]:
        return [st for st in self.states if self.owner is not None or public_server(st.cfg)]

    def _log_progress(self, event: str, *, level: int = logging.INFO) -> None:
        states = self._connection_states()
        ready = sum(st.status == "ready" for st in states)
        pending = ", ".join(st.id for st in states if st.status == "starting") or "aucun"
        errors = ", ".join(st.id for st in states if st.status == "error") or "aucun"
        login = ", ".join(st.id for st in states if st.status == "auth_required") or "aucun"
        log.log(level, "%s — catalogue %s : %d/%d MCP prêts ; en attente: %s ; en erreur: %s ; login requis: %s",
                event, self.log_catalog, ready, len(states), pending, errors, login)

    async def _connection_progress_loop(self) -> None:
        while not self._stop.is_set():
            try:
                await asyncio.wait_for(self._stop.wait(), CONNECTION_PROGRESS_INTERVAL)
            except TimeoutError:
                if any(st.status != "ready" for st in self._connection_states()):
                    self._log_progress("Connexions MCP incomplètes")

    async def add_server(self, cfg: dict) -> ServerState:
        async with self._mutation_lock:
            return await self._add_server(cfg)

    async def _add_server(self, cfg: dict) -> ServerState:
        st = ServerState(cfg)
        if any(s.id == st.id for s in self.states):
            raise ValueError(f"id '{st.id}' déjà utilisé")
        if st.private or st.cfg.get("dynamic"):
            await self._dyn_put(st.cfg)
        self.states.append(st)
        self._tasks.append(asyncio.create_task(self._serve(st), name=f"mcp:{st.id}"))
        return st

    async def _detach(self, server_id: str) -> None:
        self.auth.abort(server_id)
        task = next((t for t in self._tasks if t.get_name() == f"mcp:{server_id}"), None)
        if task is not None:
            task.cancel()
            self._tasks.remove(task)
            await asyncio.gather(task, return_exceptions=True)
        self.states = [s for s in self.states if s.id != server_id]

    async def remove_server(self, server_id: str) -> bool:
        if not any(s.id == server_id for s in self.states) and not any(
                cfg["id"] == server_id for cfg in await self._dyn_all()):
            return False
        await self._dyn_del(server_id)
        await self._detach(server_id)
        for view in list(_user_gateways.values()):
            await view._detach(server_id)
            await view._rebuild_index()
        purge_tokens(server_id=server_id)
        await self._rebuild_index()
        return True

    async def _expire_loop(self) -> None:
        """Nettoie les serveurs privés (add_mcp) arrivés à expiration."""
        while not self._stop.is_set():
            try:
                await asyncio.sleep(60)
                now = time.time()
                purge_tokens(expired_only=True)
                await self._dyn_reconcile()
                for view in list(_user_gateways.values()):
                    await view._dyn_reconcile()
                expired = [cfg["id"] for cfg in await self._dyn_all()
                           if cfg.get("expires_at") and cfg["expires_at"] <= now]
                for sid in expired:
                    log.info("serveur privé '%s' expiré — suppression", sid)
                    await self.remove_server(sid)
                async with _user_gateway_lock:
                    for owner, view in list(_user_gateways.items()):
                        if view.active_calls == 0 and time.monotonic() - view.last_used > USER_IDLE_TTL:
                            _user_gateways.pop(owner, None)
                            await view.stop()
                            view.runner.cancel()
                            await asyncio.gather(view.runner, return_exceptions=True)
            except asyncio.CancelledError:
                raise
            except Exception:
                log.warning("expiry cleanup failed")

    def _visible(self, st: ServerState, client_labels: frozenset[str]) -> bool:
        if st.private or (self.owner is None and not public_server(st.cfg)):
            return False
        return not st.labels or bool(st.labels & client_labels)

    async def _entries(self) -> list[dict]:
        entries = []
        for st in self.states:
            if st.private or (self.owner is None and not public_server(st.cfg)):
                continue
            for tool in st.tools.values():
                entries.append({
                    "tool_id": f"{st.id}.{tool['name']}",
                    "server": st.id,
                    "name": tool["name"],
                    "description": tool["description"],
                    "input_schema": tool["input_schema"],
                    "labels": sorted(st.labels),
                })
        return entries

    async def _rebuild_index(self) -> None:
        entries = await self._entries()
        fp = json.dumps(entries, sort_keys=True, default=str)
        up_to_date = (
            self._index is not None
            and fp == self._index_fp
            and (not EMBEDDINGS_BASE_URL or isinstance(self._index, EmbeddingIndex))
        )
        if up_to_date:
            return
        async with self._index_lock:
            entries = await self._entries()
            fp = json.dumps(entries, sort_keys=True, default=str)
            up_to_date = (
                self._index is not None
                and fp == self._index_fp
                and (not EMBEDDINGS_BASE_URL or isinstance(self._index, EmbeddingIndex))
            )
            if up_to_date:
                return
            index: BM25Index | EmbeddingIndex | None = None
            if EMBEDDINGS_BASE_URL:
                try:
                    index = EmbeddingIndex(entries, self._embed_client)
                    await index.build()
                except Exception as exc:
                    log.warning("embeddings indisponible (%s: %s) — fallback BM25", type(exc).__name__, exc)
                    index = None
            if index is None:
                index = BM25Index(entries)
            self._index = index
            self._index_fp = fp
            log.info("index reconstruit: mode=%s tools=%d", index.name, len(entries))

    @staticmethod
    def _load_registry(path: str) -> list[dict]:
        with open(path) as f:
            data = _expand(yaml.safe_load(f) or {})
        servers = data.get("servers") or []
        ids = [s.get("id") for s in servers]
        if len(ids) != len(set(ids)):
            raise ValueError("registry: ids de serveurs en doublon")
        for s in servers:
            if not isinstance(s.get("id"), str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]{1,30}", s["id"]):
                raise ValueError("registry: id must contain 2-31 lowercase alphanumeric or hyphen characters")
            if s.get("access", "public") not in ("public", "authenticated"):
                raise ValueError("registry: access must be public or authenticated")
            if s.get("transport", "http") != "http":
                raise ValueError(f"registry[{s.get('id')}]: transport invalide (http uniquement)")
            if not s.get("url"):
                raise ValueError(f"registry[{s['id']}]: 'url' requise")
            if s.get("timeout") is not None and (not isinstance(s["timeout"], (int, float)) or s["timeout"] <= 0):
                raise ValueError(f"registry[{s['id']}]: 'timeout' doit être un nombre de secondes > 0")
            oauth = s.get("oauth")
            if oauth is not None and not isinstance(oauth, dict):
                raise ValueError(f"registry[{s['id']}]: 'oauth' doit être un objet (vide pour DCR auto)")
            labels = s.get("labels")
            if labels is not None and (not isinstance(labels, list) or not all(isinstance(l, str) for l in labels)):
                raise ValueError(f"registry[{s['id']}]: 'labels' doit être une liste de chaînes")
        return servers

    async def _dyn_all(self) -> list[dict]:
        """Dynamic catalog shared by user views within this process only."""
        return deepcopy(list(_DYN_MEMORY.values()))

    async def _dyn_put(self, cfg: dict) -> None:
        _DYN_MEMORY[cfg["id"]] = deepcopy(cfg)

    async def _dyn_del(self, server_id: str) -> None:
        _DYN_MEMORY.pop(server_id, None)

    async def _dyn_reconcile(self) -> None:
        async with self._mutation_lock:
            await self._reconcile_locked()

    async def _reconcile_locked(self) -> None:
        """Aligne les connexions locales sur le catalogue dynamique en mémoire."""
        desired = {c["id"]: c for c in await self._dyn_all()
                   if not (c.get("expires_at") and c["expires_at"] <= time.time())
                   and (not c.get("private") or self.owner is None or c.get("owner") == self.owner)}
        current = {st.id: st for st in self.states if st.private or st.cfg.get("dynamic")}
        for sid in list(current):
            if sid not in desired:
                log.info("réplication registry: retrait de '%s'", sid)
                await self._detach(sid)
                await self._rebuild_index()
        for sid, cfg in desired.items():
            if not any(s.id == sid for s in self.states):
                log.info("réplication registry: ajout de '%s'", sid)
                self.states.append(ServerState(cfg))
                self._tasks.append(asyncio.create_task(self._serve(self.states[-1]), name=f"mcp:{sid}"))
                await self._rebuild_index()

    async def run(self) -> None:
        try:
            await self._dyn_reconcile()
            existing = {task.get_name() for task in self._tasks}
            self._tasks.extend(asyncio.create_task(self._serve(st), name=f"mcp:{st.id}")
                               for st in self.states if f"mcp:{st.id}" not in existing)
        finally:
            self.ready.set()
        if self.owner is None:
            self._tasks.append(asyncio.create_task(self._expire_loop(), name="private-expiry"))
        self._log_progress("Démarrage des connexions MCP")
        self._tasks.append(asyncio.create_task(self._connection_progress_loop(), name="connection-progress"))
        await self._stop.wait()

    async def stop(self) -> None:
        self._stop.set()
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        await self._embed_client.aclose()
        for st in self.states:
            self.auth.abort(st.id)
        if self.owner is not None:
            purge_tokens(owner=self.owner)

    def _auth_provider(self, st: ServerState) -> OAuthClientProvider:
        cfg = st.cfg.get("oauth") or {}
        redirect_uri = f"{PUBLIC_BASE_URL}/oauth/callback"
        metadata = OAuthClientMetadata(
            client_name="temba-mcp",
            redirect_uris=[redirect_uri],
            grant_types=["authorization_code", "refresh_token"],
            response_types=["code"],
            token_endpoint_auth_method="none",
            scope=cfg.get("scope"),
        )
        store = TokenStore(st.id, self.owner, st.cfg["url"])
        return OAuthClientProvider(
            st.cfg["url"],
            client_metadata=metadata,
            storage=store,
            redirect_handler=self._make_redirect_handler(st),
            callback_handler=self._make_callback_handler(st),
        )

    def _make_redirect_handler(self, st: ServerState):
        async def redirect_handler(url: str) -> None:
            self.auth.start(st, url)
        return redirect_handler

    def _make_callback_handler(self, st: ServerState):
        async def callback_handler() -> AuthorizationCodeResult:
            fut = self.auth.futures.get(st.id)
            if fut is None:
                raise RuntimeError("aucun flow OAuth en attente")
            try:
                return await asyncio.shield(fut)
            except asyncio.CancelledError:
                if fut.cancelled() and not asyncio.current_task().cancelling():
                    raise RuntimeError("OAuth flow expired or cancelled") from None
                raise
        return callback_handler

    @asynccontextmanager
    async def _open_transport(self, st: ServerState):
        cfg = st.cfg
        client_kwargs: dict[str, Any] = {"headers": cfg.get("headers") or None}
        if cfg.get("oauth") is not None:
            client_kwargs["auth"] = self._auth_provider(st)
        if cfg.get("oauth") is not None:
            require_https(cfg["url"], allow_query=True)
            allowed_origins = (cfg.get("oauth") or {}).get("allowedOrigins", [])
            if not isinstance(allowed_origins, list):
                raise ValueError("oauth.allowedOrigins must be a list of trusted HTTPS origins")
            trusted = [] if st.private else [cfg["url"]]
            for origin in allowed_origins:
                require_https(origin)
                if urlparse(origin).path not in ("", "/"):
                    raise ValueError("oauth.allowedOrigins entries must not contain a path")
                if not st.private:
                    trusted.append(origin)

            def origin_tuple(url):
                parsed = urlparse(str(url))
                return parsed.scheme, parsed.hostname, parsed.port or 443

            trusted_origins = {origin_tuple(url) for url in trusted}
            async def check_request(request):
                if request.url.scheme != "https":
                    raise ValueError("OAuth endpoints require HTTPS")
                if origin_tuple(request.url) not in trusted_origins:
                    await _assert_public_url(str(request.url))
            client_kwargs["event_hooks"] = {"request": [check_request]}
        client = httpx2.AsyncClient(trust_env=False, **client_kwargs)
        try:
            async with streamable_http_client(cfg["url"], http_client=client) as (read, write):
                yield read, write
        finally:
            await client.aclose()

    async def _serve(self, st: ServerState) -> None:
        # The admin catalog never authenticates a user's OAuth server.
        if self.owner is None and not public_server(st.cfg):
            st.status = "auth_required"
            return
        if not AUTH_ENABLED and st.cfg.get("oauth") is not None:
            st.status, st.error = "error", "OAuth requires Temba authentication"
            return
        last_failure = None
        while not self._stop.is_set():
            try:
                st.status = "starting"
                async with self._open_transport(st) as (read, write):
                    def on_message(message: Any, _st: ServerState = st) -> None:
                        if isinstance(message, ToolListChangedNotification):
                            _st.wake.set()

                    session = ClientSession(read, write, message_handler=on_message)
                    async with session:
                        init_timeout = st.timeout or (AUTH_LOGIN_TIMEOUT if st.cfg.get("oauth") is not None else LIST_TIMEOUT)
                        await asyncio.wait_for(session.initialize(), init_timeout)
                        st.session = session
                        await self._sync_tools(st)
                        last_failure = None
                        while not self._stop.is_set():
                            try:
                                await asyncio.wait_for(st.wake.wait(), timeout=HEALTH_INTERVAL)
                            except TimeoutError:
                                pass
                            st.wake.clear()
                            await self._sync_tools(st)
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                st.status = "error"
                st.error = f"{type(exc).__name__}: upstream connection failed"
                st.tools = {}
                self.auth.abort(st.id)
                # Never log exception messages, URLs, headers or token response bodies.
                def error_types(error):
                    return [name for child in getattr(error, "exceptions", []) for name in error_types(child)] or [type(error).__name__]
                failure = ", ".join(sorted(set(error_types(exc))))
                if failure != last_failure:
                    self._log_progress(f"MCP '{st.id}' indisponible ({failure})", level=logging.WARNING)
                    last_failure = failure
                if not self._stop.is_set():
                    await asyncio.sleep(RECONNECT_DELAY)
            finally:
                st.session = None

    async def _sync_tools(self, st: ServerState) -> None:
        was_ready = st.status == "ready"
        async with asyncio.timeout(st.timeout or LIST_TIMEOUT):
            async with st.call_lock:
                res = await st.session.list_tools()
        st.tools = {
            t.name: {"name": t.name, "description": t.description or "", "input_schema": t.input_schema}
            for t in res.tools
        }
        st.status = "ready"
        st.error = None
        await self._rebuild_index()
        if not was_ready:
            self._log_progress(f"MCP '{st.id}' prêt ({len(st.tools)} outils)")

    def catalog(self, client_labels: frozenset[str] = frozenset()) -> dict[str, dict]:
        """Catalogue lisible par le LLM : statut du serveur inclus, filtré par labels.
        Les serveurs privés (add_mcp) n'y apparaissent jamais."""
        out: dict[str, dict] = {}
        for st in self.states:
            if not self._visible(st, client_labels):
                continue
            if st.status == "auth_required":
                out[st.id] = {
                    "status": "auth_required",
                    "tools": [],
                    "hint": f"Authentification requise: demande à l'utilisateur d'ouvrir "
                            + (f"{PUBLIC_BASE_URL}/auth?t={st.login_ticket} (lien à usage unique)"
                               if st.login_ticket else "Connexion en cours dans le navigateur ; termine-la puis réessaie."),
                }
            elif st.status != "ready":
                out[st.id] = {"status": st.status, "tools": sorted(st.tools), "error": st.error}
            else:
                out[st.id] = {"status": "ready", "tools": sorted(st.tools)}
        return out

    async def search(self, query: str, client_labels: frozenset[str] = frozenset()) -> dict:
        if self._index is None:
            await self._rebuild_index()
        ranked = await self._index.search(query or "", TOOL_SEARCH_LIMIT * 4)
        visible = [
            (score, entry)
            for score, entry in ranked
            if not entry["labels"] or (set(entry["labels"]) & client_labels)
        ][:TOOL_SEARCH_LIMIT]
        results = [self._format(entry, score) for score, entry in visible]
        payload: dict[str, Any] = {
            "query": query,
            "index": self._index.name,
            "count": len(results),
            "results": results,
        }
        if not results:
            payload["hint"] = "Aucun tool ne correspond. Utilise le champ 'catalog' pour voir les serveurs et leurs tools, puis reformule la recherche."
            payload["catalog"] = self.catalog(client_labels)
        return payload

    @staticmethod
    def _example_args(schema: dict) -> dict:
        example = {}
        for prop, spec in (schema or {}).get("properties", {}).items():
            ptype = spec.get("type", "value") if isinstance(spec, dict) else "value"
            example[prop] = f"<{ptype}>"
        return example

    def _format(self, entry: dict, score: float) -> dict:
        tool_id = entry["tool_id"]
        return {
            "tool_id": tool_id,
            "server": entry["server"],
            "name": entry["name"],
            "description": entry["description"],
            "input_schema": entry["input_schema"],
            "score": score,
            "next_step": f"call_tool(tool_id=\"{tool_id}\", arguments={self._example_args(entry['input_schema'])})",
        }

    async def call(self, tool_id: str, arguments: dict | None, client_labels: frozenset[str] = frozenset()) -> dict:
        if "." not in tool_id:
            return {"tool_id": tool_id, "ok": False, "error": "format tool_id attendu: <server_id>.<tool_name>"}
        server_id, tool_name = tool_id.split(".", 1)
        st = next((s for s in self.states if s.id == server_id), None)
        if st is None:
            return {"tool_id": tool_id, "ok": False, "error": f"serveur inconnu '{server_id}'", "catalog": self.catalog(client_labels)}
        if st.expires_at and st.expires_at <= time.time():
            return {"ok": False, "error": "server expired"}
        if st.private and st.cfg.get("owner") != self.owner:
            return {"ok": False, "error": "server unavailable"}
        if self.owner is None and not public_server(st.cfg):
            return {"tool_id": tool_id, "ok": False, "auth_required": True,
                    "hint": "Ce serveur exige l'accès authentifié à Temba (/mcp)."}
        # Private entries belong to the authenticated principal, regardless of labels.
        if not st.private and not self._visible(st, client_labels):
            return {
                "tool_id": tool_id,
                "ok": False,
                "error": f"tool non visible avec tes labels (serveur '{server_id}' réservé aux labels {sorted(st.labels)})",
            }
        if st.status == "auth_required":
            # Course normale : le callback OAuth vient de résoudre, l'échange
            # code->token est en cours. On laisse le handshake finir au lieu de
            # renvoyer auth_required à un LLM qui vient de faire authentifier.
            fut = self.auth.futures.get(st.id)
            if fut is not None and fut.done():
                deadline = time.time() + 12
                while time.time() < deadline and st.status == "auth_required":
                    await asyncio.sleep(0.4)
            if st.status == "auth_required":
                if not st.login_ticket:
                    return {"tool_id": tool_id, "ok": False, "auth_required": True,
                            "hint": "Connexion en cours dans le navigateur ; termine-la puis réessaie."}
                auth_url = f"{PUBLIC_BASE_URL}/auth?t={st.login_ticket}"
                return {
                    "tool_id": tool_id,
                    "ok": False,
                    "auth_required": True,
                    "error": f"authentification requise pour le serveur '{server_id}'",
                    "hint": f"demande à l'utilisateur d'ouvrir {auth_url} — lien personnel valable "
                            f"{int(LOGIN_TICKET_TTL / 60)} min, ne concerne que ce serveur ; "
                            f"une fois le login fait, réessaie call_tool à l'identique",
                }
        if tool_name == "__temba_connect__":
            return {"tool_id": tool_id, "ok": st.status == "ready", "status": st.status,
                    "tools": [{"tool_id": f"{st.id}.{tool['name']}",
                               "description": tool["description"], "input_schema": tool["input_schema"]}
                              for tool in st.tools.values()]}
        if tool_name not in st.tools:
            return {"tool_id": tool_id, "ok": False, "error": f"tool inconnu sur '{server_id}'", "catalog": self.catalog(client_labels)}
        if st.session is None:
            return {
                "tool_id": tool_id,
                "ok": False,
                "error": f"serveur '{server_id}' indisponible ({st.error or st.status})",
                "hint": "le serveur se reconnecte automatiquement, réessaie plus tard",
            }
        try:
            # Serializing calls also prevents concurrent refresh-token rotation.
            async with st.call_lock, asyncio.timeout(st.timeout or CALL_TIMEOUT):
                res = await st.session.call_tool(tool_name, arguments or {})
            payload: dict[str, Any] = {
                "tool_id": tool_id,
                "ok": not res.is_error,
                "content": [b.model_dump(mode="json", by_alias=True, exclude_none=True) for b in res.content],
            }
            if getattr(res, "structured_content", None) is not None:
                payload["structured_content"] = res.structured_content
            return payload
        except Exception as exc:
            return {
                "tool_id": tool_id,
                "ok": False,
                "error": f"{type(exc).__name__}: upstream call failed",
                "hint": "le serveur a peut-être été redémarré, relance tool_search puis réessaie",
            }


_gateway: Gateway | None = None
_user_gateways: dict[str, Gateway] = {}
_user_gateway_lock = asyncio.Lock()
_verifier = KeycloakTokenVerifier(AUTH_ISSUER, AUTH_AUDIENCE, PUBLIC_BASE_URL + "/mcp",
                                allow_insecure_http=ALLOW_INSECURE_HTTP) if AUTH_ENABLED else None
if AUTH_ENABLED and not AUTH_COOKIE_SECURE:
    log.warning("Local HTTP development mode enabled: authentication traffic and cookies are not protected by TLS")
_browser_login = BrowserLogin(_verifier, AUTH_BROWSER_CLIENT_ID, PUBLIC_BASE_URL + "/oidc/callback",
                              os.getenv("AUTH_BROWSER_CLIENT_SECRET", "")) if AUTH_ENABLED else None
_auth_settings = AuthSettings(issuer_url=AUTH_ISSUER, resource_server_url=PUBLIC_BASE_URL + "/mcp",
                              required_scopes=AUTH_SCOPES, validate_token_resource=True) if AUTH_ENABLED else None


@asynccontextmanager
async def user_gateway():
    if not AUTH_ENABLED:
        yield _gateway
        return
    token = get_access_token()
    if token is None or not token.subject or not token.claims or token.claims.get("iss") != AUTH_ISSUER:
        raise PermissionError("An authenticated user is required")
    owner = identity_key(AUTH_ISSUER, token.subject)
    async with _user_gateway_lock:
        view = _user_gateways.get(owner)
        if view is None:
            if len(_user_gateways) >= MAX_USER_GATEWAYS:
                raise RuntimeError("Too many active users; retry later")
            configs = [dict(st.cfg) for st in _gateway.states
                       if not st.private or st.cfg.get("owner") == owner]
            view = Gateway(os.getenv("REGISTRY_PATH", "registry.yaml"), owner, configs=configs)
            _user_gateways[owner] = view
            view.runner = asyncio.create_task(view.run(), name="user-gateway")
        view.last_used = time.monotonic()
        view.active_calls += 1
    try:
        await asyncio.wait_for(view.ready.wait(), 15)
        if view.runner.done():
            await view.runner
        yield view
    finally:
        view.active_calls -= 1
        view.last_used = time.monotonic()


def auth_for_ticket(ticket: str):
    for view in _user_gateways.values():
        if view.auth.ticket_ok(ticket):
            return view.auth
    return None


def auth_for_state(state: str):
    for view in _user_gateways.values():
        if state in view.auth.by_state:
            return view.auth
    return None


def _client_labels(ctx) -> frozenset[str]:
    """Labels du client, depuis le header X-Temba-Labels (conf opencode)."""
    raw = (ctx.headers or {}).get(LABELS_HEADER, "") if ctx else ""
    return frozenset(part.strip().lower() for part in raw.split(",") if part.strip())


@asynccontextmanager
async def lifespan(_server):
    global _gateway
    _gateway = Gateway(os.getenv("REGISTRY_PATH", "registry.yaml"))
    task = asyncio.create_task(_gateway.run())
    try:
        await _gateway.ready.wait()
        if task.done():
            await task
        yield {"gateway": _gateway}
    finally:
        for view in list(_user_gateways.values()):
            await view.stop()
            view.runner.cancel()
            await asyncio.gather(view.runner, return_exceptions=True)
        _user_gateways.clear()
        if _browser_login:
            _browser_login.pending.clear()
        await _gateway.stop()
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        if _verifier:
            await _verifier.client.aclose()
        _TOKEN_MEMORY.clear()
        _TOKEN_MEMORY_EXPIRY.clear()
        _DYN_MEMORY.clear()


mcp = MCPServer(
    "temba-mcp",
    instructions=(
        "Ce serveur est un portail vers un catalogue de tools découverts dynamiquement ; "
        "ces tools ne sont PAS chargés dans ta liste de tools. "
        "Dès qu'un besoin ne relève pas explicitement de tes autres tools, procède en 2 étapes : "
        "1) tool_search(query) avec des mots-clés décrivant le besoin — les exemples ci-dessous "
        "sont fictifs, sert-toi du champ 'catalog' de la réponse pour te faire une idée réelle du "
        "catalogue (exemples fictifs : 'météo à Paris' pour découvrir un get_weather, "
        "'envoyer une facture' pour un send_invoice) ; la réponse renvoie les tools pertinents "
        "avec leur tool_id, leur description et leur input_schema ; "
        "2) call_tool(tool_id, arguments) en respectant l'input_schema reçue. "
        "Si tool_search renvoie 0 résultat, reformule avec des mots-clés plus génériques. "
        + ("add_mcp(url) permet d'étendre tes capacités avec un MCP distant : il est privé "
           "(server_key secret retourné) et expire automatiquement. " if ADD_MCP_ENABLED else "")
        + "Les erreurs 429/rate-limit sont transitoires : réessaie ou passe au tool alternatif proposé."
    ),
    lifespan=lifespan,
    auth=_auth_settings,
    token_verifier=_verifier,
)


@mcp.tool()
async def tool_search(query: str, ctx: Context) -> str:
    """Cherche dans le catalogue de tools disponibles tous les tools pertinents pour un besoin.

    Args:
        query: description en langage naturel de ce que tu veux faire
            (ex: "météo à Paris", "envoyer une facture au client").

    Returns:
        JSON: liste de tools correspondants avec tool_id, description et
        input_schema (JSON Schema des arguments). Pour exécuter un tool trouvé,
        appelle ensuite call_tool avec son tool_id et ses arguments. Si aucun
        résultat, la réponse contient 'catalog' (serveurs + leurs tools) pour
        reformuler la recherche.
    """
    async with user_gateway() as view:
        await _wait_catalog(view)
        return json.dumps(await view.search(query, _client_labels(ctx)), ensure_ascii=False, indent=2)


@mcp.tool()
async def call_tool(tool_id: str, arguments: dict = {}, ctx: Context = None) -> str:
    """Exécute un tool trouvé via tool_search.

    Args:
        tool_id: identifiant reçu de tool_search, format "<server_id>.<tool_name>".
        arguments: arguments du tool, conformes à son input_schema.

    Returns:
        JSON: {ok, content, structured_content?} — résultat de l'exécution.
    """
    async with user_gateway() as view:
        if isinstance(tool_id, str) and tool_id.endswith(".__temba_connect__"):
            st = next((st for st in view.states if st.id == tool_id.split(".", 1)[0]), None)
            if st:
                await _wait_ready(st, timeout=10)
        return json.dumps(await view.call(tool_id, arguments, _client_labels(ctx)), ensure_ascii=False, indent=2)


async def _assert_public_url(url: str) -> None:
    """Garde anti-SSRF pour add_mcp : refuse IP privées/loopback/link-local.
    Strict, sans exception : add_mcp = internet public, rien d'autre."""
    parsed = urlparse(url)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password or parsed.fragment:
        raise ValueError("url invalide (HTTPS sans credentials ni fragment attendu)")
    try:
        addresses = [ipaddress.ip_address(parsed.hostname)]
    except ValueError:
        try:
            infos = await asyncio.to_thread(socket.getaddrinfo, parsed.hostname, None)
        except socket.gaierror as exc:
            raise ValueError(f"hôte introuvable: {parsed.hostname}") from exc
        addresses = [ipaddress.ip_address(info[4][0]) for info in infos]
    if not addresses:
        raise ValueError("hôte sans adresse")
    for ip in addresses:
        if (
            ip.is_private or ip.is_loopback or ip.is_link_local
            or ip.is_reserved or ip.is_multicast or ip.is_unspecified
        ):
            raise ValueError(
                f"add_mcp refuse l'hôte {parsed.hostname} ({ip}) : adresse privée. "
                "Pour un MCP interne, demande à un admin de l'ajouter via l'API."
            )


async def _wait_ready(st: ServerState, timeout: float = 30) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline and st.status in ("starting",):
        await asyncio.sleep(0.3)


async def _wait_catalog(view: Gateway) -> None:
    deadline = time.monotonic() + 10
    while any(st.status == "starting" for st in view.states) and time.monotonic() < deadline:
        await asyncio.sleep(0.1)


if ADD_MCP_ENABLED:

    @mcp.tool()
    async def add_mcp(url: str, name: str = "", headers: dict = {}, ctx: Context = None) -> str:
        """Ajoute un MCP distant (streamable HTTP) pour étendre tes propres capacités.

        Le serveur est PRIVÉ : il n'apparaît pas dans le catalogue partagé, ses tools
        ne sont utilisables que par ton identité authentifiée, via le server_key (préfixe des
        tool_id). Il expire automatiquement (24h) : si ta session dure plus longtemps,
        ré-ajoute-le. Interdit aux adresses privées/internes. Si le MCP distant exige
        une authentification OAuth, le lien de login est fourni dans la réponse.

        Args:
            url: URL du MCP distant (streamable HTTP), ex "https://mon-mcp.exemple.com/mcp".
            name: optionnel, court préfixe lisible inclus dans le server_key.
            headers: optionnel, headers HTTP à envoyer au MCP (ex: {"Authorization": "Bearer ..."}).

        Returns:
            JSON: {server_key, tools: [{tool_id, description, input_schema}], expires_at}
            — appelle ces tools via call_tool avec leur tool_id.
        """
        async with user_gateway() as view:
            url = url.strip()
            try:
                await _assert_public_url(url)
            except ValueError as exc:
                return json.dumps({"ok": False, "error": str(exc)}, ensure_ascii=False)
            # Dé-dup : un même URL ne s'enregistre qu'une fois — si le MCP exige une
            # authentification, on ne recrée PAS un flow, on réutilise le serveur
            # existant et son ticket de login en attente.
            st = next(
                (s for s in view.states
                 if s.private and s.cfg.get("owner") == view.owner and s.cfg.get("url") == url and not (s.expires_at and s.expires_at <= time.time())),
                None,
            )
            already = st is not None
            if st is None:
                key = f"p-{secrets.token_urlsafe(16)}"
                if name:
                    safe = re.sub(r"[^a-z0-9-]", "-", name.lower())[:20].strip("-")
                    key = f"p-{safe}-{secrets.token_urlsafe(12)}" if safe else key
                cfg = {
                    "id": key,
                    "url": url,
                    "transport": "http",
                    "private": True,
                    "owner": view.owner,
                    # OAuth dynamique activé par défaut : no-op si le MCP n'en a pas besoin,
                    # déclenche le flow (DCR + ticket de login) s'il répond 401.
                    "oauth": {},
                    "expires_at": time.time() + PRIVATE_SERVER_TTL,
                }
                if headers:
                    cfg["headers"] = headers
                st = await view.add_server(cfg)
            await _wait_ready(st)
            out = {
                "ok": st.status in ("ready", "auth_required"),
                "server_key": st.id,
                "status": st.status,
                "already_registered": already,
                "expires_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(st.expires_at)),
                "note": (
                    f"SERVEUR PRIVÉ ET ÉPHÉMÈRE (suppression auto dans {int(PRIVATE_SERVER_TTL / 3600)}h). "
                    "Il n'apparaît JAMAIS dans tool_search ni dans le catalog. "
                    "NE relance PAS add_mcp pour ce serveur : garde ce server_key et appelle ses tools "
                    "directement via call_tool avec les tool_id ci-dessous."
                ),
            }
            if st.status == "ready":
                out["tools"] = [
                    {"tool_id": f"{st.id}.{t['name']}", "description": t["description"], "input_schema": t["input_schema"]}
                    for t in st.tools.values()
                ]
                out["next_step"] = "call_tool(tool_id='<server_key>.<tool_name>', arguments=...)"
            elif st.status == "auth_required":
                out["auth_required"] = True
                out["hint"] = (
                    f"le MCP ajouté exige une authentification: demande à l'utilisateur d'ouvrir "
                    f"{_auth_page_url(st)}. Une fois le login effectué, réessaie call_tool avec les "
                    f"tool_id <server_key>.<tool> — n'appelle PAS add_mcp à nouveau."
                )
            else:
                out["error"] = f"connexion impossible: {st.error or st.status}"
            return json.dumps(out, ensure_ascii=False, indent=2)


def _auth_page_url(st: ServerState) -> str:
    """URL personnelle de login pour ce flow (ticket volant, serveur unique)."""
    if st.login_ticket:
        return f"{PUBLIC_BASE_URL}/auth?t={st.login_ticket}"
    return f"{PUBLIC_BASE_URL}/auth"


@mcp.custom_route("/servers", methods=["GET", "POST"])
async def admin_servers(request: Request) -> JSONResponse:
    """API admin : lister / ajouter des MCP au catalogue partagé (bearer token)."""
    if not ADMIN_API_TOKEN or not secrets.compare_digest(
        request.headers.get("authorization", ""), f"Bearer {ADMIN_API_TOKEN}"
    ):
        return JSONResponse({"error": "API admin désactivée ou token invalide"}, status_code=403)
    if request.method == "GET":
        return JSONResponse({"servers": [
            {
                "id": st.id,
                "status": st.status,
                "labels": sorted(st.labels),
                "private": st.private,
                "tools": len(st.tools),
                "expires_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(st.expires_at)) if st.expires_at else None,
                "error": st.error,
            }
            for st in _gateway.states
        ]})
    body = await request.json()
    cfg = {
        "id": str(body.get("id", "")).strip(),
        "url": str(body.get("url", "")).strip(),
        "transport": "http",
        "dynamic": True,
    }
    if not cfg["id"] or not re.fullmatch(r"[a-z0-9][a-z0-9-]{1,30}", cfg["id"]):
        return JSONResponse({"error": "'id' requis (a-z0-9-, 2-31 chars)"}, status_code=400)
    if not cfg["url"].startswith(("http://", "https://")):
        return JSONResponse({"error": "'url' requise (http/https)"}, status_code=400)
    if body.get("access") is not None:
        if body["access"] not in ("public", "authenticated"):
            return JSONResponse({"error": "access must be public or authenticated"}, status_code=400)
        cfg["access"] = body["access"]
    if body.get("labels"):
        if not isinstance(body["labels"], list) or not all(isinstance(l, str) for l in body["labels"]):
            return JSONResponse({"error": "'labels' doit être une liste de chaînes"}, status_code=400)
        cfg["labels"] = body["labels"]
    if body.get("headers"):
        if not isinstance(body["headers"], dict):
            return JSONResponse({"error": "'headers' doit être un objet"}, status_code=400)
        cfg["headers"] = body["headers"]
    if body.get("timeout") is not None:
        cfg["timeout"] = body["timeout"]
    if body.get("oauth") is not None:
        cfg["oauth"] = body["oauth"]
    try:
        st = await _gateway.add_server(cfg)
    except ValueError as exc:
        return JSONResponse({"error": str(exc)}, status_code=409)
    await _wait_ready(st)
    return JSONResponse({
        "id": st.id,
        "status": st.status,
        "labels": sorted(st.labels),
        "tools": sorted(st.tools),
        "error": st.error,
    }, status_code=201)


@mcp.custom_route("/servers/{server_id}", methods=["DELETE"])
async def admin_delete_server(request: Request) -> JSONResponse:
    if not ADMIN_API_TOKEN or not secrets.compare_digest(
        request.headers.get("authorization", ""), f"Bearer {ADMIN_API_TOKEN}"
    ):
        return JSONResponse({"error": "API admin désactivée ou token invalide"}, status_code=403)
    deleted = await _gateway.remove_server(request.path_params["server_id"])
    if not deleted:
        return JSONResponse({"error": "serveur inconnu"}, status_code=404)
    return JSONResponse({"deleted": request.path_params["server_id"]})


AUTH_HEADERS = {"Cache-Control": "no-store", "Referrer-Policy": "no-referrer",
                "Content-Security-Policy": "default-src 'none'; base-uri 'none'; frame-ancestors 'none'",
                "X-Content-Type-Options": "nosniff"}


def auth_html(body: str, status_code: int = 200) -> HTMLResponse:
    return HTMLResponse(body, status_code=status_code, headers=AUTH_HEADERS)


def flow_cookie(state: str) -> str:
    return "temba_oauth_" + hashlib.sha256(state.encode()).hexdigest()[:16]


@mcp.custom_route("/auth", methods=["GET"])
async def auth_page(request: Request) -> HTMLResponse:
    ticket = request.query_params.get("t", "")
    auth = auth_for_ticket(ticket)
    st = auth.server_for_ticket(ticket) if auth else None
    if st is None:
        return auth_html("<p>Lien invalide ou expiré. Relance ton action dans l'agent.</p>", 401)
    return auth_html(
        f"<p>Connecter ton compte au serveur <b>{html.escape(st.id)}</b> "
        f"({html.escape(urlparse(st.cfg['url']).netloc)}).</p>"
        f"<p><a href='/auth/{html.escape(st.id, quote=True)}/login?t={html.escape(ticket, quote=True)}'>"
        "S'authentifier maintenant</a></p>"
        "<p>Ce compte sera utilisé pour tes appels uniquement. Ne partage pas ce lien.</p>")


@mcp.custom_route("/auth/{server_id}/login", methods=["GET"])
async def auth_login(request: Request) -> HTMLResponse | RedirectResponse:
    ticket = request.query_params.get("t", "")
    auth = auth_for_ticket(ticket)
    if not auth or not auth.ticket_ok(ticket, request.path_params["server_id"]):
        return auth_html("<p>Lien invalide ou expiré.</p>", 403)
    result = auth.begin_login(ticket)
    if not result:
        return auth_html("<p>Lien invalide ou expiré.</p>", 403)
    st, downstream_state, downstream_browser = result
    try:
        state, browser, url = await _browser_login.begin(auth.gateway.owner, auth, downstream_state, downstream_browser)
    except Exception:
        auth.abort(st.id)
        return auth_html("<p>Connexion SSO indisponible. Relance ton action.</p>", 503)
    response = RedirectResponse(url, headers=AUTH_HEADERS)
    response.set_cookie(flow_cookie(state), browser, max_age=600,
                        httponly=True, secure=AUTH_COOKIE_SECURE, samesite="lax", path="/oidc/callback")
    return response


@mcp.custom_route("/oidc/callback", methods=["GET"])
async def oidc_callback(request: Request) -> HTMLResponse | RedirectResponse:
    state = request.query_params.get("state", "")
    code = request.query_params.get("code", "")
    result = await _browser_login.complete(state, request.cookies.get(flow_cookie(state), ""), code,
                                            request.query_params.get("iss")) if _browser_login and code else None
    if not result:
        return auth_html("<p>Connexion SSO invalide ou identité différente de celle de l'agent. Relance ton action.</p>", 403)
    st, entry = result
    response = RedirectResponse(st.auth_url, headers=AUTH_HEADERS)
    response.set_cookie(flow_cookie(entry["downstream_state"]), entry["downstream_browser"],
                        max_age=max(1, int(entry["exp"] - time.time())), httponly=True,
                        secure=AUTH_COOKIE_SECURE, samesite="lax", path="/oauth/callback")
    response.delete_cookie(flow_cookie(state), path="/oidc/callback", secure=AUTH_COOKIE_SECURE, httponly=True, samesite="lax")
    return response


@mcp.custom_route("/oauth/callback", methods=["GET"])
async def oauth_callback(request: Request) -> HTMLResponse:
    state = request.query_params.get("state", "")
    code = request.query_params.get("code", "")
    auth = auth_for_state(state)
    browser = request.cookies.get(flow_cookie(state), "")
    if not code or not auth or not auth.complete(code, state, browser, request.query_params.get("iss")):
        return auth_html("<p>Callback OAuth invalide ou expiré.</p>", 400)
    response = auth_html("<p>Le gateway termine la connexion. Retourne dans ton agent et relance ton action.</p>")
    response.delete_cookie(flow_cookie(state), path="/oauth/callback", secure=AUTH_COOKIE_SECURE, httponly=True, samesite="lax")
    return response


public_mcp = MCPServer("temba-mcp-public", instructions=(
    "Les outils publics sont accessibles immédiatement. tool_search peut aussi renvoyer protected_servers : "
    "pour utiliser un de ces serveurs, suis son hint avec call_tool ; le client gère la connexion SSO. "
    "Une fois connecté, recherche les outils du serveur et suis ses éventuelles instructions OAuth."))
if ADD_MCP_ENABLED:
    public_mcp.tool(name="add_mcp")(add_mcp)


@public_mcp.tool(name="tool_search")
async def public_tool_search(query: str, ctx: Context) -> str:
    """Cherche les outils des MCP sans authentification."""
    await _wait_catalog(_gateway)
    payload = await _gateway.search(query, _client_labels(ctx))
    protected = {st.id: {
        "status": "temba_auth_required" if AUTH_ENABLED else "disabled",
        "hint": (f"Pour utiliser ce serveur, appelle call_tool(tool_id='{st.id}.__temba_connect__'). "
                 "Le client déclenchera la connexion SSO, puis tu pourras rechercher ses outils."
                 if AUTH_ENABLED else "L'administrateur doit activer l'accès authentifié à Temba.")}
        for st in _gateway.states if not st.private and not public_server(st.cfg)
        and (not st.labels or bool(st.labels & _client_labels(ctx)))}
    if protected:
        payload["protected_servers"] = protected
    return json.dumps(payload, ensure_ascii=False, indent=2)


@public_mcp.tool(name="call_tool")
async def public_call_tool(tool_id: str, arguments: dict = {}, ctx: Context = None) -> str:
    """Exécute uniquement un outil d'un MCP sans credentials."""
    return json.dumps(await _gateway.call(tool_id, arguments, _client_labels(ctx)), ensure_ascii=False, indent=2)


class CatalogRouter:
    """Authenticate only requests that need a protected catalog entry.

    Inspection never executes tools. Both paths retain the SDK's JSON-RPC validation;
    protected requests retain its bearer/scope/resource validation and 401 challenge.
    """
    def __init__(self, public, protected, max_body_size: int):
        self.public, self.protected, self.max_body_size = public, protected, max_body_size

    async def __call__(self, scope, receive, send):
        if scope["type"] != "http" or scope.get("path") != "/mcp":
            return await self.protected(scope, receive, send)
        if any(key.lower() == b"authorization" for key, _ in scope.get("headers", [])):
            return await self.protected(scope, receive, send)
        messages, body = [], bytearray()
        if scope["method"] == "POST":
            while True:
                message = await receive()
                if message["type"] == "http.disconnect":
                    return
                messages.append(message)
                body.extend(message.get("body", b""))
                if len(body) > self.max_body_size:
                    return await JSONResponse({"error": "request too large"}, status_code=413)(scope, receive, send)
                if not message.get("more_body", False):
                    break
        protected = False
        try:
            request = json.loads(body) if body else {}
            if isinstance(request, dict) and request.get("method") == "tools/call":
                params = request.get("params") or {}
                if isinstance(params, dict):
                    protected = params.get("name") == "add_mcp"
                    arguments = params.get("arguments") or {}
                    if params.get("name") == "call_tool" and isinstance(arguments, dict):
                        tool_id = arguments.get("tool_id")
                        if isinstance(tool_id, str):
                            sid = tool_id.split(".", 1)[0]
                            st = next((st for st in _gateway.states if st.id == sid), None)
                            protected = st is not None and not public_server(st.cfg)
        except (ValueError, TypeError):
            pass  # SDK returns its normal validation error for malformed JSON.

        async def replay():
            if messages:
                return messages.pop(0)
            return await receive()

        if protected and not AUTH_ENABLED:
            return await JSONResponse({"error": "Protected MCP access is not configured"}, status_code=403)(scope, replay, send)
        return await (self.protected if protected else self.public)(scope, replay, send)


def build_app(**transport_options):
    """One client URL: /mcp. Keycloak is requested only for protected calls."""
    from starlette.applications import Starlette
    from starlette.routing import Mount
    protected = mcp.streamable_http_app(stateless_http=True, **transport_options)
    public = public_mcp.streamable_http_app(stateless_http=True, **transport_options)

    @asynccontextmanager
    async def combined_lifespan(app):
        async with protected.router.lifespan_context(protected), public.router.lifespan_context(public):
            yield

    router = CatalogRouter(public, protected, transport_options.get("max_request_body_size", 1048576))
    return Starlette(routes=[Mount("/", app=router)], lifespan=combined_lifespan)


if __name__ == "__main__":
    import uvicorn
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(levelname)s %(message)s")
    # OAuth codes and login tickets are in query strings. Do not log access URLs.
    logging.getLogger("httpx2").setLevel(logging.WARNING)
    uvicorn.run(
        build_app(host=os.getenv("HOST", "0.0.0.0")),
        host=os.getenv("HOST", "0.0.0.0"), port=PORT, access_log=False,
    )
