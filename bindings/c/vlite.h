#ifndef VLITE_H
#define VLITE_H
#include <stddef.h>
#include <stdint.h>
#ifdef _WIN32
#define VLITE_API __declspec(dllimport)
#else
#define VLITE_API
#endif
#ifdef __cplusplus
extern "C" {
#endif

/* ABI major version 1. JSON fields have their own documented v1 contract;
   format-4 collections, revisions, planned/hybrid search, ANN quality and
   resumable imports are additive v1 operations (see bindings/README.md). */
VLITE_API uint32_t vlite_abi_version(void);
/* Input is borrowed for the call; output must be freed with this library.
   handle=0 for create/open. Handles belong to this loaded library/process.
   Invalid non-NULL pointers remain caller undefined behavior.
   Additive features use {"op":"features","request":{"op":..., ...}}.
   See FEATURES.md for operations, typed values, limits and error codes.
   Feature create/open/open_from_bytes/restore_to require handle=0.
   Feature-created database handles use the same close/free functions.
   {"op":"capabilities"} (any handle) returns {"graph":bool}. Property-graph
   commands use {"op":"graph","request":{"op":..., ...}} and need a library
   built from source with the non-default `graph` feature; other builds return
   error code "unsupported" (builds predating graph return "invalid_input").
   Graph create requires handle=0; see FEATURES.md "Property graph". */
VLITE_API char *vlite_execute(uint64_t handle, const uint8_t *input, size_t length);
/* Optional additive symbols: detect availability when loading older ABI-1 builds.
   vectors is borrowed, aligned native-endian IEEE float32, immutable until return.
   count is the number of floats (not bytes), at most 2097152; all must be finite.
   NULL vectors is allowed only for count=0. JSON options are 1..8388608 bytes.
   Search: existing {"op":"search",...} or {"op":"search_approximate_ready",...}
   request with vector omitted.
   Bulk: {"dimensions":D,"operations":[{"id":"1",...},...],"collection":...};
   put fields are id, payload_base64 and metadata; vectors are row-major.
   D>0, rows<=4096, count=rows*D. Uses the existing atomic batch/sync contract.
   Responses/errors/free and serialization are identical to vlite_execute. */
VLITE_API char *vlite_search_f32(uint64_t handle, const uint8_t *input, size_t length,
                               const float *vectors, size_t count);
VLITE_API char *vlite_batch_put_f32(uint64_t handle, const uint8_t *input, size_t length,
                                  const float *vectors, size_t count);
VLITE_API void vlite_response_free(char *response);

#ifdef __cplusplus
}
#endif
#endif
