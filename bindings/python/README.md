# VLite embedded Python

In-process binding for Python 3.11+, using ctypes and a bundled VLite shared
library. Platform wheels require no server, HTTP, Docker or Rust installation
at runtime. Install the wheel matching your operating system and architecture,
then use `NativeLibrary()` to load the included library automatically.
An explicit native library path is still supported.

See the [embedded deployment guide](https://github.com/mahpate/Vlite/blob/main/bindings/README.md)
for builds, installation, examples, ABI ownership and limits.
Licensed under MIT.
