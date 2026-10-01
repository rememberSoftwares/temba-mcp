#!/usr/bin/env bash
# Déploiement minikube du gateway dynmcp.
#
# Usage :
#   ./k8s.sh apply          # régénère la ConfigMap code + secrets manquants + apply
#   ./k8s.sh delete         # supprime tout (garder les secrets)
#   ./k8s.sh delete --purge # supprime tout y compris les secrets
#   ./k8s.sh secrets        # (re)crée les secrets depuis les fichiers locaux
#   ./k8s.sh logs           # suit les logs du gateway
#   ./k8s.sh status         # pods + services + page /auth
set -euo pipefail
cd "$(dirname "$0")"

JIRA_ENV="${JIRA_ENV:-/home/elancelot/Documents/rfc/jira.env}"
OPENCODE_CONF="${OPENCODE_CONF:-$HOME/.config/opencode/opencode.json}"

need_kubectl() { command -v kubectl >/dev/null || { echo "kubectl introuvable" >&2; exit 1; }; }

secrets() {
  need_kubectl
  # Token Grafana : extrait de la conf opencode (sans l'afficher)
  local gtoken
  gtoken=$(python3 -c "import json,sys;print(json.load(open('$OPENCODE_CONF'))['mcp']['grafana']['environment']['GRAFANA_SERVICE_ACCOUNT_TOKEN'])")
  # GRAFANA_MCP_TOKEN : garde la valeur existante si le secret existe déjà
  local gmcp
  gmcp=$(kubectl get secret dynmcp-secrets -o jsonpath='{.data.GRAFANA_MCP_TOKEN}' 2>/dev/null | base64 -d || true)
  [ -z "$gmcp" ] && gmcp=$(openssl rand -hex 24)
  kubectl create secret generic dynmcp-secrets \
    --from-literal=GRAFANA_SERVICE_ACCOUNT_TOKEN="$gtoken" \
    --from-literal=GRAFANA_MCP_TOKEN="$gmcp" \
    --dry-run=client -o yaml | kubectl apply -f -

  if [ -f "$JIRA_ENV" ]; then
    kubectl create secret generic mcp-atlassian-env \
      --from-env-file="$JIRA_ENV" \
      --dry-run=client -o yaml | kubectl apply -f -
  else
    echo "⚠  $JIRA_ENV absent — secret mcp-atlassian-env non (re)créé"
  fi
  echo "secrets OK"
}

apply() {
  need_kubectl
  # La ConfigMap code est régénérée depuis src/ (source de vérité)
  kubectl create configmap dynmcp-code --from-file=src/server.py --dry-run=client -o yaml > k8s/configmap-code.yaml
  kubectl get secret dynmcp-secrets >/dev/null 2>&1 || secrets
  kubectl get secret mcp-atlassian-env >/dev/null 2>&1 || secrets
  kubectl apply -k k8s
  kubectl rollout restart deploy/dynmcp
  kubectl rollout status deploy/dynmcp --timeout=300s
  echo
  echo "Gateway : kubectl port-forward svc/dynmcp 18080:8080  →  http://localhost:18080/mcp (page auth: /auth)"
}

delete() {
  need_kubectl
  kubectl delete -k k8s --ignore-not-found
  if [ "${1:-}" = "--purge" ]; then
    kubectl delete secret dynmcp-secrets mcp-atlassian-env --ignore-not-found
    echo "secrets supprimés"
  fi
}

case "${1:-}" in
  apply)   apply ;;
  secrets) secrets ;;
  delete)  shift; delete "${1:-}" ;;
  logs)    kubectl logs -f deploy/dynmcp ;;
  status)  need_kubectl; kubectl get pods,svc -l 'app in (dynmcp,mcp-atlassian,mcp-grafana)' -L app; echo; curl -s -m 5 http://localhost:18080/auth | sed 's/<[^>]*>/ /g' || true ;;
  *) grep '^#' "$0" | sed 's/^# \{0,1\}//' ;;
esac
