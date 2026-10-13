"""Minimal asyncio websocket client (RFC 6455) speaking the two GraphQL
subscription protocols Hasura V2 supports. Standard library only, so the
benchmark can open thousands of sockets without third-party packages.

  graphql-transport-ws  connection_init / subscribe / next / complete, ping/pong
  graphql-ws            Apollo legacy: connection_init / start / data / stop, "ka"
"""

from __future__ import annotations

import asyncio
import base64
import hashlib
import json
import os
import struct
from urllib.parse import urlparse

PROTOCOLS = ("graphql-transport-ws", "graphql-ws")
_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


class WSClosed(Exception):
    pass


class WebSocket:
    def __init__(self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
        self.reader, self.writer = reader, writer

    @classmethod
    async def connect(cls, url: str, subprotocol: str, timeout: float = 30.0) -> "WebSocket":
        u = urlparse(url)
        host, port = u.hostname, u.port or (443 if u.scheme == "wss" else 80)
        reader, writer = await asyncio.wait_for(asyncio.open_connection(host, port, limit=2**24), timeout)
        key = base64.b64encode(os.urandom(16)).decode()
        path = (u.path or "/") + (f"?{u.query}" if u.query else "")
        writer.write(
            (f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
             f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
             f"Sec-WebSocket-Protocol: {subprotocol}\r\n\r\n").encode()
        )
        head = (await asyncio.wait_for(reader.readuntil(b"\r\n\r\n"), timeout)).decode(errors="replace")
        status, *lines = head.split("\r\n")
        hdrs = {k.strip().lower(): v.strip() for k, _, v in (ln.partition(":") for ln in lines if ln)}
        # Check what real clients (browsers, Apollo, ws) check, so the
        # benchmark can't pass an engine they would reject.
        accept = base64.b64encode(hashlib.sha1((key + _GUID).encode()).digest()).decode()
        problem = None
        if " 101 " not in status + " ":
            problem = status
        elif hdrs.get("sec-websocket-accept") != accept:
            problem = f"bad Sec-WebSocket-Accept {hdrs.get('sec-websocket-accept')!r}"
        elif hdrs.get("sec-websocket-protocol") != subprotocol:
            problem = f"server chose subprotocol {hdrs.get('sec-websocket-protocol')!r}, asked {subprotocol!r}"
        if problem:
            writer.close()
            raise WSClosed(f"handshake failed: {problem}")
        return cls(reader, writer)

    def send_text(self, text: str):
        self._send(0x1, text.encode())

    def _send(self, opcode: int, payload: bytes):
        mask = os.urandom(4)
        n = len(payload)
        if n < 126:
            header = struct.pack("!BB", 0x80 | opcode, 0x80 | n)
        elif n < 65536:
            header = struct.pack("!BBH", 0x80 | opcode, 0x80 | 126, n)
        else:
            header = struct.pack("!BBQ", 0x80 | opcode, 0x80 | 127, n)
        masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
        self.writer.write(header + mask + masked)

    async def recv_text(self) -> str:
        """Next complete text message; answers pings, raises WSClosed on close."""
        chunks, first_op = [], None
        while True:
            b1, b2 = await self.reader.readexactly(2)
            fin, opcode, n = b1 & 0x80, b1 & 0x0F, b2 & 0x7F
            if n == 126:
                (n,) = struct.unpack("!H", await self.reader.readexactly(2))
            elif n == 127:
                (n,) = struct.unpack("!Q", await self.reader.readexactly(8))
            mask = await self.reader.readexactly(4) if b2 & 0x80 else None
            data = await self.reader.readexactly(n)
            if mask:
                data = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
            if opcode == 0x8:
                code = struct.unpack("!H", data[:2])[0] if len(data) >= 2 else None
                raise WSClosed(f"closed by server: {code} {data[2:].decode(errors='replace')}")
            if opcode == 0x9:
                self._send(0xA, data)
                continue
            if opcode == 0xA:
                continue
            if opcode in (0x1, 0x2):
                first_op, chunks = opcode, [data]
            elif opcode == 0x0:
                chunks.append(data)
            if fin and first_op is not None:
                return b"".join(chunks).decode()

    async def close(self):
        try:
            self._send(0x8, struct.pack("!H", 1000))
            await self.writer.drain()
        except (ConnectionError, RuntimeError):
            pass
        self.writer.close()


class GraphQLSubscription:
    """One socket carrying one subscription, as most clients do."""

    def __init__(self, ws: WebSocket, protocol: str):
        self.ws, self.protocol = ws, protocol

    @classmethod
    async def open(cls, url: str, protocol: str, headers: dict, query: str, variables=None,
                   timeout: float = 30.0) -> "GraphQLSubscription":
        ws = await WebSocket.connect(url, protocol, timeout)
        ws.send_text(json.dumps({"type": "connection_init", "payload": {"headers": headers}}))
        while True:
            msg = json.loads(await asyncio.wait_for(ws.recv_text(), timeout))
            if msg.get("type") == "connection_ack":
                break
            if msg.get("type") in ("connection_error", "error"):
                raise WSClosed(f"connection_init rejected: {msg}")
        start = "subscribe" if protocol == "graphql-transport-ws" else "start"
        payload = {"query": query}
        if variables is not None:
            payload["variables"] = variables
        ws.send_text(json.dumps({"id": "1", "type": start, "payload": payload}))
        return cls(ws, protocol)

    async def next_raw(self) -> tuple[str, str]:
        """(kind, raw text) of the next data-carrying message; kind is "data",
        "error" or "complete". Keep-alives and pings are handled here."""
        while True:
            raw = await self.ws.recv_text()
            # Cheap type sniffing: the hot path must not json-parse every frame.
            if '"next"' in raw[:40] or '"data"' in raw[:40]:
                return "data", raw
            msg = json.loads(raw)
            t = msg.get("type")
            if t == "ping":
                self.ws.send_text(json.dumps({"type": "pong"}))
            elif t in ("ka", "pong"):
                continue
            elif t == "complete":
                return "complete", raw
            elif t == "error":
                return "error", raw
            elif t in ("next", "data"):
                return "data", raw

    async def close(self):
        await self.ws.close()
