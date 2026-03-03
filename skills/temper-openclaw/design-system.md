# Temper Design System

*The default look. This file is pluggable — replace it with your own and every generated UI follows your aesthetic instead.*

---

## Our Palette

These are the named colors. Use the names, not raw hex. When an agent reads this file, these names become the vocabulary.

```
── Surfaces (warm dark, blue undertone) ──────────────────
  s1   #0c0c10   page background
  s2   #14141a   card / panel
  s3   #1c1c24   elevated — dropdowns, modals
  s4   #24242e   active / hover

── Text ──────────────────────────────────────────────────
  t1   #eeeef2   primary — read this
  t2   #8b8b9e   secondary — supporting
  t3   #55556a   muted — labels, whispers

── Accent — electric violet ──────────────────────────────
  a1   #8b5cf6   primary actions, focus, highlights
  a2   #a78bfa   hover, lighter variant
  a3   rgba(139,92,246,0.12)   tinted backgrounds

── Highlight colors — these are design elements ──────────
  hl-violet   rgba(139,92,246,0.25)   violet wash
  hl-lime     rgba(163,230,53,0.18)   lime wash
  hl-rose     rgba(251,113,133,0.20)  rose wash
  hl-amber    rgba(251,191,36,0.20)   amber wash
  hl-sky      rgba(56,189,248,0.18)   sky wash

── Status ────────────────────────────────────────────────
  green   #3dd68c   / dim: rgba(61,214,140,0.10)
  amber   #e5a63e   / dim: rgba(229,166,62,0.10)
  red     #fb7185   / dim: rgba(251,113,133,0.10)
  sky     #38bdf8   / dim: rgba(56,189,248,0.10)

── Borders ───────────────────────────────────────────────
  b1   rgba(255,255,255,0.06)   default, barely there
  b2   rgba(255,255,255,0.10)   hover
  b3   rgba(255,255,255,0.16)   active / focus
```

---

## Typography

**Two fonts. That's it.**

**Space Grotesk** — UI, body, headings. Geometric, slightly quirky. Not Inter.
**Space Mono** — metadata, labels, IDs, technical data. Monospace with personality.

```html
<link rel="preconnect" href="https://fonts.googleapis.com">
<link href="https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@400;500;600;700&family=Space+Mono:wght@400;700&display=swap" rel="stylesheet">
```

| Role | Font | Size | Weight |
|------|------|------|--------|
| Display / Hero | Space Grotesk | 32–48px | 700 |
| Title | Space Grotesk | 20–28px | 600–700 |
| Heading | Space Grotesk | 16–18px | 600 |
| Body | Space Grotesk | 15px | 400 |
| Secondary body | Space Grotesk | 14px | 400 |
| Data, IDs | Space Mono | 13px | 400 |
| Labels, badges | Space Mono | 11px | 700, uppercase, tracking 0.12em |
| Code | Space Mono | 13px | 400 |

**Rule:** if it's a human-readable sentence → Space Grotesk. If it's a value, ID, timestamp, status code, label → Space Mono.

---

## Highlight — The Primary Design Tool

Highlight is the most underused element in dark UIs. We use it as a first-class design tool, not just for selection.

### 1. Background Highlight (wash)
Soft colored wash behind text or entire sections. Low opacity — a suggestion of color, not a shout.

```html
<span style="background:rgba(139,92,246,0.20);padding:2px 8px;border-radius:4px">selected proposal</span>

<span style="background:rgba(163,230,53,0.15);padding:2px 8px;border-radius:4px">active</span>
```

**Gradient wash** — for multi-word phrases or section titles:
```html
<span style="background:linear-gradient(120deg,rgba(139,92,246,0.22) 0%,rgba(163,230,53,0.14) 100%);
  padding:2px 10px;border-radius:4px">
  key insight
</span>
```
Keep gradient transitions within the same temperature family (violet → lime, not violet → red). Avoid stark contrast.

### 2. Colored Text Highlight
One word in a sentence gets the accent color. Breaks the gray monotony.
```html
<p style="color:#8b8b9e">
  Proposal moved to <span style="color:#a78bfa;font-weight:600">Implementing</span> — CC session spawned
</p>
```

### 3. Strikethrough as Design Element
Strikethrough communicates state: completed, superseded, deprecated. But it's also visual rhythm.

```html
<!-- Simple completion -->
<span style="text-decoration:line-through;color:#55556a">old task</span>

<!-- Colored strikethrough — more expressive -->
<span style="text-decoration:line-through;text-decoration-color:rgba(139,92,246,0.6);
  text-decoration-thickness:2px;color:#8b8b9e">
  scratch this
</span>

<!-- Rose through — for rejected/scratched items -->
<span style="text-decoration:line-through;text-decoration-color:rgba(251,113,133,0.7);
  text-decoration-thickness:2px;color:#55556a">
  PROP-012: Seed Dead Worlds
</span>

<!-- Amber through — for deprecated -->
<span style="text-decoration:line-through;text-decoration-color:rgba(229,166,62,0.6);
  text-decoration-thickness:1.5px;color:#8b8b9e">
  old approach
</span>
```

**When to use colored strikethrough:**
- Scratched proposals → rose through
- Replaced decisions → amber through
- Completed items in a mixed list → violet through (done, not deleted)
- Deprecated config → t3 + gray through

### 4. Gradient Dividers
Replace boring borders with gradient lines. Color should fade to transparent — one direction, same family.

```html
<!-- Section separator — left to right, fades out -->
<div style="height:1px;background:linear-gradient(90deg,rgba(139,92,246,0.4) 0%,transparent 80%);margin:16px 0"></div>

<!-- Under a heading — narrow pop -->
<div style="height:2px;width:40px;background:linear-gradient(90deg,#8b5cf6,#a78bfa);border-radius:2px;margin-top:6px"></div>

<!-- Page footer accent — full width -->
<div style="height:1px;background:linear-gradient(90deg,transparent 0%,rgba(139,92,246,0.3) 30%,rgba(163,230,53,0.2) 70%,transparent 100%)"></div>
```

### 5. Left Accent Bars
The fastest visual scanner. 2–3px colored bar on the left edge of a card or row, color-coded by status.

```html
<div style="position:relative;padding-left:16px">
  <div style="position:absolute;left:0;top:0;bottom:0;width:3px;background:#8b5cf6;border-radius:0 2px 2px 0"></div>
  content here
</div>
```

---

## Glass Surface

Glass floats. It's not a box — it's a panel that's slightly transparent and slightly reflective. The gradient orbs behind it give it something to blur.

**What makes glass work:**
1. The surface itself has a `linear-gradient` from `rgba(white, 0.05)` to `rgba(white, 0.02)` — subtle sheen
2. `backdrop-filter: blur(16px)` — blurs whatever is behind it
3. The border is `rgba(white, 0.06)` — felt but not seen
4. The ambient orbs (`body::before` / `body::after`) are the "light source" glass distorts

```css
.glass {
  background: linear-gradient(
    135deg,
    rgba(255,255,255,0.05) 0%,
    rgba(255,255,255,0.02) 100%
  );
  border: 1px solid rgba(255,255,255,0.06);
  backdrop-filter: blur(16px);
  -webkit-backdrop-filter: blur(16px);
}
```

**Never:** `background: rgba(255,255,255,0.1)` — too washed out, loses depth.
**Never:** `border: 1px solid white` — too harsh.
**Never:** glass on s1 background with no orbs — it'll look flat without something to blur.

**Elevation:** stacking glass panels, use very slight background shift:
- Base glass: `rgba(white, 0.03–0.05)`
- Elevated (drawer, modal): `background: s1` (solid) + stronger border `b2`
- Tooltip: glass with `backdrop-filter: blur(8px)` + thin accent border

---

## Ambient Gradient Orbs

Every page gets two. They're the "light" that glass surfaces distort.

```css
body::before {
  content: '';
  position: fixed;
  top: -30%; left: -10%;
  width: 60%; height: 60%;
  background: radial-gradient(circle, rgba(139,92,246,0.08) 0%, transparent 70%);
  pointer-events: none;
  z-index: 0;
}
body::after {
  content: '';
  position: fixed;
  bottom: -20%; right: -10%;
  width: 50%; height: 50%;
  background: radial-gradient(circle, rgba(61,214,140,0.04) 0%, transparent 70%);
  pointer-events: none;
  z-index: 0;
}
```

Colors should be complementary but **low contrast** — violet + lime (not violet + red). The orbs are atmosphere, not decoration.

---

## Gradient Philosophy — Avoid Contrast

Gradients should feel like light, not like a design decision.

**Good gradients:**
- Same family: `violet (a1) → lighter violet (a2)`
- Adjacent spectrum: `violet → sky` (both cool)
- `violet → lime` with low opacity (complementary but muted)
- Fade to transparent (always preferred over fade to another color)

**Avoid:**
- High contrast: `violet → red`, `blue → orange`
- Pure white gradients
- More than 2 stops unless it's a spectrum decoration
- `opacity: 1` on both ends — always let one end breathe toward transparent

```css
/* ✅ good — low contrast, same temperature */
background: linear-gradient(135deg, rgba(139,92,246,0.3) 0%, rgba(163,230,53,0.15) 100%);

/* ✅ good — fades out */
background: linear-gradient(90deg, rgba(139,92,246,0.4) 0%, transparent 80%);

/* ❌ avoid — high contrast colors */
background: linear-gradient(135deg, #8b5cf6 0%, #ef4444 100%);

/* ❌ avoid — harsh solid fill */
background: linear-gradient(135deg, rgba(139,92,246,0.8) 0%, rgba(139,92,246,0.6) 100%);
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
  <link rel="preconnect" href="https://fonts.googleapis.com">
  <link href="https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@400;500;600;700&family=Space+Mono:wght@400;700&display=swap" rel="stylesheet">
  <script src="https://cdn.tailwindcss.com"></script>
  <script>
  tailwindcss.config = {
    theme: {
      extend: {
        colors: {
          s1:'#0c0c10', s2:'#14141a', s3:'#1c1c24', s4:'#24242e',
          t1:'#eeeef2', t2:'#8b8b9e', t3:'#55556a',
          a1:'#8b5cf6', a2:'#a78bfa', a3:'rgba(139,92,246,0.12)',
          b1:'rgba(255,255,255,0.06)', b2:'rgba(255,255,255,0.10)', b3:'rgba(255,255,255,0.16)',
        },
        fontFamily: {
          sans: ['Space Grotesk','system-ui','sans-serif'],
          mono: ['Space Mono','monospace'],
        },
      },
    },
  }
  </script>
  <style>
    *,*::before,*::after{margin:0;padding:0;box-sizing:border-box}
    body{font-family:'Space Grotesk',system-ui,sans-serif;background:#0c0c10;color:#eeeef2;
      min-height:100vh;-webkit-font-smoothing:antialiased}
    body::before{content:'';position:fixed;top:-30%;left:-10%;width:60%;height:60%;
      background:radial-gradient(circle,rgba(139,92,246,0.08) 0%,transparent 70%);
      pointer-events:none;z-index:0}
    body::after{content:'';position:fixed;bottom:-20%;right:-10%;width:50%;height:50%;
      background:radial-gradient(circle,rgba(61,214,140,0.04) 0%,transparent 70%);
      pointer-events:none;z-index:0}
    body>*{position:relative;z-index:1}
    .glass{background:linear-gradient(135deg,rgba(255,255,255,0.05) 0%,rgba(255,255,255,0.02) 100%);
      border:1px solid rgba(255,255,255,0.06);backdrop-filter:blur(16px);-webkit-backdrop-filter:blur(16px)}
    ::-webkit-scrollbar{width:6px;height:6px}
    ::-webkit-scrollbar-track{background:transparent}
    ::-webkit-scrollbar-thumb{background:rgba(255,255,255,0.08);border-radius:99px}
    ::selection{background:rgba(139,92,246,0.3);color:#eeeef2}
    :focus-visible{outline:2px solid rgba(139,92,246,0.5);outline-offset:2px}
  </style>
</head>
```

---

## Components

### Glass Card
```html
<div class="glass rounded-[10px] p-5 hover:border-b2 transition-all duration-150">
```
10px radius — not bubbly. The glass class handles background + border + blur.

### Badge
```html
<!-- Status badge -->
<span style="font-family:'Space Mono',monospace;font-size:11px;font-weight:700;
  letter-spacing:0.10em;text-transform:uppercase;padding:3px 8px;border-radius:8px;
  background:rgba(139,92,246,0.12);color:#a78bfa">
  Active
</span>

<!-- Colored strikethrough badge for Scratched/Done -->
<span style="font-family:'Space Mono',monospace;font-size:11px;font-weight:700;
  letter-spacing:0.10em;text-transform:uppercase;padding:3px 8px;border-radius:8px;
  background:rgba(251,113,133,0.08);color:#55556a;
  text-decoration:line-through;text-decoration-color:rgba(251,113,133,0.5);text-decoration-thickness:1.5px">
  Scratched
</span>
```

### Button — Primary
```html
<button style="height:36px;padding:0 16px;border-radius:8px;background:#8b5cf6;color:#eeeef2;
  font-family:'Space Grotesk',sans-serif;font-size:13px;font-weight:500;border:none;cursor:pointer;
  transition:all 0.15s">Primary</button>
```

### Button — Ghost
```html
<button style="height:36px;padding:0 16px;border-radius:8px;background:transparent;
  border:1px solid rgba(255,255,255,0.08);color:#8b8b9e;
  font-family:'Space Grotesk',sans-serif;font-size:13px;font-weight:500;cursor:pointer;
  transition:all 0.15s">Secondary</button>
```

### Input
```html
<input style="width:100%;height:40px;padding:0 14px;border-radius:8px;
  background:rgba(255,255,255,0.04);border:1px solid rgba(255,255,255,0.08);
  color:#eeeef2;font-family:'Space Grotesk',sans-serif;font-size:14px;outline:none"
  placeholder="Enter value...">
```

### Data Row
```html
<div style="display:flex;align-items:center;gap:12px;padding:12px 16px;
  border-bottom:1px solid rgba(255,255,255,0.04);cursor:pointer;transition:background 0.1s"
  onmouseover="this.style.background='rgba(255,255,255,0.02)'"
  onmouseout="this.style.background=''"
>
  <div style="width:3px;height:36px;border-radius:2px;background:#8b5cf6;flex-shrink:0"></div>
  <div style="flex:1;min-width:0">
    <div style="font-size:14px;font-weight:500;color:#eeeef2;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">
      Title
    </div>
    <div style="font-family:'Space Mono',monospace;font-size:11px;color:#55556a;margin-top:2px">
      ID · timestamp
    </div>
  </div>
  <span style="font-family:'Space Mono',monospace;font-size:11px;font-weight:700;
    letter-spacing:0.10em;text-transform:uppercase;padding:3px 8px;border-radius:8px;
    background:rgba(139,92,246,0.12);color:#a78bfa">
    Status
  </span>
</div>
```

### Section Label
```html
<div style="font-family:'Space Mono',monospace;font-size:11px;font-weight:700;
  letter-spacing:0.12em;text-transform:uppercase;color:#55556a;margin-bottom:10px">
  Section Name
</div>
```

### Drawer
```html
<div id="overlay" style="display:none;position:fixed;inset:0;background:rgba(0,0,0,0.6);
  backdrop-filter:blur(4px);z-index:50" onclick="closeDrawer()">
  <div style="position:fixed;top:0;right:0;bottom:0;width:100%;max-width:420px;
    background:#0c0c10;border-left:1px solid rgba(255,255,255,0.08);
    overflow-y:auto;padding:24px;transition:transform 0.2s ease-out"
    onclick="event.stopPropagation()">
  </div>
</div>
```

---

## Border Radius Rules

| Element | Radius |
|---------|--------|
| Cards, panels | 10px |
| Buttons, inputs | 8px |
| Badges, pills | 8px (not full round — feels more editorial) |
| Tooltips | 8px |
| Status dots | 50% (circle) |

**Not 16px+ anywhere.** That's bubbly. Not 4px everywhere either — that's boxy. 8–10px is the sweet spot.

---

## Color Rules

1. **Highlight is a design tool, not an exception.** Use it for key words, state changes, section emphasis. Don't save it for "important" moments only.
2. **One accent** — violet (`a1/a2/a3`). Status colors are for state only.
3. **Strikethrough has color.** Don't default to gray through on everything — match the meaning (rose = rejected, violet = done, amber = deprecated).
4. **Gradients fade, not contrast.** Always fade toward transparent or a related hue. No jarring color jumps.
5. **Text has three tiers** — t1/t2/t3. Never raw white.
6. **Surfaces stack** — s1 → s2 → s3 → s4. Each step is perceptibly brighter.

---

## Visual Hierarchy in Dense UIs

1. **Left accent bars** — 3px colored bar = fastest visual scanner
2. **Big numbers first** — metric value in Space Mono 28px+, label below in t3 11px
3. **Summary strip** — 3–5 key metrics at top before the detail grid
4. **Gradient dividers** — `linear-gradient(90deg, a1 at 40% opacity → transparent)`
5. **Colored sparklines** — latest bars at full opacity, older bars at 30%

---

## Replacing This System

This file is the **default** that ships with the Temper skill. It defines the aesthetic any agent uses when building Temper-backed UIs.

**To use your own aesthetic:**
1. Write your own `design-system.md` in `apps/shared/`
2. Define your palette, fonts, glass (or not), and component patterns using the same section structure
3. Every UI your agent generates will follow your system instead

The contract between the skill and the design system:
- Skill says "read `apps/shared/design-system.md` before generating any UI"
- Design system defines: palette names, font rules, glass/card pattern, component atoms
- Agents look for: palette section, component examples, color rules
- The rest is up to your aesthetic

**You are not locked into violet + dark. You are locked into reading the file before you build.**

---

*Frosted glass on a dark void. Violet wash on key words. Rose through on what didn't make it. Sharp corners, soft orbs, two fonts.*
