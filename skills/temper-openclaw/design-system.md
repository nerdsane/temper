# Temper Design System

*The default look. This file is pluggable — replace it with your own and every generated UI follows your aesthetic instead.*

---

## Philosophy

**Clean. Minimal. Only theme colors.**

Every element earns its place. No decorative clutter, no gratuitous color, no visual noise. Surfaces are quiet. Typography does the heavy lifting. Color is reserved for meaning — status, actions, emphasis. When nothing is competing for attention, the right thing stands out naturally.

- White space is a design tool, not wasted space
- Fewer elements, each with clear purpose
- Color only from the palette below — never ad-hoc hex values
- Monochrome by default, accent color only where it communicates something

---

## Our Palette — Dark Temper

Two modes. Same structure, different values. Dark is the default. Light is the alternative.

### Dark Mode (default)

```
── Surfaces ──────────────────────────────────────────────
  s1   #0A0A0A   page background
  s2   #0F0F0F   card / panel
  s3   rgba(255,255,255,0.03)   elevated — dropdowns, modals
  s4   rgba(255,255,255,0.06)   active / hover

── Text ──────────────────────────────────────────────────
  t1   #D4D4D4   primary — read this
  t2   #8A8A8A   secondary — supporting
  t3   #4A4A4A   muted — labels, whispers

── Accent ────────────────────────────────────────────────
  c1   #9333ea   violet — primary actions, focus, highlights
  c1-dim  rgba(147,51,234,0.10)   tinted backgrounds
  c2   #d97706   bronze — secondary accent, warmth, warnings
  c2-dim  rgba(217,119,6,0.10)    tinted backgrounds
  c3   #3b82f6   blue — tertiary, informational
  c3-dim  rgba(59,130,246,0.10)   tinted backgrounds

── Status ────────────────────────────────────────────────
  green   #3dd68c   / dim: rgba(61,214,140,0.10)
  amber   #d97706   / dim: rgba(217,119,6,0.10)
  red     #fb7185   / dim: rgba(251,113,133,0.10)

── Borders ───────────────────────────────────────────────
  b1   rgba(255,255,255,0.05)   default, barely there
  b2   rgba(255,255,255,0.08)   hover
  b3   rgba(255,255,255,0.16)   active / focus

── Corner accents ────────────────────────────────────────
  corner   rgba(255,255,255,0.03)

── Scrollbar ─────────────────────────────────────────────
  thumb       rgba(255,255,255,0.08)
  thumb-hover rgba(255,255,255,0.14)
```

### Light Mode

```
── Surfaces ──────────────────────────────────────────────
  s1   #FAFAFA   page background
  s2   #F5F5F5   card / panel
  s3   rgba(0,0,0,0.02)   elevated
  s4   rgba(0,0,0,0.04)   active / hover

── Text ──────────────────────────────────────────────────
  t1   #2A2A2A   primary
  t2   #6B6B6B   secondary
  t3   #A0A0A0   muted

── Accent ────────────────────────────────────────────────
  c1   #581c87   deep violet
  c1-dim  rgba(88,28,135,0.08)
  c2   #92400e   deep bronze
  c2-dim  rgba(146,64,14,0.08)
  c3   #1d4ed8   deep blue
  c3-dim  rgba(29,78,216,0.08)

── Status ────────────────────────────────────────────────
  green   #15803d   / dim: rgba(21,128,61,0.08)
  amber   #92400e   / dim: rgba(146,64,14,0.08)
  red     #be123c   / dim: rgba(190,18,60,0.08)

── Borders ───────────────────────────────────────────────
  b1   rgba(0,0,0,0.06)
  b2   rgba(0,0,0,0.10)
  b3   rgba(0,0,0,0.18)

── Corner accents ────────────────────────────────────────
  corner   rgba(0,0,0,0.04)

── Scrollbar ─────────────────────────────────────────────
  thumb       rgba(0,0,0,0.10)
  thumb-hover rgba(0,0,0,0.18)
```

### Mode Switching

Apply `.dark` or `.light` class to `<html>`. Use CSS custom properties so every component adapts automatically.

**Rule:** Always define both modes. Never hardcode dark-only hex values in components. Use the token names.

---

## Typography

**Three fonts. That's it.**

**Source Serif 4** — Display and title headings. Weight 300 for display (hero), 400 for titles. Editorial warmth.
**Plus Jakarta Sans** — UI, body, all readable text. Weight 400-600. Clean geometric sans.
**SF Mono / Cascadia Code / Menlo** — Data, labels, IDs, timestamps, code. Monospace.

```html
<link href="https://fonts.googleapis.com/css2?family=Source+Serif+4:ital,opsz,wght@0,8..60,200..900&family=Plus+Jakarta+Sans:ital,wght@0,200..800;1,200..800&display=swap" rel="stylesheet">
```

| Role | Font | Size | Weight |
|------|------|------|--------|
| Display / Hero | Source Serif 4 | 36-58px | 300 |
| Title | Source Serif 4 | 20-28px | 400 |
| Heading | Plus Jakarta Sans | 16-18px | 600 |
| Body | Plus Jakarta Sans | 14px | 400 |
| Secondary body | Plus Jakarta Sans | 13px | 400 |
| Data, IDs | SF Mono | 12px | 400 |
| Labels, badges | SF Mono | 11px | 500, uppercase, tracking 0.06em |
| Code | SF Mono | 13px | 400 |

**Rule:** Display/title heading = Source Serif 4. Sentence = Plus Jakarta Sans. Value, ID, timestamp, status, label = SF Mono.

---

## Highlight — The Primary Design Tool

Color is scarce. When it appears, it means something. These are the sanctioned highlight patterns.

### 1. Corner Accents
L-shaped decorative borders at two opposite corners. Frames content with architectural precision.

```css
.corner-accents { position: relative; }
.corner-accents::before, .corner-accents::after {
  content: ''; position: absolute; width: 48px; height: 48px; pointer-events: none;
}
.corner-accents::before { top: -8px; left: -8px; border-left: 1px solid var(--corner-color); border-top: 1px solid var(--corner-color); }
.corner-accents::after { bottom: -8px; right: -8px; border-right: 1px solid var(--corner-color); border-bottom: 1px solid var(--corner-color); }
```

### 2. Gradient Text
Violet-to-bronze gradient clip for emphasis words, hero text, and key metrics.

```html
<!-- Dark mode -->
<span style="background:linear-gradient(135deg,#9333ea 0%,#d97706 100%);
  -webkit-background-clip:text;background-clip:text;-webkit-text-fill-color:transparent">
  key insight
</span>
```

Use sparingly — one gradient text element per section maximum. Best on display headings and hero numbers.

### 3. Temper Wash
Two-color gradient background that gives panels depth and identity.

```html
<!-- Dark mode -->
<div style="background:linear-gradient(135deg,rgba(147,51,234,0.08) 0%,transparent 50%),
  linear-gradient(225deg,rgba(217,119,6,0.05) 0%,transparent 50%),
  #0F0F0F;padding:24px;border-radius:2px">
  hero section content
</div>
```

Use on hero panels, featured cards, or section introductions. Not on every card.

### 4. Gradient Dividers
Replace boring borders with gradient lines. Color fades to transparent.

```html
<!-- Section separator -->
<div style="height:1px;background:linear-gradient(90deg,rgba(147,51,234,0.4) 0%,transparent 80%);margin:16px 0"></div>

<!-- Under a heading -->
<div style="height:2px;width:40px;background:linear-gradient(90deg,#9333ea,#d97706);border-radius:2px;margin-top:6px"></div>
```

### 5. Left Accent Bars
The fastest visual scanner. 2-3px colored bar on the left edge of a card or row.

```html
<div style="position:relative;padding-left:16px">
  <div style="position:absolute;left:0;top:0;bottom:0;width:3px;background:#9333ea;border-radius:0 2px 2px 0"></div>
  content here
</div>
```

### 6. Strikethrough as Design Element
Strikethrough communicates state. Color-coded by meaning.

- Rejected/scratched = red through
- Deprecated = bronze through
- Completed = violet through

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
Canvas 2D animation with drifting radial gradient blobs and rising spark particles. Blobs drift slowly as ambient light sources. Sparks rise with slight lateral drift.

- Blobs: 2-3 large radial gradients using `rgba(147,51,234,0.06)` (violet) and `rgba(217,119,6,0.04)` (bronze)
- Sparks: tiny circles (1-2px) in c1 or c2 at low opacity (0.2-0.5), rising with randomized speed
- Canvas sits behind all content at `z-index: 0`
- Bias toward bronze/warm tones for visual warmth

### Ambient Orbs (Static Fallback)
When canvas animation is not needed, use CSS pseudo-elements:

```css
/* Dark mode */
body::before {
  content: '';
  position: fixed; top: -30%; left: -10%; width: 60%; height: 60%;
  background: radial-gradient(circle, rgba(147,51,234,0.06) 0%, transparent 70%);
  pointer-events: none; z-index: 0;
}
body::after {
  content: '';
  position: fixed; bottom: -20%; right: -10%; width: 50%; height: 50%;
  background: radial-gradient(circle, rgba(217,119,6,0.03) 0%, transparent 70%);
  pointer-events: none; z-index: 0;
}
```

Violet top-left, bronze bottom-right. The orbs are atmosphere, not decoration.

---

## Gradient Philosophy — Avoid Contrast

Gradients should feel like light, not like a design decision.

**Good gradients:**
- Same family: `violet (c1) → bronze (c2)` at low opacity
- Fade to transparent (always preferred)
- One end always breathes toward transparent

**Avoid:**
- High contrast color jumps
- Pure white gradients
- More than 2 stops
- `opacity: 1` on both ends

---

## Easing & Motion

One easing curve for everything: `cubic-bezier(0.16, 1, 0.3, 1)` — expo out. Confident deceleration.

| Purpose | Duration |
|---------|----------|
| Instant feedback (button, toggle) | 100-150ms |
| State change (hover, menu) | 200-300ms |
| Layout change (accordion, modal) | 300-500ms |
| Entrance animation (page load) | 500-800ms |

**Rule:** Exit animations are 75% of entrance duration. Never use bounce or elastic easing. Always respect `prefers-reduced-motion`.

```css
@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after {
    animation-duration: 0.01ms !important;
    animation-iteration-count: 1 !important;
    transition-duration: 0.01ms !important;
  }
}
```

---

## Full Head Block

Every Temper UI starts with this. Supports both dark and light modes.

```html
<!DOCTYPE html>
<html lang="en" class="dark">
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
          s1:'var(--s1)', s2:'var(--s2)', s3:'var(--s3)', s4:'var(--s4)',
          t1:'var(--t1)', t2:'var(--t2)', t3:'var(--t3)',
          c1:'var(--c1)', c2:'var(--c2)', c3:'var(--c3)',
          b1:'var(--b1)', b2:'var(--b2)', b3:'var(--b3)',
        },
        fontFamily: {
          sans: ['Plus Jakarta Sans','system-ui','sans-serif'],
          serif: ['Source Serif 4','Georgia','serif'],
          mono: ['SF Mono','Cascadia Code','Menlo','monospace'],
        },
        borderRadius: { DEFAULT: '2px' },
      },
    },
  }
  </script>
  <style>
    *,*::before,*::after{margin:0;padding:0;box-sizing:border-box}

    .dark {
      --s1:#0A0A0A; --s2:#0F0F0F; --s3:rgba(255,255,255,0.03); --s4:rgba(255,255,255,0.06);
      --t1:#D4D4D4; --t2:#8A8A8A; --t3:#4A4A4A;
      --c1:#9333ea; --c2:#d97706; --c3:#3b82f6;
      --c1-dim:rgba(147,51,234,0.10); --c2-dim:rgba(217,119,6,0.10); --c3-dim:rgba(59,130,246,0.10);
      --b1:rgba(255,255,255,0.05); --b2:rgba(255,255,255,0.08); --b3:rgba(255,255,255,0.16);
      --corner-color:rgba(255,255,255,0.03);
    }
    .light {
      --s1:#FAFAFA; --s2:#F5F5F5; --s3:rgba(0,0,0,0.02); --s4:rgba(0,0,0,0.04);
      --t1:#2A2A2A; --t2:#6B6B6B; --t3:#A0A0A0;
      --c1:#581c87; --c2:#92400e; --c3:#1d4ed8;
      --c1-dim:rgba(88,28,135,0.08); --c2-dim:rgba(146,64,14,0.08); --c3-dim:rgba(29,78,216,0.08);
      --b1:rgba(0,0,0,0.06); --b2:rgba(0,0,0,0.10); --b3:rgba(0,0,0,0.18);
      --corner-color:rgba(0,0,0,0.04);
    }

    body{font-family:'Plus Jakarta Sans',system-ui,sans-serif;background:var(--s1);color:var(--t1);
      min-height:100vh;-webkit-font-smoothing:antialiased;font-optical-sizing:auto;
      transition:background-color 0.3s cubic-bezier(0.16,1,0.3,1),color 0.3s cubic-bezier(0.16,1,0.3,1)}
    body>*{position:relative;z-index:1}
    .corner-accents{position:relative}
    .corner-accents::before,.corner-accents::after{content:'';position:absolute;width:48px;height:48px;pointer-events:none}
    .corner-accents::before{top:-8px;left:-8px;border-left:1px solid var(--corner-color);border-top:1px solid var(--corner-color)}
    .corner-accents::after{bottom:-8px;right:-8px;border-right:1px solid var(--corner-color);border-bottom:1px solid var(--corner-color)}
    ::-webkit-scrollbar{width:6px;height:6px}
    ::-webkit-scrollbar-track{background:transparent}
    ::-webkit-scrollbar-thumb{background:rgba(255,255,255,0.08);border-radius:99px}
    ::selection{background:var(--c1-dim);color:var(--t1)}
    :focus-visible{outline:2px solid color-mix(in srgb, var(--c1) 50%, transparent);outline-offset:2px}
  </style>
</head>
```

---

## Components

### Card
```html
<div class="bg-s2 border border-b1 rounded-[2px] p-5 hover:border-b2 transition-all duration-150 relative corner-accents">
```
2px radius. Corner accents frame the card. No shadows — borders and surface color create depth.

### Badge
```html
<!-- Status badge — uses c1 -->
<span style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
  letter-spacing:0.06em;text-transform:uppercase;padding:3px 9px;border-radius:2px;
  background:var(--c1-dim);color:var(--c1)">
  Active
</span>

<!-- Warning badge — uses c2 -->
<span style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
  letter-spacing:0.06em;text-transform:uppercase;padding:3px 9px;border-radius:2px;
  background:var(--c2-dim);color:var(--c2)">
  Pending
</span>
```

### Button — Primary
```html
<button style="height:36px;padding:0 16px;border-radius:2px;background:var(--c1);color:var(--s1);
  font-family:'Plus Jakarta Sans',sans-serif;font-size:13px;font-weight:500;border:none;cursor:pointer;
  transition:all 120ms cubic-bezier(0.16,1,0.3,1)">Deploy</button>
```

### Button — Ghost
```html
<button style="height:36px;padding:0 16px;border-radius:2px;background:transparent;
  border:1px solid var(--b1);color:var(--t2);
  font-family:'Plus Jakarta Sans',sans-serif;font-size:13px;font-weight:500;cursor:pointer;
  transition:all 120ms cubic-bezier(0.16,1,0.3,1)">Cancel</button>
```

### Button — Accent Soft
```html
<button style="height:36px;padding:0 16px;border-radius:2px;background:var(--c1-dim);
  color:var(--c1);border:none;font-family:'Plus Jakarta Sans',sans-serif;font-size:13px;
  font-weight:500;cursor:pointer;transition:all 120ms cubic-bezier(0.16,1,0.3,1)">Evolve</button>
```

### Input
```html
<input style="width:100%;height:40px;padding:0 14px;border-radius:2px;
  background:var(--s3);border:1px solid var(--b1);
  color:var(--t1);font-family:'Plus Jakarta Sans',sans-serif;font-size:14px;outline:none"
  placeholder="Enter value...">
```

### Data Row
```html
<div style="display:flex;align-items:center;gap:12px;padding:10px 0;
  border-bottom:1px solid var(--b1);cursor:pointer;transition:color 120ms">
  <div style="flex:1;min-width:0">
    <div style="font-size:13px;font-weight:450;color:var(--t1)">Title</div>
    <div style="font-family:'SF Mono',monospace;font-size:12px;color:var(--t3);margin-top:2px">ID · timestamp</div>
  </div>
  <span style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
    letter-spacing:0.06em;text-transform:uppercase;padding:3px 9px;border-radius:2px;
    background:var(--c1-dim);color:var(--c1)">Status</span>
</div>
```

### Section Label
```html
<div style="font-family:'SF Mono',monospace;font-size:11px;font-weight:500;
  letter-spacing:0.06em;text-transform:uppercase;color:var(--t3);margin-bottom:10px">
  Section Name
</div>
```

### Drawer
```html
<div id="overlay" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,0.6);
  backdrop-filter:blur(4px);z-index:50" onclick="closeDrawer()">
  <div style="position:fixed;top:0;right:0;bottom:0;width:100%;max-width:420px;
    background:var(--s1);border-left:1px solid var(--b1);
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

**Sharp, not bubbly. Architectural.** 2px everywhere keeps the system rigid and intentional.

---

## Color Rules

1. **Only theme colors.** Never introduce colors outside this palette. No random blues, no brand colors from elsewhere. If it's not in the palette, it doesn't exist.
2. **Three accents** — violet (`c1`), bronze (`c2`), blue (`c3`). Violet is primary. Bronze is secondary/warm. Blue is informational. Status colors are for state only.
3. **Monochrome first.** Default to surfaces and text tiers. Add accent color only when it communicates meaning — a status, an action, emphasis.
4. **Gradients fade, not contrast.** Always fade toward transparent. No jarring color jumps.
5. **Text has three tiers** — t1/t2/t3. Never raw white. Never raw black.
6. **Surfaces stack** — s1 → s2 → s3 → s4. Each step is perceptibly different.
7. **Temper wash** — violet+bronze gradient on hero sections and featured panels only.
8. **Both modes must work.** Every component uses CSS variables, never hardcoded hex.

---

## Visual Hierarchy in Dense UIs

1. **Left accent bars** — 3px colored bar = fastest visual scanner
2. **Big numbers first** — metric value in SF Mono 28px+, label below in t3 11px
3. **Summary strip** — 3-5 key metrics at top before the detail grid
4. **Gradient dividers** — `linear-gradient(90deg, c1 at 40% opacity → transparent)`
5. **White space** — generous padding between sections, tight padding within groups

---

## Replacing This System

This file is the **default** that ships with the Temper skill. It defines the aesthetic any agent uses when building Temper-backed UIs.

**To use your own aesthetic:**
1. Write your own `design-system.md` in `apps/shared/`
2. Define your palette, fonts, design elements, and component patterns
3. Every UI your agent generates will follow your system instead

**You are not locked into Dark Temper. You are locked into reading the file before you build.**

---

*Sharp corners, quiet surfaces, three fonts. Violet wash on key moments. Bronze warmth rising from below. Corner accents frame the content. Clean, minimal, intentional.*
