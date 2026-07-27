#!/usr/bin/env bash
#
# tf-import.sh — adopt the hand-built media boxes into Terraform state.
#
#   cp terraform.tfvars.example terraform.tfvars   # then paste your token
#   ./tf-import.sh
#
# Both boxes were created by hand and main.tf was written afterwards to describe
# them, so there is no state and Terraform believes nothing exists. This script
# discovers the real resource IDs from the Hetzner API and imports each one.
#
# SAFE BY CONSTRUCTION: `terraform import` only writes local state — it never
# creates, changes or destroys anything at Hetzner. The dangerous command is the
# `apply` afterwards, which is why this script finishes with a plan and refuses to
# pretend a dirty plan is fine.
#
# Re-runnable: resources already in state are skipped.
#
# Needs: terraform, curl, python3. Deliberately NOT the hcloud CLI — one less
# thing to install, and the REST API is stable.
set -euo pipefail
cd "$(dirname "$0")"

TFVARS="terraform.tfvars"
[ -f "$TFVARS" ] || { echo "manca $TFVARS — copialo da terraform.tfvars.example e inserisci il token" >&2; exit 1; }

# State lives on Cloudflare R2 (see backend.tf). Its bucket, endpoint and keys are
# gitignored because this repo is public, so init needs them passed in explicitly.
BACKEND="backend.hcl"
[ -f "$BACKEND" ] || { echo "manca $BACKEND — copialo da backend.hcl.example (istruzioni dentro)" >&2; exit 1; }
if grep -q 'ACCOUNT_ID_HERE\|ACCESS_KEY_ID_HERE\|SECRET_ACCESS_KEY_HERE' "$BACKEND"; then
  echo "$BACKEND contiene ancora dei segnaposto — completalo prima di continuare" >&2; exit 1
fi

# Read the token out of tfvars rather than asking for it twice. Never echoed.
TOKEN="$(python3 - "$TFVARS" <<'PY'
import re, sys
src = open(sys.argv[1]).read()
m = re.search(r'^\s*hcloud_token\s*=\s*"([^"]+)"', src, re.M)
print(m.group(1) if m else "")
PY
)"
if [ -z "$TOKEN" ] || [ "$TOKEN" = "PASTE_TOKEN_HERE" ]; then
  echo "hcloud_token non impostato in $TFVARS" >&2; exit 1
fi

api() { curl -sS -H "Authorization: Bearer $TOKEN" "https://api.hetzner.cloud/v1/$1"; }

# Resolve a resource id by name. Prints nothing and returns 1 when absent, so a
# missing resource is reported by name instead of importing an empty id.
id_of() {
  api "$1" | python3 -c '
import json,sys
data=json.load(sys.stdin)
key,name=sys.argv[1],sys.argv[2]
for item in data.get(key,[]):
    if item.get("name")==name:
        print(item["id"]); break
' "$2" "$3"
}

# Skip anything already in state so the script can be re-run after a failure.
in_state() { terraform state list 2>/dev/null | grep -qxF "$1"; }

import_one() { # addr, endpoint, json-key, hetzner-name
  local addr="$1" id
  if in_state "$addr"; then echo "  = $addr già in state"; return 0; fi
  id="$(id_of "$2" "$3" "$4")"
  if [ -z "$id" ]; then
    echo "  ! $addr — nessuna risorsa chiamata '$4' su Hetzner, salto" >&2
    return 0
  fi
  echo "  + $addr  <-  $4 (id $id)"
  terraform import -input=false -var-file="$TFVARS" "$addr" "$id" >/dev/null
}

echo "==> terraform init (state remoto su R2)"
terraform init -input=false -backend-config="$BACKEND" >/dev/null

echo "==> import (sola scrittura di state locale, nessuna modifica su Hetzner)"
# NOTE: the origin server is called `vox-media` at Hetzner — NOT `vox-media-01`,
# which is what main.tf originally guessed. There is also no SSH key named
# `vox-media` (the account has one, `micio86dev@gmail.com`), so no key is imported:
# ssh_keys is create-time only and main.tf ignores it.
import_one 'hcloud_firewall.media'       'firewalls' 'firewalls' 'vox-media-fw'
import_one 'hcloud_server.media'         'servers'   'servers'   'vox-media'
import_one 'hcloud_firewall.replica[0]'  'firewalls' 'firewalls' 'vox-media-replica-fw'
import_one 'hcloud_server.replica[0]'    'servers'   'servers'   'vox-media-replica-01'

echo
echo "==> terraform plan"
# -detailed-exitcode: 0 = no changes, 2 = changes pending, 1 = error.
set +e
terraform plan -input=false -var-file="$TFVARS" -detailed-exitcode -out=/dev/null
rc=$?
set -e

echo
case $rc in
  0) echo "OK — il piano non propone nulla. Lo stato rispecchia l'infrastruttura reale." ;;
  2) cat <<'MSG'
ATTENZIONE — il piano propone modifiche. NON eseguire `terraform apply`.
Significa che main.tf e la realtà divergono; va corretto main.tf (o terraform.tfvars),
mai lasciare che il piano "sistemi" la realtà.

Divergenze note e cosa vogliono dire:
  - hcloud_server sostituito       -> location sbagliata (l'origin è in hel1)
  - hcloud_ssh_key sostituito      -> ssh_public_key_path punta a una chiave diversa
                                      da quella registrata su Hetzner
  - regola 8554 rimossa            -> replica_ips vuoto: rimettici l'IP della replica
  - regola 22 ristretta            -> main.tf la vuole aperta; se la console la mostra
                                      su un IP singolo, la CI non riesce più a deployare

Rilancia questo script dopo ogni correzione: gli import già fatti vengono saltati.
MSG
     ;;
  *) echo "terraform plan è fallito (exit $rc)." ;;
esac
exit 0
