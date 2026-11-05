"""UDP-прокси «плохая сеть» для тестов: ограничение скорости, буфер роутера, задержка, потери.

python badnet.py LISTEN_PORT SERVER_HOST:PORT --up-kbps 4000 --down-kbps 0 --delay-ms 40 --loss 0.5

up   — от клиента к серверу (канал стримера), down — от сервера к клиенту (канал зрителя).
0 — без ограничения. Буфер роутера = 150 мс на заданной скорости: всё, что не влезло, теряется
(так ведёт себя настоящий домашний роутер при перегрузке).
"""
import argparse
import asyncio
import random
import time


class Shaper:
    def __init__(self, kbps, delay_ms, loss, send):
        self.rate = kbps * 1000 / 8 if kbps else 0  # байт/с
        self.delay = delay_ms / 1000
        self.loss = loss / 100
        self.send = send
        self.queue_bytes = 0
        self.limit = self.rate * 0.15 if self.rate else 0
        self.next_free = time.monotonic()
        self.sent = self.dropped = 0

    def push(self, data):
        if random.random() < self.loss:
            self.dropped += 1
            return
        now = time.monotonic()
        if self.rate:
            if self.queue_bytes + len(data) > self.limit:
                self.dropped += 1
                return
            start = max(now, self.next_free)
            self.next_free = start + len(data) / self.rate
            self.queue_bytes += len(data)
            at = self.next_free + self.delay
        else:
            at = now + self.delay
        asyncio.get_running_loop().call_later(max(0, at - now), self._out, data)

    def _out(self, data):
        if self.rate:
            self.queue_bytes -= len(data)
        self.sent += 1
        self.send(data)


class Upstream(asyncio.DatagramProtocol):
    def __init__(self, proxy, client):
        self.proxy, self.client = proxy, client

    def connection_made(self, t):
        self.t = t
        self.down = Shaper(self.proxy.a.down_kbps, self.proxy.a.delay_ms / 2, self.proxy.a.loss,
                           lambda d: self.proxy.t.sendto(d, self.client))
        self.up = Shaper(self.proxy.a.up_kbps, self.proxy.a.delay_ms / 2, self.proxy.a.loss, lambda d: self.t.sendto(d))

    def datagram_received(self, data, _):
        self.down.push(data)


class Proxy(asyncio.DatagramProtocol):
    def __init__(self, a, server):
        self.a, self.server, self.clients = a, server, {}

    def connection_made(self, t):
        self.t = t

    def datagram_received(self, data, addr):
        up = self.clients.get(addr)
        if up is None:
            up = self.clients[addr] = Upstream(self, addr)
            asyncio.ensure_future(asyncio.get_running_loop().create_datagram_endpoint(lambda: up, remote_addr=self.server))
            up.pending = [data]
            return
        if not hasattr(up, "up"):
            up.pending.append(data)
            return
        for d in getattr(up, "pending", []):
            up.up.push(d)
        up.pending = []
        up.up.push(data)


async def main():
    p = argparse.ArgumentParser()
    p.add_argument("port", type=int)
    p.add_argument("server")
    p.add_argument("--up-kbps", type=float, default=0)
    p.add_argument("--down-kbps", type=float, default=0)
    p.add_argument("--delay-ms", type=float, default=0)
    p.add_argument("--loss", type=float, default=0)
    a = p.parse_args()
    host, port = a.server.rsplit(":", 1)
    loop = asyncio.get_running_loop()
    proxy = Proxy(a, (host, int(port)))
    await loop.create_datagram_endpoint(lambda: proxy, local_addr=("127.0.0.1", a.port))
    print(f"badnet :{a.port} → {a.server}  up={a.up_kbps or '∞'} down={a.down_kbps or '∞'} кбит/с, "
          f"задержка {a.delay_ms} мс, потери {a.loss}%", flush=True)
    while True:
        await asyncio.sleep(5)
        for c, u in proxy.clients.items():
            if hasattr(u, "up"):
                print(f"  {c[1]}: вверх {u.up.sent} отпр/{u.up.dropped} потер., вниз {u.down.sent}/{u.down.dropped}", flush=True)


asyncio.run(main())
