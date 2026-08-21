# Testing

Every scenario from the specification, as something you can actually run. The Python reference
sender lets the whole receiver side be tested before a Windows build exists.

```bash
# terminal 1 - receiver with a visible log
/Applications/RemoteInputBridge.app/Contents/MacOS/RemoteInputBridge --begin-pairing --log DEBUG
```

**Stop the Windows sender first.** Only one session exists at a time, and a newly authenticated
sender replaces the previous one — leaving both running makes them kick each other in a loop.

The pairing code is printed as `PAIRING CODE: XXXX-YYYY`.

---

## Unit and self tests

```bash
cd windows-sender && cargo test                        # 27 tests: protocol, crypto, keymap,
                                                       # hotkeys, sticky-key rules, telemetry
cargo check --target x86_64-pc-windows-msvc            # type-checks all Win32 code from any host

mac-receiver/.build/debug/RemoteInputBridge --crypto-selftest
python3 scripts/test-sender.py --vectors
```

The last three print or assert the **same** key-schedule vectors. All three implementations
derive identical keys, so a change to one that breaks the wire fails loudly instead of turning
into an unexplained authentication error.

---

## Scenario 1 - ordinary work (spec §47)

```bash
python3 scripts/test-sender.py --host <mac-ip> --pair XXXX-YYYY \
        --circle 5 --click --scroll 3 --type "hello world"
```

Expected: the cursor draws two circles, clicks once, scrolls three notches, types the text; no
key remains held afterwards (`releasing 0 key(s)` or no release line at all in the log).

With the real Windows sender: press `Ctrl+Alt+←`, use the Mac, press `Ctrl+Alt+→`. No hangs, no
sticky keys, no doubled input.

---

## Scenario 2 - 1000 Hz mouse (spec §48)

Requires the Windows sender; Python cannot sustain 1000 Hz.

```powershell
.\rib-sender.exe --console --no-tray --diagnostics --mac <mac-ip> --interval 2
```

Watch the diagnostics line: `mouse in` should read ~1000 Hz while moving, `net out` ~500 Hz at a
2 ms interval, `loss` 0.0 %, `jitter` well under 1 ms. Check slow, fast, circular, short precise,
flick and diagonal movement. Compare 1 / 2 / 4 ms intervals from the tray menu, which takes effect
without a reconnect.

---

## Scenario 3 - Wi-Fi jitter and packet loss (spec §49, §60)

```bash
for loss in 0 1 3 10; do
  python3 scripts/test-sender.py --host <mac-ip> --circle 4 --rate 500 --loss $loss
done
```

Expected: `missing` in the receiver's report tracks the injected loss, and **the cursor ends up in
the same place regardless of loss** — cumulative totals mean a dropped datagram costs one frame of
smoothness, never a permanent offset. Nothing "breaks" after a loss burst.

---

## Scenario 4 - disconnect (spec §50)

```bash
python3 scripts/test-sender.py --host <mac-ip> --stuck-modifier &
sleep 3 ; kill -9 %1        # sender dies without saying goodbye
```

Expected in the receiver log, immediately (TCP reset) or within one second (heartbeat timeout):

```
WARN session ended: read failed: ... Connection reset by peer
INFO releasing 1 key(s) and 0 button(s): ...
```

Also verify: turning Wi-Fi off on the Mac, quitting the receiver, and sending the Mac to sleep.
In each case the Windows side must return to `ACTIVE TARGET: WINDOWS` on its own — check the tray
status, or the log line `forcing input back to Windows`.

---

## Scenario 5 - modifier recovery (spec §51)

The `--stuck-modifier` run above is exactly this test for Shift. Repeat with the Windows sender by
holding Ctrl, Alt, Command and a mouse button, then pulling the network. After the automatic
reconnect nothing may be logically held: type in TextEdit and confirm no modifier is active, and
that a single click is not interpreted as a drag.

---

## Security checks

```bash
# an unpaired sender is refused
rm ~/.config/remote-input-bridge/test-sender-keys.json
python3 scripts/test-sender.py --host <mac-ip> --handshake-only    # → NOT_PAIRED

# a wrong code is refused (press "Show pairing code" on the Mac first)
python3 scripts/test-sender.py --host <mac-ip> --pair AAAA-BBBB --handshake-only  # → BAD_PROOF
```

Forged datagrams, replayed control frames and packets from a stale session are covered by
`--crypto-selftest` and by `cargo test` (`tampered_frame_is_rejected`, `wrong_key_is_rejected`,
`replayed_counter_is_rejected`).

---

## Metrics worth recording when comparing against Deskflow (spec §52)

| Metric | Where to read it |
|--------|------------------|
| Input polling rate | Windows diagnostics line, `mouse in` |
| Network send rate | same line, `net out` |
| Packets/sec, loss | same line, `loss` (reported by the Mac, not guessed) |
| RTT, jitter | same line |
| Mac event rate | same line, `mac events` |
| CPU / RAM | Task Manager and Activity Monitor. Expect < 1 % idle |

Subjective: smoothness, precision, lag, micro-stutter, acceleration consistency. Test at 60, 120,
144 and 165 Hz on the Windows monitor — the Mac cadence is independent of it.
