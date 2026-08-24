# Remote Input Bridge

One mouse and keyboard for two machines on the same LAN. The **Windows PC** owns the physical
mouse and keyboard; the **Mac** sits to its left and can be driven from them on demand.

```
┌──────────────────────┐        LAN / Wi-Fi        ┌──────────────────────┐
│        Mac           │ ◀──────────────────────── │      Windows PC      │
│   Swift + CGEvent    │   UDP 47822  movement     │  Rust + Raw Input    │
│   menu bar app       │   TCP 47821  everything   │  tray app            │
└──────────────────────┘              else        └──────────────────────┘
```

`Ctrl+Alt+←` hands the input to the Mac. `Ctrl+Alt+→` takes it back.
`Ctrl+Alt+Shift+Esc` always takes it back, whatever state the app thinks it is in.

The whole design is built around one priority: **the cursor on the Mac must feel like a locally
connected mouse, including a 1000 Hz gaming mouse.** Everything else is subordinate to that.

---

## Repository layout

| Path | What it is |
|------|-----------|
| `windows-sender/` | Rust sender: Raw Input → coalescing → authenticated UDP/TCP, tray + settings window |
| `mac-receiver/` | Swift receiver: Network.framework → event scheduler → `CGEvent`, menu bar app |
| `scripts/test-sender.py` | Reference sender. Speaks the real protocol, so the Mac side can be exercised (and packet loss injected) without a Windows box |
| `scripts/build-mac-app.sh` | Builds the receiver into a `.app` bundle |
| `scripts/build-windows.ps1` | Builds the sender and packs the per-user installer |
| `scripts/make-windows-icon.sh` | Draws `app.ico` from the same source as the Mac icon (run on a Mac; the result is committed) |
| `installer/windows/rib-setup.iss` | Inno Setup script for that installer |
| `.github/workflows/release.yml` | Tag → build both halves → publish the release the apps update from |
| `docs/PROTOCOL.md` | Wire protocol v1, byte for byte |
| `docs/TESTING.md` | The acceptance scenarios, as commands you can actually run |
| `docs/RELEASING.md` | How to cut a release, and what updating looks like from the app's side |

---

## How the smoothness is achieved

| Problem | What this project does |
|---------|------------------------|
| A hook that stops being called takes the input with it | Every event has a source that survives: movement, buttons and the wheel come from Raw Input, which is delivered whatever window has focus and arrives even when the hook swallowed the event. Keys are the exception — a swallowed keystroke never becomes Raw Input, so the hook both hides and forwards those. The asymmetry is measured, not assumed, and getting it wrong is visible either way: forward a mouse button from the hook as well and every click lands twice; do not forward a key and the keyboard goes silent |
| A 1000 Hz mouse produces 1000 events/s | `GetRawInputBuffer` drains events in batches; movement is two `fetch_add`s on cumulative counters, nothing more |
| 1000 packets/s is a burst generator | A separate thread on a **high-resolution waitable timer** samples those counters every 1/2/4/8 ms and sends one packet (spec §9) |
| A lost UDP packet must not shift the cursor forever | Packets carry **cumulative totals**, not deltas. The next packet to arrive re-establishes the truth on its own (spec §10.2) |
| Bursty Wi-Fi delivery becomes visible stutter | The receiver coalesces everything that arrives inside one interval into a single `CGEvent`, then *spreads* it over the next few milliseconds and keeps moving through the gap that follows — measured on a real link, a third of all datagrams arrive inside the same millisecond as their predecessor (spec §33) |
| Wi-Fi power saving turns gaps into clumps | Movement goes out the moment it happens, plus a keep-alive packet whenever the stream has been quiet for 10 ms, so the radio never dozes off. Sending on *every* tick instead was measurably worse: 500 tiny frames per second on a weak link cost airtime and invited bursts of retransmission |
| Double pointer acceleration | Windows sends **raw device counts**; the only scaling applied anywhere is one linear factor on the Mac (spec §55) |
| Idle CPU | Nothing is sent when the mouse is still; no polling loop on either side |
| A dead link leaving the user with no input | Every failure path — TCP close, heartbeat timeout, receiver crash, Wi-Fi loss, sleep — funnels through one fail-safe that returns input to Windows and releases every key on the Mac (spec §17, §18, §51) |
| An open input port on the LAN | Pairing code → device key → per-session keys; every packet and every frame carries an HMAC and a monotonic counter (spec §28-§30) |

Measured on loopback with `scripts/test-sender.py`: **RTT 1.4-2.6 ms, jitter 0.2 ms**, no drift
after deliberate 2 % packet loss.

---

## Install

Both halves ship together from the [releases page](https://github.com/maketryuk/remote-Input-bridge/releases)
under one version number, so a release is always a matched pair. Building from source is in
[Build](#build) below and is not needed to use the thing.

### Windows

Download `RemoteInputBridge-Setup-<version>.exe` and run it.

It installs **for the current user only**, into `%LOCALAPPDATA%\Programs\RemoteInputBridge`, and
never asks for administrator rights — which is also what lets it update itself without a UAC prompt
every time. It adds a Start menu entry, an uninstaller in *Apps & features*, and, unless you untick
it, a startup entry so the bridge is there when you sign in.

> **SmartScreen will warn you.** The installer is not code signed — a certificate costs a few
> hundred dollars a year — so Windows shows *"Windows protected your PC"* on first run. Choose
> **More info → Run anyway**. The same warning appears again after each update, because the
> reputation SmartScreen builds is tied to a signature this build does not have.

### macOS

Download `RemoteInputBridge-<version>-macos.zip`, unzip it, and drag `RemoteInputBridge.app` into
`/Applications`.

The app is not notarised, so the first launch is refused with *"Apple could not verify …"*. Open
**System Settings → Privacy & Security**, scroll to the bottom and press **Open Anyway**; or clear
the download flag yourself:

```bash
xattr -dr com.apple.quarantine /Applications/RemoteInputBridge.app
```

Then grant **Accessibility** (System Settings → Privacy & Security → Accessibility) and, in the
app's settings, tick **Start at login**.

---

## Updates

Both apps check once a day, and on demand from the settings window or the tray/menu bar item. When
a newer release exists you get one button: it downloads the file, checks its SHA-256 against the
digest published in the release, and installs it. Nothing is installed without that click, and the
daily check can be switched off with **Check for updates automatically**.

* **Windows** — the installer runs silently and starts the bridge again afterwards. Settings and
  paired keys are untouched.
* **macOS** — the bundle is replaced in place and the app relaunches. If `/Applications` is not
  writable for you, the update says so instead of failing halfway.
* Either half can update on its own. The wire protocol carries its own version number and both
  sides insist on an exact match, so a release that changes the protocol has to be installed on
  both machines — when that happens the two say so in as many words
  (*"the Mac speaks protocol 2, this build speaks 1"*) instead of failing obscurely.

> **macOS asks for Accessibility again after an update** unless the release was built with a stable
> signing identity — macOS ties the permission to the bundle's signature, and an ad-hoc signature
> changes with every build. See [docs/RELEASING.md](docs/RELEASING.md) for the (free, five minute)
> way to fix that for your own releases.

What leaves the machine for this: one HTTPS request to `github.com`, with a user agent that carries
the version and nothing else. No identifiers, no configuration, no host name. See
[Privacy](#privacy).

---

## Build

Only needed to develop on it — released builds come from the [Install](#install) section above.

### macOS receiver

Requires Xcode command line tools (Swift 5.9+, macOS 13+).

```bash
./scripts/build-mac-app.sh --install --run    # build → /Applications → launch
```

It lands in `/Applications/RemoteInputBridge.app`, so it shows up in Launchpad and Spotlight like
any other app. `--install` alone skips the launch; no arguments at all builds into
`mac-receiver/build/` without touching `/Applications`.

Then grant **Accessibility**: System Settings → Privacy & Security → Accessibility → enable
*RemoteInputBridge*. Without it `CGEventPost` fails silently — the app logs an explicit error when
it notices the cursor is not following the events it posts.

> **The grant does not survive a rebuild.** An ad-hoc signature is a hash of the bundle contents,
> and macOS ties the permission to the signature — so after every rebuild the checkbox stays on
> while the permission no longer applies. `--install` therefore clears the stale entry
> (`tccutil reset Accessibility studio.lince.remoteinputbridge`) and you grant it once more.
> To stop that happening, create a self-signed *Code Signing* certificate once in Keychain Access
> (Certificate Assistant → Create a Certificate) and build with
> `RIB_SIGN_IDENTITY="your cert name" ./scripts/build-mac-app.sh --install`.

**The sender's log is mirrored into the receiver's**, so one file covers both machines — which is
the only practical way to read Windows-side telemetry without sitting at the Windows machine.
Lines from the sender are tagged with its name: `[WINDOWS-PC] diag link Connected …`.

Logs always go to `~/Library/Logs/RemoteInputBridge.log`. To watch them live, or to run with a
different level:

```bash
tail -f ~/Library/Logs/RemoteInputBridge.log
/Applications/RemoteInputBridge.app/Contents/MacOS/RemoteInputBridge --log DEBUG
```

Useful flags: `--begin-pairing` (enable pairing mode and print the code, handy over SSH),
`--no-prompt`, `--log LEVEL`, `--crypto-selftest`.

### Windows sender

Requires Rust (`https://rustup.rs`) with the MSVC toolchain.

```powershell
cd windows-sender
cargo build --release
.\target\release\rib-sender.exe
```

The code cross-checks from macOS too, which is how it was developed:

```bash
cd windows-sender
cargo check --target x86_64-pc-windows-msvc   # type-checks the Win32 paths
cargo test                                    # 30 unit tests, host-native
```

---

## First run

1. **Mac** — open the app. If Accessibility permission is missing it says so and offers a button
   to System Settings; grant it to *RemoteInputBridge* and the warning disappears within two
   seconds. Then choose **Show pairing code** from the menu bar item.
2. **Windows** — start `rib-sender.exe`, open **Status and settings…** from the tray icon, fill in
   the Mac's IP address, type the pairing code, press **Pair**. The code is needed once; the
   device key is stored in `%APPDATA%\RemoteInputBridge\keys.json`.
3. Press `Ctrl+Alt+←`. The tray status and the menu bar icon both switch to "Mac".

Nothing about this needs an account or a cloud service. The bridge itself never touches the
internet — the only outbound connection either app makes is the update check, and that can be
switched off.

---

## Hotkeys

| Default | Action |
|---------|--------|
| `Ctrl+Alt+←` | switch input to the Mac |
| `Ctrl+Alt+→` | switch input back to Windows |
| `Ctrl+Alt+Shift+Esc` | force input back to Windows |

They are recognised on Windows and never forwarded (spec §16). All three are editable in the
settings window; a spec that does not parse is rejected with an explanation instead of being
silently stored. Format: modifiers `Ctrl` / `Alt` / `Shift` / `Win` plus one key
(`Left`, `Right`, `Escape`, `F5`, `A`, …).

While the Mac has the input, the Windows-side low-level hooks swallow local clicks, wheel and
keystrokes so the same action is not acted on twice, and the Windows cursor is pinned in place.

**While Windows has the input, the mouse hook is removed entirely.** It sits on the path of every
report a 1000 Hz mouse produces, and the system delivers that input to nobody until the callback
returns — so when nothing needs suppressing, the cheapest thing this app can do is not be on that
path at all. It comes back the instant the input goes to the Mac, or stays if edge switching is on,
which needs it. The keyboard hook is kept: it fires at typing speed, and it is what stops
`Ctrl+Alt+←` from also reaching whatever has focus.

Nothing goes out over the network while Windows owns the input either, beyond a heartbeat roughly
three times a second — the movement stream only exists while the Mac is the target.

---

## Settings

**Windows** (tray → *Status and settings…*): Mac IP, TCP/UDP ports, device name, mouse update
interval (1 / 2 / 4 / 8 ms), the three hotkeys, edge switching, local input suppression,
auto-connect, UDP on/off, diagnostics line, start with Windows, automatic update checks. Stored in
`%APPDATA%\RemoteInputBridge\config.json`.

"Start with Windows" reads the registry rather than the config file, so it always shows what
Windows will actually do — including when the installer set it.

**Mac** (menu bar → *Settings…*): ports, device name, pointer speed, event scheduling mode
(`smoothed` / `coalesced` / `paced` / `immediate`) and the smoothing window, scroll mode and
scaling, natural scrolling, modifier mapping, edge switching, heartbeat timeout, log level, start
at login, automatic update checks.

The scheduling modes exist to be compared on your own link: `immediate` is the lowest latency and
the most exposed to jitter, `smoothed` (default, 10 ms) is the most even. Both extremes are one
click apart, and the diagnostics line shows what each costs. Stored in
`~/Library/Application Support/RemoteInputBridge/config.json`.

Default modifier mapping is `Ctrl→Control`, `Alt→Option`, `Win→Command`, `Shift→Shift`; the first
three are remappable (spec §15).

A keyboard with a hardware Mac mode of its own needs nothing here: in that mode it already sends
Command where Alt used to be, and the default mapping passes it through as Command. If you would
rather not switch the keyboard — its mode applies to both machines at once — set `Alt → Command`
and `Windows key → Option` instead, which does the same thing for what is sent to the Mac only.

---

## Diagnostics

Windows: the settings window shows two live lines, the same line is written to
`%APPDATA%\RemoteInputBridge\rib-sender.log` and forwarded to the Mac's log, and
`rib-sender.exe --console` prints it to a console too:

```
link Connected     target Mac      mouse in   1000 Hz  keys   0 Hz  net out   500 Hz (   2 Hz reliable)  25.9 kbit/s  loss  0.0%  rtt  2.51 ms  jitter 0.20 ms  mac events   498 Hz  reconnects 0
```

Loss is not a local guess: the Mac reports how many datagrams it actually saw in every `PONG`.

Mac: the same numbers appear in the menu and in the settings window, and
`--log TRACE` logs individual mouse packets (only at `TRACE`, per spec §40).

For the proof-of-concept comparison described in spec §59, run the sender as a console app with
no tray icon:

```powershell
.\rib-sender.exe --console --no-tray --diagnostics --mac 192.168.1.123 --interval 2
```

---

## Privacy

The bridge carries everything you type. What that means in practice:

* **Keystrokes and mouse movement go to one place: the Mac you paired with**, over your own
  network, authenticated and integrity-checked with a key established at pairing
  (see [docs/PROTOCOL.md](docs/PROTOCOL.md)). There is no server in the middle and no account.
* **Nothing is logged that you typed.** The log records event *rates*, not content; key identifiers
  appear only at `TRACE`, which is off by default and never sent anywhere.
* **The only internet connection either app makes is the update check** — a GET to
  `github.com`, sending a user agent of the form `RemoteInputBridge/0.2.0 (Windows)` and nothing
  else. GitHub sees your IP address, as it would for any download. Turn the check off and neither
  app opens a socket to anything but the machine you paired with.
* **Your settings and keys stay local**: `%APPDATA%\RemoteInputBridge\` on Windows,
  `~/Library/Application Support/RemoteInputBridge/` on the Mac. Neither is included in anything the
  apps send, and the Windows uninstaller asks before deleting them.

---

## Firewall

* **Windows**: only outbound connections are made; no rule is normally required.
* **macOS**: the first launch may ask whether *RemoteInputBridge* may accept incoming
  connections — allow it. If the answer was "Deny", remove the entry in
  *System Settings → Network → Firewall → Options*.

Both machines must be on the same IP subnet and able to route to each other. Windows on Ethernet
and the Mac on Wi-Fi is the tested configuration.

---

## Known limitations

These are deliberate MVP boundaries, not bugs:

* **Games read the mouse past the hooks.** Suppression uses documented user-space mechanisms
  only: the low-level hooks swallow buttons, wheel and keys, and the pointer is pinned with
  `ClipCursor`. Movement is deliberately *not* swallowed — a hook that swallows an event drops it
  before the system turns it into Raw Input, for every process including this one, which would
  cost exactly the 1000 raw deltas a second that make the remote cursor smooth. So an application
  reading Raw Input or DirectInput directly, which is most games, still sees device movement.

  The mitigation is focus: switching to the Mac raises a small always-on-top window and gives it
  the foreground, and virtually every game ignores input while it is not in front. Being on top is
  not the same as being in front, though, and Windows only lets a process change the foreground
  window under conditions a background tray app does not always meet — so the result is checked,
  and a window that will not give up the foreground is minimised instead, then restored when the
  input comes back. The log says which window that was. Still no kernel driver, no injection,
  nothing an anti-cheat has cause to object to (spec §20, §21).
* **Payloads are authenticated, not encrypted.** Every datagram and frame is HMAC-tagged with a
  per-session key and rejected on replay, so an unpaired device cannot inject input. Keystrokes
  are not confidential on the wire. Trusted LAN only; TLS/DTLS is v2.
* **Media keys are not forwarded** (optional in spec §14). Numpad, function keys, arrows,
  modifiers and JIS keys are.
* **One Mac, address entered by hand.** No mDNS discovery yet (spec §44), no clipboard, no file
  transfer, no second machine.
* **Windows is always the sender**; the Mac cannot drive Windows.
* **Start at login on the Mac needs a signed bundle.** With the ad-hoc signature produced by
  `build-mac-app.sh`, `SMAppService` refuses to register and the app says so. Ad-hoc identities
  also change on every rebuild, so macOS may ask for Accessibility permission again.
* **Screen-edge switching is off by default** and is a convenience, not the primary path.

---

## Troubleshooting

| Symptom | Cause and fix |
|---------|---------------|
| Mac cursor does not move, everything else looks fine | Accessibility permission. The menu bar icon shows a warning triangle and `~/Library/Logs/RemoteInputBridge.log` says so outright. `CGEventPost` fails silently without it |
| Permission is enabled in System Settings but the app still says it is missing | The bundle was rebuilt, so its ad-hoc signature changed and the existing grant no longer matches. Run `tccutil reset Accessibility studio.lince.remoteinputbridge`, relaunch, grant again — or build with `RIB_SIGN_IDENTITY` (see above) |
| Windows says "the Mac rejected our key" |  The two sides held different device keys. The sender now discards a key the Mac refuses and asks to pair again; show a new code on the Mac and press Pair |
| The receiver keeps swapping between two senders | Only one session exists at a time and a newly authenticated sender replaces the previous one. Do not run `scripts/test-sender.py` while the real Windows sender is connected |
| Windows says "not paired with this Mac yet" | The Mac has no key for this PC. Press *Show pairing code* on the Mac, then *Pair* on Windows |
| "the Mac is not in pairing mode" | The code expires after three minutes and is consumed by a successful pairing. Generate a new one |
| Connects, then drops every second | Heartbeat timeout: the Mac is not seeing frames. Check the firewall on the Mac and that TCP 47821 is reachable |
| Cursor moves but far too slowly or quickly | Pointer speed on the Mac (Settings → Pointer). Windows sends raw counts, so Windows pointer speed has no effect by design |
| `rtt` above ~5 ms on a LAN | That is the link, not the app. Roughly 20 ms round trip and bursts of 28 % loss were measured on a 5 GHz Wi-Fi link two metres from the access point; the same setup over Ethernet sits under 1 ms. Nothing in the app can hide 20 ms |
| Movement is smooth locally but stutters over Wi-Fi | Check `jitter` in the diagnostics line. Above a few ms it is the link, not the app: prefer 5 GHz, get closer to the access point, or put the Mac on Ethernet. Then raise **Smoothing** (Settings → Pointer) until it is even — it absorbs roughly its own value in jitter |
| Motion is smooth but feels coarse, in visible steps | Check `mouse in` in the diagnostics line. If it reads 125 Hz the mouse is at its default polling rate; set it to 1000 Hz in the mouse's own software and the steps get eight times finer |
| A modifier appears stuck on the Mac | Should be impossible: every disconnect releases everything. If it happens, `Ctrl+Alt+→` then `Ctrl+Alt+←` re-syncs, and please report the log |
| A game keeps reacting to the mouse while the Mac is active | Games read Raw Input directly, which no hook can hide (see limitations). Switching raises a banner that takes the foreground, and a game that is not in front ignores input — if it still reacts, it is one of the few that reads input unfocused, and the only remedy is to minimise it |
| Windows input feels doubled while the Mac is active | Local suppression is off, the foreground app reads Raw Input directly (see limitations), or the foreground window is elevated and this app is not — a low-level hook is skipped for higher-integrity windows. Run the sender as administrator to cover those. Forwarding to the Mac keeps working either way |
| Windows says "the update check failed" | It is a plain HTTPS GET to github.com: a proxy that needs credentials, a firewall rule, or no route at all will stop it. The rest of the app is unaffected — the check is the only thing that ever leaves the LAN |
| The update installed but the Mac asks for Accessibility again | Expected for a release signed ad-hoc: macOS ties the grant to the signature and an ad-hoc one changes every build. See [docs/RELEASING.md](docs/RELEASING.md) for the stable-identity fix |
| "Start with Windows" is ticked but nothing starts | Check `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` for a `RemoteInputBridge` value pointing at an executable that still exists. Untick and tick it again to rewrite it to the current path |
| The mouse reports only 125 Hz | If it is behind a KVM switch, the KVM is very likely the limit — many run their HID emulation at 125 Hz regardless of the mouse. Plugging the mouse straight into the PC is the only way to get 1000 Hz through |

---

## Licence

MIT — see [LICENSE](LICENSE).

---

## Thread model

**Windows**: UI/tray thread (also drains Raw Input and runs the hooks) · realtime thread
(high-resolution timer, coalescing, UDP) · network thread (connection lifecycle, heartbeat,
reliable events) · control reader thread · telemetry thread. The Raw Input path never blocks on
the network — it only touches atomics and an unbounded channel.

**macOS**: main/UI · control queue (`userInitiated`) · realtime receive queue (`userInteractive`)
· event scheduler thread (`userInteractive`). The receive path never waits for the UI.
