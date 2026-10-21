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
   Invalid non-NULL pointers remain caller undefined behavior. */
VLITE_API char *vlite_execute(uint64_t handle, const uint8_t *input, size_t length);
VLITE_API void vlite_response_free(char *response);

#ifdef __cplusplus
}
#endif
#endif
