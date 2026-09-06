# ADR number reservations

A row here means a lane has claimed a number and is working on the real
ADR — see `scripts/adr-reserve.sh` (ADR 0131). Claim before starting work,
never after: reading this table and `docs/adr/README.md`'s own table to
guess "the next free number" is exactly the race ADR 0131 exists to close.

**The row is temporary.** The PR that lands the real
`docs/adr/NNNN-slug.md` and its `docs/adr/README.md` row deletes this row
in the same commit — the number's home becomes the real table, and it has
no further business here. A row that outlives its lane (the branch was
abandoned, the PR was closed unmerged) is released with
`scripts/adr-release.sh <number>`, which refuses to touch a number that
already has a real entry in `docs/adr/README.md`.

| Number | Reserved (UTC) | Branch | Working title |
| --- | --- | --- | --- |
