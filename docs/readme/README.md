# README translations

The canonical English README stays at [README.md](../../README.md). Each other language keeps its README and editable SVG diagrams together under `docs/readme/<LANG>/`, using the language tags below. The list follows the maintainer's requested order.

| Language         | Tag     | README text                                   | Diagrams                                                                                        |
| ---------------- | ------- | --------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| English          | `en`    | [Canonical source](../../README.md)           | [Shared English artwork](../assets/readme/)                                                     |
| Français         | `fr`    | [Complete translation](fr/README.fr.md)       | [Architecture](fr/architecture.fr.svg), [task lifecycle](fr/queue-lifecycle.fr.svg)             |
| Deutsch          | `de`    | [Complete translation](de/README.de.md)       | [Architecture](de/architecture.de.svg), [task lifecycle](de/queue-lifecycle.de.svg)             |
| Italiano         | `it`    | [Complete translation](it/README.it.md)       | [Architecture](it/architecture.it.svg), [task lifecycle](it/queue-lifecycle.it.svg)             |
| 日本語           | `ja`    | [Complete translation](ja/README.ja.md)       | [Architecture](ja/architecture.ja.svg), [task lifecycle](ja/queue-lifecycle.ja.svg)             |
| Español          | `es`    | [Complete translation](es/README.es.md)       | [Architecture](es/architecture.es.svg), [task lifecycle](es/queue-lifecycle.es.svg)             |
| 한국어           | `ko`    | [Complete translation](ko/README.ko.md)       | [Architecture](ko/architecture.ko.svg), [task lifecycle](ko/queue-lifecycle.ko.svg)             |
| 简体中文         | `zh-CN` | [Complete translation](zh-CN/README.zh-CN.md) | [Architecture](zh-CN/architecture.zh-CN.svg), [task lifecycle](zh-CN/queue-lifecycle.zh-CN.svg) |
| 繁體中文         | `zh-TW` | [Complete translation](zh-TW/README.zh-TW.md) | [Architecture](zh-TW/architecture.zh-TW.svg), [task lifecycle](zh-TW/queue-lifecycle.zh-TW.svg) |
| Русский          | `ru`    | [Complete translation](ru/README.ru.md)       | [Architecture](ru/architecture.ru.svg), [task lifecycle](ru/queue-lifecycle.ru.svg)             |
| Bahasa Indonesia | `id`    | [Complete translation](id/README.id.md)       | [Architecture](id/architecture.id.svg), [task lifecycle](id/queue-lifecycle.id.svg)             |
| Português (BR)   | `pt-BR` | [Complete translation](pt-BR/README.pt-BR.md) | [Architecture](pt-BR/architecture.pt-BR.svg), [task lifecycle](pt-BR/queue-lifecycle.pt-BR.svg) |

## Layout

```text
README.md                              # canonical English source at repository root
docs/assets/readme/                    # shared hero and English SVG sources
docs/readme/
  README.md                            # language inventory and maintenance guidance
  <LANG>/
    README.<LANG>.md
    architecture.<LANG>.svg
    queue-lifecycle.<LANG>.svg
```

## Complete a translation

- New editions start as scaffolds: they preserve the English section order, executable Bash examples, and command-reference rows, with prose TODOs pointing to the canonical source.
- Translate prose, headings, image alt text, and code comments. Preserve command names, flags, paths, identifiers, output schemas, example values, and numerical limits. Do not turn optional capabilities into requirements or resource annotations into enforced allocation.
- Keep the language switcher in the requested order. From a locale directory, the English README is `../../../README.md`, other languages are `../<LANG>/README.<LANG>.md`, the design spec is `../../design.md`, and shared artwork is `../../assets/readme/`. Localized diagrams are sibling files.
- Update page-fragment links when translating headings. Check all relative links from the locale directory.
- Keep SVG text editable. Translate visible labels and the accessible `title`/`desc`, retain literal directory names and technical identifiers, set `lang`/`xml:lang`, and use a font stack covering the target script. Render both SVGs to check glyphs, clipping, and arrows.
- When an edition is complete, remove its scaffold notice and TODO comments, update its status in this table, and adjust the root README's status note. Review the matching README and both SVGs whenever the English source changes.

Run `just fmt` and `just fmt-check` for Markdown formatting. Verify links and image rendering separately; formatting alone does not validate translation quality or diagram layout.
