---
name: multi-page-and-pagination
description: Use when a task spans several pages: wizard forms, paginated results, infinite scroll, "load more", long lists.
triggers: next page, multi-step, multi page, multiple pages, wizard, pagination, load more, keep scrolling, all the results, step 1 of, step 2 of
---
# Multi-page flows and pagination

Done means: every page the task needs was handled, what you found is kept, and you know why you stopped.

Wizard forms
1. Find the progress cue ("Step 2 of 5") and note the total.
2. Per page: form_read, form_autofill, form_fill, then form_click Next. The reply says whether the page
   changed, the errors, and required_still_empty. Repeat on the new page.
3. Stay in the form's tab. Do not call switch_tab or open a new tab mid-form.
4. If page_changed is false: errors are the cause. Fix the fields it names, press once more. Still nothing:
   report the blocker.
5. Review page: compare with what you entered, then submit per the base prompt.

Results lists
- Decide how many pages the goal needs. Keep a one-line note: page, items taken.
- form_click Next, Next results or More. Stop at the first page that holds the answer.
- Infinite scroll: scroll a screen, observe. Nothing new twice means the end.

Same page repeating or three pages with nothing new: stop paging, use the site search or a filter
(find-your-way-on-a-website). Lost your place: see recovery. Report how far you got.
