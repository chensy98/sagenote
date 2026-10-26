# Notebook format

## Schema

`.sagenote`, JSON, serde, `format_version: 1`:

```json
{"format_version":1,
 "meta":{"created":"...","sage_version":"..."},
 "cells":[{"id":"<uuid>","source":"...",
           "outputs":[{"mime":"text/latex","data":"...","display":"block","stale":false}],
           "state":"ok"}]}
```

## Save semantics

Atomic save: temp file in the destination directory, fsync, `fs::rename`
(atomic within one filesystem on both targets).

Dirty flag, close confirm, "unsaved changes" in the title bar.

## Load semantics

Loading sets `stale = true` on every output and normalises transient
`queued`/`running`/`interrupted` states to `idle`. The `stale` flag is in the
format so the decision is reversible.

`NotebookCodec` trait (`decode`/`encode`): `SagenoteCodec` implemented;
`IpynbCodec` returns a clear "not implemented in this prototype" error. Hooks
only.

## Codec dispatch

`codec_for_path` dispatches on the file extension: `.ipynb` → `IpynbCodec`,
anything else (including `.sagenote`) → `SagenoteCodec`.
