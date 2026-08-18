#!/bin/bash
# Live end-to-end check for platform-issued token verification (ARN-255).
#
# Boots a real temper server, activates a trusted issuer through the same
# environment configuration a deployment uses, mints real ES256 tokens with the
# matching private key, and drives the real HTTP surface to prove:
#   1. a valid token authenticates                            (not 401)
#   2. a token signed by an unknown key is rejected            (401)
#   3. an expired token is rejected                            (401)
#   4. a token from an unregistered issuer is rejected         (401)
#   5. a garbage/tampered token is rejected                    (401)
#   6. the operator key still works — the change is additive   (200)
#   7. a verified agent token CANNOT register an issuer        (403)
#      (the takeover path: register your own key, mint owner tokens)
#   8. a verified agent token CANNOT bump a generation         (403)
#      (per-user sign-out denial of service)
#
# Requires: cargo, python3 with 'cryptography', curl. Usage:
#   scripts/e2e-trusted-issuer.sh [port]
set -uo pipefail

PORT="${1:-3477}"
BASE="http://localhost:${PORT}"
TENANT="default"
API_KEY="local-e2e-operator-key"
ISSUER="https://e2e.issuer.local"
AUD="temper-e2e"
WORK="$(mktemp -d)"
SERVER_PID=""
cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "Minting a P-256 key, its JWKS, and four test tokens"
python3 - "$WORK" "$ISSUER" "$AUD" <<'PY'
import base64, json, sys, time
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.hazmat.primitives.asymmetric.utils import decode_dss_signature
from cryptography.hazmat.primitives import hashes

work, issuer, aud = sys.argv[1], sys.argv[2], sys.argv[3]
b64 = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()

def mint(key, claims, kid="e2e-k1"):
    head = {"alg": "ES256", "kid": kid, "typ": "JWT"}
    si = f'{b64(json.dumps(head).encode())}.{b64(json.dumps(claims).encode())}'
    r, s = decode_dss_signature(key.sign(si.encode(), ec.ECDSA(hashes.SHA256())))
    return f'{si}.{b64(r.to_bytes(32,"big") + s.to_bytes(32,"big"))}'

key = ec.generate_private_key(ec.SECP256R1())
pn = key.public_key().public_numbers()
open(f"{work}/jwks.json","w").write(json.dumps({"keys":[{
    "kty":"EC","crv":"P-256","kid":"e2e-k1",
    "x": b64(pn.x.to_bytes(32,"big")), "y": b64(pn.y.to_bytes(32,"big"))}]}))

now = int(time.time())
base = {"iss": issuer, "aud": aud, "sub": "human-e2e", "client_id": "kc_agent_e2e",
        "agent_type": "contributor", "grant_id": "grant-e2e", "nbf": now - 300}
open(f"{work}/valid.txt","w").write(mint(key, {**base, "exp": now + 900}))
open(f"{work}/expired.txt","w").write(mint(key, {**base, "exp": now - 600}))
open(f"{work}/bad_iss.txt","w").write(mint(key, {**base, "iss": "https://unregistered.example", "exp": now + 900}))
open(f"{work}/rogue.txt","w").write(mint(ec.generate_private_key(ec.SECP256R1()), {**base, "exp": now + 900}))
print("  4 tokens + JWKS ready")
PY
[ -f "$WORK/valid.txt" ] || { echo "token minting failed"; exit 1; }

say "Starting a real temper server on :$PORT with the issuer activated by env"
TEMPER_API_KEY="$API_KEY" \
TEMPER_TRUSTED_ISSUER_URL="$ISSUER" \
TEMPER_TRUSTED_ISSUER_JWKS="$(cat "$WORK/jwks.json")" \
TEMPER_TRUSTED_ISSUER_AUD="$AUD" \
  cargo run -q -p temper-cli --bin temper -- serve --port "$PORT" --no-observe \
  >"$WORK/server.log" 2>&1 &
SERVER_PID=$!
for _ in $(seq 1 150); do
  curl -sf "$BASE/healthz" >/dev/null 2>&1 && break
  sleep 2
  kill -0 "$SERVER_PID" 2>/dev/null || { echo "server died:"; tail -30 "$WORK/server.log"; exit 1; }
done
curl -sf "$BASE/healthz" >/dev/null || { echo "never healthy:"; tail -30 "$WORK/server.log"; exit 1; }
echo "  healthy"
grep -q "Trusted issuer '$ISSUER' registered" "$WORK/server.log" \
  && echo "  issuer registered from environment at boot" \
  || { echo "  ISSUER NOT REGISTERED — see log"; tail -20 "$WORK/server.log"; }

code() { # code <token> [method] [path] [body]
  local tok="$1" method="${2:-GET}" path="${3:-/tdata/TrustedIssuers}" body="${4:-}"
  if [ -n "$body" ]; then
    curl -s -o /dev/null -w '%{http_code}' -X "$method" "$BASE$path" \
      -H "Authorization: Bearer $tok" -H "X-Tenant-Id: $TENANT" \
      -H "Content-Type: application/json" -d "$body"
  else
    curl -s -o /dev/null -w '%{http_code}' -X "$method" "$BASE$path" \
      -H "Authorization: Bearer $tok" -H "X-Tenant-Id: $TENANT"
  fi
}

PASS=0; FAIL=0
check() { # check <name> <got> <expected...>
  local name="$1" got="$2"; shift 2
  for want in "$@"; do
    if [ "$got" = "$want" ]; then printf '  \033[32mPASS\033[0m  %s (HTTP %s)\n' "$name" "$got"; PASS=$((PASS+1)); return; fi
  done
  printf '  \033[31mFAIL\033[0m  %s (got HTTP %s, wanted %s)\n' "$name" "$got" "$*"; FAIL=$((FAIL+1))
}

VALID=$(cat "$WORK/valid.txt")
ISS_ENC="https%3A%2F%2Fe2e.issuer.local"

say "Token verification"
check "valid token authenticates"           "$(code "$VALID")"                  200 403 404
check "rogue-key token rejected"            "$(code "$(cat "$WORK/rogue.txt")")"   401
check "expired token rejected"              "$(code "$(cat "$WORK/expired.txt")")" 401
check "unregistered issuer rejected"        "$(code "$(cat "$WORK/bad_iss.txt")")"  401
check "garbage token rejected"              "$(code 'not.a.jwt')"                401
check "operator key still works (additive)" "$(code "$API_KEY")"                 200

say "Privilege boundary on the identity entities"
REG_BODY='{"issuer":"https://attacker.example","jwks_json":"{\"keys\":[]}","audience":"x","algorithms":"ES256","description":"takeover attempt","created_by":"attacker"}'
check "agent token CANNOT register an issuer" \
  "$(code "$VALID" POST "/tdata/TrustedIssuers('https%3A%2F%2Fattacker.example')/Temper.RegisterIssuer" "$REG_BODY")" 403
check "agent token CANNOT rotate issuer keys" \
  "$(code "$VALID" POST "/tdata/TrustedIssuers('$ISS_ENC')/Temper.RotateIssuerKeys" '{"jwks_json":"{\"keys\":[]}"}')" 403
check "agent token CANNOT bump a generation" \
  "$(code "$VALID" POST "/tdata/PrincipalGenerations('human-e2e')/Temper.BumpGeneration" '{}')" 403

say "Result: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
