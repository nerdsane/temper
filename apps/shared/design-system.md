# Temper Design System v3

*The default look. Users can replace this file with their own.*

This is a markdown file. Agents read it before generating any UI. The Temper skill tells them where to find it. If you want a different aesthetic, write your own `design-system.md` with your palette, fonts, and component patterns — drop it in `apps/shared/` and every generated UI will follow it.

---

## Foundation

**Tailwind CSS CDN** + **Geist font** (Vercel's typeface — geometric, modern, not-Inter-not-system-ui).

Every Temper UI starts with this `<head>`:

```html
<script src="https://cdn.tailwindcss.com"></script>
<script>
tailwindcss.config = {
  theme: {
    extend: {
      colors: {
        // Surface stack — NOT pure black. Warm dark with blue undertone.
        s1: '#0c0c10',    // page background
        s2: '#14141a',    // card / panel background
        s3: '#1c1c24',    // elevated (dropdowns, modals)
        s4: '#24242e',    // active/hover surfaces

        // Text — warm white, not #fff
        t1: '#ededf0',    // primary text
        t2: '#8b8b9e',    // secondary / descriptions
        t3: '#55556a',    // muted / labels / placeholders

        // Accent — electric violet, one hue, three stops
        a1: '#8b5cf6',    // primary actions, focus rings
        a2: '#a78bfa',    // hover / lighter variant
        a3: 'rgba(139,92,246,0.12)', // tinted backgrounds

        // Status — muted, desaturated, not neon
        green:  { DEFAULT: '#3dd68c', dim: 'rgba(61,214,140,0.10)' },
        amber:  { DEFAULT: '#e5a63e', dim: 'rgba(229,166,62,0.10)' },
        red:    { DEFAULT: '#e5534b', dim: 'rgba(229,83,75,0.10)' },

        // Borders — visible but quiet
        b1: 'rgba(255,255,255,0.06)',  // default
        b2: 'rgba(255,255,255,0.10)',  // hover
        b3: 'rgba(255,255,255,0.16)',  // active / focus
      },
      fontFamily: {
        sans: ['Geist', 'system-ui', 'sans-serif'],
        mono: ['Geist Mono', '"JetBrains Mono"', 'monospace'],
      },
    },
  },
}
</script>

<!-- Geist font -->
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/geist@1.3.1/dist/fonts/geist-sans/style.min.css">
<link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/geist@1.3.1/dist/fonts/geist-mono/style.min.css">
```

## Base Styles

Paste this `<style>` block in every UI:

```html
<style>
  *, *::before, *::after { margin: 0; padding: 0; box-sizing: border-box; }

  body {
    font-family: 'Geist', system-ui, sans-serif;
    background: #0c0c10;
    color: #ededf0;
    min-height: 100vh;
    -webkit-font-smoothing: antialiased;
    -moz-osx-font-smoothing: grayscale;
  }

  /* ── Ambient gradient orbs ─────────────────────────────
     Two soft blurred circles behind everything.
     They give glass surfaces something to distort. */
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

  /* Everything above the gradient orbs */
  body > * { position: relative; z-index: 1; }

  /* ── Glass surface ──────────────────────────────────── */
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

  /* ── Scrollbar ──────────────────────────────────────── */
  ::-webkit-scrollbar { width: 6px; height: 6px; }
  ::-webkit-scrollbar-track { background: transparent; }
  ::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.08); border-radius: 99px; }
  ::-webkit-scrollbar-thumb:hover { background: rgba(255,255,255,0.16); }

  /* ── Focus ring ─────────────────────────────────────── */
  :focus-visible {
    outline: 2px solid rgba(139,92,246,0.5);
    outline-offset: 2px;
  }

  /* ── Selection ──────────────────────────────────────── */
  ::selection {
    background: rgba(139,92,246,0.3);
    color: #ededf0;
  }
</style>
```

## Design Philosophy

### 1. Depth through glass, not borders
Cards are frosted glass panels. The ambient gradient orbs behind them create subtle color shifts when you scroll or resize. Borders exist but they're `rgba(255,255,255,0.06)` — felt, not seen.

### 2. One accent, three weights
Violet (`a1` = electric, `a2` = light hover, `a3` = tinted bg). That's the entire accent system. Status colors (green/amber/red) are for *state only* — never decorative.

### 3. Warm dark, not cold black
`s1` (#0c0c10) has a blue undertone. Pure black (#000) is banned. The surface stack goes s1→s2→s3→s4 with increasing brightness. Every step is perceivable.

### 4. Text has three tiers
`t1` (primary), `t2` (secondary), `t3` (muted). Never use `text-white` — it's too harsh. `t1` is #ededf0, warm and easy on the eyes.

### 5. Inputs are tinted, not hollow
Form fields use `bg-s2` or `bg-s3` background — NOT transparent. They should be clearly distinguishable from the surrounding surface. The border glows `a1` on focus.

### 6. Responsive is structural
Use Tailwind's responsive prefixes (`sm:` `md:` `lg:`). Grids use `auto-fit` with `minmax`. Touch targets are 44px minimum. Test at 320px.

## Layout

### Container
```html
<div class="mx-auto w-full max-w-5xl px-4 sm:px-6 lg:px-8 py-8 sm:py-12">
```

### Responsive Grid
```html
<!-- Auto-fit: no breakpoints needed -->
<div class="grid gap-4" style="grid-template-columns: repeat(auto-fit, minmax(min(100%, 18rem), 1fr))">

<!-- Or explicit breakpoints -->
<div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
```

## Components

### Glass Card
```html
<div class="glass rounded-2xl p-5 sm:p-6 transition-all duration-150
  hover:border-b2">
```
Never use flat `bg-s2` without the glass gradient. The gradient is what makes it feel like a surface, not a box.

### Badge
```html
<span class="inline-flex items-center h-5 px-2 rounded-full
  text-[11px] font-mono font-medium tracking-wide
  bg-a3 text-a2">
  Active
</span>
<!-- Status: bg-green-dim text-green / bg-amber-dim text-amber / bg-red-dim text-red -->
```
Badges are `rounded-full` (pill shape), not `rounded`. Small, quiet, informational.

### Button — Primary
```html
<button class="h-9 px-4 rounded-xl bg-a1 text-white text-[13px] font-medium
  hover:bg-a2 active:scale-[0.98] transition-all duration-150
  focus-visible:ring-2 focus-visible:ring-a1/50 focus-visible:ring-offset-2 focus-visible:ring-offset-s1">
  Action
</button>
```

### Button — Ghost
```html
<button class="h-9 px-4 rounded-xl border border-b1 text-t2 text-[13px] font-medium
  hover:border-b2 hover:text-t1 hover:bg-white/[0.02] transition-all duration-150">
  Secondary
</button>
```

### Input
```html
<input class="w-full h-10 px-3.5 rounded-xl bg-s3 border border-b1 text-t1 text-sm
  placeholder:text-t3 hover:border-b2
  focus:border-a1/50 focus:ring-1 focus:ring-a1/20 focus:outline-none
  transition-all duration-150" placeholder="Enter something...">
```
Key: `bg-s3` gives it a visible tinted background. NOT transparent, NOT white.

### Textarea
```html
<textarea class="w-full min-h-[6rem] px-3.5 py-2.5 rounded-xl bg-s3 border border-b1 text-t1 text-sm
  placeholder:text-t3 hover:border-b2
  focus:border-a1/50 focus:ring-1 focus:ring-a1/20 focus:outline-none
  resize-y leading-relaxed transition-all duration-150"></textarea>
```

### Select
```html
<select class="w-full h-10 px-3.5 rounded-xl bg-s3 border border-b1 text-t1 text-sm
  appearance-none hover:border-b2
  focus:border-a1/50 focus:ring-1 focus:ring-a1/20 focus:outline-none
  transition-all duration-150
  bg-[url('data:image/svg+xml;utf8,<svg xmlns=%22http://www.w3.org/2000/svg%22 width=%2212%22 height=%2212%22 fill=%22%2355556a%22><path d=%22M6 8L1 3h10z%22/></svg>')]
  bg-no-repeat bg-[position:right_12px_center]">
  <option>Option</option>
</select>
```

### Section Label
```html
<h3 class="text-[11px] font-mono font-medium uppercase tracking-[0.1em] text-t3 mb-3">
  Section
</h3>
```

### Interactive Row
```html
<div class="flex items-center gap-3 px-4 py-3 -mx-4 rounded-xl cursor-pointer
  hover:bg-white/[0.03] active:bg-white/[0.05] transition-all duration-150">
```
The `-mx-4` extends the hover zone to the container edges. Feels more spacious.

### Empty State
```html
<div class="flex flex-col items-center justify-center py-16 text-t3">
  <div class="w-10 h-10 rounded-full bg-s3 flex items-center justify-center text-lg mb-4">∅</div>
  <p class="text-sm">Nothing here yet</p>
</div>
```

### Toast
```html
<div class="fixed bottom-5 right-5 glass rounded-xl px-4 py-3 text-[13px] font-mono
  shadow-[0_8px_32px_rgba(0,0,0,0.4)] transition-all duration-200
  translate-y-2 opacity-0 [&.show]:translate-y-0 [&.show]:opacity-100">
```

### Drawer / Slide-over
```html
<div class="fixed inset-0 bg-black/60 backdrop-blur-sm z-50" onclick="close()">
  <div class="fixed top-0 right-0 bottom-0 w-full max-w-md bg-s1 border-l border-b1
    overflow-y-auto transition-transform duration-200 ease-out p-6" onclick="event.stopPropagation()">
  </div>
</div>
```

## Typography

| Role | Classes |
|------|---------|
| Display | `text-2xl sm:text-3xl font-semibold tracking-tight text-t1` |
| Title | `text-lg sm:text-xl font-semibold tracking-tight text-t1` |
| Heading | `text-base font-semibold text-t1` |
| Body | `text-sm text-t1 leading-relaxed` |
| Secondary | `text-sm text-t2` |
| Caption | `text-xs text-t3` |
| Label | `text-[11px] font-mono font-medium uppercase tracking-[0.1em] text-t3` |
| Data / Code | `text-[13px] font-mono text-t2` |

## Color Rules

1. **One accent** — `a1`/`a2`/`a3`. For interactive elements, focus, selection.
2. **Three text tiers** — `t1` (read this), `t2` (supporting), `t3` (whisper).
3. **Status = state change only** — `green`/`amber`/`red` with their `dim` variants. Never decorative.
4. **Surfaces stack** — `s1` < `s2` < `s3` < `s4`. Each step up = closer to the user.
5. **No raw hex** in component classes. Always use token names.
6. **No `text-white` or `bg-black`**. Use `t1` and `s1`.

## Motion

```
transition-all duration-150   /* default: hover, focus, border */
duration-200                  /* panels, drawers, modals */
duration-300                  /* page transitions */
active:scale-[0.98]           /* button press feedback */
```

`prefers-reduced-motion: reduce` → kill all transitions. Already handled by Tailwind's `motion-reduce:` prefix.

## Replacing This System

This file is read by agents before generating UI. To use your own aesthetic:

1. Create your own `design-system.md` in `apps/shared/`
2. Define your Tailwind config, base styles, and component patterns
3. All generated UIs will follow your system instead

The structure should match this file: Tailwind config → base styles → philosophy → components → typography → color rules. Agents look for these sections.

---

*Frosted glass floating on a dark void. Violet glow behind everything. Warm, not cold. Felt, not seen.*
