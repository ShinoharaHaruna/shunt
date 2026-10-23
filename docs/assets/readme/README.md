# README artwork

These shared assets support the canonical English project homepage in [README.md](../../../README.md). Localized READMEs and their SVG diagrams live together under [docs/readme/](../../readme/README.md); all editions reuse the shared hero.

| Asset                                      | Source                                                                                                                                                                                                                                                   |
| ------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [hero.png](hero.png)                       | Generated with the built-in imagegen tool on 2026-10-21. The generator's model identifier was not exposed. This is a conceptual illustration of the rail-yard metaphor.                                                                                  |
| [architecture.svg](architecture.svg)       | Hand-authored, editable SVG. Describes server/client responsibilities, independent named queues, and SSH/local transport from [design spec §4.1a](../../design.md#41a-named-queues-within-a-managed-instance) and [§5](../../design.md#5-wire-protocol). |
| [queue-lifecycle.svg](queue-lifecycle.svg) | Hand-authored, editable SVG. Summarizes normal task transitions, orphan recovery, declared retries, and optional drain behavior from [design spec §3](../../design.md#3-client-queue-semantics).                                                         |

Keep technical labels in the SVG sources so they remain editable, searchable, and independent of image generation. Detailed edge cases remain in the design spec. The hero is copied into this directory and requires no external image host.

Each localized diagram translates visible prose and accessibility descriptions while retaining literal directory names and technical identifiers. Its editable text uses a font stack covering the target script. Keep all language variants in sync when changing the English SVG sources; the [translation guide](../../readme/README.md) lists the supported editions and their status.

## Hero generation prompt

The original prompt below records the artwork's 2026-10-21 provenance. Its rented-GPU framing reflects the brief at that time; current project positioning covers local and remote experiment machines, including owned workstations and lab servers.

```text
Use case: stylized-concept
Asset type: wide hero banner for the GitHub README of the open-source project "shunt".
Primary request: Create a refined editorial illustration for a task queue that sends experiments from a developer machine to rented GPU boxes. The name comes from railway shunting: a marshalling yard routes jobs onto tracks.
Scene and subject: an elegant miniature rail-switching yard made of precise dark metal rails on a clean light studio surface. Small task cartridges travel along the rails toward a compact GPU compute module; several finished cartridges rest on a quiet siding. Keep this a conceptual illustration, not a labeled technical diagram. The visual should communicate purposeful routing and orderly work, with beautiful tactile details and soft grounded shadows.
Style and medium: premium product illustration, architectural model photography mixed with restrained industrial design, crisp geometry, calm editorial composition, appropriate for a respected developer tool. Avoid playful toy trains.
Composition: very wide landscape banner, roughly 3:1. Large legible project wordmark in the left third; the rail-switching sculpture occupies the center and right with generous breathing room. Keep all important content well inside the frame so the entire banner can be displayed without cropping.
Text (verbatim): "shunt" in lowercase, large, confident dark sans-serif type. No other text, letters, numbers, or labels.
Color: neutral warm light background, graphite metal, restrained warm signal and cool compute accents.
Constraints: polished but understated; the project wordmark must be perfectly spelled. No logos of other companies, no badges, no terminal windows, no UI screenshots, no charts, no watermarks, no gradients behind the wordmark, no extra slogans.
```
