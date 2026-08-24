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
#
# --force creates one even when an Apple Development identity is already present. That is what you
# want for the release workflow: its private key has to be uploaded to the repository secrets, and
# a disposable certificate made for exactly this is a far better thing to hand over than an
# Apple-issued one tied to your developer account.
set -euo pipefail

FORCE=0
EXPORT_P12=""
ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --force) FORCE=1 ;;
        # Writes the same key out as a PKCS#12 for the release workflow's secrets. Done here, from
        # the material this script already holds, because `security export` cannot single out one
        # identity - it would hand over every identity in the keychain, Apple-issued ones included.
        --export-p12) shift; EXPORT_P12="${1:?--export-p12 needs a path}" ;;
        *) ARGS+=("$1") ;;
    esac
    shift
done
NAME="${ARGS[0]:-Remote Input Bridge Local}"
KEYCHAIN="$HOME/Library/Keychains/login.keychain-db"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# A real Apple Development certificate is strictly better than a self-signed one: the designated
# requirement becomes identifier + team, so the Accessibility grant survives rebuilds outright.
if [ "$FORCE" = 0 ] && security find-identity -v -p codesigning | grep -q "Apple Development"; then
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

# OpenSSL 3 packs PKCS#12 with AES-256 and a SHA-256 MAC, which Apple's Security framework
# refuses to import ("MAC verification failed"). -legacy asks for the older algorithms it does
# understand. LibreSSL, which is what /usr/bin/openssl is, already writes those and has no such
# flag - hence the version test rather than always passing it.
COMPAT=()
if openssl version | grep -q "^OpenSSL 3"; then
    COMPAT=(-legacy)
fi
# The password is random and thrown away with the temporary directory: it exists only because
# `security import` fails MAC verification on a PKCS#12 with an empty password, whatever the
# algorithms. Nothing downstream needs it - the identity lives in the keychain afterwards.
PASSWORD="$(openssl rand -hex 16)"
openssl pkcs12 -export "${COMPAT[@]}" -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
    -out "$WORK/identity.p12" -passout "pass:$PASSWORD" -name "$NAME" 2>/dev/null

echo "==> importing into the login keychain"
# -A lets codesign use the key without a confirmation dialog on every build.
security import "$WORK/identity.p12" -k "$KEYCHAIN" -P "$PASSWORD" -T /usr/bin/codesign -A

echo "==> trusting it for code signing"
# Self-signed certificates are not trusted by default; codesign will not use an untrusted
# identity. This writes user-level trust settings and may ask for your password.
security add-trusted-cert -r trustRoot -p codeSign -k "$KEYCHAIN" "$WORK/cert.pem" || {
    echo "    trust step failed - open Keychain Access, find \"$NAME\", set"
    echo "    'Code Signing' to 'Always Trust' in Get Info > Trust"
}

if [ -n "$EXPORT_P12" ]; then
    EXPORT_PASSWORD="${RIB_P12_PASSWORD:?set RIB_P12_PASSWORD when using --export-p12}"
    openssl pkcs12 -export "${COMPAT[@]}" -inkey "$WORK/key.pem" -in "$WORK/cert.pem" \
        -out "$EXPORT_P12" -passout "pass:$EXPORT_PASSWORD" -name "$NAME" 2>/dev/null
    chmod 600 "$EXPORT_P12"
    echo "==> wrote $EXPORT_P12 - it holds a private key, so keep it out of the repository"
fi

echo "==> available code signing identities:"
security find-identity -v -p codesigning || true
echo
echo "Build with it:"
echo "  RIB_SIGN_IDENTITY=\"$NAME\" ./scripts/build-mac-app.sh --install"
