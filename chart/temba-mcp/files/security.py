"""Authentication for Temba's resource server."""
from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import time
from urllib.parse import urlparse, urlencode
import secrets

import httpx2
import jwt
from mcp.server.auth.provider import AccessToken


def identity_key(issuer: str, subject: str) -> str:
    """An identity survives token refresh, and subjects cannot collide across issuers."""
    return hashlib.sha256(json.dumps([issuer, subject]).encode()).hexdigest()


def require_https(url: str, *, allow_query: bool = False) -> None:
    parsed = urlparse(url)
    if (parsed.scheme != "https" or not parsed.hostname or parsed.username
            or parsed.password or parsed.fragment or (parsed.query and not allow_query)):
        raise ValueError("A trusted HTTPS URL without credentials, query or fragment is required")


def require_resource_url(url: str, *, allow_insecure_http: bool = False) -> None:
    """Permit an explicit HTTP exception only for Temba's local development URL."""
    parsed = urlparse(url)
    if (allow_insecure_http and parsed.scheme == "http"
            and parsed.hostname in {"localhost", "127.0.0.1", "::1"}
            and not (parsed.username or parsed.password or parsed.query or parsed.fragment)):
        return
    require_https(url)


class KeycloakTokenVerifier:
    """Validate access JWTs against one configured issuer and audience, never ID tokens.

    Network locations come from operator configuration and trusted discovery, never
    JWT headers. An unknown kid can trigger at most one JWKS fetch per 30 seconds.
    """

    def __init__(self, issuer: str, audience: str, resource: str, *, allow_insecure_http: bool = False):
        if not issuer:
            raise ValueError("AUTH_ISSUER is required when authentication is enabled: configure the Keycloak realm URL")
        try:
            require_https(issuer)
        except ValueError as exc:
            raise ValueError("AUTH_ISSUER must be an HTTPS Keycloak realm URL without credentials, query or fragment") from exc
        require_resource_url(resource, allow_insecure_http=allow_insecure_http)
        if not audience:
            raise ValueError("AUTH_AUDIENCE is required")
        self.issuer, self.audience, self.resource = issuer, audience, resource
        self.client = httpx2.AsyncClient(timeout=10, follow_redirects=False, trust_env=False)
        self.lock = asyncio.Lock()
        self.keys: dict[str, jwt.PyJWK] = {}
        self.fetched_at = 0.0
        self.metadata: dict = {}

    async def _keys(self, kid: str) -> jwt.PyJWK | None:
        async with self.lock:
            now = time.monotonic()
            if now - self.fetched_at >= 300 or (kid not in self.keys and now - self.fetched_at >= 30):
                self.fetched_at = now
                discovery = await self.client.get(self.issuer.rstrip("/") + "/.well-known/openid-configuration")
                discovery.raise_for_status()
                metadata = discovery.json()
                if metadata.get("issuer") != self.issuer:
                    raise ValueError("Discovery issuer mismatch")
                self.metadata = metadata
                jwks_uri = metadata["jwks_uri"]
                require_https(jwks_uri)
                if urlparse(jwks_uri).netloc != urlparse(self.issuer).netloc:
                    raise ValueError("JWKS must remain on the configured issuer origin")
                response = await self.client.get(jwks_uri)
                response.raise_for_status()
                self.keys = {
                    key["kid"]: jwt.PyJWK.from_dict(key, algorithm="RS256")
                    for key in response.json()["keys"]
                    if key.get("kty") == "RSA" and key.get("use", "sig") == "sig"
                    and key.get("alg", "RS256") == "RS256" and key.get("kid")
                }
            return self.keys.get(kid)

    async def verify_token(self, token: str) -> AccessToken | None:
        try:
            if len(token) > 16384:
                return None
            header = jwt.get_unverified_header(token)
            if header.get("alg") != "RS256" or not isinstance(header.get("kid"), str):
                return None
            key = await self._keys(header["kid"])
            if key is None:
                return None
            claims = jwt.decode(token, key.key, algorithms=["RS256"], audience=self.audience,
                                issuer=self.issuer, options={"require": ["iss", "sub", "aud", "exp", "iat"]})
            # Keycloak access tokens have payload typ=Bearer. ID tokens don't.
            if claims.get("typ") != "Bearer" or not isinstance(claims["sub"], str) or not claims["sub"]:
                return None
            client_id = claims.get("azp")
            scope = claims.get("scope", "")
            if not isinstance(client_id, str) or not client_id or not isinstance(scope, str):
                return None
            return AccessToken(token=token, client_id=client_id, subject=claims["sub"],
                               scopes=scope.split(), expires_at=int(claims["exp"]),
                               resource=self.resource, claims={"iss": self.issuer})
        except (jwt.PyJWTError, httpx2.HTTPError, ValueError, KeyError, TypeError):
            # Fail closed; never include a token, discovery body or exception in logs.
            return None


class BrowserLogin:
    """Confirm the ticket owner's Keycloak identity in the user's browser.

    A public client with a fixed redirect URI and S256 PKCE is sufficient. A
    confidential client may additionally supply a secret, stored only on Temba.
    No upstream tokens or codes are exposed to the LLM or persisted outside process memory.
    """

    def __init__(self, verifier: KeycloakTokenVerifier, client_id: str, redirect_uri: str,
                 client_secret: str = ""):
        if not client_id:
            raise ValueError("AUTH_BROWSER_CLIENT_ID is required")
        self.verifier, self.client_id = verifier, client_id
        self.redirect_uri, self.client_secret = redirect_uri, client_secret
        self.pending: dict[str, dict] = {}

    def prune(self):
        for state in list(self.pending):
            if self.pending[state]["exp"] <= time.time():
                self.pending.pop(state)

    async def begin(self, owner: str, auth, downstream_state: str, downstream_browser: str):
        self.prune()
        if len(self.pending) >= 256:
            raise ValueError("Too many browser logins")
        await self.verifier._keys("")
        endpoint = self.verifier.metadata["authorization_endpoint"]
        self._endpoint(endpoint)
        state, nonce, browser, verifier = (secrets.token_urlsafe(32) for _ in range(4))
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).decode().rstrip("=")
        self.pending[state] = {
            "owner": owner, "auth": auth, "downstream_state": downstream_state,
            "downstream_browser": downstream_browser, "nonce": nonce,
            "browser": hashlib.sha256(browser.encode()).hexdigest(), "verifier": verifier,
            "exp": min(time.time() + 600, auth.browser_flows[downstream_state]["exp"]),
        }
        url = endpoint + "?" + urlencode({"client_id": self.client_id, "redirect_uri": self.redirect_uri,
            "response_type": "code", "scope": "openid", "state": state, "nonce": nonce,
            "code_challenge": challenge, "code_challenge_method": "S256"})
        return state, browser, url

    def _endpoint(self, url: str):
        require_https(url)
        if urlparse(url).netloc != urlparse(self.verifier.issuer).netloc:
            raise ValueError("Browser OAuth endpoints must remain on the configured issuer origin")

    async def complete(self, state: str, browser: str, code: str, issuer: str | None):
        self.prune()
        entry = self.pending.get(state)
        if not entry or not browser or not secrets.compare_digest(
                entry["browser"], hashlib.sha256(browser.encode()).hexdigest()):
            return None
        self.pending.pop(state)  # single use, including failed exchanges
        accepted = False
        try:
            if issuer is not None and issuer != self.verifier.issuer:
                return None
            if self.verifier.metadata.get("authorization_response_iss_parameter_supported") and issuer is None:
                return None
            endpoint = self.verifier.metadata["token_endpoint"]
            self._endpoint(endpoint)
            data = {"grant_type": "authorization_code", "client_id": self.client_id,
                    "redirect_uri": self.redirect_uri, "code": code, "code_verifier": entry["verifier"]}
            if self.client_secret:
                data["client_secret"] = self.client_secret
            response = await self.verifier.client.post(endpoint, data=data)
            response.raise_for_status()
            id_token = response.json()["id_token"]
            header = jwt.get_unverified_header(id_token)
            if header.get("alg") != "RS256" or not isinstance(header.get("kid"), str):
                return None
            key = await self.verifier._keys(header["kid"])
            if not key:
                return None
            claims = jwt.decode(id_token, key.key, algorithms=["RS256"], audience=self.client_id,
                issuer=self.verifier.issuer, options={"require": ["iss", "sub", "aud", "exp", "iat", "nonce"]})
            if claims["nonce"] != entry["nonce"] or not isinstance(claims["sub"], str) or not claims["sub"]:
                return None
            if ("azp" in claims and claims["azp"] != self.client_id) or (
                    isinstance(claims["aud"], list) and len(claims["aud"]) > 1 and "azp" not in claims):
                return None
            if identity_key(self.verifier.issuer, claims["sub"]) != entry["owner"]:
                return None
            auth = entry["auth"]
            auth._prune()
            if entry["downstream_state"] not in auth.by_state:
                return None
            accepted = True
            return auth.by_state[entry["downstream_state"]], entry
        except (jwt.PyJWTError, httpx2.HTTPError, ValueError, KeyError, TypeError):
            return None
        finally:
            if not accepted:
                st = entry["auth"].by_state.get(entry["downstream_state"])
                if st:
                    entry["auth"].abort(st.id)
