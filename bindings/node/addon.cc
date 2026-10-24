#include <node_api.h>
#include <stdint.h>
#include <string>
#include <vector>
#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif

struct Library {
#ifdef _WIN32
  HMODULE module = nullptr;
#else
  void *module = nullptr;
#endif
  uint32_t (*version)() = nullptr;
  char *(*execute)(uint64_t, const uint8_t *, size_t) = nullptr;
  char *(*search_f32)(uint64_t, const uint8_t *, size_t, const float *, size_t) = nullptr;
  char *(*batch_put_f32)(uint64_t, const uint8_t *, size_t, const float *, size_t) = nullptr;
  void (*free_response)(char *) = nullptr;
  ~Library() {
    if (module) {
#ifdef _WIN32
      FreeLibrary(module);
#else
      dlclose(module);
#endif
    }
  }
};

static napi_value fail(napi_env env, const char *message) {
  napi_throw_error(env, nullptr, message);
  return nullptr;
}

static void cleanup(napi_env, void *data, void *) {
  delete static_cast<Library *>(data);
}

static napi_value load(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value arg;
  if (napi_get_cb_info(env, info, &argc, &arg, nullptr, nullptr) != napi_ok || argc != 1)
    return fail(env, "Expected an absolute native library path");
  Library *library;
  if (napi_get_instance_data(env, reinterpret_cast<void **>(&library)) != napi_ok)
    return fail(env, "Missing addon state");
  if (library->module) return fail(env, "Native library already loaded");
#ifdef _WIN32
  size_t size;
  if (napi_get_value_string_utf16(env, arg, nullptr, 0, &size) != napi_ok)
    return fail(env, "Expected a library path string");
  std::vector<char16_t> path(size + 1);
  if (napi_get_value_string_utf16(env, arg, path.data(), path.size(), &size) != napi_ok)
    return fail(env, "Invalid library path");
  library->module = LoadLibraryExW(reinterpret_cast<LPCWSTR>(path.data()), nullptr,
                                   LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS);
#define SYMBOL(name) GetProcAddress(library->module, name)
#else
  size_t size;
  if (napi_get_value_string_utf8(env, arg, nullptr, 0, &size) != napi_ok)
    return fail(env, "Expected a library path string");
  std::vector<char> path(size + 1);
  if (napi_get_value_string_utf8(env, arg, path.data(), path.size(), &size) != napi_ok)
    return fail(env, "Invalid library path");
  int flags = RTLD_NOW | RTLD_LOCAL;
#ifdef __linux__
  // Rust registers pthread TLS destructors that run after N-API cleanup. Keep
  // their code mapped until process exit, even when the last worker dlcloses it.
  flags |= RTLD_NODELETE;
#endif
  library->module = dlopen(path.data(), flags);
#define SYMBOL(name) dlsym(library->module, name)
#endif
  if (!library->module) return fail(env, "Cannot load native VLite library for this platform");
  library->version = reinterpret_cast<decltype(library->version)>(SYMBOL("vlite_abi_version"));
  library->execute = reinterpret_cast<decltype(library->execute)>(SYMBOL("vlite_execute"));
  library->search_f32 = reinterpret_cast<decltype(library->search_f32)>(SYMBOL("vlite_search_f32"));
  library->batch_put_f32 = reinterpret_cast<decltype(library->batch_put_f32)>(SYMBOL("vlite_batch_put_f32"));
  library->free_response = reinterpret_cast<decltype(library->free_response)>(SYMBOL("vlite_response_free"));
#undef SYMBOL
  if (!library->version || !library->execute || !library->free_response || library->version() != 1)
    return fail(env, "Unsupported native VLite ABI");
  napi_value result;
  napi_get_undefined(env, &result);
  return result;
}

struct TypedInput {
  Library *library;
  uint64_t handle;
  const uint8_t *request;
  size_t request_length;
  const float *vectors;
  size_t vector_count;
};

static bool read_typed_input(napi_env env, napi_callback_info info, size_t expected_argc,
                             napi_value *args, TypedInput *input) {
  size_t argc = expected_argc;
  if (napi_get_cb_info(env, info, &argc, args, nullptr, nullptr) != napi_ok || argc != expected_argc) {
    fail(env, "Expected handle, request buffer and Float32Array");
    return false;
  }
  if (napi_get_instance_data(env, reinterpret_cast<void **>(&input->library)) != napi_ok ||
      !input->library || !input->library->execute || !input->library->free_response) {
    fail(env, "Native library not loaded");
    return false;
  }
  bool lossless;
  if (napi_get_value_bigint_uint64(env, args[0], &input->handle, &lossless) != napi_ok || !lossless) {
    fail(env, "Handle must be a lossless u64 bigint");
    return false;
  }
  bool is_buffer;
  void *request_data;
  if (napi_is_buffer(env, args[1], &is_buffer) != napi_ok || !is_buffer ||
      napi_get_buffer_info(env, args[1], &request_data, &input->request_length) != napi_ok) {
    fail(env, "Request must be a buffer");
    return false;
  }
  input->request = static_cast<const uint8_t *>(request_data);
  bool is_typed_array;
  if (napi_is_typedarray(env, args[2], &is_typed_array) != napi_ok || !is_typed_array) {
    fail(env, "Vectors must be a Float32Array");
    return false;
  }
  napi_typedarray_type type;
  void *data;
  napi_value array_buffer;
  size_t byte_offset;
  if (napi_get_typedarray_info(env, args[2], &type, &input->vector_count, &data,
                               &array_buffer, &byte_offset) != napi_ok ||
      type != napi_float32_array) {
    fail(env, "Vectors must be a Float32Array");
    return false;
  }
  input->vectors = static_cast<const float *>(data);
  return true;
}

static napi_value native_string(napi_env env, Library *library, char *response) {
  if (!response) return fail(env, "Native response missing; mutation outcome unknown");
  napi_value result;
  napi_status status = napi_create_string_utf8(env, response, NAPI_AUTO_LENGTH, &result);
  library->free_response(response);
  if (status != napi_ok) return fail(env, "Cannot decode native response; mutation outcome unknown");
  return result;
}

static napi_value typed_available(napi_env env, napi_callback_info info) {
  Library *library;
  if (napi_get_instance_data(env, reinterpret_cast<void **>(&library)) != napi_ok || !library) {
    return fail(env, "Missing addon state");
  }
  napi_value result;
  napi_get_boolean(env, library->search_f32 && library->batch_put_f32, &result);
  return result;
}

static napi_value search_f32(napi_env env, napi_callback_info info) {
  napi_value args[3];
  TypedInput input{};
  if (!read_typed_input(env, info, 3, args, &input)) return nullptr;
  if (!input.library->search_f32) return fail(env, "Native library does not export vlite_search_f32");
  return native_string(env, input.library, input.library->search_f32(
      input.handle, input.request, input.request_length, input.vectors, input.vector_count));
}

static napi_value batch_put_f32(napi_env env, napi_callback_info info) {
  napi_value args[3];
  TypedInput input{};
  if (!read_typed_input(env, info, 3, args, &input)) return nullptr;
  if (!input.library->batch_put_f32) return fail(env, "Native library does not export vlite_batch_put_f32");
  return native_string(env, input.library, input.library->batch_put_f32(
      input.handle, input.request, input.request_length, input.vectors, input.vector_count));
}

static napi_value search_many_f32(napi_env env, napi_callback_info info) {
  napi_value args[4];
  TypedInput input{};
  if (!read_typed_input(env, info, 4, args, &input)) return nullptr;
  if (!input.library->search_f32) return fail(env, "Native library does not export vlite_search_f32");
  uint32_t dimensions;
  if (napi_get_value_uint32(env, args[3], &dimensions) != napi_ok || dimensions == 0 ||
      input.vector_count % dimensions != 0 || input.vector_count / dimensions > 128) {
    return fail(env, "Invalid bounded Float32Array search matrix");
  }
  const size_t queries = input.vector_count / dimensions;
  napi_value results;
  if (napi_create_array_with_length(env, queries, &results) != napi_ok) {
    return fail(env, "Cannot allocate search result array");
  }
  for (size_t i = 0; i < queries; ++i) {
    char *response = input.library->search_f32(input.handle, input.request, input.request_length,
                                                input.vectors + i * dimensions, dimensions);
    if (!response) return fail(env, "Native response missing; read outcome unknown");
    napi_value item;
    napi_status status = napi_create_string_utf8(env, response, NAPI_AUTO_LENGTH, &item);
    input.library->free_response(response);
    if (status != napi_ok || napi_set_element(env, results, i, item) != napi_ok) {
      return fail(env, "Cannot decode native response; read outcome unknown");
    }
  }
  return results;
}

static napi_value execute(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value args[2];
  if (napi_get_cb_info(env, info, &argc, args, nullptr, nullptr) != napi_ok || argc != 2)
    return fail(env, "Expected handle and UTF-8 request buffer");
  Library *library;
  napi_get_instance_data(env, reinterpret_cast<void **>(&library));
  if (!library || !library->execute || !library->free_response)
    return fail(env, "Native library not loaded");
  uint64_t handle;
  bool lossless;
  if (napi_get_value_bigint_uint64(env, args[0], &handle, &lossless) != napi_ok || !lossless)
    return fail(env, "Handle must be a lossless u64 bigint");
  bool is_buffer;
  if (napi_is_buffer(env, args[1], &is_buffer) != napi_ok || !is_buffer)
    return fail(env, "Request must be a buffer");
  void *data;
  size_t length;
  if (napi_get_buffer_info(env, args[1], &data, &length) != napi_ok)
    return fail(env, "Invalid request buffer");
  char *response = library->execute(handle, static_cast<const uint8_t *>(data), length);
  if (!response) return fail(env, "Native response missing; mutation outcome unknown");
  napi_value result;
  napi_status status = napi_create_string_utf8(env, response, NAPI_AUTO_LENGTH, &result);
  library->free_response(response);
  if (status != napi_ok) return fail(env, "Cannot decode native response; mutation outcome unknown");
  return result;
}

static napi_value init(napi_env env, napi_value exports) {
  auto *library = new Library();
  if (napi_set_instance_data(env, library, cleanup, nullptr) != napi_ok) {
    delete library;
    return fail(env, "Cannot initialize addon");
  }
  napi_property_descriptor properties[] = {
    {"load", nullptr, load, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"execute", nullptr, execute, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"typedAvailable", nullptr, typed_available, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"searchF32", nullptr, search_f32, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"batchPutF32", nullptr, batch_put_f32, nullptr, nullptr, nullptr, napi_default, nullptr},
    {"searchManyF32", nullptr, search_many_f32, nullptr, nullptr, nullptr, napi_default, nullptr}
  };
  if (napi_define_properties(env, exports, 6, properties) != napi_ok)
    return fail(env, "Cannot export addon");
  return exports;
}

NAPI_MODULE(NODE_GYP_MODULE_NAME, init)
