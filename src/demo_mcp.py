"""MCP de démo : deux tools sans rapport, pour tester la découverte dynamique."""
import random

from mcp.server.mcpserver import MCPServer

mcp = MCPServer("demo")


@mcp.tool()
def get_weather(city: str) -> dict:
    """Donne la météo du jour pour une ville (température et conditions)."""
    conditions = ["ensoleillé", "nuageux", "pluie", "averse", "ciel voilé"]
    return {
        "city": city,
        "temperature_c": random.randint(-2, 32),
        "condition": random.choice(conditions),
    }


@mcp.tool()
def send_invoice(invoice: str) -> dict:
    """Envoie une facture (contenu sérialisé) au service comptabilité."""
    return {"sent": True, "invoice_id": f"INV-{random.randint(1000, 9999)}", "echo": invoice}


if __name__ == "__main__":
    mcp.run(transport="stdio")
