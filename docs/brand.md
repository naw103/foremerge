# Brand

Foremerge has one mark, one palette, two typefaces, and a set of rules that keep
them honest. This document is the working reference for anyone touching a
Foremerge surface: the README, this docs directory, the CLI, the website, a
conference slide, or a screenshot in an issue.

One rule sits above the rest. **Nothing here is decoration.** If a rule cannot be
justified by what the product does, it is not a rule. The mark is a measurement
of the product's idea: anyone may use it, nobody may redraw it.

The full specification is the Foremerge Brand Book, a 57-page identity manual
kept with the maintainer's operational material. This page is the part that
contributors need, and it is authoritative for everything it covers.

## Assets

Every asset the public surfaces use is served from `foremerge.com`. SVG is the
source; PNG and JPG are generated and must never be hand-edited.

| Asset | URL |
| --- | --- |
| Favicon, tight box (`viewBox 2 8 44 32`) | `https://foremerge.com/favicon.svg` |
| Apple touch icon | `https://foremerge.com/app-icon-180.png` |
| Open Graph / Twitter card | `https://foremerge.com/og-1200x630.png` |
| Marks (color, reversed, mono, currentColor, animated) | `https://foremerge.com/brand/svg/mark*.svg` |
| Horizontal lockups (light, dark, mono) | `https://foremerge.com/brand/svg/lockup-horizontal-*.svg` |
| Stacked lockups | `https://foremerge.com/brand/svg/lockup-stacked-*.svg` |
| App icon tiles | `https://foremerge.com/brand/svg/icon-tile*.svg` |
| App icon rasters (192, 512, 1024) | `https://foremerge.com/brand/png/app-icon-*.png` |
| Lockup rasters | `https://foremerge.com/brand/png/lockup-horizontal-*.png` |
| GitHub social preview, 1280x640 | `https://foremerge.com/brand/png/github-social-preview-1280x640.png` |
| Icon set, 14 icons | `https://foremerge.com/brand/icons/<name>.svg` |

The wordmark in every SVG lockup is already outlined, so lockups render
identically on machines without Space Grotesk installed. Do not re-typeset them.

Assets not served here, because no public surface needs them yet: stickers,
slide templates, social avatars and banners, launch-listing graphics, and the
no-alpha JPGs. Ask the maintainer if a surface needs one.

## The mark

Two scopes, interlocked, sharing one measured region. It is a diagram of the
product's single idea: two plans can be complete, isolated, and still contend,
and the contention is the only thing lit.

Construction, on a 48-unit grid:

- Two diamonds, half-diagonal 13, centred at `x = 18` and `x = 30`
- Stroke 4.5, round joins
- Outer corner radius 3.2, inner radius 1.6
- The orange region is the geometric intersection of the two scopes, 14 x 14

Three rules are load-bearing. Change any one and the mark stops describing the
product:

1. **The offset stays 12 of 26.** Closer and the shapes fuse into a bowtie, so
   there is one shape and no contention. Further and the intersection
   disappears.
2. **The lens is measured, not placed.** It is the exact geometric intersection.
   It cannot be resized, recentred, or given its own outline.
3. **Radius 3.2 outside, 1.6 inside.** Do not soften the two inner vertices to
   match the outer ones. That turns a contention symbol into a partnership
   symbol.

Never stretch, skew, recolor, fade the lens, redraw the weave, or place the mark
on busy or low-contrast ground. Where a process cannot hold the weave, use the
mono mark rather than simplifying the geometry.

Use the mark alone wherever the lockup will not fit or the name is already
present: favicon, app badge, avatar, embroidery.

## Wordmark and lockups

Space Grotesk SemiBold (600), tracking -4%. One word, capital F: **Foremerge**.
Never `ForeMerge`, `fore-merge`, or `FOREMERGE`. Lowercase `foremerge` only as
the CLI binary name.

Horizontal is the default lockup: website header, README, docs, talk titles.
Stacked is for square surfaces only. The gap between mark and word is 20% of the
mark's width, never eyeballed. Six approved files and no others.

**Clear space.** Keep an exclusion zone equal to the lens height on all four
sides: 7 units on the 48-unit grid. At a 48px mark that is 7px; at 192px it is
28px. Nothing enters it, including other logos and rules.

**Minimum size.** Never take the lockup below 24px in height. Below that use the
mark alone; below 20px use the tight-box favicon. Icons ship at 16, 20, and 24
only; below 16 use the status dot.

Never use the full lockup as an avatar. At 32px in a comment thread the wordmark
is illegible. The square badge is the avatar, always.

## Color

Signal orange is the finding, and nothing else. It is never a background, never
a mood, and never a second accent. Keep it under 5% of any surface.

| Role | Hex | Job |
| --- | --- | --- |
| Signal | `#FF6A2C` | The finding, and HIGH severity |
| Ink | `#0F1317` | Type on light, ground on dark |
| Paper | `#F7F5F1` | Warm; never pure white on screen |
| Rust | `#C1471A` | Signal darkened for text and links on light |
| Graphite | `#3E464C` | Body copy on light, default icon color |
| Slate | `#6B747C` | Labels and borders, 20px+ only |
| Mist | `#B9C0C6` | Body copy on ink |
| Fog | `#E6E1D8` | Rules, chips, panel fills on paper |
| Panel | `#161B20` | Cards and code blocks on ink |
| Line | `#262D33` | Hairlines on ink, 1px, never 2 |
| Ash | `#8A9199` | Secondary text on ink |

Dark is the default for anything terminal-adjacent: the site, OG cards, CLI
docs. Three steps only on dark, ground then panel then line.

**Severity.** Severity is the only place color makes a claim.

| Severity | Hex | Meaning |
| --- | --- | --- |
| HIGH | `#FF6A2C` | Both plans cannot be true. Blocks acceptance unless deliberately overridden with a stated reason |
| MEDIUM | `#E8A33D` | Likely rework. Worth a message between agents before either commits to an approach |
| LOW | `#6B747C` | Shared context worth knowing. Neutral grey on purpose: information, not alarm |
| RESOLVED | `#3E8E5A` | Recorded resolution with a rationale. The only green in the system, and it must be earned |

**Lifecycle.** Seven states, one ramp with a meaning: grey while it is only
declared, amber while it is a candidate, green once Foremerge itself verified
it, ink once Git has it.

| State | Hex | Band |
| --- | --- | --- |
| `INTENT` | `#B9C0C6` | Declared. Nothing proven yet, so nothing gets a hue |
| `CLAIMED` | `#8A9199` | Declared |
| `IN_PROGRESS` | `#6B747C` | Declared |
| `PROVISIONAL` | `#E8A33D` | Candidate. A ChangeSet exists but its evidence is self-reported |
| `VALIDATED` | `#3E8E5A` | Proven. Foremerge ran the check against the exact fingerprint |
| `ACCEPTED` | `#2F7A4B` | Proven |
| `COMMITTED` | `#0F1317` | Landed. Git owns it; the state is a receipt, not a status |

**Contrast, measured rather than assumed.** The one trap: signal orange is
beautiful and fails as body text on paper at 2.5:1.

| Pairing | Ratio | Use |
| --- | --- | --- |
| Ink on paper | 17.6:1 | Any size. The default pairing |
| Graphite on paper | 8.9:1 | Body copy, preferred over pure ink at length |
| Slate on paper | 4.3:1 | 20px and above only. Never body copy |
| Paper on ink | 16.6:1 | Any size, reversed default |
| Signal on ink | 6.6:1 | Safe for labels and severity tokens on dark |
| Signal on paper | 2.5:1 | Fills and badges only, never text |

Links are Rust `#C1471A` on light, underlined, darkening to ink on hover. On ink
they are signal lightened to `#FF8B5C` for the 4.5:1 floor. Never signal orange
as inline link text on light; never Rust on dark.

Primary actions are ink on light and paper on dark. Signal is a fill for at most
one action per view.

Two ramp steps sit below 4.5:1 as small text on dark and are known exceptions
rather than mistakes: `IN_PROGRESS` at 3.9:1 and `ACCEPTED` at 3.6:1 on ink.
They are deliberately quiet steps in a ramp whose loud end carries the meaning.
Never rely on either color alone to convey state; the word is always present.

No CMYK build is specified. Convert from the sRGB values with the printer's
profile and approve a proof. A guessed CMYK build is worse than none.

## Typography

**Space Grotesk** for headings (600, tracking -4%) and emphasis (500, -2%).

**IBM Plex Mono** for labels (500, tracking +14%), body copy, and anything the
machine said: scopes, rule ids, model names, fingerprints, commands, output. If
a value came out of the store, it is set in mono. That is how a reader knows a
person did not write it. Never IBM Plex Mono for headings above 40px.

Both are open-licensed, so any contributor, vendor, or CI job can use the real
files without a license conversation.

The scale, six steps at a 16px base. Pick a step; never interpolate.

| Step | Size / leading | Tracking |
| --- | --- | --- |
| Display | 76 / 0.95 | -4% |
| H1 | 48 / 1.1 | -4% |
| H2 | 32 / 1.2 | -4% |
| Lead | 22 / 1.55 | — |
| Body | 17 / 1.6 | — |
| Label | 12 / 1 | +14% |

## Icons

One set, drawn for this domain. No Lucide, no emoji, no borrowed devops clip
art. Twelve concept icons plus a chevron and a status dot cover every current
surface. Adding an icon means the protocol grew, not that a page needed
decoration.

| Icon | Means |
| --- | --- |
| `intent` | Announced planned work, before code exists |
| `claim` | A leased, advisory hold on a semantic scope |
| `scope` | A closed region one agent has declared |
| `finding` | The measured contention between two scopes |
| `severity` | The weight a finding carries |
| `changeset` | Recorded implementation, tests, decisions, provenance |
| `validation` | A check Foremerge itself executed |
| `agent` | A registered agent with model provenance |
| `worktree` | One agent's isolated checkout |
| `merge-gate` | The gates acceptance applies |
| `provenance` | Who, which model, which fingerprint |
| `event-chain` | The hash-chained semantic event log |
| `chevron` | Disclosure and direction |
| `status` | A thing the store knows about |

Drawing spec:

1. 24 x 24 grid with 2px padding, so a 20 x 20 live area. Nothing touches the
   edge.
2. 2px stroke, round caps and joins, no fills except the signature dot.
3. Rectangles carry a 2-unit corner radius, echoing the mark's outer corners.
4. The filled dot is radius 1.2 to 2 and means "a thing the store knows about".
   It is the set's signature, and it echoes the lens.
5. Only the `finding` icon and the status dot may be orange. Everything else is
   graphite, or paper on ink.
6. Sizes 16, 20, and 24 only. Below 16 use the status dot; above 24 use the mark.

The SVGs use `stroke="currentColor"`, so set the color in CSS. Three dressings
exist, quietest first: plain line (the default everywhere), tint circle, and
solid badge. Use at most two dressings per view, and the solid badge at most
once. It is the loudest thing on any page that is not the mark.

Migrating older material:

- emoji → drop entirely
- `git-branch`, `git-fork` → `worktree`
- `lock`, `shield` → `claim`, never a padlock, because claims do not lock
- `alert-triangle` → `severity`
- `check-circle` → `validation`, and only after Foremerge ran it

## Diagrams

Foremerge explains itself with graphs, so diagrams are a brand surface. Nodes
are filled dots, r6 at 560 wide; the finding node is r9 and orange, and it is
the only orange in the frame. Edges are 1.5px graphite hairlines with no
arrowheads, because the graph is a record and not a flowchart. Labels are mono
15px: sentence case for people, exact case for machine values. Time runs left to
right and agents stack vertically; never radial, never isometric, never 3D.

Green appears only after verification. A diagram showing green before a
validation step is wrong about the product.

## CLI output

The binary is the product's main surface, so its color is brand. Color the
token, never the line: a wall of orange is unreadable and dishonest about
severity.

1. Two-space indent, aligned columns, severity token first.
2. Rule ids always visible, dimmed. A finding without its id is unarguable.
3. No ASCII-art wordmark on start-up. The binary is not a brand moment.
4. Everything printable with `--json`. Human output is a courtesy; machine
   output is the contract.

Degradation ladder: truecolor uses the brand hex as specified; 256-color uses
209, 179, 244, and 71; 8-color uses red, yellow, bright-black, and green; and
with `NO_COLOR` set the output is plain text with severity spelled out.

Never color by agent, model, or provider. The protocol carries nothing
provider-specific and neither does its palette.

Never print a severity word without its color, and never print a color without
its word. Someone is reading this over SSH with `NO_COLOR` set.

## README and docs

The README is the front door and it is read on a phone as often as a desktop.
One lockup, one sentence, one command, then the honest status note, before any
feature list.

- Header image: the horizontal lockup at 240px, dark-mode safe
- Badges: three at most, flat style
- Status note: a blockquote, above the feature list
- Screenshots: real output, labelled, never mocked

Order, always: problem statement, then quickstart, then what the project
deliberately does not claim.

Any HTML surface, including the site and any generated docs page, ships the same
head block:

```html
<link rel="icon" href="/favicon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/app-icon-180.png">
<meta name="theme-color" content="#0F1317">
<meta property="og:image" content="https://foremerge.com/og-1200x630.png">
<meta property="og:title" content="Foremerge">
<meta property="og:description"
      content="Catch intent conflicts before code conflicts.">
<meta name="twitter:card" content="summary_large_image">
```

Use `favicon.svg`, not the padded `0 0 48 48` mark, which loses a third of its
size to whitespace in a tab. OG images are 1200 x 630 on ink ground: paper
lockup, one line of tagline, no screenshot. Never the light lockup on white,
because it disappears in dark-mode previews.

## Voice

The test for every sentence: could a sceptical engineer check it? If not, it is
not shipping copy, it is marketing.

- **Plain-spoken.** A maintainer's voice, not a company's. Short sentences,
  ordinary words, no unglossed jargon. If a reader needs a glossary, the
  sentence is wrong.
- **De-hyped.** No "revolutionary", no "seamless", no "AI-powered". State the
  limitation in the same breath as the capability.
- **Precise.** Name the thing exactly: intent, claim, scope, ChangeSet,
  fingerprint. Product nouns are capitalised as the code capitalises them and
  never swapped for synonyms.
- **Advisory.** Foremerge raises a finding and suggests; the human decides. Copy
  that instructs where the product only advises is off-brand.

Worked examples:

| Topic | Say | Not |
| --- | --- | --- |
| Detection | Deterministic rules compare declared scopes before implementation. It can miss synonyms and warn on compatible work. | AI-powered semantic understanding catches every conflict automatically. |
| Claims | Claims are leased and advisory. Overlap produces a warning and shared context, never a lock. | Lock a symbol so no other agent can touch it. |
| Evidence | Passing validation proves the recorded command passed for the recorded fingerprint. Nothing more. | Verified safe to merge. |

Banned in listings and launch copy: revolutionary, seamless, effortless,
AI-powered, game-changing, and any performance claim before benchmarks are
published.

## Governance

The kit is reviewed like code.

1. SVG is the source. PNG and JPG are generated, never hand-edited.
2. Naming is `component-variant-size.ext`, lowercase and hyphenated.
3. A geometry change is a major version of the kit and needs a changelog note
   saying what moved and why.
4. Outline the wordmark before handing lockups to anyone without Space Grotesk
   installed.

If a surface needs something this kit does not have, open an issue describing
the surface and its constraints, not a redrawn logo.
