# Dhan API Support Communication Archive

Every technical email to Dhan API support (`apihelp@dhan.co`) is drafted
here as a markdown file, committed to git, and shared with Dhan as a
**GitHub rendered link**. The goal is:

- **One source of truth** — every incident, every request, every reply is in git
- **Perfect formatting** — GitHub renders tables, headers, code blocks natively
- **Zero Gmail layout issues** — Arial proportional font destroys ASCII tables
- **Full history** — every past communication is browsable, searchable, diff-able
- **Precise details by default** — the template enforces contract-level specificity

## Workflow

1. **New incident / reply needed?** Copy `TEMPLATE.md`:
   ```bash
   cp docs/dhan-support/TEMPLATE.md \
      docs/dhan-support/YYYY-MM-DD-<ticket>-<topic>.md
   ```

2. **Fill in every placeholder.** Run this to find them all:
   ```bash
   grep -n '<[A-Z_]' docs/dhan-support/YYYY-MM-DD-<ticket>-<topic>.md
   ```

3. **Commit and push:**
   ```bash
   git add docs/dhan-support/YYYY-MM-DD-<ticket>-<topic>.md
   git commit -m "docs(dhan-support): <subject>"
   git push
   ```

4. **Copy the GitHub URL** from the pushed branch:
   ```
   https://github.com/SJParthi/tickvault/blob/<branch>/docs/dhan-support/YYYY-MM-DD-<ticket>-<topic>.md
   ```

5. **Reply in Gmail with a SHORT message + the link:**

   > Hi Trishul,
   >
   > Full details with server correlation data, verbatim logs, and
   > reproduction contracts are here:
   > https://github.com/SJParthi/tickvault/blob/.../docs/dhan-support/2026-04-15-reply-to-trishul.md
   >
   > Thank you,
   > Parthiban

6. **Do NOT paste the markdown body into Gmail.** Gmail's proportional
   font will misalign tables. Share the link, let GitHub render it.

## Non-negotiable details in every email

Every support email must include:

- **Client ID** (always `1106656882`)
- **Name** (Parthiban Subramanian)
- **UCC** (NWXF17021Q)
- **Precise contract labels** (e.g. `NIFTY-Jun2026-28500-CE` — NOT generic `NIFTY-ATM-CE`)
- **SecurityId** for every contract cited
- **Microsecond timestamps** in IST
- **Verbatim JSON logs** in fenced code blocks
- **What works + what fails** table (rules out account/token/IP issues)
- **Specific numbered questions** (not "please help")
- **Diagnostic offer** ("we are happy to run tcpdump, different SIDs, etc.")

## File naming convention

```
YYYY-MM-DD-<ticket-number-if-any>-<short-topic>.md
```

Examples:
- `2026-04-09-ticket-5519522-initial-200-depth-failure.md`
- `2026-04-15-ticket-5519522-still-failing.md`
- `2026-04-15-reply-to-trishul.md`
- `2026-04-20-ticket-5525125-prev-close-routing.md`

## Archive

| File | Ticket | Subject |
|---|---|---|
| `TEMPLATE.md` | — | Reusable template |
| `2026-04-15-ticket-5519522-still-failing.md` | #5519522 | Initial escalation after both fixes applied |
| `2026-04-15-reply-to-trishul.md` | #5519522 | Reply to Trishul with today's 4 precise ATM contracts |

## Source of the precise labels

The precise contract labels in every email (e.g. `NIFTY-Jun2026-28500-CE`)
come directly from the app's structured logs, which were wired in commits:

- `4d9350a` — Stage C.2 WAL + 4-WS dual-write
- `3903193` — 200-depth precise contract labels in logs + Telegram

Every 200-depth disconnect now emits a structured log line with:
- `contract=NIFTY-Jun2026-28500-CE`
- `security_id=55190`

Plus a Telegram alert with the same precise info. **So future emails can
be drafted straight from the Telegram alert text** — no manual
correlation against the subscription planner needed.
