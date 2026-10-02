"""Client de test local du gateway dynmcp (streamable HTTP)."""
import asyncio
import json
import os
import sys

from mcp import ClientSession
from mcp.client.streamable_http import streamable_http_client


async def main() -> None:
    url = os.getenv("DYNMCP_URL", "http://localhost:8080/mcp")
    query = " ".join(sys.argv[1:]) or "va t-il faire beau à paris aujourd'hui"
    async with streamable_http_client(url) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            tools = await session.list_tools()
            print("== tools exposés par le gateway ==")
            for t in tools.tools:
                print(" -", t.name)
            res = await session.call_tool("tool_search", {"query": query})
            print(f"\n== tool_search({query!r}) ==")
            print(res.content[0].text)
            results = json.loads(res.content[0].text)["results"]
            if results:
                top = results[0]
                res2 = await session.call_tool("call_tool", {"tool_id": top["tool_id"], "arguments": {"city": "Paris"}})
                print(f"\n== call_tool({top['tool_id']}, city=Paris) ==")
                print(res2.content[0].text)


if __name__ == "__main__":
    asyncio.run(main())
