#!/usr/bin/env node
// The e2e OCR vision endpoint. The extractor renders one PDF page per
// request and asks a vision model to transcribe it; this fixture answers
// with the stable page texts the suite asserts on. The first request of a
// PDF job is page one, the second is page two; later requests rotate back.

import http from "node:http";

const PORT = Number(process.argv[2] ?? "8092");

let requestCount = 0;

const server = http.createServer((request, response) => {
  if (request.method === "GET" && request.url === "/health") {
    response.writeHead(200, { "Content-Type": "text/plain" });
    response.end("ok");
    return;
  }
  let body = "";
  request.on("data", (chunk) => {
    body += chunk;
  });
  request.on("end", () => {
    if (request.method !== "POST" || !request.url?.includes("chat/completions")) {
      response.writeHead(404, { "Content-Type": "application/json" });
      response.end('{"error":"not found"}');
      return;
    }
    let payload = null;
    try {
      payload = JSON.parse(body);
    } catch {
      // Fall through to the default transcription below.
    }
    const model = payload && typeof payload.model === "string" ? payload.model : "e2e-vision";
    requestCount += 1;
    const pageText = requestCount % 10 === 2 ? "E2E PDF page two" : "E2E PDF page one";
    response.writeHead(200, { "Content-Type": "application/json" });
    response.end(
      JSON.stringify({
        choices: [{ message: { content: pageText }, finish_reason: "stop" }],
        model,
      }),
    );
  });
});

server.listen(PORT, "127.0.0.1");