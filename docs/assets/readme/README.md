# README artwork

These assets support the English project homepage in [README.md](../../../README.md).

| Asset                                      | Source                                                                                                                                                                  |
| ------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [hero.png](hero.png)                       | Generated with the built-in imagegen tool on 2026-10-21. The generator's model identifier was not exposed. This is a conceptual illustration of the rail-yard metaphor. |
| [architecture.svg](architecture.svg)       | Hand-authored, editable SVG. Describes the server/client responsibilities and SSH transport in [design spec §2](../../design.md#2-architecture).                        |
| [queue-lifecycle.svg](queue-lifecycle.svg) | Hand-authored, editable SVG. Summarizes normal task transitions, orphan recovery, and opt-in shutdown from [design spec §3](../../design.md#3-client-queue-semantics).  |

Keep technical labels in the SVG sources so they remain editable, searchable, and independent of image generation. Detailed edge cases remain in the design spec. The hero is copied into this directory and requires no external image host.

## Hero generation prompt

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
