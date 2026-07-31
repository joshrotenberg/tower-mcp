import asyncio
import sys

from mcp import Client


async def run(url: str, protocol: str) -> None:
    mode = "auto" if protocol == "2026-07-28" else "legacy"
    async with Client(url, mode=mode) as client:
        assert client.protocol_version == protocol, (
            f"negotiated {client.protocol_version}, expected {protocol}"
        )

        tools = await client.list_tools()
        assert any(tool.name == "interop_add" for tool in tools.tools), (
            "tools/list omitted interop_add"
        )
        called = await client.call_tool("interop_add", {"a": 19, "b": 23})
        assert not called.is_error
        assert getattr(called.content[0], "text", None) == "42"

        resources = await client.list_resources()
        assert any(str(resource.uri) == "interop://fixture" for resource in resources.resources), (
            "resources/list omitted fixture"
        )
        read = await client.read_resource("interop://fixture")
        assert getattr(read.contents[0], "text", None) == "sdk-interop resource"

        prompts = await client.list_prompts()
        assert any(prompt.name == "interop_greet" for prompt in prompts.prompts), (
            "prompts/list omitted interop_greet"
        )
        prompt = await client.get_prompt("interop_greet", {"name": "Tower"})
        assert getattr(prompt.messages[0].content, "text", None) == "Hello, Tower!"

        print(f"PASS Python SDK client -> {url} ({protocol})")


def main() -> None:
    if len(sys.argv) != 3 or sys.argv[2] not in {"2025-11-25", "2026-07-28"}:
        raise SystemExit(
            "usage: python client.py <url> <2025-11-25|2026-07-28>"
        )
    asyncio.run(run(sys.argv[1], sys.argv[2]))


if __name__ == "__main__":
    main()
