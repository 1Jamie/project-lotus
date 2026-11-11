# Nested external apps (Linux)

Lotus can host nested Wayland/Xwayland clients inside a pane (games surfaces, sidecars). The capability is always linked on Linux builds; **runtime stays idle until used**.

## Opt-in

| Step | API | Effect |
|------|-----|--------|
| 1 | `win.createExternalAppPane(id, opts)` | Starts the nested compositor on first call for that window; spawns the child |
| 2 | `win.setGameMode(true)` | Routes keyboard/pointer to the primary external pane |

- `game_mode` defaults to **false** on every window.
- `setGameMode(true)` is ignored until the nested compositor exists. The check runs on the **winit event loop** so it is safe to call immediately after `createExternalAppPane` (same FIFO queue).

Gamescope / Steam Game Mode host tagging still runs when the session is detected, with or without nesting.

## Nest window APIs

```js
win.createExternalAppPane(id, {
  command, args, anchor, zIndex, visible,
  pointerPassthrough,
  dbusNotifications, dbusNotificationApps, // optional Notify bridge
});

win.setGameMode(enabled);
win.setFullscreen(enabled);

// Window-scoped host hotkeys (PhysicalKey / KeyboardEvent.code).
// Work with an empty nest; consume only when the table is non-empty.
win.registerHostHotkey({ id, key: 'F12', modifiers: { ctrl?, alt?, shift?, super? } });
win.unregisterHostHotkey(id);
win.clearHostHotkeys();

pane.setPassthrough(bool);
pane.zIndex = n;
```

## Host hotkeys

Matching uses **W3C `KeyboardEvent.code`** → `PhysicalKey::Code` plus Ctrl|Alt|Shift|Super. CapsLock / NumLock / ScrollLock are ignored.

Pressed matches emit renderer IPC `nest:hotkey` `{ id }`. Matching Released for consumed presses is also consumed so clients do not see orphan releases. Leaving game mode clears keyboard focus on the nested seat (`wl_keyboard::leave`).

## Renderer IPC (engine → panes)

| Channel | Payload |
|---------|---------|
| `nest:hotkey` | `{ id }` |
| `nest:game-mode` | `{ game_mode: boolean }` |
| `nest:fullscreen` | `{ fullscreen: boolean }` |

Product apps (e.g. atrium) forward these to Node and own UX. Atrium lives under `lib/atrium/`, `bin/atrium`, and `test_apps/atrium_hud_app/` — it is a consumer, not part of the nest engine contract.

## Atrium (reference consumer)

```bash
./bin/atrium -- %command%
# or: npm run atrium --workspace=@lotus-gui/core
```

Profile keybinds: `~/.config/lotus-atrium/overlay-profile.json` → `keybinds`.
