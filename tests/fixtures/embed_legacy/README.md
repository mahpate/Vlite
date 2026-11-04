# Released embedding responses

These files are raw HTTP response bodies captured from the unmodified `v0.6.1`
server, commit `fb46702f6c14a6a2d9de750858cf1c14503fc563`, built with the
release's locked dependencies and `server,http-embedder` features. Body files
have no trailing newline. `cases.json` records each exact request, dimensions,
HTTP path and status. Expected numbers are not calculated by the candidate.

Build an isolated `git archive v0.6.1` extraction with a separate target
directory, then run:

```powershell
python tests\capture_embed_legacy.py <path-to-released-vlite-server.exe>
python tests\capture_embed_legacy.py <path-to-released-vlite-server.exe> --check
```

`--check` compares status and raw bytes without updating fixtures. It can also
check a candidate executable, including the rejected-provider cases.

The corpus covers hashing at 3, 7 and 16 dimensions; default, explicit and null
provider selection; empty text; UTF-8 text; and empty, malformed, whitespace,
unknown, unsupported and overlong provider names. The oversized-text case pins
validation precedence with an unknown provider.

`/v1/embed` is not registered in the released server: its exact 404 responses
are preserved instead of inventing a legacy success response. The released
HTTP-provider handler panics while constructing its blocking reqwest runtime
inside Tokio, before returning a response, so no successful HTTP body is
claimed as a release golden. All providers use the same response serializer;
the serializer's HTTP and managed-alias cases are also checked separately.
