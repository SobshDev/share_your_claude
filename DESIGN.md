# Shared Router interface

Owner operations dashboard for reading a small amount of real account data and making explicit access changes. The design source is the Paper file "Shared Router — Dashboard Redesign".

Simplicity first. The interface carries no subtitles, helper paragraphs, or explanations; when something needs explaining, change the design until it does not. Labels are one or two words, and a control's state shows what it does (a switch, a checked box, a "New" badge, a "Shown once" tag).

Structure: a 240px rail (Overview and Requests under Usage; People & keys, Models, and Claude connection under Access, with the connection status and Sign out at the bottom) and one 880px column. Overview and Requests share one filter bar: person, model, and a 7d / 30d / 90d / Custom period. Rare actions live in a "…" menu; destructive ones sit last, in red, behind a confirmation. Sheets (dialogs) hold one task each. On narrow screens the rail becomes a horizontal row above the page.

Tokens: white surface, recessed #FAFAF9, lines #EDEDEA and #DCDCD8, ink #111, muted #6B6B66, faint #A8A8A2, primary evergreen #1E5B3E, soft #8DB39B, wash #EEF4F0, partial #9A6B00, failure #B4321F. Type is Manrope, with IBM Plex Mono for small uppercase labels, IDs, and code; both fall back to system fonts. Numbers are tabular and right-aligned. Unknown values show an em dash; unmeasured and failed requests stay visible in their own colors.

A new key's secret appears once, in a sheet that stays open until it is copied, and is removed from the DOM when the sheet closes. Every form has visible labels and recoverable error feedback. Honor reduced-motion preferences. Product truth comes from PRODUCT.md and the behavior documented in README.md.
