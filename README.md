# Font Changer

[English](README.md) · [Русский](README.ru.md)

Fast, cross-platform batch changer of **fonts and font sizes** in Word, Excel, PowerPoint, LibreOffice, RTF, HTML and CSS files.
Built with Rust + Tauri, same look and feel as [Text Replacer](https://github.com/Pavel-us/TextReplacer). EN/RU interface.

## What it does

Choose a folder, say *what to find* and *what to change it to*, press **Preview**, then **Change**:

| Find | Change to | Result |
|---|---|---|
| Arial | Calibri | every Arial becomes Calibri, sizes stay |
| *(any font)* | Times New Roman | all fonts unified |
| *(any size)* | 14 | all text gets 14 pt |
| 10 pt | 11 pt | only 10 pt text grows |
| *(any)* + *(any)* | Times New Roman + 14 | the classic "official document" format |

Several rules can be queued in a list or loaded from a tab-separated file.

**Font and size are matched independently.** The rule *Arial 12 → Calibri 14* changes every Arial to Calibri **and** every 12 pt size to 14 pt — it does not mean "only 12 pt Arial text".

## Supported formats

| Format | What is changed |
|---|---|
| Word `.docx .docm .dotx .dotm` | text, styles, defaults, headers/footers, footnotes, lists, theme fonts |
| Excel `.xlsx .xlsm .xltx .xltm` | cell fonts, rich text in cells, comments, text boxes, theme fonts |
| PowerPoint `.pptx .pptm .ppsx .potx …` | slides, layouts, masters, notes, SmartArt, theme fonts |
| LibreOffice / OpenOffice `.odt .ods .odp .odg` + templates | all text styles, default styles |
| RTF `.rtf` | font table and `\fs` sizes; text bytes are never touched |
| HTML / CSS `.html .htm .xhtml .css` | `font-family`, `font-size`, `font` shorthand, `<font face>` (sizes written in pt; `@font-face` never renamed) |

**Not supported** (reported as "skipped"): PDF (text is drawn with embedded fonts), old binary `.doc .xls .ppt` (save as `.docx/.xlsx/.pptx` first). Plain text has no fonts and is ignored.
Not changed inside supported files: charts, embedded objects, pictures, embedded font files.

## Safe by design

- **Preview** shows exactly what would change; nothing is written.
- `.bak` backups (on by default), atomic writes (temporary file + rename).
- **Symbol fonts** (Symbol, Wingdings, Webdings, …) are kept by "any font" rules, so bullets and icons survive.
- "Any font" changes Latin / Cyrillic / Greek fonts; fonts of Asian and complex scripts (CJK, Arabic, Hebrew, Thai…) are kept unless you switch that option on. A font you name explicitly is always replaced.
- Theme fonts are rewritten too, so text that follows the theme changes as well.
- A damaged or password-protected file is reported and never stops the run.
- Changing a font or size can change page count and layout — check the result.

## Scanning: which fonts are in my files?

When you pick a folder (or change the mask), Font Changer scans the files in the background with the same parser that does the real change, so the list always matches what can actually be changed.

- **One font found** – it is shown in the *Find → Font* field as grey text (the field itself stays empty, which means "any font").
- **Several fonts found** – the field says "any font"; click it to open a list of everything found, with the number of files and tags for symbol fonts and other scripts. Choosing an entry fills the field.
- Sizes work the same way. A summary line shows the totals.
- Automatic scanning can be switched off (then use the **Scan** button). A new scan cancels the previous one.

### Where does a size come from?

Documents contain sizes that are not visible on the page, so the size list is split in two:

- **in text** – the size of visible text. Only these count for the grey "found" hint.
- **not visible in the text** (grey) – sizes that are only **in styles** (document defaults, style definitions, list levels, slide masters) or **hidden** (the paragraph mark ¶, empty runs, spaces, tabs). A 1 pt paragraph mark is how thin spacer lines are usually made.

Below the found sizes the list also offers common sizes and common fonts, so it works even when scanning is switched off. A font name such as `inherit` (left over from copying a web page) is marked "invalid name".
In Excel all cell sizes count as text; for HTML and CSS the format does not say, so they count as text too.

**Protection of tiny sizes.** "Any size → X" leaves sizes **below 6 pt** alone by default (setting in the side panel, 0 = off), so spacer lines keep their height. A rule with an explicit source size (e.g. `1 → 2`) always applies.

## Rule templates (`.fctpl`)

**Save rules…** writes the rule list (or the filled-in fields) to a `.fctpl` file; **Load rules…** reads it back, together with the mask and the two options that change the result (symbol fonts, other scripts). A `.fctpl` file is plain JSON:

```json
{
  "format": "fctpl",
  "version": 1,
  "name": "official-docs",
  "mask": "*.docx",
  "options": { "keepSymbols": true, "scripts": false },
  "rules": [
    { "fromFont": "Arial", "toFont": "Calibri", "fromSize": "", "toSize": "" },
    { "fromFont": "", "toFont": "Times New Roman", "fromSize": "", "toSize": "14" }
  ]
}
```

Empty strings mean "any" (left side) or "unchanged" (right side). Unknown fields are ignored, and files with a newer `version` are refused with a clear message.
Old tab-separated rule files (`from font ⇥ to font ⇥ from size ⇥ to size`, `#` = comment) can still be loaded.

## Build

```
npm install
npm run dev            # run
npm run test           # translation check + engine tests (cargo test)
npm run build          # installers
```

Requires Rust, Node.js 18+ and the [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/).

## Installing on Linux (Linux Mint and others)

The program is listed under **Office** in the application menu.

- **.deb (recommended):** `npm run build:deb`, then `sudo apt install ./src-tauri/target/release/bundle/deb/*.deb`. The menu entry, icon and the "Office" category are installed with it, and `sudo apt remove font-changer` removes it.
- **AppImage:** `npm run build:appimage`, then `scripts/install-appimage-linux.sh "src-tauri/target/release/bundle/appimage/<file>.AppImage"` copies it to `~/.local/share/font-changer` and adds the menu entry (no root needed). `scripts/install-appimage-linux.sh --uninstall` removes it.

If the entry does not show up at once, log out and in again or run `update-desktop-database ~/.local/share/applications`.

## Releases and automatic updates

Identical to Text Replacer: the release workflow signs update packages with `TAURI_SIGNING_PRIVATE_KEY`.
`src-tauri/tauri.conf.json` currently contains the **same public key as Text Replacer**, so you can reuse the same private key as a repository secret.
To use a separate key: `npx tauri signer generate -w ~/.tauri/fontchanger.key`, then `node scripts/set-updater-key.mjs ~/.tauri/fontchanger.key.pub`.
Replace the placeholder icons with `npm run icons` after changing `app-icon.png`.

## Languages

The interface is available in English and Russian. A language can be added **without rebuilding**: open the gear menu and press 📂 – it opens the folder for custom translations (`locales` inside the application's config folder, e.g. `~/.config/dev.fontchanger.app/locales` on Linux). Put a `<code>.json` file there (copy [`src/locales/en.json`](src/locales/en.json) as a template and set `_meta.code` and `_meta.name`), then restart the program. Files whose names start with `_` are ignored. To contribute a translation to the project, see [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
