# Save States

Save what's open on your Windows desktop and come back to it later. A save state
records which apps and windows are open, where they sit, which virtual desktop each is
on, the folders open in File Explorer, the folder or workspace in every VS Code window,
and the tabs in every Chrome window, per Chrome profile. Loading a state reopens what's
missing and puts each window back in place.

Save States lives in the notification area. Its window exists only while it's open:
closing it destroys the window and its GPU context, so the app costs almost nothing
in the background.

## Using it

- **New save state**: name it, choose *this virtual desktop* or *all virtual desktops*
  (you can tell it to remember the choice), and Save. The window gets out of the way
  for the screenshot that becomes the state's thumbnail.
- **Load**: Save States checks what's already open, then asks before doing anything:
  it lists what it will open, what's already open, and offers to also close windows
  that aren't part of the state (off by default; apps get a normal close request, so
  they can still ask you to save).
- **Tray menu**: open the window, quick save, load any state (favorites first), quit.
- Per state: favorite, rename, replace the thumbnail with any image, update it with
  what's open now, delete.

## Building

Requires Rust through [rustup](https://rustup.rs). `rust-toolchain.toml` pins 1.95.0
for this folder only (eframe 0.37 and sysinfo 0.39 need it); rustup fetches it on the
first build.

```
cargo build --release
```

This produces `target\release\savestates-app.exe` and `savestates-bridge.exe`; keep the
two together. Command-line flags:

| Flag | Effect |
|---|---|
| `--minimized` | Start in the tray (for starting with Windows) |
| `--quit` | Ask a running instance to exit |

Launching it again while it runs just brings the running instance forward.

## Chrome tabs: one-time setup

Save States reopens Chrome windows in the right profile on its own. Saving the *tabs*
inside them needs a small companion extension in each Chrome profile. There's no way
around that: Chrome keeps the live session file of a running profile locked
exclusively, and has refused remote debugging on the real profile since Chrome 136.

1. Start Save States once. It registers its Chrome helper (a per-user registry key,
   no admin rights needed).
2. In each Chrome profile, open `chrome://extensions`, turn on **Developer mode**,
   click **Load unpacked**, and choose this repo's `extension` folder.
3. *Chrome integration* in the app shows each profile turn to "Tabs are saved".

Chrome doesn't allow installing extensions from outside the Web Store automatically,
which is why step 2 is manual.

## How it works

```
savestates-app.exe        tray + window, worker thread, named-pipe server
   |  \\.\pipe\SaveStates.v1
savestates-bridge.exe     one per Chrome profile, started by Chrome (native messaging)
   |  stdio
extension (MV3)           pushes this profile's windows and tabs on every change
```

- **Windows**: an alt-tab-style filter over all top-level windows. DWM-cloaked windows
  count as "on another virtual desktop" unless the shell says they're on the current
  one, which marks a suspended UWP ghost. Run `--debug-windows` to see each window's
  verdict.
- **Chrome profile of a window**: Chrome stamps every browser window with a
  per-profile AppUserModelID (`Chrome` for Default, `Chrome.UserData.Profile2` for
  "Profile 2"), so the profile comes straight from the HWND.
- **Chrome tabs**: matched from extension snapshots to live windows by active-tab
  title plus DPI-scaled bounds.
- **VS Code** (and Insiders, VSCodium, Cursor, or anything else built from it, found by
  its `product.json`) records its open windows in `User\globalStorage\storage.json` and
  rewrites that list whenever one of its windows loses focus, which clicking Save States
  always causes. Each entry is paired with a live window by the folder or workspace name
  in the window's title, with bounds breaking ties. Developer tools and floating editor
  windows aren't in that list, so they're left out. On load, each folder or workspace
  opens with `--new-window`, one at a time, and the new window is recognized by its title.
- **Store apps** relaunch by AppUserModelID. **Explorer** windows reopen at their
  folders. **Other apps** relaunch from their executable, with only arguments that
  name an existing file or folder. Replaying full command lines is unsafe.
- **Placement**: new windows are found by diffing the window list (not by PID, since
  launchers hand off), then put back with `SetWindowPlacement` and, for all-desktop
  states, moved to their virtual desktop.
- **Virtual desktops** use [winvd](https://crates.io/crates/winvd), which drives an
  undocumented Windows API. It's probed once and switched off on failure, so a future
  Windows build that breaks it degrades to "everything on the current desktop".

Crates: `savestates-core` (capture, restore, store; no UI), `savestates-app` (eframe
UI and tray), `savestates-bridge` (native-messaging host, `serde_json` only).

## Where things are stored

| What | Where |
|---|---|
| Save states | `%APPDATA%\SaveStates\states.json` |
| Thumbnails, logs, Chrome helper files | `%LOCALAPPDATA%\SaveStates\` |
| Chrome helper registration | `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.savestates.bridge` |

`states.json` is written atomically. A file that fails to parse is moved aside and never
overwritten, and items written by a newer version are kept when an older one saves.

To remove everything: quit the app, delete both `SaveStates` folders and the registry
key, and remove the extension from Chrome.

## Development

```
cargo test --workspace
```

Unit tests cover the restore planner, Chrome window matching and profile mapping,
VS Code window state parsing and title matching, the store's forward-compatibility,
and framing. One integration test runs the real
bridge binary, plays the extension's side over its stdio, and checks every hop
through the named pipe.

Backend commands, no UI needed:

```
savestates-app --debug-windows                 every window and the filter's verdict
savestates-app --debug-capture [--all]         what a save would capture, as JSON
savestates-app --debug-save [name] [--all]     save a state, print its id
savestates-app --debug-list                    list saved states
savestates-app --debug-plan <id>               what loading would do
savestates-app --debug-restore <id>            load a state (never closes anything)
```

Environment variables: `SAVESTATES_HOME` (use a scratch data folder), `SAVESTATES_PIPE`
(use a different pipe name), `SAVESTATES_LOG=debug` (more logging).

## Known limitations

- Without the extension, a profile's Chrome windows come back with a new tab page
  instead of their tabs.
- Pinned tabs, tab groups and the active tab are applied by the extension after
  Chrome opens the window, so they need it connected at load time.
- If Chrome is closed and set to "Continue where you left off", starting it also
  restores Chrome's own last session alongside the saved one.
- Incognito windows are never restored.
- An app's internal state (unsaved documents, which project an editor had open) is
  up to that app. Multi-window apps are launched once and restore their own windows.
- VS Code windows are matched by the folder name in their titles. With a custom
  `window.title` that leaves it out, VS Code is treated like any other multi-window app.
  Unsaved editors come back through VS Code's own hot exit, and floating editor windows
  aren't recreated.
- Explorer tabs come back as separate windows; Windows has no API to open a tab.
- Apps that need administrator rights are reported rather than triggering a UAC
  prompt mid-restore.
- Once the window has been opened, the OpenGL driver keeps some memory committed
  after it closes (about 55 MB on an NVIDIA GPU, almost none of it resident).
  Starting with `--minimized` and never opening the window stays at about 3 MB.
