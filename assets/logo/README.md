# Impyard logo

The mark is a terminal prompt whose cursor has grown two small horns — an
imp living in a shell. It reads as a CLI prompt and an imp at the same time,
and it survives being shrunk to a favicon.

## Files

| File | Use |
| --- | --- |
| `impyard-icon.svg` | Icon only, tuned for **dark** backgrounds |
| `impyard-icon-light.svg` | Icon only, tuned for **light** backgrounds |
| `impyard-icon-mono.svg` | Single-color mark; inherits `currentColor` (READMEs, terminals, stamps) |
| `impyard-app.svg` | Coal rounded-square tile for avatars / favicons (Discord, Slack, app icon) |
| `impyard-lockup.svg` | Horizontal icon + wordmark, for **dark** backgrounds |
| `impyard-lockup-light.svg` | Horizontal icon + wordmark, for **light** backgrounds |

All assets are flat SVG and render crisp at any size.

## Color

| Token | Hex | Role |
| --- | --- | --- |
| magenta | `#E8438F` | The cursor block — the imp |
| magenta (light bg) | `#CE2F7F` | Cursor block on light grounds |
| horn tint | `#F58ABC` | Horns on dark grounds |
| horn tint (light bg) | `#A82468` | Horns on light grounds |
| slate | `#8C8375` | The prompt chevron (constant on both grounds) |
| coal | `#17120E` | Dark ground |
| paper | `#F3ECE0` | Light ground |

## Type

The wordmark is set in **JetBrains Mono Bold** to keep the CLI feel. In the
lockups it is **converted to vector outlines**, so the SVGs carry no font
dependency and render identically everywhere — GitHub, browsers, design
tools — with no font to install.

JetBrains Mono is free, under the [SIL Open Font License 1.1](https://github.com/JetBrains/JetBrainsMono/blob/master/OFL.txt).
To re-typeset the wordmark (different text or weight), set it in JetBrains
Mono and flatten the text to a path.
