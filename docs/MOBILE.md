# WebUI — Touchscreen Optimization Report

**Target device:** 800×600 touchscreen tablet, poor finger sensitivity
**Date:** 2026-03-07

---

## Critical — Tap Target Size

The single biggest issue. Current controls are **26px tall** — far too small for poor finger sensitivity. Standard recommendation is 44px (Apple HIG) or 48px (Material Design). On 800×600 a compromise of **36–40px** is realistic.

| Element | Current | Recommended |
|---|---|---|
| Filter inputs, selects, buttons | 26px | 36px |
| MType / Column picker dropdown items | `5px 10px` padding | `10px 14px` |
| Live/Pause toggle | 26px | 36px |
| Status bar height | 32px | 40px |
| Table row height | ~20px (2px pad) | ~28px (6px pad) |

## Important — Header Density

At 800px wide the header barely fits in one row. Making controls 36px tall is fine vertically, but **touch accuracy also suffers horizontally** — controls spaced only 4–6px apart get accidentally mis-tapped.

**Recommendation:** Move the filter controls to a **collapsible second row** toggled by a single filter button in the header. The header collapses to just: `[Title] [⚙ Cols] [↓ CSV] [Filters ▼] [● Live]` — all large, well-spaced. Tapping "Filters ▼" expands a second row at full 800px width where filters have more breathing room.

This also reclaims header vertical space for the table when filters are not needed.

## Important — `<details>` Dropdowns

The MType multi-select and Column picker use `<details>/<summary>`. These are **unreliable on touch** — they often don't close when tapping outside, and opening them can scroll the page on some tablet browsers. Recommend replacing with `onClick`-toggled `<div>` panels or at minimum adding:

```css
touch-action: manipulation; /* kills 300ms tap delay */
```

on all interactive elements.

## Moderate — Font Size in Table

Data cells are 12px. On a tablet at arm's length, **13px for data and 11px for headers** improves readability without sacrificing columns.

## Moderate — Sort Header Target

The **Time column header** is the sort button — it's a `<th>` roughly 26px tall. On a 800px table that's a ~140px wide × 26px tall touch target at the top edge. Fine for mouse, hard to tap reliably. Recommend making it a clearly styled dedicated button within the header cell, or increasing `th` height to 32px.

## Minor — Checkbox Tap Areas

Checkboxes in the MType and Column pickers are bare `<input>` elements. The label wraps them (so the whole label row is tappable), which is correct — but the **single-column layout** would be safer than the current 2-column grid for the Column picker, since columns are only ~50px wide each.

---

## Summary

| Priority | Change | Where |
|---|---|---|
| 🔴 Must | Controls height 26px → 36px | `index.css` |
| 🔴 Must | Table row padding 2px → 6px | `index.css` |
| 🔴 Must | Dropdown item padding → 10px 14px | `index.css` |
| 🟠 Should | Collapsible filter row | `App.tsx` + `FilterBar.tsx` |
| 🟠 Should | `touch-action: manipulation` on controls | `index.css` |
| 🟡 Nice | Replace `<details>` with click-toggled divs | components |
| 🟡 Nice | Column picker → single-column list | `index.css` |
| 🟡 Nice | Table data font 12px → 13px | `index.css` |
