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
| `docs/PROTOCOL.md` | Wire protocol v1, byte for byte |
| `docs/TESTING.md` | The acceptance scenarios, as commands you can actually run |

---

## How the smoothness is achieved

| Problem | What this project does |
|---------|------------------------|
| A hook that hides local input also hides it from *us* | Movement is never swallowed, because a swallowed event never becomes Raw Input for anyone — that alone cost 99 % of the movement data. The local pointer is pinned with `ClipCursor` instead, while buttons, wheel and keys are swallowed by the hook and read from it |
| A 1000 Hz mouse produces 1000 events/s | `GetRawInputBuffer` drains events in batches; movement is two `fetch_add`s on cumulative counters, nothing more |
| 1000 packets/s is a burst generator | A separate thread on a **high-resolution waitable timer** samples those counters every 1/2/4/8 ms and sends one packet (spec §9) |
| A lost UDP packet must not shift the cursor forever | Packets carry **cumulative totals**, not deltas. The next packet to arrive re-establishes the truth on its own (spec §10.2) |
| Bursty Wi-Fi delivery becomes visible stutter | The receiver coalesces everything that arrives inside one interval into a single `CGEvent`, then *spreads* it over the next few milliseconds and keeps moving through the gap that follows — measured on a real link, a third of all datagrams arrive inside the same millisecond as their predecessor (spec §33) |
| Wi-Fi power saving turns gaps into clumps | While the Mac holds the input the sender emits a packet every tick even when the mouse is still, so the radio never dozes off between movements. ~22 kB/s while switched over, nothing while Windows has the input |
| Double pointer acceleration | Windows sends **raw device counts**; the only scaling applied anywhere is one linear factor on the Mac (spec §55) |
| Idle CPU | Nothing is sent when the mouse is still; no polling loop on either side |
| A dead link leaving the user with no input | Every failure path — TCP close, heartbeat timeout, receiver crash, Wi-Fi loss, sleep — funnels through one fail-safe that returns input to Windows and releases every key on the Mac (spec §17, §18, §51) |
| An open input port on the LAN | Pairing code → device key → per-session keys; every packet and every frame carries an HMAC and a monotonic counter (spec §28-§30) |

Measured on loopback with `scripts/test-sender.py`: **RTT 1.4-2.6 ms, jitter 0.2 ms**, no drift
after deliberate 2 % packet loss.

---

## Build

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
cargo test                                    # 27 unit tests, host-native
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

Nothing about this needs an account, a cloud service or an internet connection.

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

While the Mac has the input, the Windows-side low-level hooks swallow local mouse and keyboard
events so the same movement is not acted on twice, and the Windows cursor stays put.

---

## Settings

**Windows** (tray → *Status and settings…*): Mac IP, TCP/UDP ports, device name, mouse update
interval (1 / 2 / 4 / 8 ms), the three hotkeys, edge switching, local input suppression,
auto-connect, UDP on/off, diagnostics line, start with Windows. Stored in
`%APPDATA%\RemoteInputBridge\config.json`.

**Mac** (menu bar → *Settings…*): ports, device name, pointer speed, event scheduling mode
(`smoothed` / `coalesced` / `paced` / `immediate`) and the smoothing window, scroll mode and
scaling, natural scrolling, modifier mapping, edge switching, heartbeat timeout, log level, start
at login.

The scheduling modes exist to be compared on your own link: `immediate` is the lowest latency and
the most exposed to jitter, `smoothed` (default, 10 ms) is the most even. Both extremes are one
click apart, and the diagnostics line shows what each costs. Stored in
`~/Library/Application Support/RemoteInputBridge/config.json`.

Default modifier mapping is `Ctrl→Control`, `Alt→Option`, `Win→Command`, `Shift→Shift`; the first
three are remappable (spec §15).

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

* **Games can still see local input.** Suppression uses documented user-space mechanisms only:
  the low-level hooks swallow buttons, wheel and keys, and the pointer is pinned with
  `ClipCursor`. An application that reads Raw Input or DirectInput directly — which is most
  games, and anything behind an anti-cheat — still sees device movement. No kernel driver, no
  injection, no anti-cheat interference (spec §20, §21).
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
| Movement is smooth locally but stutters over Wi-Fi | Check `jitter` in the diagnostics line. Above a few ms it is the link, not the app: prefer 5 GHz, get closer to the access point, or put the Mac on Ethernet. Then raise **Smoothing** (Settings → Pointer) until it is even — it absorbs roughly its own value in jitter |
| Motion is smooth but feels coarse, in visible steps | Check `mouse in` in the diagnostics line. If it reads 125 Hz the mouse is at its default polling rate; set it to 1000 Hz in the mouse's own software and the steps get eight times finer |
| A modifier appears stuck on the Mac | Should be impossible: every disconnect releases everything. If it happens, `Ctrl+Alt+→` then `Ctrl+Alt+←` re-syncs, and please report the log |
| Windows input feels doubled while the Mac is active | Local suppression is off, or the foreground app reads Raw Input directly (see limitations) |

---

## Thread model

**Windows**: UI/tray thread (also drains Raw Input and runs the hooks) · realtime thread
(high-resolution timer, coalescing, UDP) · network thread (connection lifecycle, heartbeat,
reliable events) · control reader thread · telemetry thread. The Raw Input path never blocks on
the network — it only touches atomics and an unbounded channel.

**macOS**: main/UI · control queue (`userInitiated`) · realtime receive queue (`userInteractive`)
· event scheduler thread (`userInteractive`). The receive path never waits for the UI.
