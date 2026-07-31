import sys

from mcp.server import MCPServer


mcp = MCPServer("python-sdk-interop", version="2.0.0")


@mcp.tool()
def interop_add(a: int, b: int) -> str:
    """Add two integers for SDK interoperability testing."""
    return str(a + b)


@mcp.resource("interop://fixture", mime_type="text/plain")
def interop_fixture() -> str:
    """Return static SDK interoperability content."""
    return "sdk-interop resource"


@mcp.prompt()
def interop_greet(name: str) -> str:
    """Render a greeting for SDK interoperability testing."""
    return f"Hello, {name}!"


def main() -> None:
    if len(sys.argv) != 2:
        raise SystemExit("usage: python server.py <port>")
    port = int(sys.argv[1])
    mcp.run(
        transport="streamable-http",
        host="127.0.0.1",
        port=port,
        json_response=True,
        stateless_http=True,
    )


if __name__ == "__main__":
    main()
