# Lore Helm chart

Kubernetes Helm chart for the [Lore](https://github.com/EpicGames/lore) revision control server from Epic Games.

This chart deploys the lore server with a default deployment that consists of one durable **data** pod plus a two pod caching **handler** tier, on three 500Gi PVCs, listening behind a `NodePort` service. This chart attempts to be very compatible in its defaults for an easy first time deployment. An even simpler deployment is possible by setting `topology.mode: single`, which enables a small one-pod install for testing.

Before running Lore in production, you must consider your performance requirements and tune the values this chart uses appropriately. Epic highly recommends that you use direct-attached NVMe drives as seen in the [NVMe example](examples/local-nvme.values.yaml).

## Install

From this directory:

```sh
helm install lore .
```

The chart defaults to `ghcr.io/epicgames/lore/loreserver` at the chart's `appVersion`, which is public — nothing to configure and no pull secret. That tag is `linux/amd64` only and runs under emulation on arm64. On Graviton3 or newer, use the native arm64 build instead:

```sh
helm install lore . --set image.tag=0.9.0-graviton
```

The `-graviton` tag is tuned for Graviton3+ and faults with `SIGILL` on older arm64 CPUs, Apple Silicon and Ampere included.

Set `image.repository` to use your own registry, `image.tag` to pin a version (empty means the chart's `appVersion`), or `image.digest` to pin by content. A bare string — `--set image=repo:tag` — still works for compatibility, but the fields are what `--set image.tag=` and image-updater tooling expect.

## Connect

The client data path is QUIC over **UDP 41337**. It needs a NodePort or a UDP-capable LoadBalancer — it does not work through an Ingress or `kubectl port-forward` (both are HTTP/TCP only).

Ports and protocols the server exposes (open the client ports 41337/41339 on your firewall / load balancer):

| Port | Protocol | Purpose |
|------|----------|---------|
| 41337 | UDP | QUIC — clone / push / pull (the client data path) |
| 41337 | TCP | gRPC — admin and revision API (`repository`, `branch`, …) |
| 41339 | TCP | HTTP — `/health_check` and presigned URLs |
| 41340 | UDP + TCP | Internal replication (quic_internal + replication), peer-to-peer. Opt-in via `replication.enabled`; not a client port. See [Cross-cluster replication](#cross-cluster-replication). |

QUIC and gRPC share port number 41337 on different protocols, so a LoadBalancer must serve **both TCP and UDP** on it (e.g. an AWS NLB with `aws-load-balancer-enable-tcp-udp-listener: "true"`). A TCP-only load balancer breaks clone/push.

Get the address (NodePort default). `service.externalTrafficPolicy` defaults to
`Local`, so only the node running a serving pod accepts traffic on this port. Find
that node from the Service's EndpointSlice:

```sh
NAMESPACE=default     # the namespace you installed into
# The resource name, not the release name. `helm install myrel .` -> "myrel-lore".
SVC=lore

# Wait for the pod, or there is no endpoint to look up yet.
kubectl rollout status statefulset/"$SVC" -n "$NAMESPACE" --timeout=5m

NODE_NAME=$(kubectl get endpointslice -n "$NAMESPACE" -l kubernetes.io/service-name="$SVC" -o jsonpath='{.items[0].endpoints[0].nodeName}')
NODE_IP=$(kubectl get node "$NODE_NAME" -o jsonpath='{.status.addresses[?(@.type=="ExternalIP")].address}')
[ -z "$NODE_IP" ] && NODE_IP=$(kubectl get node "$NODE_NAME" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
QUIC_PORT=$(kubectl get svc "$SVC" -n "$NAMESPACE" -o jsonpath='{.spec.ports[?(@.name=="quic")].nodePort}')
echo "lore://$NODE_IP:$QUIC_PORT"   # open that UDP port on the node's firewall
```

`helm install` prints this command with your own release's names filled in.

Set `service.externalTrafficPolicy=Cluster` to accept traffic on any node. That
adds a hop and loses the client's real source IP.

For a stable external address instead, install with `--set service.type=LoadBalancer` on your `helm` command.


## Getting Started with Lore

Create and clone a repo (auth is off by default):

```sh
lore repository create lore://<SERVER>/myrepo
lore clone lore://<SERVER>/myrepo
```

Use the `lore://` scheme — the default certificate is self-signed, so verification is skipped.

## Verified TLS (`lores://`)

QUIC always encrypts, so `lore://` is not plaintext — it just skips certificate
verification. `lores://` additionally verifies the server certificate.

`lores://` talks `https://` to the **gRPC** listener on TCP 41337. Setting `tls.mode`
alone only puts a certificate on the QUIC listener, so `lores://` fails with a
transport error. Verified TLS needs `tls.grpcTls=true`:

```yaml
tls:
  mode: certManager               # requires cert-manager in the cluster
  grpcTls: true
  issuerName: my-real-ca-issuer   # an Issuer backed by a CA your clients trust
  extraSANs:
    - lore.example.com            # the address clients actually dial
```

The gRPC listener is **either** plaintext **or** TLS, so this is a hard switch:

| `tls.grpcTls` | Client scheme | Notes |
|---------------|---------------|-------|
| `false` (default) | `lore://` | gRPC is plaintext h2c. QUIC still encrypted; cert not verified. `lores://` will not connect. |
| `true` | `lores://` | gRPC is TLS. `lore://` stops working. Clients must trust the cert. |

Two things to get right:

- **`tls.grpcTls=true` needs a mode other than `ephemeral`.** The server makes the
  ephemeral cert itself, so the chart cannot point gRPC at it. The install fails
  rather than ignoring the setting.
- **Clients must trust the certificate.** `tls.mode=selfSigned` produces an
  untrusted cert, so `lores://` fails unless every client installs it as a trusted
  root. Use `tls.mode=certManager` with a real CA `issuerName`, or
  `tls.mode=secret` with a publicly-trusted cert.
- **The cert must cover the dialed address.** Put your external DNS name or IP in
  `tls.extraSANs`; the chart already includes the in-cluster Service and pod names.

In tiered mode the handlers dial the data pod over `lores://` too. The chart handles
this for you: when `tls.grpcTls=true` it runs an init container that appends the
chart-managed certificate to the image's CA bundle and points `SSL_CERT_FILE` at the
result, so the handler can verify the data pod even with a self-signed cert. Without
that the handler fails with `InvalidCertificate(UnknownIssuer)` and reads through a
handler return `Not found`.

## Common settings

Override with `--set key=value`. See `values.yaml` for the full, commented list and `examples/` for ready-made sample values.yaml files.

| Key | Default | What it does |
|-----|---------|--------------|
| `image.repository` / `.tag` / `.digest` | `ghcr.io/epicgames/lore/loreserver` / `appVersion` / — | The loreserver image. Public; `linux/amd64`. Use the `-graviton` tag on Graviton3+. |
| `fullnameOverride` | `""` | Pin the names of the created objects, including the Service — i.e. the in-cluster DNS name clients dial. Otherwise derived from the release name. |
| `priorityClassName` | `""` | PriorityClass for the pods. Set it where the server shares nodes and should win or lose preemption. |
| `terminationGracePeriodSeconds` | `45` | SIGTERM-to-SIGKILL window. Must cover the preStop hook plus shutdown. |
| `lifecycle` | preStop `sleep 5` | Container lifecycle hooks. The default delay lets endpoint removal land before shutdown starts; `null` removes it (`{}` does not — Helm coalesces it against the default). |
| `topology.mode` | `tiered` | `tiered` (data + handler cache) or `single` (one self-contained pod). |
| `handler.replicas` | `2` | Number of cache pods. |
| `service.type` | `NodePort` | `NodePort` \| `LoadBalancer` \| `ClusterIP`. |
| `tls.mode` | `ephemeral` | `ephemeral` (self-signed, zero setup) \| `selfSigned` \| `certManager` \| `secret`. `certManager` requires [cert-manager](https://cert-manager.io) installed in the cluster; the other modes do not. |
| `data.persistence.size` / `.storageClass` | `500Gi` / cluster default | Durable store size and class. Use fast disk (NVMe / provisioned-IOPS) for real workloads. |
| `handler.persistence.size` / `.storageClass` / `.type` | `500Gi` / default / `pvc` | Cache size and class; `type: hostPath` puts the cache on node-local NVMe. |
| `data.store.maxSize` / `handler.store.maxSize` | `""` (off) | Size cap that lets the store reclaim space. Off by default, and off means unbounded — see [Bounding the store](#bounding-the-store). |
| `auth.enabled` | `false` | JWT auth. Off = open server. |

### Storage

Each tier's `/data` is backed by one of three `persistence.type` values:

- **`pvc`** (default) — a PersistentVolumeClaim. The only option that follows the pod to another node, so it is the right choice for the authoritative data tier.
- **`hostPath`** — a directory on the node's own disk. Fast, but it does not follow the pod, and it needs the node to actually have that path. See the caveat below.
- **`emptyDir`** — node-local scratch that lives and dies with the pod.

**Getting local NVMe.** There are two shapes, and which one you have is decided by the cluster, not by this chart:

- If the node exposes its NVMe as a **mounted directory**, use `hostPath` with `persistence.hostPath.path` pointing at it, and pin the pods there with `nodeSelector`. This is what [`examples/local-nvme.values.yaml`](examples/local-nvme.values.yaml) shows.
- If the node's NVMe already **backs kubelet's ephemeral storage** — which is what Karpenter's `instanceStorePolicy: RAID0`, EKS-managed instance-store setups and Bottlerocket nodes generally do — then `emptyDir` lands on the NVMe with no host path involved and no extra configuration. Set `resources.requests/limits` for `ephemeral-storage` to cover `persistence.emptyDir.sizeLimit`, keeping the size limit a little under the resource limit, since that limit also counts container logs and the writable layer.

**`hostPath` needs a permissive namespace.** The kubelet creates a `DirectoryOrCreate` path as `root:root`, and `fsGroup` does not apply to `hostPath`, so the chart runs a small init container as root (adding `CHOWN`, `DAC_OVERRIDE`, `FOWNER`) to fix ownership for the non-root server. Pod Security `restricted` requires dropping all capabilities and permits adding only `NET_BIND_SERVICE`, so it rejects that init container: `hostPath` only works in a namespace at `baseline` or `privileged`.

Under `restricted`, use `emptyDir` on a node whose ephemeral storage is NVMe-backed (above), or have your node provisioning pre-create a directory per pod with the right ownership so no chown is needed. The chart cannot solve this itself — changing the owner of a root-owned directory requires root.

### Bounding the store

**The store does not reclaim space unless you tell it to.** The server reads an absent cap as `0`, and `0` disables the matching background task outright, so a stock install grows until the volume can no longer hold it. How that ends depends on the volume: a PVC gives ENOSPC, while an `emptyDir` with a `sizeLimit` gets the pod evicted and the volume deleted — losing the whole store in one step rather than shedding the coldest part of it.

Two caps, set per tier under `data.store` and `handler.store`:

| Key | What it bounds |
|-----|----------------|
| `maxSize` | Total payload bytes. Above it, the compactor drops least-recently-accessed payloads until the store is back under 70% of the cap, then reclaims the pack files they occupied. Takes a Kubernetes quantity (`200Gi`), the same units as `persistence.size` and `emptyDir.sizeLimit`, so the two can be read against each other. **This is the knob that bounds disk.** |
| `maxCapacity` | Number of stored fragments, via a separate evictor that also drops oldest-accessed first. Floored by the server at 1048576, so smaller values do nothing. Usually redundant once `maxSize` is set. |

Set `compactionDelaySeconds` whenever you set `maxSize`. The server's own default is 86400 — one pass a day — which is far too slow for a store that fills in hours. `evictionDelaySeconds` (default 10) governs the `maxCapacity` pass and rarely needs changing.

```yaml
data:
  store:
    maxSize: 200Gi
    compactionDelaySeconds: 900
```

**Leaving the caps off is right for the data tier by default.** In `tiered` mode it holds the only authoritative copy, so eviction there is deletion, not cache turnover. Turn it on when the store is deliberately *acting* as a cache — a `single`-mode server in front of disposable content, say a shared build cache — and you would rather lose the coldest content gradually than all of it at once. Be clear about the trade: reading content that has aged out fails, it does not merely run slow.

On `handler`, eviction is safe by construction. A handler's `/data` is a cache in front of the durable store on the data pod, so an evicted fragment is re-fetched rather than lost.

## Cross-cluster replication

`replication.enabled=true` turns on the server's two internal replication endpoints on
port 41340 — `quic_internal` (UDP) and `replication` (gRPC/TCP) — on the data pod only.
Both require mutual TLS, so you must supply a Secret holding the server cert, key, and
CA chain (`replication.tls.secretName`).

By default those ports are published only on the headless Service, which has **no
external address** — so they reach in-cluster peers and nothing else. For peers in
another cluster, enable the dedicated replication Service:

```yaml
replication:
  enabled: true
  tls:
    secretName: lore-replication-tls
  service:
    enabled: true
    type: LoadBalancer
    loadBalancerClass: service.k8s.aws/nlb   # must carry TCP + UDP on 41340
    loadBalancerSourceRanges: ["10.0.0.0/8"] # restrict to your peer networks
  topology:
    provider: fixed
    fixed:
      peers:
        - address: lore-repl.dc2.example
          port: 41340
          locality: OtherRegion
```

That renders `<release>-replication`, a Service selecting the data pod and publishing
41340 on both protocols. Then point each cluster's `replication.topology.fixed.peers`
at the other clusters' replication addresses.

This endpoint is an internal trust boundary: it grants blanket access to every storage
partition. Keep `verifyClientCerts: true`, and restrict reachability with
`loadBalancerSourceRanges` or an internal-only LB annotation. Leaving
`replication.service.enabled=false` means cross-cluster replication needs plumbing you
provide yourself (peering, a manually managed Service, a gateway).

See [examples/replication.values.yaml](examples/replication.values.yaml).

## Notes

- **Not highly available while configured to use local disk.** The single data instance is the sole authoritative store; while it is down, writes and `repository list` fail. For redundant storage, please use the [AWS S3 + DynamoDB backend](examples/aws-s3-dynamodb.values.yaml).
- **Fast disk matters.** Lore is IO-heavy — prefer NVMe or provisioned-IOPS storage classes.

## Uninstall

```sh
helm uninstall lore   # PVCs are retained; remove with: kubectl delete pvc -l app.kubernetes.io/instance=lore
```
