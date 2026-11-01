# aw-timer

[繁體中文](README.zh-TW.md)

A Rize-style automatic focus timer for macOS, built on top of [ActivityWatch](https://activitywatch.net/).
It lives in the menu bar as a tiny animated Sisyphus: pushing his boulder while you work, resting under a starry sky while you take a break.

All tracking data stays on your Mac. No account, no cloud.

![Menu bar animation frames](assets/sisyphus-preview.png)

*Top to bottom: working, working with music (he sings), break starting (the boulder rolls away), break with music (headphones, stars, shooting stars).*

> The user interface (menus, notifications, report) is in Traditional Chinese.

## Features

- **Automatic tracking.** Reads ActivityWatch's window and AFK data and sorts every app or page into focus, neutral or distraction using rules you can edit.
- **Menu bar timer.** The top line shows the minutes of your current focus streak, and the bottom line shows today's focus hours (e.g. `25m / 3.2h`). The animation runs on Core Animation and uses about 0% CPU.
- **Focus sessions.** A focus session is at least 25 focused minutes with no interruption of 5 minutes or more.
- **Notifications.**
  - A break reminder after 50 minutes at the computer since your last real break (any activity counts, not just focus).
  - Get back on track after 10 minutes of distraction.
  - A message when a focus session starts.
- **Forced breaks** of 5, 10, 15 or 30 minutes, or until you end them.
- **Focus goal** that you type in. It appears in the menu, in notifications and in the report.
- **Projects.** Time is assigned to a project by window-title rules, by the working folder of your [Claude Code](https://claude.com/claude-code) session, or by a project you pick from the menu.
- **Daily report** in the terminal (`aw-timer report -i`), where you can also reclassify items interactively.
- **Background sound.** Right-click the icon to play or pause.
  - Free internet radio: Lofi, Jazz, Lounge, and 97.7 Classical (好家庭).
  - Procedurally generated rain, coffee shop, white, pink and brown noise, and binaural beats.
  - Spotify and [Feishin](https://github.com/jeffvli/feishin) control. These pause automatically when you switch sources.
  - Volume always follows the system volume. aw-timer never changes it.

## Requirements

| | |
|---|---|
| macOS | **11 Big Sur or later**, on Apple Silicon or Intel (universal binary). The build targets macOS 11.0; it has only been tested on macOS 27 (Apple Silicon). |
| [ActivityWatch](https://activitywatch.net/) | Required. Tested with v0.14.1. Grant it the permissions it asks for so window titles can be read. |
| Spotify / Feishin | Optional. To use Feishin, turn on its remote-control server (Settings) and put its password in `config.toml`. |
| Building from source | [Rust](https://rustup.rs) 1.88+ and the Xcode Command Line Tools (`xcode-select --install`). |

## Install

1. Install and start ActivityWatch:

   ```bash
   brew install --cask activitywatch
   ```

   ```bash
   open -a ActivityWatch
   ```

2. Install aw-timer with **Homebrew** (builds from source; Homebrew installs Rust for the build if needed):

   ```bash
   brew tap samuel20040521/tap && brew trust samuel20040521/tap
   ```

   ```bash
   brew install aw-timer && brew services start aw-timer
   ```

   `brew services` starts aw-timer now and at every login. Update with `brew upgrade aw-timer`; remove with `brew services stop aw-timer && brew uninstall aw-timer`.

   **Or** download the prebuilt release and run the installer:

   ```bash
   curl -L -o aw-timer.tar.gz https://github.com/Samuel20040521/aw-timer/releases/latest/download/aw-timer-macos-universal.tar.gz
   ```

   ```bash
   tar xzf aw-timer.tar.gz && cd aw-timer-macos-universal && ./install.sh
   ```

   The installer does the following:
   - copies the binary to `~/Library/Application Support/aw-timer/`;
   - links it to `~/.local/bin/aw-timer`;
   - creates `~/.config/aw-timer/config.toml`, but only if you don't already have one;
   - registers a LaunchAgent so aw-timer starts at login.

> **About Gatekeeper:** the binary is not signed by an Apple Developer ID. Files downloaded with `curl`, as shown above, run normally. If you downloaded the archive in a browser, macOS will block it. You can review the code and build it yourself, or remove the quarantine flag at your own discretion: `xattr -dr com.apple.quarantine aw-timer-macos-universal`.

### Build from source

```bash
git clone https://github.com/Samuel20040521/aw-timer.git
```

```bash
cd aw-timer && ./install.sh
```

## Usage

| Action | Effect |
|---|---|
| Left-click the icon | Open the menu: rest, goal, project, background sound, report |
| Right-click (or Control-click) | Play or pause background sound |
| `aw-timer status` | Current state |
| `aw-timer report [-1 \| YYYY-MM-DD] [-i]` | Daily report. `-i` lets you classify items as focus, entertainment or neutral |
| `aw-timer restart` | Reload after editing the config |

Configuration lives in `~/.config/aw-timer/config.toml`: thresholds, category rules, projects and sound URLs. Every option is documented in [`default_config.toml`](default_config.toml).

## Privacy

- Everything runs locally. aw-timer reads ActivityWatch's local API at `localhost:5600`.
- To tell which Claude Code session you were in, aw-timer reads only the `timestamp`, `cwd`, `sessionId` and session-title fields of the transcripts in `~/.claude/projects/`. Message content is never stored or sent anywhere.
- Network access happens only when you play an internet radio station.

## Uninstall

```bash
./uninstall.sh
```

The uninstaller removes the LaunchAgent and the binary. Your config in `~/.config/aw-timer/` is kept.

## Credits

- [ActivityWatch](https://activitywatch.net/) provides the tracking data.
- [Symphonia](https://github.com/pdeljanov/Symphonia) decodes audio and [rodio](https://github.com/RustAudio/rodio) plays it.
- [objc2](https://github.com/madsmtm/objc2) provides the AppKit, Core Animation and CoreGraphics bindings.
- Station URLs come from the community [Radio Browser](https://www.radio-browser.info/) directory. Radio content belongs to its stations.

## License

[MIT](LICENSE)
