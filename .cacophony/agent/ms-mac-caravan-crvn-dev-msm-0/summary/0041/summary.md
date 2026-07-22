# Follow-up summary — collapsed sidebar grid stability

## Regression

Desktop sidebars participate in CSS grid auto-placement. Setting the repository sidebar `hidden` removed that grid item; the content element then auto-shifted into the first, deliberately zero-width collapsed column, making the entire application appear to disappear.

## Fix

- Repository sidebar is explicitly pinned to grid column 1.
- Main content is explicitly pinned to grid column 2.
- Attention sidebar is explicitly pinned to grid column 3.
- Collapsing either sidebar now changes only that column to zero and expands the main content into the freed horizontal space; hiding a grid item cannot reorder the remaining panels.
- The existing mobile rule continues to pin content to its single column.

## Proof

- Browser render at 1440×900 clicked `#toggle-repositories`.
- Repository panel became hidden.
- Dashboard remained visible.
- Main content bounding box became x=0, width=1155px.
- Computed grid columns were `0px 1155px 285px`, proving only the repository column collapsed while attention remained.
- JavaScript syntax, focused embedded asset contract and diff checks green.
- Asset contract now asserts all three explicit desktop column assignments.

## Commit

- `926b98e`.
