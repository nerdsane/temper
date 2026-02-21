# Temper UI Design System v2

*One system. Any surface. Always responsive. Always snatched.*

## Foundation

**Tailwind CSS via CDN** — every Temper UI includes this in `<head>`:

```html
<script src="https://cdn.tailwindcss.com"></script>
<script>
tailwindcss.config = {
  theme: {
    extend: {
      colors: {
        // Surfaces
        base: { DEFAULT: '#0a0a0c', 50: '#12121a', 100: '#1a1a24', 200: '#242430' },
        // Primary palette — cool indigo with slight warmth
        pri: { DEFAULT: '#7c6cff', dim: 'rgba(124,108,255,0.12)', light: '#9d8fff', dark: '#5b4dd4' },
        // Semantic — muted, not screaming
        ok:   { DEFAULT: '#34d399', dim: 'rgba(52,211,153,0.12)' },
        warn: { DEFAULT: '#fbbf24', dim: 'rgba(251,191,36,0.12)' },
        err:  { DEFAULT: '#f87171', dim: 'rgba(248,113,113,0.12)' },
        // Text
        txt:  { DEFAULT: '#eeeef2', 2: '#9898a8', 3: '#5c5c6e' },
        // Glass border
        glass: { DEFAULT: 'rgba(255,255,255,0.06)', 2: 'rgba(255,255,255,0.1)' },
      },
      fontFamily: {
        sans: ['Inter', 'system-ui', 'sans-serif'],
        mono: ['"JetBrains Mono"', '"SF Mono"', 'monospace'],
      },
      backgroundImage: {
        'glass': 'linear-gradient(135deg, rgba(255,255,255,0.03) 0%, rgba(255,255,255,0.01) 100%)',
        'glass-hover': 'linear-gradient(135deg, rgba(255,255,255,0.06) 0%, rgba(255,255,255,0.02) 100%)',
        'glow': 'radial-gradient(ellipse at 50% 0%, rgba(124,108,255,0.15) 0%, transparent 60%)',
      },
      boxShadow: {
        'glass': '0 0 0 1px rgba(255,255,255,0.06), 0 2px 12px rgba(0,0,0,0.3)',
        'glass-lg': '0 0 0 1px rgba(255,255,255,0.06), 0 8px 32px rgba(0,0,0,0.4)',
        'glow': '0 0 24px rgba(124,108,255,0.15)',
        'glow-sm': '0 0 12px rgba(124,108,255,0.1)',
      },
      borderRadius: {
        'xl': '12px',
        '2xl': '16px',
      },
    },
  },
}
</script>
<link href="https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap" rel="stylesheet">
```

## Philosophy

1. **Glass over borders.** Surfaces are semi-transparent with subtle gradients. Not flat cards with hard borders — glass panels that breathe.
2. **One palette, three weights.** Primary (`pri`), plus dim and light variants. Semantic colors only for actual state. That's it.
3. **Responsive is not optional.** If it doesn't work at 320px, it's broken. Tailwind's responsive prefixes (`sm:`, `md:`, `lg:`) + grid `auto-fit` handle this. No excuses.
4. **Gradients are gentle.** Never saturated. Always near-transparent whites or the faintest accent glow. The gradient should be felt, not seen.
5. **Typography carries hierarchy.** Size + weight + opacity. Not color.
6. **Mono for data, sans for everything else.** Timestamps, IDs, metrics, code → `font-mono`. Prose, labels, headings → `font-sans`.
7. **Motion is subtle.** `transition-all duration-150 ease-out`. Never bounce. Never delay.

## Base Styles

Every Temper UI includes this `<style>` block after the Tailwind config:

```html
<style>
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body {
    font-family: 'Inter', system-ui, sans-serif;
    background: #0a0a0c;
    color: #eeeef2;
    min-height: 100vh;
    -webkit-font-smoothing: antialiased;
  }
  /* Glass card */
  .glass {
    background: linear-gradient(135deg, rgba(255,255,255,0.04) 0%, rgba(255,255,255,0.01) 100%);
    border: 1px solid rgba(255,255,255,0.06);
    backdrop-filter: blur(12px);
    -webkit-backdrop-filter: blur(12px);
  }
  .glass:hover {
    border-color: rgba(255,255,255,0.1);
  }
  /* Glow accent on headers/heroes */
  .glow-top {
    background-image: radial-gradient(ellipse at 50% -20%, rgba(124,108,255,0.12) 0%, transparent 60%);
  }
  /* Scrollbar */
  ::-webkit-scrollbar { width: 6px; }
  ::-webkit-scrollbar-track { background: transparent; }
  ::-webkit-scrollbar-thumb { background: rgba(255,255,255,0.08); border-radius: 3px; }
  ::-webkit-scrollbar-thumb:hover { background: rgba(255,255,255,0.15); }
  /* Focus */
  :focus-visible {
    outline: 2px solid rgba(124,108,255,0.5);
    outline-offset: 2px;
  }
</style>
```

## Layout Patterns

### Container
```html
<div class="mx-auto w-full max-w-6xl px-4 sm:px-6 lg:px-8 py-8">
```

### Responsive Grid (auto-fit, no breakpoints needed)
```html
<div class="grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4">
```

Or truly fluid:
```html
<div class="grid gap-4" style="grid-template-columns: repeat(auto-fit, minmax(min(100%, 18rem), 1fr))">
```

### Stack
```html
<div class="flex flex-col gap-4">
```

### Cluster
```html
<div class="flex flex-wrap items-center gap-2">
```

## Component Patterns

### Glass Card
```html
<div class="glass rounded-xl p-4 sm:p-5 transition-all duration-150">
  <!-- content -->
</div>
```

### Badge
```html
<span class="inline-flex items-center px-2 py-0.5 rounded text-[11px] font-mono font-medium uppercase tracking-wider bg-pri-dim text-pri">
  Active
</span>
```

Semantic variants:
- OK: `bg-ok-dim text-ok`
- Warn: `bg-warn-dim text-warn`
- Error: `bg-err-dim text-err`

### Button — Primary
```html
<button class="h-9 px-4 rounded-lg bg-pri text-white text-sm font-medium
  hover:bg-pri-light active:bg-pri-dark transition-all duration-150
  focus-visible:ring-2 focus-visible:ring-pri/50 focus-visible:ring-offset-2 focus-visible:ring-offset-base">
  Action
</button>
```

### Button — Ghost
```html
<button class="h-9 px-4 rounded-lg border border-glass text-txt-2 text-sm font-medium
  hover:border-glass-2 hover:text-txt transition-all duration-150">
  Secondary
</button>
```

### Input
```html
<input class="w-full h-9 px-3 rounded-lg bg-base-50 border border-glass text-txt text-sm
  placeholder:text-txt-3 focus:border-pri/50 focus:outline-none transition-all duration-150">
```

### Section Label
```html
<h3 class="text-[11px] font-mono font-medium uppercase tracking-[0.08em] text-txt-3 mb-3">
  Section Title
</h3>
```

### Interactive Row
```html
<div class="flex items-center gap-3 px-4 py-3 rounded-lg cursor-pointer
  hover:bg-white/[0.03] transition-all duration-150">
```

### Toast
```html
<div class="fixed bottom-6 right-6 glass rounded-lg px-5 py-3 text-sm font-mono
  shadow-glass-lg transition-all duration-200 translate-y-2 opacity-0"
  id="toast">
</div>
```

### Empty State
```html
<div class="flex flex-col items-center justify-center py-12 text-txt-3 text-sm">
  <span class="text-2xl mb-3">∅</span>
  <span>Nothing here yet</span>
</div>
```

### Drawer / Slide-over
```html
<!-- Overlay -->
<div class="fixed inset-0 bg-black/50 z-50 transition-opacity" onclick="close()">
  <!-- Panel -->
  <div class="fixed top-0 right-0 bottom-0 w-full max-w-md bg-base border-l border-glass
    p-6 overflow-y-auto transition-transform duration-200" onclick="event.stopPropagation()">
  </div>
</div>
```

## Color Rules

1. **Primary (`pri`)** — interactive elements, accents, focus rings, selected states
2. **Semantic** — ONLY for actual state: `ok` = success/healthy, `warn` = caution/pending, `err` = error/danger
3. **Text hierarchy** — `txt` (primary), `txt-2` (secondary), `txt-3` (muted/labels)
4. **Surfaces** — `base` (darkest bg), `base-50` (input bg), `base-100` (card bg), `base-200` (elevated)
5. **Glass** — `glass` (subtle border), `glass-2` (hover border)
6. **Never use raw hex in components.** Always reference the token.

## Glass & Gradient Rules

- **Cards**: Always use `.glass` class. Never flat `bg-*` without the gradient.
- **Page header area**: Use `.glow-top` for a subtle purple radial glow from the top
- **Hover states**: Border goes from `glass` → `glass-2`. Background shifts by +2% white.
- **Selected/active**: `bg-pri-dim border-pri/30` — tinted, not flooded
- **Gradients are always white-to-transparent or accent-to-transparent.** Never two saturated colors.

## Responsive Rules

1. **Text**: Base 14px body. 13px on mobile is fine for data. Never below 11px for anything.
2. **Padding**: `p-4 sm:p-5 lg:p-6` — scales up, never cramped.
3. **Grids**: `grid-cols-1 sm:grid-cols-2 lg:grid-cols-3` or `auto-fit` with `minmax`.
4. **Tables on mobile**: Convert to stacked cards below `sm:`. Or hide low-priority columns.
5. **Touch targets**: Minimum 36px height for any interactive element. 44px preferred on mobile.
6. **Drawers**: `w-full max-w-md` — full width on mobile, capped on desktop.
7. **Test at 320px, 768px, 1440px.** If it breaks at any, it's wrong.

## Typography

| Role | Classes |
|------|---------|
| Display | `text-2xl sm:text-3xl font-bold tracking-tight` |
| Title | `text-lg sm:text-xl font-semibold tracking-tight` |
| Heading | `text-base font-semibold` |
| Body | `text-sm` |
| Caption | `text-xs text-txt-2` |
| Label | `text-[11px] font-mono font-medium uppercase tracking-[0.08em] text-txt-3` |
| Data | `text-[13px] font-mono` |

## Agent Prompt

When generating a Temper UI, include this in the prompt to CC or any agent:

> Build a single-file HTML page. Include Tailwind CDN with the custom config from `apps/shared/design-system.md`. Use glass cards, subtle gradients, and the pri/ok/warn/err palette. Make it fully responsive — test at 320px and 1440px. Use Inter for text, JetBrains Mono for data. Follow the component patterns exactly. The UI should feel like frosted glass floating on a dark void with a faint purple glow.
