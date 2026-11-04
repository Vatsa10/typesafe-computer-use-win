---
name: fill-form
description: Use when filling any web or app form: sign-up, contact, survey, checkout details, profile fields.
triggers: fill this form, fill out, fill in, enter my details, sign up form, complete the form, contact form, register, registration, checkout form
---
# Fill a form

Done means: every required field holds a correct value from a known source, errors are clear, and the
form is submitted (or left ready, per the mode rule in the base prompt).

Tools: form_read, form_autofill, form_fill, form_click, form_answer, get_profile, save_answer.

1. form_read: fields, required marks, current values, including below the fold.
2. get_profile once, then form_autofill. It fills everything it can answer and returns `remaining`.
3. For each remaining field: form_answer, then form_fill with every entry you have a value for. Retry a
   mismatch once. Leave out what has no answer.
4. Sensitive fields (salary, visa, health, demographics): only from form_answer or saved answers. needs_user
   on an optional field: skip. On a required one: ask once, listing all missing, then save_answer.
5. Check the errors in the form_fill reply, then form_click Next or Submit. Do not use pointer_run for page
   buttons.
6. Done only when form_click shows a confirmation in new_on_page, or page_changed with no errors and
   required_still_empty empty. Report what you saw.

Never: invent facts, enter a password or card number, tick consent boxes the user did not approve.
If form_fill finds no fields, fall back to pointer_run with one goal. Typing went nowhere or a banner is
in the way: see recovery.
