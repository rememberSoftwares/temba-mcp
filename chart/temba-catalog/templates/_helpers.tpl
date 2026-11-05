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
{{- $names := dict -}}
{{- range $index, $server := .Values.catalog.servers -}}
  {{- if not (kindIs "map" $server) -}}
    {{- fail (printf "catalog.servers[%d] must be an object" $index) -}}
  {{- end -}}
  {{- if hasKey $server "id" -}}
    {{- fail (printf "catalog.servers[%d].id is no longer supported; replace id with name" $index) -}}
  {{- end -}}
  {{- $name := $server.name -}}
  {{- if not (kindIs "string" $name) -}}
    {{- fail (printf "catalog.servers[%d].name must be a nonempty string" $index) -}}
  {{- end -}}
  {{- if or (not (regexMatch "^[A-Za-z0-9_-]{1,64}$" $name)) (hasPrefix "__" $name) -}}
    {{- fail (printf "catalog.servers[%d].name is invalid: use 1-64 letters, digits, hyphens or underscores, without the reserved __ prefix" $index) -}}
  {{- end -}}
  {{- if hasKey $names $name -}}
    {{- fail (printf "catalog.servers[%d].name: duplicate MCP name %q; already used by catalog.servers[%v].name. Each name must be unique" $index $name (get $names $name)) -}}
  {{- end -}}
  {{- $_ := set $names $name $index -}}
{{- end -}}
{{- end -}}
