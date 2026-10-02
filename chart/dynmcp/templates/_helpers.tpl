{{- define "dynmcp.fullname" -}}
{{- default .Release.Name .Values.fullnameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "dynmcp.labels" -}}
app.kubernetes.io/name: dynmcp
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* URL publique : dérivée de l'Ingress si non fixée explicitement */}}
{{- define "dynmcp.publicBaseUrl" -}}
{{- if .Values.publicBaseUrl -}}
{{ .Values.publicBaseUrl }}
{{- else if .Values.ingress.enabled -}}
{{- $scheme := "http" -}}
{{- if .Values.ingress.tls -}}{{- $scheme = "https" -}}{{- end -}}
{{- printf "%s://%s" $scheme .Values.ingress.host -}}
{{- else -}}
{{- printf "http://localhost:8080" -}}
{{- end -}}
{{- end -}}

{{- define "dynmcp.secretName" -}}
{{- if .Values.secrets.create -}}
{{ .Values.secrets.name }}
{{- else -}}
{{ .Values.adminApi.existingSecret }}
{{- end -}}
{{- end -}}
