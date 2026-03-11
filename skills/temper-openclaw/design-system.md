# Temper Design System

*The default look. This file is pluggable — replace it with your own and every generated UI follows your aesthetic instead.*

---

## Our Palette

These are the named colors. Use the names, not raw hex. When an agent reads this file, these names become the vocabulary.

```
── Surfaces ──────────────────────────────────────────────
  s1   #0A0A0A   page background
  s2   #0F0F0F   card / panel
  s3   #141414   elevated — dropdowns, modals
  s4   #1C1C1C   active / hover

── Text ──────────────────────────────────────────────────
  t1   #D4D4D4   primary — read this
  t2   #8A8A8A   secondary — supporting
  t3   #4A4A4A   muted — labels, whispers

── Accent — Blue Steel ───────────────────────────────────
  c1   #7dd3fc   steel — primary actions, focus, highlights
  c1-dim  rgba(125,211,252,0.08)   tinted backgrounds
  c2   #fef08a   straw — secondary accent, warnings
  c2-dim  rgba(254,240,138,0.07)   tinted backgrounds
  c3   #94a3b8   gunmetal — tertiary, neutral accent
  c3-dim  rgba(148,163,184,0.07)   tinted backgrounds

── Status ────────────────────────────────────────────────
  green   #3dd68c   / dim: rgba(61,214,140,0.10)
  amber   #fef08a   / dim: rgba(254,240,138,0.10)
  red     #fb7185   / dim: rgba(251,113,133,0.10)
  sky     #7dd3fc   / dim: rgba(125,211,252,0.10)

── Borders ───────────────────────────────────────────────
  b1   rgba(255,255,255,0.05)   default, barely there
  b2   rgba(255,255,255,0.08)   hover
  b3   rgba(255,255,255,0.16)   active / focus
```

---

## Typography

**Three fonts. That's it.**

**Source Serif 4** — Display and title headings. Weight 300 for display (hero), 400 for titles. Brings editorial warmth.
**Plus Jakarta Sans** — UI, body, all readable text. Weight 400–600. Clean geometric sans.
**SF Mono / Cascadia Code / Menlo** — Data, labels, IDs, timestamps, code. Monospace.

```html
<link href="https://fonts.googleapis.com/css2?family=Source+Serif+4:ital,opsz,wght@0,8..60,200..900&family=Plus+Jakarta+Sans:ital,wght@0,200..800;1,200..800&display=swap" rel="stylesheet">
```

| Role | Font | Size | Weight |
|------|------|------|--------|
| Display / Hero | Source Serif 4 | 36–58px | 300 |
| Title | Source Serif 4 | 20–28px | 400 |
| Heading | Plus Jakarta Sans | 16–18px | 600 |
| Body | Plus Jakarta Sans | 14px | 400 |
| Secondary body | Plus Jakarta Sans | 13px | 400 |
| Data, IDs | SF Mono | 12px | 400 |
| Labels, badges | SF Mono | 11px | 500, uppercase, tracking 0.06em |
| Code | SF Mono | 13px | 400 |

**Rule:** if it's a display/title heading → Source Serif 4. If it's a sentence → Plus Jakarta Sans. If it's a value, ID, timestamp, status code, label → SF Mono.

---

## Highlight — The Primary Design Tool

Highlight is the most underused element in dark UIs. We use it as a first-class design tool, not just for selection.

### 1. Corner Accents
L-shaped decorative borders at two opposite corners. Frames content with architectural precision.

```html
<div style="position:relative">
  <!-- Top-left corner -->
  <div style="position:absolute;top:-8px;left:-8px;width:48px;height:48px;
    border-left:1px solid rgba(255,255,255,0.03);border-top:1px solid rgba(255,255,255,0.03);
    pointer-events:none"></div>
  <!-- Bottom-right corner -->
  <div style="position:absolute;bottom:-8px;right:-8px;width:48px;height:48px;
    border-right:1px solid rgba(255,255,255,0.03);border-bottom:1px solid rgba(255,255,255,0.03);
    pointer-events:none"></div>
  content here
</div>
```

As CSS pseudo-elements:
```css
.corner-accents { position: relative; }
.corner-accents::before, .corner-accents::after {
  content: ''; position: absolute; width: 48px; height: 48px; pointer-events: none;
}
.corner-accents::before { top: -8px; left: -8px; border-left: 1px solid rgba(255,255,255,0.03); border-top: 1px solid rgba(255,255,255,0.03); }
.corner-accents::after { bottom: -8px; right: -8px; border-right: 1px solid rgba(255,255,255,0.03); border-bottom: 1px solid rgba(255,255,255,0.03); }
```

### 2. Gradient Text
Steel-to-straw gradient clip for emphasis words, hero text, and key metrics.

```html
<span style="background:linear-gradient(135deg,#7dd3fc 0%,#fef08a 100%);
  -webkit-background-clip:text;background-clip:text;-webkit-text-fill-color:transparent">
  key insight
</span>
```

Use sparingly — one gradient text element per section maximum. Best on display headings and hero numbers.

### 3. Temper Wash
Two-color gradient background that gives panels depth and identity.

```html
<div style="background:linear-gradient(135deg,rgba(125,211,252,0.10) 0%,transparent 50%),
  linear-gradient(225deg,rgba(254,240,138,0.07) 0%,transparent 50%),
  rgba(15,15,15,0.6);padding:24px;border-radius:2px">
  hero section content
</div>
```

Use on hero panels, featured cards, or section introductions. Not on every card.

### 4. Gradient Dividers
Replace boring borders with gradient lines. Color should fade to transparent — one direction, same family.

```html
<!-- Section separator — left to right, fades out -->
<div style="height:1px;background:linear-gradient(90deg,rgba(125,211,252,0.4) 0%,transparent 80%);margin:16px 0"></div>

<!-- Under a heading — narrow pop -->
<div style="height:2px;width:40px;background:linear-gradient(90deg,#7dd3fc,#fef08a);border-radius:2px;margin-top:6px"></div>

<!-- Page footer accent — full width -->
<div style="height:1px;background:linear-gradient(90deg,transparent 0%,rgba(125,211,252,0.3) 30%,rgba(254,240,138,0.2) 70%,transparent 100%)"></div>
```

### 5. Left Accent Bars
The fastest visual scanner. 2–3px colored bar on the left edge of a card or row, color-coded by status.

```html
<div style="position:relative;padding-left:16px">
  <div style="position:absolute;left:0;top:0;bottom:0;width:3px;background:#7dd3fc;border-radius:0 2px 2px 0"></div>
  content here
</div>
```

### 6. Strikethrough as Design Element
Strikethrough communicates state: completed, superseded, deprecated. Color-coded by meaning.

```html
<!-- Rose through — for rejected/scratched items -->
<span style="text-decoration:line-through;text-decoration-color:rgba(251,113,133,0.7);
  text-decoration-thickness:2px;color:#4A4A4A">
  PROP-012: Seed Dead Worlds
</span>

<!-- Amber through — for deprecated -->
<span style="text-decoration:line-through;text-decoration-color:rgba(254,240,138,0.6);
  text-decoration-thickness:1.5px;color:#8A8A8A">
  old approach
</span>

<!-- Steel through — for completed items -->
<span style="text-decoration:line-through;text-decoration-color:rgba(125,211,252,0.6);
  text-decoration-thickness:2px;color:#8A8A8A">
  done task
</span>
```

**When to use colored strikethrough:**
- Scratched proposals → rose through
- Replaced decisions → amber through
- Completed items in a mixed list → steel through (done, not deleted)
- Deprecated config → t3 + gray through

---

## Design Elements

### Grain Overlay
Subtle noise texture across the entire page. Adds tactile depth to flat surfaces.

```css
.grain-overlay {
  position: fixed; top: 0; left: 0; right: 0; bottom: 0;
  pointer-events: none; z-index: 9999; opacity: 0.018;
  background-image: url("data:image/svg+xml,%3Csvg viewBox='0 0 256 256' xmlns='http://www.w3.org/2000/svg'%3E%3Cfilter id='n'%3E%3CfeTurbulence type='fractalNoise' baseFrequency='0.9' numOctaves='4' stitchTiles='stitch'/%3E%3C/filter%3E%3Crect width='100%25' height='100%25' filter='url(%23n)'/%3E%3C/svg%3E");
  background-repeat: repeat; background-size: 256px 256px;
}
```

Add `<div class="grain-overlay"></div>` as the last child of `<body>`.

### Flow+Sparks Canvas Background
Canvas 2D animation with drifting radial gradient blobs and rising spark particles using Blue Steel palette colors. The blobs drift slowly across the page as ambient light sources, while small spark particles rise upward with slight lateral drift.

- Blobs: 2–3 large radial gradients using `rgba(125,211,252,0.06)` and `rgba(254,240,138,0.03)`, drifting on sine-wave paths
- Sparks: tiny circles (1–2px) in `#7dd3fc` or `#fef08a` at low opacity (0.2–0.5), rising with randomized speed and lateral wobble
- Canvas sits behind all content at `z-index: 0`, replaces static ambient orbs when animation is desired

### Ambient Orbs (Static Fallback)
When canvas animation is not needed, use CSS pseudo-elements for ambient light:

```css
body::before {
  content: '';
  position: fixed; top: -30%; left: -10%; width: 60%; height: 60%;
  background: radial-gradient(circle, rgba(125,211,252,0.06) 0%, transparent 70%);
  pointer-events: none; z-index: 0;
}
body::after {
  content: '';
  position: fixed; bottom: -20%; right: -10%; width: 50%; height: 50%;
  background: radial-gradient(circle, rgba(254,240,138,0.03) 0%, transparent 70%);
  pointer-events: none; z-index: 0;
}
```

Colors are steel + straw — cool top-left, warm bottom-right. The orbs are atmosphere, not decoration.

---

## Gradient Philosophy — Avoid Contrast

Gradients should feel like light, not like a design decision.

**Good gradients:**
- Same family: `steel (c1) → straw (c2)` at low opacity
- Fade to transparent (always preferred over fade to another color)
- `steel → gunmetal` with low opacity (cool family)
- One end always breathes toward transparent

**Avoid:**
- High contrast: `steel → red`, `straw → rose`
- Pure white gradients
- More than 2 stops unless it's a spectrum decoration
- `opacity: 1` on both ends — always let one end breathe toward transparent

```css
/* good — low contrast, fades out */
background: linear-gradient(135deg, rgba(125,211,252,0.3) 0%, rgba(254,240,138,0.15) 100%);

/* good — fades out */
background: linear-gradient(90deg, rgba(125,211,252,0.4) 0%, transparent 80%);

/* avoid — high contrast colors */
background: linear-gradient(135deg, #7dd3fc 0%, #ef4444 100%);

/* avoid — harsh solid fill */
background: linear-gradient(135deg, rgba(125,211,252,0.8) 0%, rgba(125,211,252,0.6) 100%);
```

---

## Full Head Block

Every Temper UI starts with this:

```html
<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0">
  <title>App Name</title>
  <link href="https://fonts.googleapis.com/css2?family=Source+Serif+4:ital,opsz,wght@0,8..60,200..900&family=Plus+Jakarta+Sans:ital,wght@0,200..800;1,200..800&display=swap" rel="stylesheet">
  <script src="https://cdn.tailwindcss.com"></script>
  <script>
  tailwindcss.config = {
    theme: {
      extend: {
        colors: {
          s1:'#0A0A0A', s2:'#0F0F0F', s3:'#141414', s4:'#1C1C1C',
          t1:'#D4D4D4', t2:'#8A8A8A', t3:'#4A4A4A',
          c1:'#7dd3fc', c2:'#fef08a', c3:'#94a3b8',
          b1:'rgba(255,255,255,0.05)', b2:'rgba(255,255,255,0.08)', b3:'rgba(255,255,255,0.16)',
        },
        fontFamily: {
          sans: ['Plus Jakarta Sans','system-ui','sans-serif'],
          serif: ['Source Serif 4','Georgia','serif'],
          mono: ['SF Mono','Cascadia Code','Menlo','monospace'],
        },
        borderRadius: {
          DEFAULT: '2px',
        },
      },
    },
  }
  </script>
  <style>
    *,*::before,*::after{margin:0;padding:0;box-sizing:border-box}
    body{font-family:'Plus Jakarta Sans',system-ui,sans-serif;background:#0A0A0A;color:#D4D4D4;
      min-height:100vh;-webkit-font-smoothing:antialiased;font-optical-sizing:auto}
    body::before{content:'';position:fixed;top:-30%;left:-10%;width:60%;height:60%;
      background:radial-gradient(circle,rgba(125,211,252,0.06) 0%,transparent 70%);
      pointer-events:none;z-index:0}
    body::after{content:'';position:fixed;bottom:-20%;right:-10%;width:50%;height:50%;
      background:radial-gradient(circle,rgba(254,240,138,0.03) 0%,transparent 70%);
      pointer-events:none;z-index:0}
    body>*{position:relative;z-index:1}
    .corner-accents{position:relative}
    .corner-accents::before,.corner-accents::after{content:'';position:absolute;width:48px;height:48px;pointer-events:none}
    .corner-accents::before{top:-8px;left:-8px;border-left:1px solid rgba(255,255,255,0.03);border-top:1px solid rgba(255,255,255,0.03)}
    .corner-accents::after{bottom:-8px;right:-8px;border-right:1px solid rgba(255,255,255,0.03);border-bottom:1px solid rgba(255,255,255,0.03)}
    ::-webkit-scrollbar{width:6px;height:6px}
    ::-webkit-scrollbar-track{background:transparent}
    ::-webkit-scrollbar-thumb{background:rgba(255,255,255,0.08);border-radius:99px}
    ::selection{background:rgba(125,211,252,0.3);color:#D4D4D4}
    :focus-visible{outline:2px solid rgba(125,211,252,0.5);outline-offset:2px}
  </style>
</head>
```

---

## Components

### Card
```html
<div class="bg-s2 border border-b1 rounded-[2px] p-5 hover:border-b2 transition-all duration-150 relative corner-accents">
```
2px radius — sharp, not bubbly. Corner accents frame the card.

### Badge
```html
<!-- Status badge -->
<span style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
  letter-spacing:0.06em;text-transform:uppercase;padding:3px 9px;border-radius:2px;
  background:rgba(125,211,252,0.08);color:#7dd3fc">
  Active
</span>

<!-- Colored strikethrough badge for Scratched/Done -->
<span style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
  letter-spacing:0.06em;text-transform:uppercase;padding:3px 9px;border-radius:2px;
  background:rgba(251,113,133,0.08);color:#4A4A4A;
  text-decoration:line-through;text-decoration-color:rgba(251,113,133,0.5);text-decoration-thickness:1.5px">
  Scratched
</span>
```

### Button — Primary
```html
<button style="height:36px;padding:0 16px;border-radius:2px;background:#D4D4D4;color:#0A0A0A;
  font-family:'Plus Jakarta Sans',sans-serif;font-size:13px;font-weight:500;border:none;cursor:pointer;
  transition:all 120ms cubic-bezier(0.16,1,0.3,1)">Deploy</button>
```

### Button — Ghost
```html
<button style="height:36px;padding:0 16px;border-radius:2px;background:transparent;
  border:1px solid rgba(255,255,255,0.05);color:#8A8A8A;
  font-family:'Plus Jakarta Sans',sans-serif;font-size:13px;font-weight:500;cursor:pointer;
  transition:all 120ms cubic-bezier(0.16,1,0.3,1)">Cancel</button>
```

### Button — Accent Soft
```html
<button style="height:36px;padding:0 16px;border-radius:2px;background:rgba(125,211,252,0.08);
  color:#7dd3fc;border:none;font-family:'Plus Jakarta Sans',sans-serif;font-size:13px;
  font-weight:500;cursor:pointer">Evolve</button>
```

### Input
```html
<input style="width:100%;height:40px;padding:0 14px;border-radius:2px;
  background:rgba(255,255,255,0.04);border:1px solid rgba(255,255,255,0.05);
  color:#D4D4D4;font-family:'Plus Jakarta Sans',sans-serif;font-size:14px;outline:none"
  placeholder="Enter value...">
```

### Data Row
```html
<div style="display:flex;align-items:center;gap:12px;padding:10px 0;
  border-bottom:1px solid rgba(255,255,255,0.05);cursor:pointer;transition:color 120ms">
  <div style="flex:1;min-width:0">
    <div style="font-size:13px;font-weight:450;color:#D4D4D4">Title</div>
    <div style="font-family:'SF Mono',monospace;font-size:12px;color:#4A4A4A;margin-top:2px">ID · timestamp</div>
  </div>
  <span style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
    letter-spacing:0.06em;text-transform:uppercase;padding:3px 9px;border-radius:2px;
    background:rgba(125,211,252,0.08);color:#7dd3fc">Status</span>
</div>
```

### Section Label
```html
<div style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
  letter-spacing:0.06em;text-transform:uppercase;color:#4A4A4A;margin-bottom:10px">
  Section Name
</div>
```

### Drawer
```html
<div id="overlay" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,0.6);
  backdrop-filter:blur(4px);z-index:50" onclick="closeDrawer()">
  <div style="position:fixed;top:0;right:0;bottom:0;width:100%;max-width:420px;
    background:#0A0A0A;border-left:1px solid rgba(255,255,255,0.05);
    overflow-y:auto;padding:24px;transition:transform 200ms cubic-bezier(0.16,1,0.3,1)"
    onclick="event.stopPropagation()">
  </div>
</div>
```

---

## Border Radius Rules

| Element | Radius |
|---------|--------|
| Cards, panels | 2px |
| Buttons, inputs | 2px |
| Badges, pills | 2px |
| Tooltips | 2px |
| Status dots | 50% (circle) |

**Sharp, not bubbly. Not rounded — architectural.** 2px everywhere keeps the system rigid and intentional.

---

## Color Rules

1. **Corner accents and gradient text are design tools, not exceptions.** Use corner accents on featured cards, gradient text on hero elements. Don't overuse — one gradient text per section.
2. **Three accents** — steel (`c1`), straw (`c2`), gunmetal (`c3`). Steel is primary. Straw is secondary/warning. Gunmetal is neutral. Status colors are for state only.
3. **Strikethrough has color.** Don't default to gray through on everything — match the meaning (rose = rejected, steel = done, amber = deprecated).
4. **Gradients fade, not contrast.** Always fade toward transparent or a related hue. No jarring color jumps.
5. **Text has three tiers** — t1/t2/t3. Never raw white.
6. **Surfaces stack** — s1 → s2 → s3 → s4. Each step is perceptibly brighter.
7. **Temper wash** — use the two-color steel+straw gradient on hero sections and featured panels, not on every surface.

---

## Visual Hierarchy in Dense UIs

1. **Left accent bars** — 3px colored bar = fastest visual scanner
2. **Big numbers first** — metric value in SF Mono 28px+, label below in t3 11px
3. **Summary strip** — 3–5 key metrics at top before the detail grid
4. **Gradient dividers** — `linear-gradient(90deg, c1 at 40% opacity → transparent)`
5. **Colored sparklines** — latest bars at full opacity, older bars at 30%

---

## Replacing This System

This file is the **default** that ships with the Temper skill. It defines the aesthetic any agent uses when building Temper-backed UIs.

**To use your own aesthetic:**
1. Write your own `design-system.md` in `apps/shared/`
2. Define your palette, fonts, design elements (or not), and component patterns using the same section structure
3. Every UI your agent generates will follow your system instead

The contract between the skill and the design system:
- Skill says "read `apps/shared/design-system.md` before generating any UI"
- Design system defines: palette names, font rules, design element patterns, component atoms
- Agents look for: palette section, component examples, color rules
- The rest is up to your aesthetic

**You are not locked into Blue Steel + dark. You are locked into reading the file before you build.**

---

*Sharp corners, soft orbs, three fonts. Steel wash on key moments. Straw sparks rising from below. Corner accents frame the content.*
