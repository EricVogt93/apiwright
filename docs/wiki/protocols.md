# Protocols

## HTTP and GraphQL

HTTP is the main executable request model. It supports headers, query parameters, JSON/text/form/multipart/binary bodies, cookies, redirects, TLS controls, assertions, extractors, hooks and mocks.

The execution engine limits decoded response bodies to 32 MiB by default,
including gzip and chunked responses. Larger responses fail explicitly; a
truncated body is never passed off as a complete response to assertions.
Core integrations can configure this limit with
`HttpEngine::with_max_response_bytes(bytes)`.

Cross-origin redirects disable Digest, NTLM and AWS SigV4 authentication for
the remainder of that redirect chain and remove explicit authorization,
cookie and AWS session-token headers. Same-origin redirects retain authentication.
The request-v1 editor's **Stop** button cancels its active request, matrix,
sequence or batch; requests that have already reached a server cannot be undone.

GraphQL uses the same HTTP engine: ApiWright builds the standard `{query, variables, operationName}` JSON payload and validates GraphQL syntax before sending. Schema introspection is available for servers that permit the standard introspection query. GraphQL errors returned with HTTP 200 still need assertions against the response body.

## WebSocket

The WebSocket adapter supports `ws`/`wss`, handshake headers, text and binary messages, ping/pong and a clean close handshake. Incoming events retain timestamps and expose connected, text, binary, pong, closed and error states. Dropping a session cancels its background task and closes the socket.

## Server-Sent Events

The SSE adapter sends a GET with `Accept: text/event-stream`, parses event IDs, event names and data, timestamps received events and exposes open/error/closed states. Closing a subscription cancels the streaming HTTP connection.

## gRPC

ApiWright currently supports unary gRPC from `.proto` definitions. The GUI dialog and CLI can list services/methods and call a selected method with a JSON request body:

```sh
apiwright grpc list proto/catalog.proto -I proto
apiwright grpc call proto/catalog.proto catalog.v1.Catalog/GetItem \
  --endpoint https://localhost:50051 \
  --data '{"id":"42"}'
```

Streaming gRPC is not implemented. Custom CA roots and client certificate/key material are supported by the protocol adapters where configured.
