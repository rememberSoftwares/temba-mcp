{{- define "temba.name" -}}{{ .Release.Name }}{{- end -}}
{{- define "temba.selector" -}}
app.kubernetes.io/name: temba-mcp
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
{{- define "temba.labels" -}}
{{ include "temba.selector" . }}
app.kubernetes.io/component: catalog
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "temba.validateCatalog" -}}
{{- $ids := dict -}}
{{- range $index, $server := .Values.catalog.servers -}}
  {{- if not (kindIs "map" $server) -}}
    {{- fail (printf "catalog.servers[%d] must be an object" $index) -}}
  {{- end -}}
  {{- if hasKey $server "name" -}}
    {{- fail (printf "catalog.servers[%d].name is no longer supported; keep only id" $index) -}}
  {{- end -}}
  {{- $id := $server.id -}}
  {{- if not (kindIs "string" $id) -}}
    {{- fail (printf "catalog.servers[%d].id must be a nonempty string" $index) -}}
  {{- end -}}
  {{- if or (not (regexMatch "^[A-Za-z0-9_-]{1,64}$" $id)) (hasPrefix "__" $id) -}}
    {{- fail (printf "catalog.servers[%d].id is invalid: use 1-64 letters, digits, hyphens or underscores, without the reserved __ prefix" $index) -}}
  {{- end -}}
  {{- if hasKey $ids $id -}}
    {{- fail (printf "Duplicate MCP id %q in catalog.servers; each id must be unique" $id) -}}
  {{- end -}}
  {{- $_ := set $ids $id true -}}
{{- end -}}
{{- end -}}
