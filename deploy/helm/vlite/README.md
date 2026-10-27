# VLite Helm chart

This chart runs exactly **one** database owner in a StatefulSet. The database
file (`/data/vectors.vlite`) and administration state (`/data/admin`) share a
PVC. PVCs survive Helm uninstall by the StatefulSet retention default. Back up
the volume before upgrades; the chart is not replication, HA or an online
backup system. The complete live dataset must fit in the container memory limit.

Build and push the API image from the repository before installation:

```sh
docker build --target api -t registry.example/vlite:0.6.0 .
docker push registry.example/vlite:0.6.0
kubectl create secret generic vlite-auth --from-literal=token="$VLITE_AUTH_TOKEN"
helm upgrade --install vectors deploy/helm/vlite \
  --set image.repository=registry.example/vlite --set image.tag=0.6.0 \
  --set auth.existingSecret=vlite-auth --set database.dimensions=3
kubectl port-forward service/vectors-vlite 8080:8080
```

The token must have 32..256 base64url characters. The existing Secret key defaults
to `token` (`auth.secretKey` changes it). Alternatively pass `auth.token` via a
private values file to create a Secret; beware Helm stores supplied values in
release metadata. No insecure default token is generated. Token files are mounted
read-only (0440, readable via fsGroup); recreate/restart the pod after changing an
existing Secret to reload credentials.

Key values:

- `database.dimensions` / `database.format`: creation only; format 4 by default.
  Existing database headers remain authoritative. Dimensions cannot be changed
  by updating chart values.
- `persistence.size`, `persistence.storageClass`, `persistence.accessModes`: PVC
  creation settings; null storage class selects the cluster default, `""` disables
  dynamic provisioning. Existing claims require cluster-specific expansion.
- `resources`: defaults 100m/256Mi requested and 1 CPU/1Gi limited. Size for your
  entire in-memory dataset, ANN index and restart/rebuild peaks.
- `service.type` / `service.port`: ClusterIP/8080 by default. There is no ingress
  or public LoadBalancer by default. Use TLS beyond trusted loopback access.
- `tls.existingSecret`: optional TLS Secret mounted at `/run/secrets/tls`. This
  is **only a mount**: this chart does not enable native REST TLS flags.
  Configure an external ingress/reverse proxy for REST HTTPS; this chart does not
  deploy one and does not expose a gRPC listener.
- `podSecurityContext` / `securityContext`: non-root UID/GID 10001, fsGroup,
  read-only root filesystem, dropped capabilities, no escalation, RuntimeDefault
  seccomp. Storage must support writable fsGroup ownership at `/data`.
- Startup/liveness/readiness use the existing unauthenticated `/health` endpoint.
  Startup allows ten minutes for replay; increase it for large databases. Health
  indicates process/database health, not ANN readiness or query quality.

Do not scale above one replica or mount this file in another database process.
Updates deliberately stop the single owner before starting its replacement.

Validation (Helm 3):

```sh
helm lint deploy/helm/vlite --set auth.existingSecret=vlite-auth
helm template vectors deploy/helm/vlite --set auth.existingSecret=vlite-auth
helm template vectors deploy/helm/vlite --set auth.token=abcdefghijklmnopqrstuvwxyz012345 --set tls.existingSecret=vlite-tls
```
