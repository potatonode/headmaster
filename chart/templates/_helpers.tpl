{{- define "headmaster.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "headmaster.fullname" -}}
headmaster
{{- end }}

{{- define "headmaster.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "headmaster.labels" -}}
helm.sh/chart: {{ include "headmaster.chart" . }}
app.kubernetes.io/name: {{ include "headmaster.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "headmaster.selectorLabels" -}}
app.kubernetes.io/name: {{ include "headmaster.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}
