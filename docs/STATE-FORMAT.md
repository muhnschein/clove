# clove state format

clove's on-disk state is an **API**, not an implementation detail (the SQLite
doctrine, `SCOPE.md` §3). This file specifies it. The rules:

- Every resume file carries an integer `version` (currently **3**).
- **Newer clove reads older state.** Older clove **refuses newer state
  cleanly** — a clear error, no write, no corruption (worst case: you downgrade
  and re-add the torrent).
- A format change bumps `version` and is a release-notes headline item.
- Files are written **atomically** (write to `<name>.tmp`, `fsync`, `rename`
  over the target). A crash mid-write never corrupts state; the worst outcome
  is re-verification.

## Directory layout

Under the data directory (`data_dir`, default `$XDG_DATA_HOME/clove`):

```
<data_dir>/
├── token                       API token (32 random bytes, hex, mode 0600)
├── state/
│   ├── <info-hash>.torrent     the exact .torrent bytes, verbatim
│   └── <info-hash>.resume      resume data (bencode, see below)
└── downloads/
    └── <name>/…                the torrent's files
```

`<info-hash>` is the 40-character lowercase hex of the torrent's info-hash.
The `.torrent` is stored byte-for-byte so the info dictionary (and thus the
info-hash) is preserved exactly; clove never re-encodes it.

## Resume file (`<info-hash>.resume`)

A single bencoded dictionary. Keys (all required unless noted); unknown keys are
**refused** on read, because resume files are machine-written and an unexpected
key means version discipline failed somewhere:

| Key | Type | Meaning |
|---|---|---|
| `version` | int | Format version. A version above the one this clove knows is refused. |
| `info_hash` | 20 bytes | The torrent identity; must match the sibling `.torrent`. |
| `num_pieces` | int ≥ 1 | Piece count, so the bitfields are unambiguous. |
| `have` | bytes | MSB-first piece bitfield (BEP 3); trailing spare bits must be 0. |
| `verified` | bytes | Pieces that passed SHA-1 verification (same shape as `have`). |
| `priorities` | bytes | One byte per file, on-wire order: 0 skip, 1 normal, 2 high. |
| `uploaded` | int | Lifetime bytes uploaded. |
| `downloaded` | int | Lifetime bytes downloaded. |
| `trackers` | list of list of byte-strings | Announce tiers (BEP 12) in current order. |
| `paused` | int (optional, v2+) | `1` if paused. Absent (a v1 file) reads as `0`. |
| `sequential` | int (optional, v3+) | `1` to pick pieces in file order instead of rarest-first. Absent (a v1 or v2 file) reads as `0`. |

Version history: **v1** initial; **v2** added the optional `paused` flag; **v3**
added the optional `sequential` flag.

`have` and `verified` are stored separately on purpose: a crash between writing
a piece and verifying it costs a re-verification, never false trust.

The canonical encoder/decoder and its hostile-input tests live in
`clove-core::resume`; this document tracks what that module guarantees.

## Compatibility procedure for a version bump

1. Add the new field(s) with a sane default so a *missing* field (old file read
   by new clove) is well-defined.
2. Bump `VERSION`.
3. Confirm old clove rejects the new file with the `FutureVersion` error (it
   does so for any `version` greater than it knows).
4. Note the change in the release notes.
