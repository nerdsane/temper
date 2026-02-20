# Temper UI Design System

*For any agent generating any UI served by Temper. Not a template — a system.*

## Philosophy

- **Minimal but alive.** Whitespace is structure. Borders are rare. Color is signal.
- **Fluid, not fixed.** Everything scales. No breakpoint snapping — fluid type, fluid space, container queries.
- **Monochrome + one accent.** The palette is zinc. Color means something happened.
- **Typography carries hierarchy.** If you need color to show importance, your type is wrong.

## Tokens

### Colors (Dark Mode Default)

```css
:root {
  /* Surfaces — zinc scale, not pure black */
  --bg:         #09090b;
  --surface:    #18181b;
  --surface-2:  #27272a;
  --surface-3:  #3f3f46;
  
  /* Text */
  --text:       #fafafa;
  --text-2:     #a1a1aa;
  --text-3:     #71717a;
  
  /* Borders — barely there */
  --border:     #27272a;
  --border-2:   #3f3f46;
  
  /* Accent — one hue, three weights */
  --accent:     #6c8cff;
  --accent-dim: rgba(108, 140, 255, 0.15);
  --accent-bg:  rgba(108, 140, 255, 0.08);
  
  /* Semantic — only for state */
  --ok:         #4ade80;
  --ok-dim:     rgba(74, 222, 128, 0.15);
  --warn:       #fbbf24;
  --warn-dim:   rgba(251, 191, 36, 0.15);
  --err:        #f87171;
  --err-dim:    rgba(248, 113, 113, 0.15);
  --info:       #22d3ee;
  --info-dim:   rgba(34, 211, 238, 0.15);
  --purple:     #a78bfa;
  --purple-dim: rgba(167, 139, 250, 0.15);
}
```

### Typography

```css
/* Two fonts. No more. */
--font-sans: 'Inter', -apple-system, system-ui, sans-serif;
--font-mono: 'JetBrains Mono', 'SF Mono', 'Cascadia Code', monospace;

/* Import */
@import url('https://fonts.googleapis.com/css2?family=Inter:wght@300;400;500;600;700&family=JetBrains+Mono:wght@400;500&display=swap');
```

**Scale** — fluid, clamped:

| Role | Size | Weight | Font |
|------|------|--------|------|
| Display | clamp(1.5rem, 3vw, 2.25rem) | 700 | sans |
| Title | clamp(1.125rem, 2vw, 1.5rem) | 600 | sans |
| Heading | 1rem | 600 | sans |
| Body | 0.875rem (14px) | 400 | sans |
| Caption | 0.75rem (12px) | 400 | sans |
| Label | 0.6875rem (11px) | 500, uppercase, ls: 0.08em | sans |
| Code/Data | 0.8125rem (13px) | 400 | mono |

**Line height:** headings 1.2, body 1.5, tight data 1.3.

**Letter spacing:** headings -0.02em, labels +0.08em.

### Spacing

```
--space-1: 4px    /* icon gaps, tight coupling */
--space-2: 8px    /* related elements */
--space-3: 12px   /* compact padding */
--space-4: 16px   /* standard gap, card padding */
--space-5: 24px   /* section breaks */
--space-6: 32px   /* major sections */
--space-7: 48px   /* page sections */
--space-8: 64px   /* hero spacing */
```

Only use these values. No 14px. No 22px. No 38px.

### Radius

```
--radius-sm: 4px   /* badges, small buttons */
--radius-md: 8px   /* cards, inputs */
--radius-lg: 12px  /* panels, modals */
--radius-xl: 16px  /* hero cards */
--radius-full: 9999px  /* pills, avatars */
```

### Depth

**Borders over shadows.** Shadows only for floating elements (tooltips, dropdowns).

```css
/* Surface separation = border, not shadow */
.card { border: 1px solid var(--border); }

/* Only floating elements get shadow */
.tooltip, .dropdown, .modal {
  box-shadow: 0 8px 32px rgba(0,0,0,0.4), 0 2px 8px rgba(0,0,0,0.2);
}
```

### Motion

```css
--ease: cubic-bezier(0.16, 1, 0.3, 1);  /* snappy decel */
--duration-fast: 100ms;   /* hover, focus */
--duration-norm: 200ms;   /* transitions */
--duration-slow: 400ms;   /* entrances */
```

Reduce motion for `prefers-reduced-motion`.

## Layout

### Responsive Container

```css
.container {
  width: min(100% - 2rem, 72rem);
  margin-inline: auto;
}
```

Never `max-width: 1200px; margin: 0 auto; padding: 0 24px`. The `min()` pattern is shorter and fluid.

### Grid

Use CSS Grid with `auto-fit` / `auto-fill` and `minmax()`. Not fixed column counts.

```css
/* Cards, stats, any repeating items */
.grid { 
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(min(100%, 16rem), 1fr));
  gap: var(--space-4);
}
```

This is responsive without a single media query.

### Stack (Vertical Rhythm)

```css
.stack > * + * { margin-block-start: var(--space-4); }
.stack-sm > * + * { margin-block-start: var(--space-2); }
.stack-lg > * + * { margin-block-start: var(--space-6); }
```

### Cluster (Horizontal Wrap)

```css
.cluster {
  display: flex;
  flex-wrap: wrap;
  gap: var(--space-2);
  align-items: center;
}
```

## Components (Atoms)

These are patterns, not rigid components. Adapt the shape, keep the tokens.

### Badge

```css
.badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 8px;
  border-radius: var(--radius-sm);
  font: 500 0.6875rem/1.3 var(--font-mono);
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
/* Semantic variants use dim backgrounds + full-chroma text */
.badge-ok   { background: var(--ok-dim);   color: var(--ok); }
.badge-warn { background: var(--warn-dim); color: var(--warn); }
.badge-err  { background: var(--err-dim);  color: var(--err); }
```

### Button

```css
.btn {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: var(--space-1);
  height: 36px;
  padding: 0 var(--space-4);
  border-radius: var(--radius-md);
  font: 500 0.8125rem var(--font-sans);
  cursor: pointer;
  transition: all var(--duration-fast) var(--ease);
  border: 1px solid transparent;
}
.btn-primary { background: var(--accent); color: #fff; }
.btn-primary:hover { filter: brightness(1.1); }
.btn-ghost { background: transparent; border-color: var(--border); color: var(--text-2); }
.btn-ghost:hover { border-color: var(--accent); color: var(--accent); }
```

### Input

```css
.input {
  height: 36px;
  padding: 0 var(--space-3);
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  color: var(--text);
  font: 0.875rem var(--font-sans);
  transition: border-color var(--duration-fast);
}
.input:focus { border-color: var(--accent); outline: none; }
.input::placeholder { color: var(--text-3); }
```

### Card

```css
.card {
  background: var(--surface);
  border: 1px solid var(--border);
  border-radius: var(--radius-lg);
  padding: var(--space-4);
}
```

### Table Row (Interactive)

```css
.row {
  display: grid;
  align-items: center;
  gap: var(--space-3);
  padding: var(--space-3) var(--space-4);
  border-radius: var(--radius-md);
  cursor: pointer;
  transition: background var(--duration-fast);
}
.row:hover { background: var(--surface-2); }
.row + .row { border-top: 1px solid var(--border); }
```

### Toast

```css
.toast {
  position: fixed;
  bottom: var(--space-5);
  right: var(--space-5);
  background: var(--surface-2);
  border: 1px solid var(--border);
  border-radius: var(--radius-md);
  padding: var(--space-3) var(--space-5);
  font: 0.8125rem var(--font-mono);
  transform: translateY(8px);
  opacity: 0;
  transition: all var(--duration-norm) var(--ease);
}
.toast.show { transform: translateY(0); opacity: 1; }
```

## Rules

1. **No raw pixel values outside the token scale.** If you need 14px, use 12 or 16.
2. **Mono font for data, sans font for prose.** Numbers, IDs, timestamps, code → mono. Everything else → sans.
3. **Labels are quiet.** 11px, uppercase, letter-spaced, `--text-3`. They support data, never compete with it.
4. **Color = state change.** Green/amber/red mean something happened. Don't use semantic colors decoratively.
5. **One accent.** If a second color is needed, use `--purple` sparingly. Three accent colors means zero accent colors.
6. **Hover = border highlight, not background flood.** `border-color: var(--accent)` on hover. Not a full accent background.
7. **Responsive by default.** Every layout uses `min()`, `clamp()`, `auto-fit`, or container queries. Zero media queries for basic responsiveness.
8. **Animate only what moves.** Border, opacity, transform. Not width, height, or layout properties.
9. **Focus states are mandatory.** Every interactive element needs a visible `:focus-visible` ring.
10. **Empty states exist.** Every list, table, and container must handle "no data" gracefully. Not blank — a message in `--text-3`.

## Generating UI

When an agent (CC, Haku, any agent) generates a Temper UI:

1. Include the `@import` for Inter + JetBrains Mono
2. Paste the `:root` tokens block
3. Use the layout primitives (container, grid, stack, cluster)
4. Build with the atom patterns (badge, btn, input, card, row, toast)
5. Never invent new spacing values, colors, or font sizes outside the scale
6. Test: resize the browser from 320px to 2560px. If anything breaks, it's wrong.

The UI should look like one hand built the whole thing, whether it's a proposal pipeline, a content calendar, a form wizard, or a world map.
