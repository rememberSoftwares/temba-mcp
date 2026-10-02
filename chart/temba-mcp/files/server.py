"""temba-mcp — agrégateur MCP : un seul point d'entrée MCP (tool_search + call_tool)
qui agrège des MCP distants (streamable HTTP) enregistrés dans un registry YAML.

Recherche de tools : sémantique (embeddings via endpoint OpenAI-compatible si
EMBEDDINGS_BASE_URL est défini) sinon fallback local BM25.

SDK mcp >= 2.x (MCPServer, snake_case, streamable_http_client).
"""
from __future__ import annotations

import asyncio
import ipaddress
import json
import logging
import math
import os
import re
import secrets
import socket
import time
from contextlib import asynccontextmanager
from typing import Any
from urllib.parse import urlparse

import httpx2
import yaml
from mcp import ClientSession
from mcp.client.auth import OAuthClientProvider
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

TOOL_SEARCH_LIMIT = int(os.getenv("TOOL_SEARCH_LIMIT", "5"))
CALL_TIMEOUT = float(os.getenv("CALL_TIMEOUT", "60"))
LIST_TIMEOUT = float(os.getenv("LIST_TIMEOUT", "30"))
HEALTH_INTERVAL = float(os.getenv("HEALTH_INTERVAL", "60"))
RECONNECT_DELAY = float(os.getenv("RECONNECT_DELAY", "5"))
PORT = int(os.getenv("PORT", "8080"))

# OAuth dynamique (MCP auth spec) : DCR + PKCE, login humain via /auth
PUBLIC_BASE_URL = os.getenv("PUBLIC_BASE_URL", f"http://localhost:{PORT}").rstrip("/")
AUTH_LOGIN_TIMEOUT = float(os.getenv("AUTH_LOGIN_TIMEOUT", "600"))

# Tickets de login à usage limité : l'URL renvoyée au LLM contient un jeton
# volant qui ne permet d'authentifier QUE le serveur concerné. Pas de page
# d'admin partagée : chaque demande produit son propre lien.
LOGIN_TICKET_TTL = float(os.getenv("LOGIN_TICKET_TTL", "900"))

# TTL glissant des tokens OAuth stockés (re-armé à chaque écriture) : un serveur
# non utilisé pendant cette durée devra se ré-authentifier.
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

# Redis (obligatoire en multi-réplicas) : état partagé (tokens OAuth + serveurs
# dynamiques). Vide = mémoire process (dev mono-process).
REDIS_URL = os.getenv("REDIS_URL", "")
REDIS_KEY_SERVERS = "temba-mcp:dynamic_servers"
REDIS_CHANNEL = "temba-mcp:events"

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


_redis: Any = None


def redis_client():
    """Client Redis partagé (None si REDIS_URL non défini)."""
    global _redis
    if REDIS_URL and _redis is None:
        import redis.asyncio as aioredis

        _redis = aioredis.from_url(REDIS_URL, decode_responses=True)
    return _redis


def _token_key(server_id: str) -> str:
    return f"temba-mcp:tokens:{server_id}"


class TokenStore:
    """TokenStorage SDK : Redis si REDIS_URL (partagé multi-réplicas), sinon mémoire."""

    def __init__(self, server_id: str):
        self.server_id = server_id

    async def _get(self) -> dict:
        if REDIS_URL:
            client = redis_client()
            try:
                raw = await client.get(_token_key(self.server_id))
                return json.loads(raw) if raw else {}
            except Exception as exc:
                log.warning("redis get tokens (%s): %s", self.server_id, exc)
                return {}
        return _TOKEN_MEMORY.get(self.server_id, {})

    async def _set(self, data: dict) -> None:
        if REDIS_URL:
            client = redis_client()
            try:
                # TTL glissant : re-armé à chaque écriture (refresh inclus)
                await client.set(_token_key(self.server_id), json.dumps(data), ex=int(TOKEN_RETENTION))
            except Exception as exc:
                log.warning("redis set tokens (%s): %s", self.server_id, exc)
            return
        _TOKEN_MEMORY[self.server_id] = data

    async def get_tokens(self) -> OAuthToken | None:
        data = await self._get()
        t = data.get("tokens")
        return OAuthToken.model_validate(t) if t else None

    async def set_tokens(self, tokens: OAuthToken) -> None:
        data = await self._get()
        data["tokens"] = tokens.model_dump(mode="json", exclude_none=True)
        await self._set(data)

    async def get_client_info(self) -> OAuthClientInformationFull | None:
        data = await self._get()
        c = data.get("client_info")
        return OAuthClientInformationFull.model_validate(c) if c else None

    async def set_client_info(self, info: OAuthClientInformationFull) -> None:
        data = await self._get()
        data["client_info"] = info.model_dump(mode="json", exclude_none=True)
        await self._set(data)


class AuthCoordinator:
    """Suit les flows OAuth en attente d'un login humain.

    Chaque flow en attente émet un ticket volant (URL /auth?t=...) transmis via
    le hint des tools : il ne permet d'authentifier que le serveur concerné,
    expire, et meurt dès que le login aboutit ou que le flow est relancé.
    """

    def __init__(self):
        self.by_state: dict[str, ServerState] = {}
        self.futures: dict[str, asyncio.Future] = {}
        self.tickets: dict[str, dict] = {}

    def _prune(self) -> None:
        now = time.time()
        for t in [k for k, v in self.tickets.items() if v["exp"] < now]:
            del self.tickets[t]

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
        return next((s for s in _gateway.states if s.id == self.tickets[ticket]["server_id"]), None)

    def start(self, st: ServerState, authorize_url: str) -> asyncio.Future:
        old = self.futures.get(st.id)
        if old is not None and not old.done():
            if st.auth_url == authorize_url:
                return old
            old.cancel()
        fut = asyncio.get_running_loop().create_future()
        self.futures[st.id] = fut
        st.auth_url = authorize_url
        st.status = "auth_required"
        st.error = None
        self.consume(st.id)
        st.login_ticket = self.issue_ticket(st.id)
        from urllib.parse import urlparse, parse_qs

        state = parse_qs(urlparse(authorize_url).query).get("state", ["?"])[0]
        self.by_state[state] = st
        log.warning("AUTH REQUIS pour '%s' — ticket de login émis (%s/auth?t=...)", st.id, PUBLIC_BASE_URL)
        return fut

    def complete(self, code: str, state: str) -> bool:
        st = self.by_state.pop(state, None)
        if st is None:
            return False
        fut = self.futures.get(st.id)
        if fut is not None and not fut.done():
            fut.set_result((code, state))
        self.consume(st.id)
        return True

    def abort(self, server_id: str) -> None:
        fut = self.futures.pop(server_id, None)
        if fut is not None and not fut.done():
            fut.cancel()
        self.consume(server_id)


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
        self.auth_url: str | None = None
        self.login_ticket: str | None = None


class Gateway:
    def __init__(self, registry_path: str):
        self.states: list[ServerState] = [ServerState(c) for c in self._load_registry(registry_path)]
        self._stop = asyncio.Event()
        self._tasks: list[asyncio.Task] = []
        self._index: BM25Index | EmbeddingIndex | None = None
        self._index_fp: str | None = None
        self._index_lock = asyncio.Lock()
        self._embed_client = httpx2.AsyncClient(timeout=30)
        self.auth = AuthCoordinator()

    async def add_server(self, cfg: dict) -> ServerState:
        st = ServerState(cfg)
        if any(s.id == st.id for s in self.states):
            raise ValueError(f"id '{st.id}' déjà utilisé")
        self.states.append(st)
        self._tasks.append(asyncio.create_task(self._serve(st), name=f"mcp:{st.id}"))
        if st.private or st.cfg.get("dynamic"):
            await self._dyn_put(st.cfg)
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
        if not any(s.id == server_id for s in self.states):
            return False
        await self._detach(server_id)
        await self._dyn_del(server_id)
        # purge immédiate des tokens du serveur supprimé
        if REDIS_URL:
            try:
                await redis_client().delete(_token_key(server_id))
            except Exception:
                pass
        else:
            _TOKEN_MEMORY.pop(server_id, None)
        await self._rebuild_index()
        return True

    async def _expire_loop(self) -> None:
        """Nettoie les serveurs privés (add_mcp) arrivés à expiration."""
        iterations = 0
        while not self._stop.is_set():
            try:
                await asyncio.sleep(60)
                now = time.time()
                expired = [st.id for st in self.states if st.expires_at and st.expires_at <= now]
                for sid in expired:
                    log.info("serveur privé '%s' expiré — suppression", sid)
                    await self.remove_server(sid)
                # purge périodique des tokens orphelins (serveur supprimé du registry)
                iterations += 1
                if iterations % 15 == 0:
                    await self._purge_orphan_tokens()
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                log.warning("expire loop: %s", exc)

    async def _purge_orphan_tokens(self) -> None:
        """Supprime les tokens OAuth dont le serveur n'existe plus."""
        known = {st.id for st in self.states}
        if REDIS_URL:
            client = redis_client()
            removed = 0
            async for key in client.scan_iter(match="temba-mcp:tokens:*"):
                sid = key.removeprefix("temba-mcp:tokens:")
                if sid and sid not in known:
                    await client.delete(key)
                    removed += 1
            if removed:
                log.info("purge de %d tokens orphelins", removed)
        else:
            for sid in [k for k in list(_TOKEN_MEMORY) if k not in known]:
                _TOKEN_MEMORY.pop(sid, None)

    def _visible(self, st: ServerState, client_labels: frozenset[str]) -> bool:
        if st.private:
            return False
        return not st.labels or bool(st.labels & client_labels)

    async def _entries(self) -> list[dict]:
        entries = []
        for st in self.states:
            if st.private:
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
        """Serveurs dynamiques (API admin + privés) : Redis si configuré, sinon mémoire."""
        if REDIS_URL:
            try:
                raw = await redis_client().hgetall(REDIS_KEY_SERVERS)
                return [json.loads(v) for v in raw.values()]
            except Exception as exc:
                log.error("redis illisible (%s) — registry dynamique vide", exc)
                return []
        return [dict(c) for c in _DYN_MEMORY.values()]

    async def _dyn_put(self, cfg: dict) -> None:
        if REDIS_URL:
            try:
                await redis_client().hset(REDIS_KEY_SERVERS, cfg["id"], json.dumps(cfg))
                await self._dyn_publish()
            except Exception as exc:
                log.error("redis hset: %s", exc)
            return
        _DYN_MEMORY[cfg["id"]] = cfg

    async def _dyn_del(self, server_id: str) -> None:
        if REDIS_URL:
            try:
                await redis_client().hdel(REDIS_KEY_SERVERS, server_id)
                await self._dyn_publish()
            except Exception as exc:
                log.error("redis hdel: %s", exc)
            return
        _DYN_MEMORY.pop(server_id, None)

    async def _dyn_publish(self) -> None:
        try:
            await redis_client().publish(REDIS_CHANNEL, "registry_changed")
        except Exception:
            pass

    async def _pubsub_loop(self) -> None:
        """Réconcilie le registry local quand une autre réplique modifie l'état."""
        client = redis_client()
        while not self._stop.is_set():
            try:
                pubsub = client.pubsub()
                await pubsub.subscribe(REDIS_CHANNEL)
                async for message in pubsub.listen():
                    if self._stop.is_set() or message.get("type") != "message":
                        continue
                    await self._dyn_reconcile()
            except asyncio.CancelledError:
                raise
            except Exception as exc:
                log.warning("pubsub registry: %s — nouvelle tentative dans 5s", exc)
                await asyncio.sleep(5)

    async def _dyn_reconcile(self) -> None:
        """Aligne les serveurs dynamiques locaux sur l'état partagé (Redis)."""
        desired = {c["id"]: c for c in await self._dyn_all() if not (c.get("expires_at") and c["expires_at"] <= time.time())}
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
        # rattrape l'état partagé (Redis) ou le fichier local (mono-réplique)
        try:
            await self._dyn_reconcile()
        except Exception as exc:
            log.error("réplication initiale: %s", exc)
        self._tasks = [asyncio.create_task(self._serve(st), name=f"mcp:{st.id}") for st in self.states]
        self._tasks.append(asyncio.create_task(self._expire_loop(), name="private-expiry"))
        if REDIS_URL:
            self._tasks.append(asyncio.create_task(self._pubsub_loop(), name="redis-pubsub"))
        await self._stop.wait()

    async def stop(self) -> None:
        self._stop.set()
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        await self._embed_client.aclose()
        client = redis_client()
        if client is not None:
            try:
                await client.aclose()
            except Exception:
                pass

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
        store = TokenStore(st.id)
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
            code, state = await asyncio.shield(fut)
            return AuthorizationCodeResult(code=code, state=state)
        return callback_handler

    @asynccontextmanager
    async def _open_transport(self, st: ServerState):
        cfg = st.cfg
        client_kwargs: dict[str, Any] = {"headers": cfg.get("headers") or None}
        if cfg.get("oauth") is not None:
            client_kwargs["auth"] = self._auth_provider(st)
        client = httpx2.AsyncClient(**client_kwargs)
        try:
            async with streamable_http_client(cfg["url"], http_client=client) as (read, write):
                yield read, write
        finally:
            await client.aclose()

    async def _serve(self, st: ServerState) -> None:
        while not self._stop.is_set():
            try:
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
                st.error = f"{type(exc).__name__}: {exc}"
                st.tools = {}
                self.auth.abort(st.id)
                if not self._stop.is_set():
                    await asyncio.sleep(RECONNECT_DELAY)
            finally:
                st.session = None

    async def _sync_tools(self, st: ServerState) -> None:
        async with asyncio.timeout(st.timeout or LIST_TIMEOUT):
            res = await st.session.list_tools()
        st.tools = {
            t.name: {"name": t.name, "description": t.description or "", "input_schema": t.input_schema}
            for t in res.tools
        }
        st.status = "ready"
        st.error = None
        await self._rebuild_index()

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
                            f"{PUBLIC_BASE_URL}/auth?t={st.login_ticket} (lien personnel valable "
                            f"{int(LOGIN_TICKET_TTL / 60)} min)",
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
        # Les serveurs privés (add_mcp) ne sont pas soumis au filtre labels :
        # leur seule protection est l'id secret que seul leur créateur connaît.
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
                auth_url = f"{PUBLIC_BASE_URL}/auth?t={st.login_ticket}" if st.login_ticket else f"{PUBLIC_BASE_URL}/auth"
                return {
                    "tool_id": tool_id,
                    "ok": False,
                    "auth_required": True,
                    "error": f"authentification requise pour le serveur '{server_id}'",
                    "hint": f"demande à l'utilisateur d'ouvrir {auth_url} — lien personnel valable "
                            f"{int(LOGIN_TICKET_TTL / 60)} min, ne concerne que ce serveur ; "
                            f"une fois le login fait, réessaie call_tool à l'identique",
                }
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
            async with asyncio.timeout(st.timeout or CALL_TIMEOUT):
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
                "error": f"{type(exc).__name__}: {exc}",
                "hint": "le serveur a peut-être été redémarré, relance tool_search puis réessaie",
            }


_gateway: Gateway | None = None


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
        yield {"gateway": _gateway}
    finally:
        await _gateway.stop()
        task.cancel()


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
    return json.dumps(await _gateway.search(query, _client_labels(ctx)), ensure_ascii=False, indent=2)


@mcp.tool()
async def call_tool(tool_id: str, arguments: dict = {}, ctx: Context = None) -> str:
    """Exécute un tool trouvé via tool_search.

    Args:
        tool_id: identifiant reçu de tool_search, format "<server_id>.<tool_name>".
        arguments: arguments du tool, conformes à son input_schema.

    Returns:
        JSON: {ok, content, structured_content?} — résultat de l'exécution.
    """
    return json.dumps(await _gateway.call(tool_id, arguments, _client_labels(ctx)), ensure_ascii=False, indent=2)


async def _assert_public_url(url: str) -> None:
    """Garde anti-SSRF pour add_mcp : refuse IP privées/loopback/link-local.
    Strict, sans exception : add_mcp = internet public, rien d'autre."""
    parsed = urlparse(url)
    if parsed.scheme not in ("http", "https") or not parsed.hostname:
        raise ValueError("url invalide (http/https attendu)")
    try:
        infos = await asyncio.to_thread(socket.getaddrinfo, parsed.hostname, None)
    except socket.gaierror as exc:
        raise ValueError(f"hôte introuvable: {parsed.hostname} ({exc})")
    for info in infos:
        ip = ipaddress.ip_address(info[4][0])
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


if ADD_MCP_ENABLED:

    @mcp.tool()
    async def add_mcp(url: str, name: str = "", headers: dict = {}) -> str:
        """Ajoute un MCP distant (streamable HTTP) pour étendre tes propres capacités.

        Le serveur est PRIVÉ : il n'apparaît pas dans le catalogue partagé, ses tools
        ne sont utilisables que toi, via le server_key secret retourné (prefixe des
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
        url = url.strip()
        try:
            await _assert_public_url(url)
        except ValueError as exc:
            return json.dumps({"ok": False, "error": str(exc)}, ensure_ascii=False)
        # Dé-dup : un même URL ne s'enregistre qu'une fois — si le MCP exige une
        # authentification, on ne recrée PAS un flow, on réutilise le serveur
        # existant et son ticket de login en attente.
        st = next(
            (s for s in _gateway.states
             if s.private and s.cfg.get("url") == url and not (s.expires_at and s.expires_at <= time.time())),
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
                # OAuth dynamique activé par défaut : no-op si le MCP n'en a pas besoin,
                # déclenche le flow (DCR + ticket de login) s'il répond 401.
                "oauth": {},
                "expires_at": time.time() + PRIVATE_SERVER_TTL,
            }
            if headers:
                cfg["headers"] = headers
            st = await _gateway.add_server(cfg)
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


def _server_by_id(server_id: str) -> ServerState | None:
    return next((s for s in _gateway.states if s.id == server_id), None)


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


@mcp.custom_route("/auth", methods=["GET"])
async def auth_page(request: Request) -> HTMLResponse:
    """Page de login propre à une demande : accessible uniquement via le ticket
    volant reçu dans le hint du LLM (?t=...). Sans ticket : refusé."""
    ticket = request.query_params.get("t", "")
    st = _gateway.auth.server_for_ticket(ticket) if ticket else None
    if st is None:
        return HTMLResponse(
            "<html><head><meta charset='utf-8'><title>temba-mcp auth</title></head><body>"
            "<p>Lien invalide ou expiré. Demande un nouveau lien à ton agent "
            "(relance l'action qui nécessite ce serveur).</p></body></html>",
            status_code=401,
        )
    ttl_min = int(LOGIN_TICKET_TTL / 60)
    if st.status == "auth_required" and st.auth_url:
        body = (
            f"<p>Serveur <b>{st.id}</b> en attente d'authentification.</p>"
            f"<p><a href='/auth/{st.id}/login?t={ticket}'>S'authentifier maintenant</a></p>"
            f"<p><small>Ce lien est personnel et valable {ttl_min} min. "
            f"Le compte avec lequel tu te connectes sera celui utilisé par le gateway pour ce serveur.</small></p>"
        )
    else:
        body = f"<p>Le serveur <b>{st.id}</b> n'attend plus d'authentification (statut: {st.status}). Tu peux fermer cette page.</p>"
    return HTMLResponse(
        "<html><head><meta charset='utf-8'><title>temba-mcp auth</title></head><body>"
        f"{body}</body></html>"
    )


@mcp.custom_route("/auth/{server_id}/login", methods=["GET"])
async def auth_login(request: Request) -> HTMLResponse | RedirectResponse:
    st = _server_by_id(request.path_params["server_id"])
    if st is None or not st.auth_url:
        return HTMLResponse("<p>Aucun flow OAuth en attente pour ce serveur.</p>", status_code=404)
    ticket = request.query_params.get("t", "")
    if not _gateway.auth.ticket_ok(ticket, st.id):
        return HTMLResponse("<p>Lien invalide ou expiré. Demande un nouveau lien à ton agent.</p>", status_code=403)
    return RedirectResponse(st.auth_url)


@mcp.custom_route("/oauth/callback", methods=["GET"])
async def oauth_callback(request: Request) -> HTMLResponse:
    code = request.query_params.get("code")
    state = request.query_params.get("state")
    if not code or not state or not _gateway.auth.complete(code, state):
        return HTMLResponse("<p>Callback OAuth invalide ou expiré.</p>", status_code=400)
    return HTMLResponse(
        "<p>Autorisation enregistrée — le gateway termine la connexion (quelques secondes). "
        "Retourne dans ton agent et relance ton action, tu peux fermer cette page.</p>"
    )


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(levelname)s %(message)s")
    mcp.run(
        "streamable-http",
        host=os.getenv("HOST", "0.0.0.0"),
        port=int(os.getenv("PORT", "8080")),
        stateless_http=True,
    )
