"""dynmcp — agrégateur MCP : un seul point d'entrée MCP (tool_search + call_tool)
qui agrège des MCP distants (streamable HTTP) enregistrés dans un registry YAML.

Recherche de tools : sémantique (embeddings via endpoint OpenAI-compatible si
EMBEDDINGS_BASE_URL est défini) sinon fallback local BM25.

SDK mcp >= 2.x (MCPServer, snake_case, streamable_http_client).
"""
import asyncio
import json
import logging
import math
import os
import re
from contextlib import asynccontextmanager
from typing import Any

import httpx2
import yaml
from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client
from mcp.server.mcpserver import MCPServer
from mcp.types import ToolListChangedNotification
from rank_bm25 import BM25Plus
import snowballstemmer

log = logging.getLogger("dynmcp")

TOOL_SEARCH_LIMIT = int(os.getenv("TOOL_SEARCH_LIMIT", "5"))
CALL_TIMEOUT = float(os.getenv("CALL_TIMEOUT", "60"))
LIST_TIMEOUT = float(os.getenv("LIST_TIMEOUT", "30"))
HEALTH_INTERVAL = float(os.getenv("HEALTH_INTERVAL", "60"))
RECONNECT_DELAY = float(os.getenv("RECONNECT_DELAY", "5"))

# Index sémantique optionnel (format OpenAI /v1/embeddings)
EMBEDDINGS_BASE_URL = os.getenv("EMBEDDINGS_BASE_URL", "").rstrip("/")
EMBEDDINGS_API_KEY = os.getenv("EMBEDDINGS_API_KEY", "")
EMBEDDINGS_MODEL = os.getenv("EMBEDDINGS_MODEL", "text-embedding-3-small")
EMBED_BATCH = int(os.getenv("EMBED_BATCH", "64"))
EMBED_MIN_SCORE = float(os.getenv("EMBED_MIN_SCORE", "0.25"))

_VAR_RE = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")


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


class ServerState:
    def __init__(self, cfg: dict):
        self.cfg = cfg
        self.id = cfg["id"]
        self.timeout: float | None = cfg.get("timeout")  # secondes, optionnel
        self.status = "starting"
        self.error: str | None = None
        self.tools: dict[str, dict] = {}
        self.session: ClientSession | None = None
        self.wake = asyncio.Event()


class Gateway:
    def __init__(self, registry_path: str):
        self.states: list[ServerState] = [ServerState(c) for c in self._load_registry(registry_path)]
        self._stop = asyncio.Event()
        self._tasks: list[asyncio.Task] = []
        self._index: BM25Index | EmbeddingIndex | None = None
        self._index_fp: str | None = None
        self._index_lock = asyncio.Lock()
        self._embed_client = httpx2.AsyncClient(timeout=30)

    async def _entries(self) -> list[dict]:
        entries = []
        for st in self.states:
            for tool in st.tools.values():
                entries.append({
                    "tool_id": f"{st.id}.{tool['name']}",
                    "server": st.id,
                    "name": tool["name"],
                    "description": tool["description"],
                    "input_schema": tool["input_schema"],
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
            if s.get("transport") != "http":
                raise ValueError(f"registry[{s.get('id')}]: transport invalide (http uniquement)")
            if not s.get("url"):
                raise ValueError(f"registry[{s['id']}]: 'url' requise")
            if s.get("timeout") is not None and (not isinstance(s["timeout"], (int, float)) or s["timeout"] <= 0):
                raise ValueError(f"registry[{s['id']}]: 'timeout' doit être un nombre de secondes > 0")
        return servers

    async def run(self) -> None:
        self._tasks = [asyncio.create_task(self._serve(st), name=f"mcp:{st.id}") for st in self.states]
        await self._stop.wait()

    async def stop(self) -> None:
        self._stop.set()
        for t in self._tasks:
            t.cancel()
        await asyncio.gather(*self._tasks, return_exceptions=True)
        await self._embed_client.aclose()

    @asynccontextmanager
    async def _open_transport(self, cfg: dict):
        client = httpx2.AsyncClient(headers=cfg.get("headers") or None)
        try:
            async with streamable_http_client(cfg["url"], http_client=client) as (read, write):
                yield read, write
        finally:
            await client.aclose()

    async def _serve(self, st: ServerState) -> None:
        while not self._stop.is_set():
            try:
                async with self._open_transport(st.cfg) as (read, write):
                    def on_message(message: Any, _st: ServerState = st) -> None:
                        if isinstance(message, ToolListChangedNotification):
                            _st.wake.set()

                    session = ClientSession(read, write, message_handler=on_message)
                    async with session:
                        await asyncio.wait_for(session.initialize(), st.timeout or LIST_TIMEOUT)
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

    def catalog(self) -> dict[str, list[str]]:
        return {st.id: sorted(st.tools) for st in self.states}

    async def search(self, query: str) -> dict:
        if self._index is None:
            await self._rebuild_index()
        ranked = await self._index.search(query or "", TOOL_SEARCH_LIMIT)
        results = [self._format(entry, score) for score, entry in ranked]
        payload: dict[str, Any] = {
            "query": query,
            "index": self._index.name,
            "count": len(results),
            "results": results,
        }
        if not results:
            payload["hint"] = "Aucun tool ne correspond. Utilise le champ 'catalog' pour voir les serveurs et leurs tools, puis reformule la recherche."
            payload["catalog"] = self.catalog()
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

    async def call(self, tool_id: str, arguments: dict | None) -> dict:
        if "." not in tool_id:
            return {"tool_id": tool_id, "ok": False, "error": "format tool_id attendu: <server_id>.<tool_name>"}
        server_id, tool_name = tool_id.split(".", 1)
        st = next((s for s in self.states if s.id == server_id), None)
        if st is None:
            return {"tool_id": tool_id, "ok": False, "error": f"serveur inconnu '{server_id}'", "catalog": self.catalog()}
        if tool_name not in st.tools:
            return {"tool_id": tool_id, "ok": False, "error": f"tool inconnu sur '{server_id}'", "catalog": self.catalog()}
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
    "dynmcp",
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
        "Les erreurs 429/rate-limit sont transitoires : réessaie ou passe au tool alternatif proposé."
    ),
    lifespan=lifespan,
)


@mcp.tool()
async def tool_search(query: str) -> str:
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
    return json.dumps(await _gateway.search(query), ensure_ascii=False, indent=2)


@mcp.tool()
async def call_tool(tool_id: str, arguments: dict = {}) -> str:
    """Exécute un tool trouvé via tool_search.

    Args:
        tool_id: identifiant reçu de tool_search, format "<server_id>.<tool_name>".
        arguments: arguments du tool, conformes à son input_schema.

    Returns:
        JSON: {ok, content, structured_content?} — résultat de l'exécution.
    """
    return json.dumps(await _gateway.call(tool_id, arguments), ensure_ascii=False, indent=2)


if __name__ == "__main__":
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(name)s %(levelname)s %(message)s")
    mcp.run(
        "streamable-http",
        host=os.getenv("HOST", "0.0.0.0"),
        port=int(os.getenv("PORT", "8080")),
        stateless_http=True,
    )
