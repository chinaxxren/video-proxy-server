# Hyper 1.x Migration

The production request, upstream, and response paths now use Hyper 1.x.
Migration was staged because the body and service APIs are not source-compatible.

## Order

1. Replace test/example clients with the shared Hyper 1 localhost client. Complete; `reqwest` has been removed.
   Test origin servers still use Hyper 0.14.
2. Introduce the streaming `AppBody` based on `http-body-util`. Complete.
3. Migrate the upstream client and TLS connector. Complete.
4. Migrate the localhost server to `hyper-util::server::conn`. Complete.
5. Migrate test origin servers and the local playground, then remove Hyper 0.14. Complete.

The upstream connector must retain `PublicOnlyResolver`. Replacing it with a
default reqwest client would reopen the DNS-rebinding SSRF path.

`ResponseBuilder`, `DataSourceManager`, `MixedSourceHandler`, `RequestHandler`,
and the localhost server now exchange `Response<AppBody>` directly. The old
server response bridge has been removed. A response-body wrapper retains each
concurrency permit until the body is consumed or dropped.

Hyper 0.14 has been removed from both production and development dependencies.
Test origin servers and the local playground now use Tokio listeners with
`hyper-util` connection drivers. The repository contains a single Hyper major
version.

## Exit Criteria

- Range, HEAD, open-ended range, and 416 responses remain unchanged.
- HLS playlist, key, map, subtitle, and segment requests remain unchanged.
- Concurrent identical ranges still produce one upstream fetch.
- Client disconnect and graceful shutdown tests pass.
- macOS C ABI smoke test passes.
