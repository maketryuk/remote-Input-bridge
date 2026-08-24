# Releasing

One version number covers both halves. It lives in `windows-sender/Cargo.toml` — Cargo is the only
build system here that cannot read it from anywhere else, so everything else reads it from there:
`build-mac-app.sh` stamps it into `Info.plist`, `build-windows.ps1` passes it to the installer, and
the release workflow refuses to publish if the tag disagrees.

## Cutting a release

```bash
# 1. Bump the single source of truth.
vim windows-sender/Cargo.toml          # version = "0.3.0"
cd windows-sender && cargo check       # refreshes Cargo.lock with the new version
cd ..

# 2. Commit, tag, push.
git commit -am "release 0.3.0"
git tag v0.3.0
git push origin master v0.3.0
```

Pushing the tag runs `.github/workflows/release.yml`, which:

1. builds `rib-sender.exe` on a Windows runner and packs it with Inno Setup,
2. builds the `.app` on a macOS runner and zips it with `ditto`,
3. composes `latest.json` from the two digests,
4. publishes the release with generated notes and `SHA256SUMS.txt`.

Nothing else is needed: both apps look for
`releases/latest/download/latest.json`, which always resolves to the newest non-prerelease release.

To rebuild a tag that already exists (a failed run, say), use **Actions → Release → Run workflow**
and give it the tag. Delete the half-published release first — `gh release create` will not
overwrite one.

## The manifest

```json
{
  "version": "0.3.0",
  "notes_url": "https://github.com/maketryuk/remote-Input-bridge/releases/tag/v0.3.0",
  "windows": { "url": "…/RemoteInputBridge-Setup-0.3.0.exe", "sha256": "…", "size": 4194304 },
  "macos":   { "url": "…/RemoteInputBridge-0.3.0-macos.zip",  "sha256": "…", "size": 1494445 }
}
```

Both apps refuse a manifest whose download is not HTTPS or whose digest is not 64 hex characters,
and discard a download whose digest does not match. That catches corruption and a swapped asset; it
is not a signature, because whoever could replace the asset could replace the manifest with it.

## Building locally

```powershell
.\scripts\build-windows.ps1        # dist\RemoteInputBridge-Setup-<version>.exe
```

```bash
./scripts/build-mac-app.sh --zip  # dist/RemoteInputBridge-<version>-macos.zip
```

Both write a `*-artifact.json` next to the file with the digest the workflow would have used, so a
locally built release can be uploaded by hand if the runners are unavailable.

## Signing the Mac releases (optional, free, worth it)

macOS ties the Accessibility grant to the bundle's *signature*. An ad-hoc signature is a hash of the
bundle contents, so every build is a different app as far as macOS is concerned: the user grants
Accessibility, the app updates itself, and the permission silently stops applying while its switch
stays on. Worse, every copy that was ever run leaves its own entry in the Privacy list, all of them
named the same, and only one of them is the one that matters.

A stable certificate fixes it: the designated requirement becomes the bundle identifier plus that
certificate, so every build signed with it is the same app, and the grant is given once.

```bash
# On your Mac, once. --force creates a certificate even if you already have an Apple Development
# one: this key is going to be uploaded, and a disposable certificate made for the purpose is a
# better thing to hand over than an Apple-issued identity tied to your developer account.
export RIB_P12_PASSWORD="$(openssl rand -base64 24)"
./scripts/create-signing-identity.sh --force --export-p12 /tmp/rib.p12 "Remote Input Bridge Local"

# Upload it. `gh secret set` reads standard input, so the key never reaches the shell history.
base64 -i /tmp/rib.p12 | tr -d '\n' | gh secret set MAC_CERT_P12
printf '%s' "$RIB_P12_PASSWORD" | gh secret set MAC_CERT_PASSWORD
printf '%s' "Remote Input Bridge Local" | gh secret set MAC_CERT_NAME
rm -f /tmp/rib.p12
```

> Do **not** reach for `security export -t identities -f pkcs12`: it exports *every* identity in
> the keychain, Apple-issued ones included, because it has no way to single one out. That is why
> the script writes the PKCS#12 itself, from the key it just generated.

The three secrets are `MAC_CERT_P12` (base64 of the file), `MAC_CERT_PASSWORD` and
`MAC_CERT_NAME`. The same certificate should sign your local builds too, so that a locally
installed app and an updated one are the same app as far as macOS is concerned:

```bash
RIB_SIGN_IDENTITY="Remote Input Bridge Local" ./scripts/build-mac-app.sh --install
```

The workflow picks them up on its own; without them it signs ad-hoc and everything still works,
just with the permission prompt after each update. Note that a self-signed certificate does **not**
help with Gatekeeper on first install — only Apple notarisation ($99/year) does that.

## Signing the Windows installer (optional, not free)

Add a `signtool sign /f cert.pfx /p … /fd sha256 /tr http://timestamp.digicert.com /td sha256`
step after the build in `release.yml`, and sign both `rib-sender.exe` and the installer. Without it
SmartScreen warns on every download; with it the warning goes away once the signature accrues
reputation. An OV certificate is roughly $200–400 a year, an EV one needs a hardware token.
