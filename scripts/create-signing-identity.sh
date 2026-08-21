#!/bin/bash
# Creates a stable, self-signed code signing identity in the login keychain.
#
# Why: an ad-hoc signature is a hash of the bundle contents, and macOS ties an Accessibility
# grant to the signature. Every rebuild therefore invalidates the grant while leaving the
# checkbox switched on - the app looks authorised and silently cannot inject anything. A stable
# identity makes the grant survive rebuilds.
#
#   ./scripts/create-signing-identity.sh
#   RIB_SIGN_IDENTITY="Remote Input Bridge Local" ./scripts/build-mac-app.sh --install
set -euo pipefail

NAME="${1:-Remote Input Bridge Local}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A real Apple Development certificate is strictly better than a self-signed one: the designated
# requirement becomes identifier + team, so the Accessibility grant survives rebuilds outright.
if security find-identity -v -p codesigning | grep -q "Apple Development"; then
    echo "==> you already have an Apple Development identity - use that instead:"
    security find-identity -v -p codesigning | grep "Apple Development"
    echo
    echo "  RIB_SIGN_IDENTITY=\"Apple Development: ...\" ./scripts/build-mac-app.sh --install"
    exit 0
fi

if security find-identity -v -p codesigning | grep -q "$NAME"; then
    echo "==> identity \"$NAME\" already exists"
    security find-identity -v -p codesigning | grep "$NAME"
    exit 0
fi

echo "==> generating a self-signed code signing certificate"
openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
    -keyout "$WORK/key.pem" -out "$WORK/cert.pem" \
    -subj "/CN=$NAME" \
    -addext "basicConstraints=critical,CA:false" \
    -addext "keyUsage=critical,digitalSignature" \
    -addext "extendedKeyUsage=critical,codeSigning" 2>/dev/null

openssl pkcs12 -export -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
    -out "$WORK/identity.p12" -passout pass: -name "$NAME" 2>/dev/null

echo "==> importing into the login keychain"
# -A lets codesign use the key without a confirmation dialog on every build.
security import "$WORK/identity.p12" -k "$KEYCHAIN" -P "" -T /usr/bin/codesign -A

echo "==> trusting it for code signing"
# Self-signed certificates are not trusted by default; codesign will not use an untrusted
# identity. This writes user-level trust settings and may ask for your password.
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$WORK/cert.pem" || {
    echo "    trust step failed - open Keychain Access, find \"$NAME\", set"
    echo "    'Code Signing' to 'Always Trust' in Get Info > Trust"
}

echo "==> available code signing identities:"
security find-identity -v -p codesigning || true
echo
echo "Build with it:"
echo "  RIB_SIGN_IDENTITY=\"$NAME\" ./scripts/build-mac-app.sh --install"
