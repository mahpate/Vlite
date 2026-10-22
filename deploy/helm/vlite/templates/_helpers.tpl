{{- define "vlite.name" -}}
{{- printf "%s-vlite" .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- define "vlite.labels" -}}
app.kubernetes.io/name: vlite
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}
{{- define "vlite.secret" -}}
{{- default (include "vlite.name" .) .Values.auth.existingSecret -}}
{{- end -}}
