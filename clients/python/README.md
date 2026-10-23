# VLite Python client

Synchronous, typed REST client for Python 3.11+ with no runtime dependencies.
For application use, install the published
[vlite-client package from PyPI](https://pypi.org/project/vlite-client/):

```text
python -m pip install vlite-client
```

The current published version is 0.3.0. For contributor development from a
VLite checkout, install the working tree with
`python -m pip install ./clients/python`.

See the [client guide](https://github.com/mahpate/Vlite/blob/main/clients/README.md)
for authentication, examples, supported operations and uncertain-write handling.
This package does not automatically retry requests or follow redirects.
Licensed under MIT; see LICENSE.
