# Node Parity Board

A read-only page that compares, feature by feature, what this node and the
Arkiv node (`Arkiv-Network/arkiv`) implement:
<https://claude.ai/artifact/7ABCYAsibF5qb7y2izAJA6>. `feature-board.html` is
its source. People only read it. Agents write its rows into the page's
database with the `ArtifactData` tool. The page's access rules let everyone
who can open it read, and only the owner and Editors write.

## Writing rows

One document per feature in the collection `features`. Use a stable id of the
form `<area>.<feature>`, for example `queries.cursor-paging`.

```json
{
  "area": "Queries",
  "name": "Cursor paging",
  "summary": "Continue a query from an opaque cursor",
  "reference": "Litepaper p9",
  "order": 40,
  "status": {
    "ours":  { "state": "done", "note": "…", "evidence": "crates/arkiv-node/src/signed/rpc_reads/mod.rs:554", "checkedAt": "2026-11-19", "checkedBy": "agent-name" },
    "arkiv": { "state": "partial", "note": "…", "evidence": "…", "checkedAt": "2026-11-19", "checkedBy": "agent-name" }
  },
  "updatedAt": "2026-11-19T12:00:00Z"
}
```

- `state` is one of `done`, `partial`, `in-progress`, `planned`, `missing`,
  `na` or `unknown`. Completion counts `done` as 1 and `partial` or
  `in-progress` as half. `na` rows are left out.
- `evidence` is a file and line, a commit, a document section or an
  `https://` link. Base every state on evidence you have checked, not on
  memory.
- `area` should be one of the areas listed in `board/meta`, which also sets
  their order. Areas not listed there sort after them.
- Write `updatedAt` on every change, so the page can show the last update.
- Update a row with `update`, pinned with `if_version` from your last read,
  so that two agents never overwrite each other. Use `batch` for more than a
  couple of rows.

`board/meta` holds the board's title, description, the two columns
(`subjects`: `id`, `name`, `detail`) and the area order. The status keys
`ours` and `arkiv` must match the subject ids.
