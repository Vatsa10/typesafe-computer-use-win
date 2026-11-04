---
name: job-application
description: Use when the user wants to apply for a job, fill an application, or submit a resume on a careers site.
triggers: apply for this job, apply to, apply for, job application, submit my resume, apply on this site, careers page, application form
---
# Job application

Done means: every required field is filled from the user's real data, the resume is attached, the form is
submitted per the mode rule in the base prompt, and a confirmation was read on the page.

1. world: confirm the role's tab is in front. Do not switch tabs once the form is open.
2. find_documents (kind resume), say which file and its date, then get_profile.
3. Press Apply with form_click. A new page or tab opens: world, then continue there.
4. Per page: form_read, form_autofill, form_fill for the remaining fields, form_click Next or Continue.
   Fix errors the click reports before pressing again.
5. Upload the resume through the page's own file control; if it fails, give the user the path.
6. Free-text questions: two plain sentences from the profile and the posting.
7. Never invent salary, notice, visa, demographics, attestations. Required and unknown: ask once, all together,
   then save_answer.
8. Before the last Submit follow the base prompt (confirm tool in ask mode). After it, read new_on_page for
   the confirmation and report it.

Blocked by login, captcha or payment: stop and tell the user. Two different routes failed on one field:
say what is left. A Next that changes nothing: read errors, fix, retry once, see recovery.
