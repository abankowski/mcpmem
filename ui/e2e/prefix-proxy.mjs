#!/usr/bin/env node
// A prefix-stripping proxy for the prefix spec. http://127.0.0.1:8082/mem/<path>
// forwards to http://127.0.0.1:8080/<path>. A request without the /mem
// prefix is refused with 404, so a prefixed page that asks for a root asset
// or a root OAuth URL fails visibly and the spec sees it in the request log.

import http from "node:http";

const proxyPort = Number(process.argv[2] ?? "8082");
const upstreamPort = Number(process.argv[3] ?? "8080");
const UPSTREAM_HOST = "127.0.0.1";

const server = http.createServer((request, response) => {
  const target = request.url ?? "/";
  if (target !== "/mem" && !target.startsWith("/mem/")) {
    response.writeHead(404, { "Content-Type": "text/plain" });
    response.end("the proxy serves only /mem/* paths");
    return;
  }
  const stripped = target === "/mem" ? "/" : target.slice("/mem".length);
  const upstream = http.request(
    {
      host: UPSTREAM_HOST,
      port: upstreamPort,
      method: request.method,
      path: stripped,
      headers: { ...request.headers, host: `${UPSTREAM_HOST}:${upstreamPort}` },
    },
    (upstreamResponse) => {
      response.writeHead(upstreamResponse.statusCode ?? 502, upstreamResponse.headers);
      upstreamResponse.pipe(response);
    },
  );
  upstream.on("error", (error) => {
    response.writeHead(502, { "Content-Type": "text/plain" });
    response.end(`proxy upstream error: ${error.message}`);
  });
  request.pipe(upstream);
});

server.listen(proxyPort, "127.0.0.1");