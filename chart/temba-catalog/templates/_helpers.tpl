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
