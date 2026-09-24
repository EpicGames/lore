{{/*
Expand the name of the chart.
*/}}
{{- define "lore.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
We truncate at 63 chars because some Kubernetes name fields are limited to this (by the DNS naming spec).
If release name contains chart name it will be used as a full name.
*/}}
{{- define "lore.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
The container image reference, from either the map form (repository + tag/digest)
or the legacy bare string. digest wins over tag; an empty tag falls back to
Chart.appVersion, so chart and image cannot drift apart by omission.

Every template that names the image must go through this, or the two forms
disagree depending on which one you read.
*/}}
{{- define "lore.image" -}}
{{- if kindIs "map" .Values.image -}}
{{- $repo := .Values.image.repository | required "image.repository is required when image is a map" -}}
{{- if .Values.image.digest -}}
{{- printf "%s@%s" $repo .Values.image.digest -}}
{{- else -}}
{{- printf "%s:%s" $repo (.Values.image.tag | default .Chart.AppVersion | toString) -}}
{{- end -}}
{{- else -}}
{{- .Values.image | required "image is required: set image.repository (and optionally image.tag or image.digest)" -}}
{{- end -}}
{{- end -}}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "lore.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "lore.labels" -}}
helm.sh/chart: {{ include "lore.chart" . }}
{{ include "lore.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "lore.selectorLabels" -}}
app.kubernetes.io/name: {{ include "lore.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Handler StatefulSet fully qualified name (the A/B caching tier).
*/}}
{{- define "lore.handlerFullname" -}}
{{- printf "%s-handler" (include "lore.fullname" .) | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Data selector labels: base selector labels plus the data component.
Used by the data StatefulSet (Pod C) and the public/headless Services in tiered mode.
*/}}
{{- define "lore.dataSelectorLabels" -}}
{{ include "lore.selectorLabels" . }}
app.kubernetes.io/component: data
{{- end }}

{{/*
Handler selector labels: base selector labels plus the handler component.
Used by the handler StatefulSet (Pods A/B), its headless Service, and the handler PDB.
*/}}
{{- define "lore.handlerSelectorLabels" -}}
{{ include "lore.selectorLabels" . }}
app.kubernetes.io/component: handler
{{- end }}

{{/*
Effective storage type for a tier's persistence dict.
Honors the legacy `enabled: false` (data tier) as emptyDir; otherwise `type`
(default pvc). Call with the persistence dict, e.g. (include "lore.storageType" .Values.data.persistence).
*/}}
{{- define "lore.storageType" -}}
{{- if and (hasKey . "enabled") (not .enabled) -}}
emptyDir
{{- else -}}
{{- .type | default "pvc" -}}
{{- end -}}
{{- end -}}

{{/*
Pod-spec volume for the /data mount when the tier is NOT pvc-backed.
Renders nothing for pvc (a volumeClaimTemplate supplies the volume instead).
Call with (dict "p" <persistence> "name" "data|cache").
*/}}
{{- define "lore.storagePodVolume" -}}
{{- $p := .p -}}
{{- $t := include "lore.storageType" $p -}}
{{- if eq $t "hostPath" }}
- name: {{ .name }}
  hostPath:
    path: {{ (($p.hostPath).path) | required (printf "%s.persistence.hostPath.path is required when type=hostPath" (ternary "handler" "data" (eq .name "cache"))) }}
    type: {{ ($p.hostPath).type | default "DirectoryOrCreate" }}
{{- else if eq $t "emptyDir" }}
{{- $ed := $p.emptyDir | default dict }}
- name: {{ .name }}
  emptyDir:
    {{- if $ed.medium }}
    medium: {{ $ed.medium }}
    {{- end }}
    {{- if $ed.sizeLimit }}
    sizeLimit: {{ $ed.sizeLimit }}
    {{- end }}
    {{- if and (not $ed.medium) (not $ed.sizeLimit) }} {}
    {{- end }}
{{- end }}
{{- end -}}

{{/*
volumeClaimTemplates block for the /data mount when the tier IS pvc-backed.
Renders nothing for hostPath/emptyDir. Call with (dict "p" <persistence> "name" "data|cache").
*/}}
{{- define "lore.storageClaimTemplate" -}}
{{- $p := .p -}}
{{- if eq (include "lore.storageType" $p) "pvc" }}
volumeClaimTemplates:
  - metadata:
      name: {{ .name }}
    spec:
      accessModes:
        - {{ $p.accessMode | default "ReadWriteOnce" }}
      resources:
        requests:
          storage: {{ $p.size | default "10Gi" }}
      {{- if $p.storageClass }}
      storageClassName: {{ $p.storageClass }}
      {{- end }}
{{- end }}
{{- end -}}

{{/*
Init container that fixes ownership of a hostPath /data mount. The kubelet creates
a hostPath (DirectoryOrCreate) dir as root:root, but the server runs as non-root and
fsGroup does not apply to hostPath — so without this the non-root process cannot
write its store. Renders only when the tier's type is hostPath.
Call with (dict "ctx" . "p" <persistence> "name" "data|cache").
*/}}
{{- define "lore.hostPathInit" -}}
{{- $ctx := .ctx -}}
{{- if eq (include "lore.storageType" .p) "hostPath" }}
  - name: init-hostpath-perms
    image: {{ include "lore.image" $ctx | quote }}
    imagePullPolicy: {{ $ctx.Values.imagePullPolicy }}
    # Create this pod's own subdirectory under the shared host path and fix its
    # ownership. StatefulSet replicas can share a node, and a hostPath is the same
    # directory for every pod on that node — so each pod gets its own subdir
    # (matched by subPathExpr $(POD_NAME) on the main container) to avoid two
    # servers colliding on one local store.
    command:
      - sh
      - -c
      - 'mkdir -p "/data/${POD_NAME}" && chown {{ $ctx.Values.podSecurityContext.runAsUser }}:{{ $ctx.Values.podSecurityContext.fsGroup }} "/data/${POD_NAME}"'
    env:
      - name: POD_NAME
        valueFrom:
          fieldRef:
            fieldPath: metadata.name
    securityContext:
      runAsUser: 0
      runAsNonRoot: false
      allowPrivilegeEscalation: false
      readOnlyRootFilesystem: true
      capabilities:
        # runs as root only to prepare the pod's subdir; DAC_OVERRIDE/FOWNER let it
        # mkdir + chown regardless of the shared host dir's existing ownership.
        drop: ["ALL"]
        add: ["CHOWN", "DAC_OVERRIDE", "FOWNER"]
    volumeMounts:
      - name: {{ .name }}
        mountPath: /data
{{- end }}
{{- end -}}

{{/*
True when gRPC really is served over TLS: tls.grpcTls is on and the mode gives the
chart a cert to point at. Ephemeral does not, so configmap.yaml rejects it.

This is the one place that answers "does lores:// work". Use it instead of writing
the same check again, or the callers drift apart.
*/}}
{{- define "lore.grpcTlsActive" -}}
{{- if and .Values.tls.grpcTls (ne .Values.tls.mode "ephemeral") -}}true{{- end -}}
{{- end -}}

{{/*
True when the pods need the chart-managed certificate added to their own trust
store: tls.grpcTls makes the data pod's gRPC listener TLS-only, so a handler dials
it over lores:// and must verify that cert. A chart-managed cert is not in the
image's CA bundle, so without this the handler fails with
InvalidCertificate(UnknownIssuer) and every read through a handler returns "Not found".
*/}}
{{- define "lore.needsCaBundle" -}}
{{- include "lore.grpcTlsActive" . -}}
{{- end -}}

{{/*
Init container that builds a CA bundle of the image's public roots PLUS the
chart-managed cert, into the ca-bundle emptyDir. SSL_CERT_FILE then points at it.
Concatenating (rather than pointing SSL_CERT_FILE straight at the chart cert) keeps
public roots working for OTLP/JWKS endpoints. Call with the root context.
*/}}
{{- define "lore.caBundleInit" -}}
{{- if include "lore.needsCaBundle" . }}
  - name: init-ca-bundle
    image: {{ include "lore.image" . | quote }}
    imagePullPolicy: {{ .Values.imagePullPolicy }}
    command:
      - sh
      - -c
      - 'cat /etc/ssl/certs/ca-certificates.crt "/etc/lore/certs/{{ .Values.tls.certKey }}" > /etc/lore/ca/bundle.crt'
    securityContext:
      {{- toYaml .Values.securityContext | nindent 6 }}
    volumeMounts:
      - name: certs
        mountPath: /etc/lore/certs
        readOnly: true
      - name: ca-bundle
        mountPath: /etc/lore/ca
{{- end }}
{{- end -}}

{{/*
The pod's initContainers key, emitted once with whichever init containers apply.
Two separate helpers each emitting `initContainers:` would produce a duplicate YAML
key and silently drop one. Call with (dict "ctx" . "p" <persistence> "name" "data|cache").
*/}}
{{- define "lore.initContainers" -}}
{{- $hostPath := include "lore.hostPathInit" . -}}
{{- $caBundle := include "lore.caBundleInit" .ctx -}}
{{- if or (trim $hostPath) (trim $caBundle) }}
initContainers:
{{- $caBundle }}
{{- $hostPath }}
{{- end }}
{{- end -}}

{{/*
Create the name of the service account to use
*/}}
{{- define "lore.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "lore.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
TLS secret name.
Returns existingSecret when mode=secret, otherwise returns <fullname>-tls.
*/}}
{{- define "lore.tlsSecretName" -}}
{{- if and (eq .Values.tls.mode "secret") .Values.tls.existingSecret }}
{{- .Values.tls.existingSecret }}
{{- else }}
{{- printf "%s-tls" (include "lore.fullname" .) }}
{{- end }}
{{- end }}

{{/*
Convert a Kubernetes quantity ("200Gi", "500M", "1024") to a byte count. The
server's TOML wants bytes, but every other size in this chart is a k8s quantity,
and max_size only makes sense read against the volume it must stay inside.

Fractions are rejected, not rounded: atoi would truncate "1.5Gi" to 1 byte.
*/}}
{{- define "lore.toBytes" -}}
{{- $q := . | toString | trim -}}
{{- $suffix := regexFind "[KMGTP]i?$" $q -}}
{{- $digits := trimSuffix $suffix $q -}}
{{- if not (regexMatch "^[0-9]+$" $digits) -}}
{{- fail (printf "cannot read %q as a size: use a whole number of bytes with an optional Ki/Mi/Gi/Ti suffix (write 1536Mi rather than 1.5Gi)" $q) -}}
{{- end -}}
{{- $factors := dict "" 1 "K" 1000 "M" 1000000 "G" 1000000000 "T" 1000000000000 "P" 1000000000000000 "Ki" 1024 "Mi" 1048576 "Gi" 1073741824 "Ti" 1099511627776 "Pi" 1125899906842624 -}}
{{- mul (atoi $digits) (index $factors $suffix) -}}
{{- end -}}

{{/*
Garbage-collection keys for a local immutable store, as bare TOML keys. Call with
a store map (.Values.data.store or .Values.handler.store).

These MUST render directly under the [immutable_store...] header they configure
and before any other table opens — see the TOML ORDERING INVARIANT on
lore.serverHttpToml.

Emitting nothing is meaningful: the server reads an absent cap as 0, and 0
disables that background task outright (lore-storage maintenance.rs `spawn_gc`),
so a store with neither cap set grows without bound.
*/}}
{{- define "lore.storeGcToml" -}}
{{- $lines := list -}}
{{- with .maxSize }}{{ $lines = append $lines (printf "max_size = %s" (include "lore.toBytes" .)) }}{{ end -}}
{{- with .maxCapacity }}{{ $lines = append $lines (printf "max_capacity = %d" (int64 .)) }}{{ end -}}
{{- with .compactionDelaySeconds }}{{ $lines = append $lines (printf "compaction_delay = %d" (int64 (mul . 1000))) }}{{ end -}}
{{- with .evictionDelaySeconds }}{{ $lines = append $lines (printf "eviction_delay = %d" (int64 (mul . 1000))) }}{{ end -}}
{{- join "\n" $lines -}}
{{- end -}}

{{/*
The complete [server.http] table: its header plus EVERY bare (non-table) key that
belongs to it, emitted as one unit.

TOML ORDERING INVARIANT — a bare `key = value` line belongs to whichever table
header last preceded it. So every block that opens a table of its own
(lore.replicationToml, lore.telemetryToml, config.extra) must be rendered AFTER
this include, and no bare [server.http] key may be written outside of it.
Otherwise the key silently lands in an unrelated table and is ignored.

Call with (dict "ctx" $ "storeHealthCheck" true|false).
*/}}
{{- define "lore.serverHttpToml" -}}
{{- $ctx := .ctx -}}
[server.http]
store_health_check = {{ .storeHealthCheck }}
{{- if $ctx.Values.presignedUrl.enabled }}
presigned_url_min_ttl_seconds = {{ $ctx.Values.presignedUrl.minTtlSeconds }}
presigned_url_default_ttl_seconds = {{ $ctx.Values.presignedUrl.defaultTtlSeconds }}
presigned_url_max_ttl_seconds = {{ $ctx.Values.presignedUrl.maxTtlSeconds }}
{{- end }}
{{- end -}}

{{/*
Telemetry (OTLP) TOML block for the server config. Lore exports logs/metrics/traces
over OTLP/gRPC — there is no Prometheus scrape endpoint — so this configures an OTLP
exporter. Callers gate this on .Values.telemetry.exporter.endpoint (setting the
endpoint is what actually enables export). Call with the root context.
*/}}
{{- define "lore.telemetryToml" -}}
{{- $t := .Values.telemetry -}}
[telemetry.exporter]
endpoint = {{ $t.exporter.endpoint | quote }}
queue_size = {{ $t.exporter.queueSize }}
timeout = {{ $t.exporter.timeoutMs }}

[telemetry.logger]
enable_otlp = {{ $t.logger.enableOtlp }}
format = {{ $t.logger.format | quote }}
output = {{ $t.logger.output | quote }}

[telemetry.metrics]
export_interval_millis = {{ $t.metrics.exportIntervalMillis }}
sample_interval_millis = {{ $t.metrics.sampleIntervalMillis }}

[telemetry.traces]
sample_rate = {{ $t.traces.sampleRate }}
sample_rate_low_tier = {{ $t.traces.sampleRateLowTier }}
{{- with $t.traces.serviceName }}
service_name = {{ . | quote }}
{{- end }}
{{- with $t.additionalLabels }}

[telemetry.additional_labels]
{{- range $k, $v := . }}
{{ $k }} = {{ $v | quote }}
{{- end }}
{{- end }}
{{- end -}}

{{/*
Internal replication TOML: the quic_internal (UDP) + grpc_internal (TCP) endpoints
on port 41340 for server-to-server (cross-cluster) replication, plus topology peers.
Both endpoints require mutual TLS. Callers gate this on .Values.replication.enabled.
Only rendered for the DATA tier (handlers are caches and do not replicate). Root ctx.

TABLE NAMES ARE VERSION-DEPENDENT. loreserver v0.8.4 renamed the gRPC half of this
endpoint from [server.replication] to [server.grpc_internal] (default.toml:61). The
QUIC half, [server.quic_internal], is unchanged.

  <= v0.8.3   [server.replication]
  >= v0.8.4   [server.grpc_internal]   <- what this chart emits

The chart targets v0.8.4+ only. This matters because #[serde(deny_unknown_fields)]
is off in the server: on an older server the table name is silently discarded, so
replication never starts, with no error in the log.
Verified by diffing the v0.8.3 and v0.8.6 release tarballs: v0.8.6's binary contains
"grpc_internal" and the warning "[server.grpc_internal] starting WITHOUT mTLS ...",
and no [server.replication] table remains in any of its config files.
*/}}
{{- define "lore.replicationToml" -}}
{{- $r := .Values.replication -}}
[server.quic_internal]
enabled = true
host = "0.0.0.0"
port = {{ $r.port }}
verify_client_certs = {{ $r.verifyClientCerts }}
{{- if $r.tls.secretName }}

[server.quic_internal.certificate]
cert_file = "/etc/lore/replication-tls/{{ $r.tls.certKey }}"
pkey_file = "/etc/lore/replication-tls/{{ $r.tls.keyKey }}"
cert_chain = "/etc/lore/replication-tls/{{ $r.tls.caKey }}"
{{- end }}

[server.grpc_internal]
enabled = true
host = "0.0.0.0"
port = {{ $r.port }}
verify_client_certs = {{ $r.verifyClientCerts }}
{{- if $r.tls.secretName }}

[server.grpc_internal.certificate]
cert_file = "/etc/lore/replication-tls/{{ $r.tls.certKey }}"
pkey_file = "/etc/lore/replication-tls/{{ $r.tls.keyKey }}"
cert_chain = "/etc/lore/replication-tls/{{ $r.tls.caKey }}"
{{- end }}
{{- if ne $r.topology.provider "none" }}

[topology]
provider = {{ $r.topology.provider | quote }}
{{- if eq $r.topology.provider "fixed" }}

[topology.fixed]
peers = [{{ range $i, $p := $r.topology.fixed.peers }}{{ if $i }}, {{ end }}{ address = {{ $p.address | quote }}, port = {{ $p.port | default $r.port }}, locality = {{ $p.locality | default "SameRegion" | quote }} }{{ end }}]
{{- else if eq $r.topology.provider "consul" }}
{{- with $r.topology.consul }}

[plugins.consul]
{{- range $k, $v := . }}
{{ $k }} = {{ $v | quote }}
{{- end }}
{{- end }}
{{- end }}
{{- end }}
{{- end -}}
