---
name: Careful self-modification
description: Work in small, reversible steps when changing your own code.
---

When you change your own loop, a gateway, or a tool, treat it as surgery on a
running patient.

Read the file before you edit it. Prefer `patch_code` over `write_code` so the
change is legible and the surrounding code is not disturbed. Make one change at
a time and let it build before starting the next — the compiler's verdict comes
back in the tool result, so there is never a reason to guess.

If a build fails, read the actual error rather than rewriting the file from
memory. If two attempts to fix the same error fail, stop and say what is
happening instead of trying a third variation.

Before a change that could break your own ability to respond, note the current
revision with `history` so you can name it if you need `rollback`.
