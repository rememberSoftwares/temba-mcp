{{- define "dynmcp.fullname" -}}
{{- default .Release.Name .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "dynmcp.labels" -}}
app.kubernetes.io/name: dynmcp
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Secret holding ADMIN_API_TOKEN: user-provided (adminApi.secretName)
     or chart-created <fullname>-admin when empty. */}}
{{- define "dynmcp.adminSecretName" -}}
{{- .Values.adminApi.secretName | default (printf "%s-admin" (include "dynmcp.fullname" .)) -}}
{{- end -}}

{{/* Key of ADMIN_API_TOKEN inside that Secret (default ADMIN_API_TOKEN). */}}
{{- define "dynmcp.adminTokenKey" -}}
{{- default "ADMIN_API_TOKEN" .Values.adminApi.tokenKey -}}
{{- end -}}

{{/* Public URL: derived from the Ingress when not set explicitly.
     Falls back to localhost when the ingress host is empty (dev port-forward). */}}
{{- define "dynmcp.publicBaseUrl" -}}
{{- if .Values.publicBaseUrl -}}
{{ .Values.publicBaseUrl }}
{{- else if and .Values.ingress.enabled .Values.ingress.host -}}
{{- $scheme := "http" -}}
{{- if .Values.ingress.tls -}}{{- $scheme = "https" -}}{{- end -}}
{{- printf "%s://%s" $scheme .Values.ingress.host -}}
{{- else -}}
{{- printf "http://localhost:8080" -}}
{{- end -}}
{{- end -}}
