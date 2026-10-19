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
  library->module = dlopen(path.data(), RTLD_NOW | RTLD_LOCAL);
#define SYMBOL(name) dlsym(library->module, name)
#endif
  if (!library->module) return fail(env, "Cannot load native VLite library for this platform");
  library->version = reinterpret_cast<decltype(library->version)>(SYMBOL("vlite_abi_version"));
  library->execute = reinterpret_cast<decltype(library->execute)>(SYMBOL("vlite_execute"));
  library->free_response = reinterpret_cast<decltype(library->free_response)>(SYMBOL("vlite_response_free"));
#undef SYMBOL
  if (!library->version || !library->execute || !library->free_response || library->version() != 1)
    return fail(env, "Unsupported native VLite ABI");
  napi_value result;
  napi_get_undefined(env, &result);
  return result;
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
    {"execute", nullptr, execute, nullptr, nullptr, nullptr, napi_default, nullptr}
  };
  if (napi_define_properties(env, exports, 2, properties) != napi_ok)
    return fail(env, "Cannot export addon");
  return exports;
}

NAPI_MODULE(NODE_GYP_MODULE_NAME, init)
