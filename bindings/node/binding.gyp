{
  "targets": [{
    "target_name": "vlite_node",
    "sources": ["addon.cc"],
    "defines": ["NAPI_VERSION=8"],
    "conditions": [
      ["OS=='linux'", {"libraries": ["-ldl"]}]
    ]
  }]
}
